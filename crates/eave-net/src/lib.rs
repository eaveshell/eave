//! Networking for eave plugins, straight from the kernel over netlink: rtnetlink for links,
//! addresses and routes, nl80211 for Wi-Fi. It works the same under NetworkManager, iwd,
//! systemd-networkd, dhcpcd or anything else, because they all configure the same kernel
//! ([D-23](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).
//!
//! One thread serves every widget. It blocks on rtnetlink's multicast groups, so a change
//! shows at once, and re-reads the Wi-Fi signal every few seconds while the primary connection
//! is Wi-Fi (the kernel sends no event for signal changes).

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use netlink_packet_route::address::{AddressAttribute, AddressMessage, AddressScope};
use netlink_packet_route::link::{
    InfoKind, LinkAttribute, LinkFlags, LinkInfo, LinkLayerType, LinkMessage, State as OperState,
};
use netlink_packet_route::route::{
    RouteAddress, RouteAttribute, RouteHeader, RouteMessage, RouteType,
};
use netlink_packet_route::{AddressFamily, RouteNetlinkMessage};
use rustix::net as socket;
use tokio::sync::watch as channel;

use netlink::{Requests, open};

mod netlink;
mod nl80211;

pub use futures_util::Stream;

/// The machine's connection to the outside world.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Network {
    pub state: State,
    /// The interface carrying the default route, when there is one.
    pub primary: Option<Connection>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Offline,
    /// A physical link is up, but there's no default route yet (associating, DHCP).
    Connecting,
    Online,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Connection {
    pub interface: String,
    pub kind: Kind,
    /// `192.168.1.15/24`. IPv4 when there is one, else a global IPv6 address.
    pub address: Option<String>,
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Kind {
    Wifi {
        ssid: String,
        /// Percent, from the signal level.
        strength: u8,
        /// MHz.
        frequency: u32,
    },
    Wired,
    Vpn,
    #[default]
    Other,
}

/// The network now, then again whenever it changes.
pub fn watch() -> impl Stream<Item = Network> {
    let receiver = networks().subscribe();
    stream::unfold((receiver, true), |(mut receiver, first)| async move {
        if !first {
            receiver.changed().await.ok()?;
        }
        let network = receiver.borrow_and_update().clone();
        Some((network, (receiver, false)))
    })
    .boxed()
}

fn networks() -> &'static channel::Sender<Network> {
    static NETWORKS: OnceLock<channel::Sender<Network>> = OnceLock::new();
    NETWORKS.get_or_init(|| {
        let (sender, _) = channel::channel(Network::default());
        let networks = sender.clone();
        std::thread::Builder::new()
            .name("eave-net".into())
            .spawn(move || {
                // Listening only stops on an error; start again rather than leave the bar
                // showing the last state forever.
                let mut retry = Duration::from_secs(1);
                loop {
                    if let Err(error) = listen(&networks) {
                        eprintln!("eave-net: {error}");
                    }
                    std::thread::sleep(retry);
                    retry = (retry * 2).min(Duration::from_secs(30));
                }
            })
            .expect("spawning a thread");
        sender
    })
}

/// How often the Wi-Fi signal is re-read while on Wi-Fi.
const SIGNAL_REFRESH: Duration = Duration::from_secs(10);

// rtnetlink multicast groups (the legacy `RTMGRP_*` bitmask).
const RTMGRP_LINK: u32 = 0x1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV4_ROUTE: u32 = 0x40;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;
const RTMGRP_IPV6_ROUTE: u32 = 0x400;

fn listen(networks: &channel::Sender<Network>) -> io::Result<()> {
    let events = open(
        None,
        RTMGRP_LINK
            | RTMGRP_IPV4_IFADDR
            | RTMGRP_IPV4_ROUTE
            | RTMGRP_IPV6_IFADDR
            | RTMGRP_IPV6_ROUTE,
    )?;
    let mut reader = Reader::new()?;
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        let network = reader.read().unwrap_or_else(|error| {
            eprintln!("eave-net: reading the network: {error}");
            Network::default()
        });
        let on_wifi = matches!(
            network.primary,
            Some(Connection {
                kind: Kind::Wifi { .. },
                ..
            })
        );
        networks.send_if_modified(|current| {
            let changed = *current != network;
            *current = network;
            changed
        });

        // Block until something changes, or until it's time to re-read the signal.
        let timeout = on_wifi.then_some(SIGNAL_REFRESH);
        socket::sockopt::set_socket_timeout(&events, socket::sockopt::Timeout::Recv, timeout)?;
        match socket::recv(&events, &mut buffer, socket::RecvFlags::empty()) {
            Ok(_) => {
                // A change is usually a burst (link, then address, then route); let it settle
                // and read once for all of it.
                socket::sockopt::set_socket_timeout(
                    &events,
                    socket::sockopt::Timeout::Recv,
                    Some(Duration::from_millis(50)),
                )?;
                while socket::recv(&events, &mut buffer, socket::RecvFlags::empty()).is_ok() {}
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            // Overrun: we missed events, which a full re-read covers.
            Err(rustix::io::Errno::NOBUFS) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Reads the whole picture, one dump at a time.
struct Reader {
    route: Requests,
    /// `None` without Wi-Fi support in the kernel.
    wifi: Option<nl80211::Nl80211>,
}

impl Reader {
    fn new() -> io::Result<Self> {
        let wifi = nl80211::Nl80211::new().unwrap_or_else(|error| {
            eprintln!("eave-net: nl80211: {error}");
            None
        });
        Ok(Self {
            route: Requests::new(None)?,
            wifi,
        })
    }

    fn read(&mut self) -> io::Result<Network> {
        let links = self.links()?;
        // A route through a link that's down (a Wi-Fi that lost its network, a pulled cable)
        // goes nowhere, even while it lingers.
        let mut routes = self.default_routes()?;
        routes.retain(|route| links.get(&route.interface).is_some_and(|link| link.up));
        loop {
            let Some(route) = primary(&routes).cloned() else {
                let connecting = links.values().any(Link::is_physical_and_up);
                let state = if connecting {
                    State::Connecting
                } else {
                    State::Offline
                };
                return Ok(Network {
                    state,
                    primary: None,
                });
            };
            let link = links.get(&route.interface);

            let kind = match self
                .wifi
                .as_mut()
                .map(|wifi| wifi.association(route.interface))
            {
                Some(Ok(Some(association))) => match association.ssid {
                    Some(ssid) => Kind::Wifi {
                        ssid,
                        strength: association.signal.map_or(0, strength),
                        frequency: association.frequency,
                    },
                    // Wi-Fi that isn't on a network: its route goes nowhere.
                    None => {
                        routes.retain(|other| other.interface != route.interface);
                        continue;
                    }
                },
                Some(Err(error)) => {
                    eprintln!("eave-net: nl80211: {error}");
                    Kind::Other
                }
                _ => link.map_or(Kind::Other, Link::kind),
            };

            return Ok(Network {
                state: State::Online,
                primary: Some(Connection {
                    interface: link.map(|link| link.name.clone()).unwrap_or_default(),
                    kind,
                    address: self.address(route.interface)?,
                    gateway: route.gateway.map(|gateway| gateway.to_string()),
                }),
            });
        }
    }

    fn links(&mut self) -> io::Result<HashMap<u32, Link>> {
        let replies = self.route.dump(
            RTM_GETLINK,
            RouteNetlinkMessage::GetLink(LinkMessage::default()),
        )?;
        Ok(replies
            .into_iter()
            .filter_map(|reply| match reply {
                RouteNetlinkMessage::NewLink(link) => Some((link.header.index, Link::from(link))),
                _ => None,
            })
            .collect())
    }

    fn default_routes(&mut self) -> io::Result<Vec<DefaultRoute>> {
        let replies = self.route.dump(
            RTM_GETROUTE,
            RouteNetlinkMessage::GetRoute(RouteMessage::default()),
        )?;
        Ok(replies
            .into_iter()
            .filter_map(|reply| match reply {
                RouteNetlinkMessage::NewRoute(route) => DefaultRoute::from(route),
                _ => None,
            })
            .collect())
    }

    /// The address to show for `interface`: IPv4 first, else a global IPv6 one.
    fn address(&mut self, interface: u32) -> io::Result<Option<String>> {
        let replies = self.route.dump(
            RTM_GETADDR,
            RouteNetlinkMessage::GetAddress(AddressMessage::default()),
        )?;
        let mut addresses: Vec<(bool, String)> = replies
            .into_iter()
            .filter_map(|reply| match reply {
                RouteNetlinkMessage::NewAddress(address) => Some(address),
                _ => None,
            })
            .filter(|address| {
                address.header.index == interface && address.header.scope == AddressScope::Universe
            })
            .filter_map(|address| {
                let ip = address
                    .attributes
                    .iter()
                    .find_map(|attribute| match attribute {
                        AddressAttribute::Local(ip) => Some(*ip),
                        _ => None,
                    });
                let ip = ip.or_else(|| {
                    address
                        .attributes
                        .iter()
                        .find_map(|attribute| match attribute {
                            AddressAttribute::Address(ip) => Some(*ip),
                            _ => None,
                        })
                })?;
                Some((ip.is_ipv4(), format!("{ip}/{}", address.header.prefix_len)))
            })
            .collect();
        // IPv4 first; the sort is stable, so the kernel's order holds within a family.
        addresses.sort_by_key(|(v4, _)| !v4);
        Ok(addresses.into_iter().next().map(|(_, address)| address))
    }
}

const RTM_GETLINK: u16 = 18;
const RTM_GETADDR: u16 = 22;
const RTM_GETROUTE: u16 = 26;

/// What we need from a link.
#[derive(Debug)]
struct Link {
    name: String,
    up: bool,
    loopback: bool,
    ether: bool,
    /// Virtual links say what they are (bridge, wireguard, tun…); physical ones say nothing.
    info_kind: Option<InfoKind>,
}

impl From<LinkMessage> for Link {
    fn from(message: LinkMessage) -> Self {
        let mut link = Link {
            name: String::new(),
            up: false,
            loopback: message.header.flags.contains(LinkFlags::Loopback),
            ether: message.header.link_layer_type == LinkLayerType::Ether,
            info_kind: None,
        };
        let lower_up = message.header.flags.contains(LinkFlags::LowerUp);
        for attribute in message.attributes {
            match attribute {
                LinkAttribute::IfName(name) => link.name = name,
                LinkAttribute::OperState(state) => {
                    link.up = matches!(state, OperState::Up)
                        || (matches!(state, OperState::Unknown) && lower_up);
                }
                LinkAttribute::LinkInfo(infos) => {
                    link.info_kind = infos.into_iter().find_map(|info| match info {
                        LinkInfo::Kind(kind) => Some(kind),
                        _ => None,
                    });
                }
                _ => {}
            }
        }
        link
    }
}

impl Link {
    fn is_physical_and_up(&self) -> bool {
        self.up && self.ether && !self.loopback && self.info_kind.is_none()
    }

    /// The kind of a link that isn't Wi-Fi.
    fn kind(&self) -> Kind {
        match &self.info_kind {
            Some(InfoKind::Wireguard | InfoKind::Tun) => Kind::Vpn,
            Some(InfoKind::Other(kind)) if kind == "tun" || kind == "wireguard" => Kind::Vpn,
            None if self.ether => Kind::Wired,
            _ => Kind::Other,
        }
    }
}

/// A default route in the main table.
#[derive(Debug, Clone, PartialEq)]
struct DefaultRoute {
    interface: u32,
    gateway: Option<IpAddr>,
    metric: u32,
    v4: bool,
}

impl DefaultRoute {
    fn from(message: RouteMessage) -> Option<Self> {
        let RouteHeader {
            address_family,
            destination_prefix_length,
            table,
            kind,
            ..
        } = message.header;
        let v4 = match address_family {
            AddressFamily::Inet => true,
            AddressFamily::Inet6 => false,
            _ => return None,
        };
        if destination_prefix_length != 0 || kind != RouteType::Unicast {
            return None;
        }
        let mut route = DefaultRoute {
            interface: 0,
            gateway: None,
            metric: 0,
            v4,
        };
        let mut table = u32::from(table);
        for attribute in message.attributes {
            match attribute {
                RouteAttribute::Oif(index) => route.interface = index,
                RouteAttribute::Priority(metric) => route.metric = metric,
                RouteAttribute::Table(id) => table = id,
                RouteAttribute::Gateway(RouteAddress::Inet(ip)) => route.gateway = Some(ip.into()),
                RouteAttribute::Gateway(RouteAddress::Inet6(ip)) => route.gateway = Some(ip.into()),
                _ => {}
            }
        }
        (table == u32::from(RouteHeader::RT_TABLE_MAIN) && route.interface != 0).then_some(route)
    }
}

/// The route traffic actually takes: the lowest-metric IPv4 default, else the IPv6 one.
fn primary(routes: &[DefaultRoute]) -> Option<&DefaultRoute> {
    routes.iter().min_by_key(|route| (!route.v4, route.metric))
}

/// Signal level in dBm to percent, the way NetworkManager shows it: -100 dBm or worse is 0,
/// -50 dBm or better is 100.
fn strength(dbm: i8) -> u8 {
    (2 * (i32::from(dbm) + 100)).clamp(0, 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(interface: u32, metric: u32, v4: bool) -> DefaultRoute {
        DefaultRoute {
            interface,
            gateway: None,
            metric,
            v4,
        }
    }

    #[test]
    fn primary_is_the_cheapest_ipv4_default() {
        let routes = [
            route(2, 600, true),
            route(3, 100, true),
            route(4, 50, false),
        ];
        assert_eq!(primary(&routes).map(|r| r.interface), Some(3));
        assert_eq!(
            primary(&[route(4, 50, false)]).map(|r| r.interface),
            Some(4)
        );
        assert_eq!(primary(&[]), None);
    }

    #[test]
    fn maps_signal_to_percent() {
        assert_eq!(strength(-40), 100);
        assert_eq!(strength(-53), 94);
        assert_eq!(strength(-75), 50);
        assert_eq!(strength(-110), 0);
    }
}

#[cfg(test)]
mod live {
    /// `cargo test -p eave-net -- --ignored --nocapture` prints what this machine looks like.
    #[test]
    #[ignore = "reads the real network"]
    fn reads_this_machine() {
        let network = super::Reader::new().unwrap().read().unwrap();
        println!("{network:#?}");
    }
}
