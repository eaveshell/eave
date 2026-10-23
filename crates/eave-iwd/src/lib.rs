//! Wi-Fi controls for eave plugins, through iwd's D-Bus API
//! ([D-42](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)): the networks
//! in range, scanning, joining (with a password, through an agent eave serves), leaving,
//! forgetting, and turning Wi-Fi on and off.
//!
//! The kernel tells eave-net what the connection is; this is only for changing it. Without iwd, or
//! without permission to use it (iwd allows root and the `wheel` and `network` groups), [`watch`]
//! says so instead of failing, and the plugin shows what the kernel knows.

use std::collections::HashMap;

use eave_dbus::{Snapshot, Stream};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

mod agent;

pub use agent::connect_with;
pub use zbus;

const SERVICE: &str = "net.connman.iwd";
const DEVICE: &str = "net.connman.iwd.Device";
const STATION: &str = "net.connman.iwd.Station";
const NETWORK: &str = "net.connman.iwd.Network";
const KNOWN: &str = "net.connman.iwd.KnownNetwork";

/// What iwd says about Wi-Fi.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wifi {
    pub access: Access,
    /// The first Wi-Fi device, when there is one.
    pub station: Option<Station>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Access {
    /// iwd answers.
    Ready,
    /// iwd isn't running.
    #[default]
    Absent,
    /// iwd is running, but its bus policy turns us away.
    Denied,
}

/// A Wi-Fi device in station mode, and what it sees.
#[derive(Debug, Clone, PartialEq)]
pub struct Station {
    /// The device's object, for [`set_powered`], [`scan`] and [`disconnect`].
    pub device: OwnedObjectPath,
    /// Like `wlan0`.
    pub interface: String,
    /// The device's MAC address.
    pub address: String,
    pub powered: bool,
    pub state: State,
    pub scanning: bool,
    /// Strongest first, the connected one among them.
    pub networks: Vec<Network>,
}

/// Where a station is with its network. iwd's `State`, the one word on whether Wi-Fi is
/// connected: a network's own `Connected` can lag behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    /// Moving to another access point of the same network; still connected.
    Roaming,
    Disconnecting,
}

impl State {
    fn from_iwd(state: &str) -> Self {
        match state {
            "connected" => Self::Connected,
            "connecting" => Self::Connecting,
            "roaming" => Self::Roaming,
            "disconnecting" => Self::Disconnecting,
            _ => Self::Disconnected,
        }
    }

    /// On a network, roaming or not.
    pub fn is_connected(self) -> bool {
        matches!(self, Self::Connected | Self::Roaming)
    }
}

/// A network in range.
#[derive(Debug, Clone, PartialEq)]
pub struct Network {
    pub path: OwnedObjectPath,
    pub name: String,
    pub security: Security,
    /// Signal, 0 to 100.
    pub strength: u8,
    /// Signal in dBm.
    pub signal: i16,
    /// Set when iwd remembers it.
    pub known: Option<Known>,
    pub connected: bool,
    /// Being joined, by eave or by iwd on its own.
    pub connecting: bool,
}

/// A network iwd remembers.
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub path: OwnedObjectPath,
    pub autoconnect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Open,
    /// A password (WPA personal).
    Psk,
    /// A company login (WPA enterprise), which needs more than a password.
    Eap,
    Wep,
}

impl Security {
    fn from_iwd(kind: &str) -> Self {
        match kind {
            "open" => Self::Open,
            "8021x" => Self::Eap,
            "wep" => Self::Wep,
            _ => Self::Psk,
        }
    }
}

/// What iwd says about the connected network, for a details page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diagnostics {
    /// The access point's MAC address.
    pub bssid: Option<String>,
    /// MHz.
    pub frequency: Option<u32>,
    pub channel: Option<u16>,
    /// Like `WPA2-Personal`.
    pub security: Option<String>,
    /// dBm.
    pub rssi: Option<i16>,
    /// Mbit/s.
    pub tx_rate: Option<f32>,
    pub rx_rate: Option<f32>,
}

/// Wi-Fi now, once: for a widget that wants to know what there is without watching it.
pub async fn read() -> Option<Wifi> {
    use futures_util::StreamExt;
    std::pin::pin!(watch()).next().await
}

/// Wi-Fi now, then again after every change iwd signals, and when iwd starts or stops. iwd is
/// quiet unless something happens, and chatty while it scans.
pub fn watch() -> impl Stream<Item = Wifi> {
    eave_dbus::watch_all(SERVICE, snapshot)
}

async fn snapshot(connection: Connection) -> zbus::Result<Snapshot<Wifi>> {
    let objects = match eave_dbus::managed_objects(&connection, SERVICE, "/").await {
        Ok(objects) => objects,
        Err(error) => {
            let access = if eave_dbus::is_access_denied(&error) {
                Access::Denied
            } else if eave_dbus::is_unavailable(&error) {
                Access::Absent
            } else {
                return Err(error);
            };
            return Ok(Snapshot {
                state: Wifi {
                    access,
                    station: None,
                },
                watch: Vec::new(),
            });
        }
    };
    let objects = Objects::from(objects);
    let device = objects.first_device();
    let ordered = match device
        .as_ref()
        .filter(|device| objects.has(device, STATION))
    {
        Some(device) => ordered_networks(&connection, device)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let station = device.map(|device| objects.station(&device, &ordered));
    Ok(Snapshot {
        state: Wifi {
            access: Access::Ready,
            station,
        },
        watch: Vec::new(),
    })
}

/// The networks a station sees, strongest first, with their signal in hundredths of a dBm.
async fn ordered_networks(
    connection: &Connection,
    device: &OwnedObjectPath,
) -> zbus::Result<Vec<(OwnedObjectPath, i16)>> {
    let reply = connection
        .call_method(
            Some(SERVICE),
            device,
            Some(STATION),
            "GetOrderedNetworks",
            &(),
        )
        .await?;
    reply.body().deserialize()
}

type Properties = HashMap<String, OwnedValue>;

/// iwd's objects, by path, then interface.
struct Objects(HashMap<String, HashMap<String, Properties>>);

impl From<zbus::fdo::ManagedObjects> for Objects {
    fn from(objects: zbus::fdo::ManagedObjects) -> Self {
        Self(
            objects
                .into_iter()
                .map(|(path, interfaces)| {
                    let interfaces = interfaces
                        .into_iter()
                        .map(|(name, properties)| (name.to_string(), properties))
                        .collect();
                    (path.to_string(), interfaces)
                })
                .collect(),
        )
    }
}

fn get<T: TryFrom<OwnedValue> + Default>(properties: &Properties, name: &str) -> T {
    properties
        .get(name)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| T::try_from(value).ok())
        .unwrap_or_default()
}

fn object_path(properties: &Properties, name: &str) -> Option<OwnedObjectPath> {
    let value = properties.get(name)?.try_clone().ok()?;
    let path = OwnedObjectPath::try_from(value).ok()?;
    (path.as_str() != "/").then_some(path)
}

impl Objects {
    fn interface(&self, path: &str, interface: &str) -> Option<&Properties> {
        self.0.get(path)?.get(interface)
    }

    fn has(&self, path: &OwnedObjectPath, interface: &str) -> bool {
        self.interface(path.as_str(), interface).is_some()
    }

    /// The first Wi-Fi device in station mode, by path, so it's the same one every time.
    fn first_device(&self) -> Option<OwnedObjectPath> {
        let first = self
            .0
            .iter()
            .filter(|(_, interfaces)| {
                interfaces
                    .get(DEVICE)
                    .is_some_and(|device| get::<String>(device, "Mode") == "station")
            })
            .map(|(path, _)| path)
            .min()?;
        OwnedObjectPath::try_from(first.as_str()).ok()
    }

    fn station(&self, device: &OwnedObjectPath, ordered: &[(OwnedObjectPath, i16)]) -> Station {
        let empty = Properties::new();
        let properties = self.interface(device.as_str(), DEVICE).unwrap_or(&empty);
        let station = self.interface(device.as_str(), STATION);
        let state = State::from_iwd(
            &station
                .map(|station| get::<String>(station, "State"))
                .unwrap_or_default(),
        );
        let current = station.and_then(|station| object_path(station, "ConnectedNetwork"));
        let networks = ordered
            .iter()
            .filter_map(|(path, signal)| {
                let mut network = self.network(path, *signal)?;
                let current = current.as_ref() == Some(path);
                network.connected = current && state.is_connected();
                network.connecting = current && state == State::Connecting;
                Some(network)
            })
            .collect();
        Station {
            device: device.clone(),
            interface: get(properties, "Name"),
            address: get(properties, "Address"),
            powered: get(properties, "Powered"),
            state,
            scanning: station.is_some_and(|station| get(station, "Scanning")),
            networks,
        }
    }

    fn network(&self, path: &OwnedObjectPath, signal: i16) -> Option<Network> {
        let network = self.interface(path.as_str(), NETWORK)?;
        let known = object_path(network, "KnownNetwork").map(|known| {
            let autoconnect = self
                .interface(known.as_str(), KNOWN)
                .is_some_and(|properties| get(properties, "AutoConnect"));
            Known {
                path: known,
                autoconnect,
            }
        });
        // GetOrderedNetworks gives hundredths of a dBm.
        let dbm = signal / 100;
        Some(Network {
            path: path.clone(),
            name: get(network, "Name"),
            security: Security::from_iwd(&get::<String>(network, "Type")),
            strength: strength(dbm),
            signal: dbm,
            known,
            connected: get(network, "Connected"),
            connecting: false,
        })
    }
}

/// A signal in dBm as 0 to 100, the way eave-net reads the kernel's: -100 dBm is nothing, -50 is
/// full.
fn strength(dbm: i16) -> u8 {
    (2 * (i32::from(dbm) + 100)).clamp(0, 100) as u8
}

async fn call(
    path: &OwnedObjectPath,
    interface: &'static str,
    method: &'static str,
) -> zbus::Result<()> {
    let connection = eave_dbus::system().await?;
    connection
        .call_method(Some(SERVICE), path, Some(interface), method, &())
        .await?;
    Ok(())
}

async fn set(
    path: &OwnedObjectPath,
    interface: &'static str,
    name: &'static str,
    value: bool,
) -> zbus::Result<()> {
    let connection = eave_dbus::system().await?;
    let properties = zbus::fdo::PropertiesProxy::builder(&connection)
        .destination(SERVICE)?
        .path(path.as_ref())?
        .build()
        .await?;
    let interface = zbus::names::InterfaceName::try_from(interface)?;
    properties
        .set(interface, name, zbus::zvariant::Value::from(value))
        .await?;
    Ok(())
}

/// Looks for networks again. iwd answers at once and reports what it finds as it goes.
pub async fn scan(station: &Station) -> zbus::Result<()> {
    call(&station.device, STATION, "Scan").await
}

/// Joins a network iwd remembers, or an open one. iwd leaves the current network itself. If a
/// remembered password no longer works, iwd asks eave's agent, which says no: the join fails
/// and the password can be typed again.
pub async fn connect(network: &Network) -> zbus::Result<()> {
    agent::register().await?;
    call(&network.path, NETWORK, "Connect").await
}

/// Leaves the network. iwd then stops joining networks on its own until the next join. Already
/// off a network is done.
pub async fn disconnect(station: &Station) -> zbus::Result<()> {
    match call(&station.device, STATION, "Disconnect").await {
        Err(error) if error_name(&error) == Some("NotConnected") => Ok(()),
        result => result,
    }
}

/// Turns the Wi-Fi device off and on again. Broadcom firmware (on Apple Silicon Macs) can get
/// stuck failing every join until it's restarted; this restarts it without a reboot.
pub async fn restart(station: &Station) -> zbus::Result<()> {
    set_powered(station, false).await?;
    set_powered(station, true).await
}

/// iwd's short name for an error, like `Failed` or `NotConnected`.
fn error_name(error: &zbus::Error) -> Option<&str> {
    match error {
        zbus::Error::MethodError(name, ..) => name.as_str().rsplit('.').next(),
        _ => None,
    }
}

/// The join failed in a way a different password might fix.
pub fn wants_password(error: &zbus::Error) -> bool {
    matches!(
        error_name(error),
        Some("Failed" | "AuthenticationFailed" | "InvalidFormat" | "Canceled")
    )
}

/// The join was replaced by another one before it finished: not a failure to show.
pub fn superseded(error: &zbus::Error) -> bool {
    error_name(error) == Some("Aborted")
}

/// Forgets a remembered network: its password, and joining it on its own.
pub async fn forget(known: &Known) -> zbus::Result<()> {
    call(&known.path, KNOWN, "Forget").await
}

/// Whether iwd joins a remembered network on its own when it's in range.
pub async fn set_autoconnect(known: &Known, on: bool) -> zbus::Result<()> {
    set(&known.path, KNOWN, "AutoConnect", on).await
}

/// Turns the Wi-Fi device on or off.
pub async fn set_powered(station: &Station, on: bool) -> zbus::Result<()> {
    set(&station.device, DEVICE, "Powered", on).await
}

/// What iwd knows about the connection right now.
pub async fn diagnostics(station: &Station) -> zbus::Result<Diagnostics> {
    let connection = eave_dbus::system().await?;
    let reply = connection
        .call_method(
            Some(SERVICE),
            &station.device,
            Some("net.connman.iwd.StationDiagnostic"),
            "GetDiagnostics",
            &(),
        )
        .await?;
    let properties: Properties = reply.body().deserialize()?;
    let some = |properties: &Properties, name: &str| properties.contains_key(name);
    Ok(Diagnostics {
        bssid: some(&properties, "ConnectedBss").then(|| get(&properties, "ConnectedBss")),
        frequency: some(&properties, "Frequency").then(|| get(&properties, "Frequency")),
        channel: some(&properties, "Channel").then(|| get(&properties, "Channel")),
        security: some(&properties, "Security").then(|| get(&properties, "Security")),
        rssi: some(&properties, "RSSI").then(|| get(&properties, "RSSI")),
        // iwd reports bitrates in 100 kbit/s.
        tx_rate: some(&properties, "TxBitrate")
            .then(|| get::<u32>(&properties, "TxBitrate") as f32 / 10.0),
        rx_rate: some(&properties, "RxBitrate")
            .then(|| get::<u32>(&properties, "RxBitrate") as f32 / 10.0),
    })
}

/// A short reason an iwd call failed, fit to show under a password field.
pub fn reason(error: &zbus::Error) -> &'static str {
    let Some(name) = error_name(error) else {
        return "Couldn't reach iwd";
    };
    match name {
        "InvalidFormat" => "That password isn't valid for this network",
        "Failed" | "AuthenticationFailed" => "Couldn't join. Check the password and try again",
        "Aborted" | "Canceled" => "Joining was cancelled",
        "Busy" | "InProgress" => "Already joining a network",
        "NotSupported" => "This network's security isn't supported",
        "NotConfigured" => "This network needs setting up first",
        "AccessDenied" => "Not allowed to change Wi-Fi",
        "Timeout" => "The network didn't answer",
        "NotFound" => "The network is out of range",
        "NoAgent" => "No one to ask for the password",
        "NotAvailable" => "Wi-Fi is off",
        _ => "Couldn't join the network",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<T: Into<zbus::zvariant::Value<'static>>>(value: T) -> OwnedValue {
        OwnedValue::try_from(value.into()).unwrap()
    }

    fn object(path: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(path).unwrap()
    }

    fn fixture() -> Objects {
        let mut objects = HashMap::new();
        let mut device = HashMap::new();
        device.insert(
            DEVICE.to_string(),
            Properties::from([
                ("Name".into(), value("wlan0")),
                ("Address".into(), value("c8:89:f3:db:f8:c1")),
                ("Powered".into(), value(true)),
                ("Mode".into(), value("station")),
            ]),
        );
        device.insert(
            STATION.to_string(),
            Properties::from([
                ("State".into(), value("connected")),
                ("Scanning".into(), value(false)),
                (
                    "ConnectedNetwork".into(),
                    value(object("/net/connman/iwd/0/2/home_psk")),
                ),
            ]),
        );
        objects.insert("/net/connman/iwd/0/2".to_string(), device);
        let network = |name: &str, kind: &str, connected: bool, known: Option<&str>| {
            let mut properties = Properties::from([
                ("Name".into(), value(name.to_string())),
                ("Type".into(), value(kind.to_string())),
                ("Connected".into(), value(connected)),
            ]);
            if let Some(known) = known {
                properties.insert("KnownNetwork".into(), value(object(known)));
            }
            HashMap::from([(NETWORK.to_string(), properties)])
        };
        objects.insert(
            "/net/connman/iwd/0/2/home_psk".into(),
            network("Home", "psk", true, Some("/net/connman/iwd/home_psk")),
        );
        objects.insert(
            "/net/connman/iwd/0/2/cafe_open".into(),
            network("Cafe", "open", false, None),
        );
        objects.insert(
            "/net/connman/iwd/home_psk".into(),
            HashMap::from([(
                KNOWN.to_string(),
                Properties::from([("AutoConnect".into(), value(true))]),
            )]),
        );
        Objects(objects)
    }

    #[test]
    fn reads_the_station_and_its_networks_in_order() {
        let objects = fixture();
        let device = objects.first_device().unwrap();
        assert_eq!(device.as_str(), "/net/connman/iwd/0/2");
        let ordered = [
            (object("/net/connman/iwd/0/2/home_psk"), -4100),
            (object("/net/connman/iwd/0/2/cafe_open"), -8000),
            (object("/net/connman/iwd/0/2/gone_psk"), -9000),
        ];
        let station = objects.station(&device, &ordered);
        assert_eq!(station.interface, "wlan0");
        assert!(station.powered);
        assert_eq!(station.state, State::Connected);
        assert_eq!(
            station.networks.len(),
            2,
            "a network that's gone is skipped"
        );
        let home = &station.networks[0];
        assert_eq!(home.name, "Home");
        assert!(home.connected);
        assert_eq!(home.security, Security::Psk);
        assert_eq!(home.signal, -41);
        assert_eq!(home.strength, 100);
        assert!(home.known.as_ref().is_some_and(|known| known.autoconnect));
        let cafe = &station.networks[1];
        assert_eq!(cafe.security, Security::Open);
        assert_eq!(cafe.known, None);
        assert_eq!(cafe.strength, 40);
    }

    /// Prints what iwd says on this machine. Reads only.
    /// `cargo test -p eave-iwd -- --ignored --nocapture`
    #[test]
    #[ignore = "needs iwd"]
    fn live() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use futures_util::StreamExt;
            let wifi = std::pin::pin!(watch()).next().await.unwrap();
            println!("{wifi:#?}");
            if let Some(station) = &wifi.station {
                println!("{:#?}", diagnostics(station).await);
            }
        });
    }

    #[test]
    fn signal_maps_like_the_kernels() {
        assert_eq!(strength(-100), 0);
        assert_eq!(strength(-75), 50);
        assert_eq!(strength(-50), 100);
        assert_eq!(strength(-30), 100);
        assert_eq!(strength(-110), 0);
    }

    #[test]
    fn errors_read_as_reasons() {
        let error = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("net.connman.iwd.Failed").unwrap(),
            None,
            zbus::Message::method_call("/", "x")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert_eq!(
            reason(&error),
            "Couldn't join. Check the password and try again"
        );
    }
}
