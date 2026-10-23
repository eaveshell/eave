//! The primary network connection: a Wi-Fi icon with as many bars as the signal, a wired icon,
//! VPN, or a struck-through Wi-Fi icon when offline. Read from the kernel through `eave-net`, so it
//! works under any network manager.
//!
//! The popover lists the Wi-Fi networks in range, the current one first, and joins, leaves and
//! forgets them through iwd ([D-42](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)),
//! asking for a password when a network needs one. Each network has a details page. Without iwd
//! it shows what the kernel knows: the connection, its address and gateway.
//!
//! In the dormer it turns Wi-Fi on and off, restarts it, leaves the network it's on, and joins
//! saved and open networks in range.
//!
//! iwd is watched all the time, so the dormer acts on what's true now, and the bar item never
//! says online while iwd says the station isn't on a network.

use std::fmt;

use eave_api::ui::{self, Action, Icon, Item, Node, Panel, Tone};
use eave_api::{Effect, Registry, Subscription, Task, Widget};
use eave_iwd::{Access, Diagnostics, Network as Wlan, Security, State as Link, Station, Wifi};
use eave_net::{Connection, Kind, Network as Status, State};
use serde::Deserialize;

pub fn register(registry: &mut Registry) {
    registry.widget::<Network>("network");
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Show the Wi-Fi network name in the bar, next to the icon.
    show_name: bool,
}

pub struct Network {
    show_name: bool,
    status: Status,
    /// What iwd says. `None` until it first answers.
    wifi: Option<Wifi>,
    open: bool,
    page: Page,
    /// Typed on the password page; cleared on leaving it.
    password: Secret,
    revealed: bool,
    /// The network being joined, by object path.
    joining: Option<String>,
    /// The last network that couldn't be joined, and why.
    failed: Option<(String, String)>,
    /// Why the last command (power, leaving, forgetting…) failed.
    problem: Option<String>,
    /// A command on the device is running; others wait for it.
    busy: bool,
    /// Scan once iwd first answers after opening.
    scan_when_ready: bool,
    diagnostics: Option<Diagnostics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    List,
    /// A network's password, by object path.
    Password(String),
    /// A Wi-Fi network's details, by object path.
    Details(String),
    /// The wired (or other) connection's details, from the kernel.
    Connection,
}

/// A password, kept out of logs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Read(Status),
    Wifi(Wifi),
    Opened,
    Closed,
    Scan,
    Power(bool),
    /// A network's row was clicked, by object path.
    Join(String),
    Input(Secret),
    Reveal,
    Submit,
    /// Joining a network ended, by object path: how, or why not.
    Joined(String, Result<(), Failure>),
    Details(String),
    ConnectionDetails,
    Diagnostics(Option<Diagnostics>),
    Back,
    Disconnect,
    Forget(String),
    AutoJoin(String, bool),
    /// Turns Wi-Fi off and on again, for a device stuck failing every join.
    Restart,
    /// A command finished, or why it failed; what it changed arrives from iwd.
    Done(Result<(), String>),
}

/// Why a join failed, and whether typing the password again might help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    why: String,
    password: bool,
}

impl Widget for Network {
    type Config = Config;
    type Message = Message;

    fn new(config: Config) -> (Self, Task<Message>) {
        let network = Self {
            show_name: config.show_name,
            status: Status::default(),
            wifi: None,
            open: false,
            page: Page::List,
            password: Secret::default(),
            revealed: false,
            joining: None,
            failed: None,
            problem: None,
            busy: false,
            scan_when_ready: false,
            diagnostics: None,
        };
        (network, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Read(status) => self.status = status,
            Message::Wifi(wifi) => {
                self.wifi = Some(wifi);
                // Joined, however the reply is doing.
                if let Some(path) = &self.joining
                    && self.network(path).is_some_and(|network| network.connected)
                {
                    self.joining = None;
                }
                if std::mem::take(&mut self.scan_when_ready) {
                    return self.scan();
                }
            }
            Message::Opened => {
                self.open = true;
                self.page = Page::List;
                self.scan_when_ready = self.station().is_none();
                return self.scan();
            }
            Message::Closed => {
                self.open = false;
                self.leave_page();
                self.page = Page::List;
            }
            Message::Scan => return self.scan(),
            Message::Power(on) => {
                let Some(station) = self.idle_station() else {
                    return Effect::none();
                };
                return self.command(async move { eave_iwd::set_powered(&station, on).await });
            }
            Message::Restart => {
                let Some(station) = self.idle_station() else {
                    return Effect::none();
                };
                self.joining = None;
                self.failed = None;
                return self.command(async move { eave_iwd::restart(&station).await });
            }
            Message::Join(path) => {
                let Some(network) = self.network(&path).cloned() else {
                    return Effect::none();
                };
                self.failed = None;
                if network.known.is_none() && network.security == Security::Psk {
                    self.leave_page();
                    self.page = Page::Password(path);
                    return Effect::none();
                }
                self.joining = Some(path.clone());
                return Task::future(async move { eave_iwd::connect(&network).await })
                    .map(move |result| Message::Joined(path.clone(), reason(result)))
                    .into();
            }
            Message::Input(password) => self.password = password,
            Message::Reveal => self.revealed = !self.revealed,
            Message::Submit => {
                let Page::Password(path) = self.page.clone() else {
                    return Effect::none();
                };
                let Some(network) = self.network(&path).cloned() else {
                    return Effect::none();
                };
                let Secret(password) = std::mem::take(&mut self.password);
                if password.is_empty() {
                    return Effect::none();
                }
                self.revealed = false;
                self.failed = None;
                self.joining = Some(path.clone());
                self.page = Page::List;
                return Task::future(
                    async move { eave_iwd::connect_with(&network, password).await },
                )
                .map(move |result| Message::Joined(path.clone(), reason(result)))
                .into();
            }
            Message::Joined(path, result) => {
                if self.joining.as_ref() == Some(&path) {
                    self.joining = None;
                }
                if let Err(failure) = result {
                    let secured = self
                        .network(&path)
                        .is_some_and(|network| network.security == Security::Psk);
                    // Back to the password, unless they've moved on since.
                    if secured && failure.password && self.open && self.page == Page::List {
                        self.page = Page::Password(path.clone());
                    }
                    self.failed = Some((path, failure.why));
                }
            }
            Message::Details(path) => {
                self.page = Page::Details(path.clone());
                self.diagnostics = None;
                let connected = self.network(&path).is_some_and(|network| network.connected);
                if let (true, Some(station)) = (connected, self.station().cloned()) {
                    return Task::future(async move { eave_iwd::diagnostics(&station).await.ok() })
                        .map(Message::Diagnostics)
                        .into();
                }
            }
            Message::ConnectionDetails => self.page = Page::Connection,
            Message::Diagnostics(diagnostics) => self.diagnostics = diagnostics,
            Message::Back => {
                self.leave_page();
                self.page = Page::List;
            }
            Message::Disconnect => {
                let Some(station) = self.idle_station() else {
                    return Effect::none();
                };
                self.page = Page::List;
                return self.command(async move { eave_iwd::disconnect(&station).await });
            }
            Message::Forget(path) => {
                let Some(known) = self.network(&path).and_then(|n| n.known.clone()) else {
                    return Effect::none();
                };
                self.page = Page::List;
                return self.command(async move { eave_iwd::forget(&known).await });
            }
            Message::AutoJoin(path, on) => {
                let Some(known) = self.network(&path).and_then(|n| n.known.clone()) else {
                    return Effect::none();
                };
                return self.command(async move { eave_iwd::set_autoconnect(&known, on).await });
            }
            Message::Done(result) => {
                self.busy = false;
                self.problem = result.err();
            }
        }
        Effect::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let kernel = Subscription::run(eave_net::watch).map(Message::Read);
        let iwd = Subscription::run(eave_iwd::watch).map(Message::Wifi);
        Subscription::batch([kernel, iwd])
    }

    fn item(&self) -> Item<Message> {
        let state = self.state();
        let tone = match state {
            State::Online => Tone::Normal,
            State::Connecting | State::Offline => Tone::Dim,
        };
        let content = match (state, &self.status.primary) {
            (State::Connecting | State::Offline, _) | (_, None) => {
                ui::icon(Icon::Wifi { strength: None })
            }
            (State::Online, Some(connection)) => match &connection.kind {
                Kind::Wifi { ssid, strength, .. } => {
                    let icon = ui::icon(Icon::Wifi {
                        strength: Some(f32::from(*strength) / 100.0),
                    });
                    if self.show_name && !ssid.is_empty() {
                        ui::row([icon, ui::label(ssid.clone())])
                    } else {
                        icon
                    }
                }
                Kind::Wired => ui::icon(Icon::Wired),
                Kind::Vpn => ui::label("VPN"),
                Kind::Other => ui::label(connection.interface.clone()),
            },
        };
        Item::new(content.tone(tone))
    }

    fn popover(&self) -> Option<Panel<Message>> {
        // The list is the first page; a network's details or password, or the connection's, one
        // further in.
        let (order, content) = match (&self.page, self.ready()) {
            (Page::Connection, _) => (1, self.connection_page()),
            (_, None) => (0, self.kernel_page()),
            (Page::List, Some(station)) => (0, self.list_page(station)),
            (Page::Password(path), Some(_)) => match self.network(path) {
                Some(network) => (1, self.password_page(network)),
                None => (0, self.list_page(self.station()?)),
            },
            (Page::Details(path), Some(station)) => match self.network(path) {
                Some(network) => (1, self.details_page(station, network)),
                None => (0, self.list_page(station)),
            },
        };
        let panel = Panel::new(ui::page(order, content))
            .on_open(Message::Opened)
            .on_close(Message::Closed);
        Some(panel)
    }

    /// Wi-Fi on or off, leaving the network it's on, and joining the ones in range it can join
    /// without asking for a password: saved ones, and open ones.
    fn actions(&self) -> Vec<Action<Message>> {
        let Some(station) = self.ready() else {
            return Vec::new();
        };
        let words = ["wifi", "wi-fi", "wireless", "network", "internet"];
        let fan = |strength: u8| {
            ui::icon(Icon::Wifi {
                strength: Some(f32::from(strength) / 100.0),
            })
        };
        if !station.powered {
            return vec![
                ui::action("Turn Wi-Fi on", Message::Power(true))
                    .id("wifi-on")
                    .keywords(words.into_iter().chain(["enable"]))
                    .leading(fan(100)),
            ];
        }
        let mut actions = vec![
            ui::action("Turn Wi-Fi off", Message::Power(false))
                .id("wifi-off")
                .keywords(words.into_iter().chain(["disable", "airplane"]))
                .leading(ui::icon(Icon::Wifi { strength: None })),
            ui::action("Restart Wi-Fi", Message::Restart)
                .id("wifi-restart")
                .subtitle("Turns it off and on again")
                .keywords(words.into_iter().chain(["reset", "reconnect", "fix"]))
                .leading(ui::icon(Icon::Refresh)),
        ];
        for network in &station.networks {
            let path = network.path.as_str();
            if network.connected {
                actions.push(
                    ui::action(
                        format!("Disconnect from {}", network.name),
                        Message::Disconnect,
                    )
                    .subtitle("Wi-Fi")
                    .keywords(words.into_iter().chain(["leave"]))
                    .leading(fan(network.strength)),
                );
            } else if self.joining.as_deref() != Some(path)
                && (network.known.is_some() || network.security == Security::Open)
            {
                let subtitle = if network.known.is_some() {
                    "Saved Wi-Fi network"
                } else {
                    "Open Wi-Fi network"
                };
                actions.push(
                    ui::action(
                        format!("Join {}", network.name),
                        Message::Join(path.to_string()),
                    )
                    .subtitle(subtitle)
                    .keywords(words.into_iter().chain(["connect"]))
                    .leading(fan(network.strength)),
                );
            }
        }
        actions
    }
}

impl Network {
    fn station(&self) -> Option<&Station> {
        self.wifi.as_ref()?.station.as_ref()
    }

    /// The station, unless a command on it is still running: one at a time, so clicks don't
    /// pile up on a device that's slow to answer.
    fn idle_station(&self) -> Option<Station> {
        (!self.busy).then(|| self.station().cloned())?
    }

    /// The kernel's word on the connection, unless iwd knows better: a route can outlive the
    /// network it went through, and the kernel can't tell joining from offline.
    fn state(&self) -> State {
        let state = self.status.state;
        let (Some(station), Some(connection)) = (self.ready(), &self.status.primary) else {
            return state;
        };
        let on_wifi = matches!(connection.kind, Kind::Wifi { .. })
            && connection.interface == station.interface;
        match station.state {
            _ if !on_wifi || state != State::Online => state,
            Link::Connected | Link::Roaming => state,
            Link::Connecting => State::Connecting,
            Link::Disconnected | Link::Disconnecting => State::Offline,
        }
    }

    fn command(
        &mut self,
        work: impl Future<Output = eave_iwd::zbus::Result<()>> + Send + 'static,
    ) -> Effect<Message> {
        self.busy = true;
        self.problem = None;
        Task::future(async move {
            Message::Done(work.await.map_err(|error| {
                eprintln!("eave-network: {error}");
                problem(&error).to_string()
            }))
        })
        .into()
    }

    /// The station, when iwd can be used.
    fn ready(&self) -> Option<&Station> {
        let wifi = self.wifi.as_ref()?;
        (wifi.access == Access::Ready).then_some(wifi.station.as_ref()?)
    }

    fn network(&self, path: &str) -> Option<&Wlan> {
        self.station()?
            .networks
            .iter()
            .find(|network| network.path.as_str() == path)
    }

    /// Forgets what was typed on a page being left.
    fn leave_page(&mut self) {
        self.password = Secret::default();
        self.revealed = false;
    }

    fn scan(&mut self) -> Effect<Message> {
        match self.ready().filter(|station| station.powered).cloned() {
            // Not `command`: a scan iwd turns down (busy joining) isn't worth saying.
            Some(station) => Task::future(async move {
                if let Err(error) = eave_iwd::scan(&station).await {
                    eprintln!("eave-network: scan: {error}");
                }
                Message::Done(Ok(()))
            })
            .into(),
            None => Effect::none(),
        }
    }

    /// The Wi-Fi network the kernel says we're on: its band.
    fn band(&self, network: &Wlan) -> Option<&'static str> {
        let frequency = self
            .diagnostics
            .as_ref()
            .and_then(|diagnostics| diagnostics.frequency)
            .or(match &self.status.primary.as_ref()?.kind {
                Kind::Wifi {
                    ssid, frequency, ..
                } if *ssid == network.name => Some(*frequency),
                _ => None,
            })?;
        band(frequency)
    }

    fn list_page(&self, station: &Station) -> Node<Message> {
        let header = ui::header("Wi-Fi").trailing(ui::toggle(
            station.powered,
            Message::Power(!station.powered),
        ));
        let mut lines = vec![header];
        if let Some(problem) = &self.problem {
            lines.push(ui::label(problem.clone()).tone(Tone::Critical));
        }
        // A join that failed, or one that doesn't end: the device may be stuck.
        let stuck =
            self.failed.is_some() || (station.state == Link::Connecting && self.joining.is_none());
        if station.powered && stuck {
            lines.push(ui::row([
                ui::label("Not joining?").tone(Tone::Dim),
                ui::fill(),
                ui::button(ui::label("Restart Wi-Fi"), Message::Restart),
            ]));
        }
        let wired =
            self.status.primary.as_ref().filter(|connection| {
                matches!(connection.kind, Kind::Wired | Kind::Other | Kind::Vpn)
            });
        if let Some(connection) = wired {
            lines.push(ui::list([self.connection_row(connection)]));
        }
        if !station.powered {
            lines.push(ui::label("Wi-Fi is off").tone(Tone::Dim));
            return ui::column(lines);
        }
        let (current, others): (Vec<&Wlan>, Vec<&Wlan>) =
            station.networks.iter().partition(|network| {
                network.connected
                    || network.connecting
                    || self.joining.as_deref() == Some(network.path.as_str())
            });
        if !current.is_empty() {
            lines.push(ui::list(
                current.into_iter().map(|network| self.row(network)),
            ));
        }
        let refresh = if station.scanning {
            ui::label("Scanning…").tone(Tone::Dim)
        } else {
            ui::button(ui::icon(Icon::Refresh), Message::Scan)
        };
        lines.push(ui::section("Networks").trailing(refresh));
        if others.is_empty() {
            let empty = if station.scanning {
                "Looking for networks…"
            } else {
                "No networks in range"
            };
            lines.push(ui::label(empty).tone(Tone::Dim));
        } else {
            lines.push(ui::list(
                others.into_iter().map(|network| self.row(network)),
            ));
        }
        ui::column(lines)
    }

    /// A network in the list: its signal, name and what to know about it, a lock if it needs a
    /// password, and an info button for the current one and saved ones.
    fn row(&self, network: &Wlan) -> Node<Message> {
        let path = network.path.as_str().to_string();
        let joining = self.joining.as_deref() == Some(path.as_str()) || network.connecting;
        let failed = self
            .failed
            .as_ref()
            .filter(|(failed, _)| *failed == path)
            .map(|(_, why)| why.clone());
        let subtitle = if joining {
            Some("Connecting…".to_string())
        } else if let Some(why) = failed {
            Some(why)
        } else if network.connected {
            Some(match self.band(network) {
                Some(band) => format!("Connected · {band}"),
                None => "Connected".into(),
            })
        } else {
            match network.security {
                Security::Eap => Some("Needs a company login".into()),
                Security::Wep => Some("WEP isn't supported".into()),
                _ if network.known.is_some() => Some("Saved".into()),
                _ => None,
            }
        };
        let mut trailing = Vec::new();
        if network.security != Security::Open && !network.connected {
            trailing.push(ui::icon(Icon::Lock));
        }
        if network.connected || network.known.is_some() {
            trailing.push(ui::button(
                ui::icon(Icon::Info),
                Message::Details(path.clone()),
            ));
        }
        let mut row = ui::list_item(network.name.clone())
            .leading(ui::icon(Icon::Wifi {
                strength: Some(f32::from(network.strength) / 100.0),
            }))
            .selected(network.connected || joining);
        if let Some(subtitle) = subtitle {
            row = row.subtitle(subtitle);
        }
        if !trailing.is_empty() {
            row = row.trailing(ui::row(trailing));
        }
        let joinable = !network.connected
            && !joining
            && matches!(network.security, Security::Open | Security::Psk);
        if joinable {
            row = row.on_press(Message::Join(path));
        }
        row
    }

    fn connection_row(&self, connection: &Connection) -> Node<Message> {
        let (name, icon) = match &connection.kind {
            Kind::Wired => ("Ethernet".to_string(), Icon::Wired),
            Kind::Vpn => ("VPN".to_string(), Icon::Wired),
            _ => (connection.interface.clone(), Icon::Wired),
        };
        ui::list_item(name)
            .leading(ui::icon(icon))
            .subtitle(format!("Connected · {}", connection.interface))
            .selected(true)
            .trailing(ui::button(ui::icon(Icon::Info), Message::ConnectionDetails))
    }

    fn password_page(&self, network: &Wlan) -> Node<Message> {
        let failed = self
            .failed
            .as_ref()
            .filter(|(path, _)| path == network.path.as_str())
            .map(|(_, why)| why.clone());
        let field = ui::text_input("Password", self.password.0.clone(), |text| {
            Message::Input(Secret(text))
        })
        .secret(!self.revealed)
        .on_submit(Message::Submit)
        .trailing(ui::button(ui::icon(Icon::Reveal), Message::Reveal));
        let field = if failed.is_some() {
            field.tone(Tone::Critical)
        } else {
            field
        };
        let mut lines = vec![
            ui::header(network.name.clone())
                .on_back(Message::Back)
                .trailing(ui::icon(Icon::Lock).tone(Tone::Dim)),
            ui::label("This network needs a password.").tone(Tone::Dim),
            field,
        ];
        if let Some(why) = failed {
            lines.push(ui::label(why).tone(Tone::Critical));
        }
        lines.push(ui::row([
            ui::fill(),
            ui::button(ui::label("Cancel"), Message::Back),
            ui::button(ui::label("Join"), Message::Submit).prominent(),
        ]));
        ui::column(lines)
    }

    fn details_page(&self, station: &Station, network: &Wlan) -> Node<Message> {
        let path = network.path.as_str().to_string();
        let diagnostics = self.diagnostics.clone().unwrap_or_default();
        let signal = diagnostics.rssi.unwrap_or(network.signal);
        let mut lines = vec![
            ui::header(network.name.clone()).on_back(Message::Back),
            ui::detail(
                "Signal",
                ui::row([
                    ui::meter(f32::from(network.strength) / 100.0),
                    ui::label(format!("{}% · {signal} dBm", network.strength)),
                ]),
            ),
        ];
        if let Some(frequency) = diagnostics.frequency {
            let band = band(frequency).unwrap_or("Wi-Fi");
            let value = match diagnostics.channel {
                Some(channel) => format!("{band} · channel {channel}"),
                None => band.to_string(),
            };
            lines.push(ui::detail("Band", ui::label(value)));
        }
        let security = diagnostics.security.clone().unwrap_or_else(|| {
            match network.security {
                Security::Open => "None",
                Security::Psk => "Password",
                Security::Eap => "Company login",
                Security::Wep => "WEP",
            }
            .to_string()
        });
        lines.push(ui::detail("Security", ui::label(security)));
        if let Some(rate) = diagnostics.tx_rate {
            lines.push(ui::detail(
                "Link speed",
                ui::label(format!("{rate:.0} Mb/s")),
            ));
        }
        if network.connected {
            lines.push(ui::separator());
            lines.extend(self.addresses());
            lines.push(ui::detail("MAC", ui::label(station.address.clone())));
        }
        if let Some(known) = &network.known {
            lines.push(ui::separator());
            lines.push(ui::detail(
                "Join automatically",
                ui::toggle(
                    known.autoconnect,
                    Message::AutoJoin(path.clone(), !known.autoconnect),
                ),
            ));
        }
        let mut buttons = Vec::new();
        if network.connected {
            buttons.push(ui::button(ui::label("Disconnect"), Message::Disconnect));
        }
        buttons.push(ui::fill());
        if network.known.is_some() {
            buttons.push(ui::button(
                ui::label("Forget").tone(Tone::Critical),
                Message::Forget(path),
            ));
        }
        if buttons.len() > 1 {
            lines.push(ui::row(buttons));
        }
        ui::column(lines)
    }

    /// The kernel's address, gateway and interface for the primary connection.
    fn addresses(&self) -> Vec<Node<Message>> {
        let Some(connection) = &self.status.primary else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        if let Some(address) = &connection.address {
            lines.push(ui::detail("IP address", ui::label(address.clone())));
        }
        if let Some(gateway) = &connection.gateway {
            lines.push(ui::detail("Gateway", ui::label(gateway.clone())));
        }
        if !connection.interface.is_empty() {
            lines.push(ui::detail(
                "Interface",
                ui::label(connection.interface.clone()),
            ));
        }
        lines
    }

    /// The primary connection's details, from the kernel.
    fn connection_page(&self) -> Node<Message> {
        let mut lines = vec![ui::header("Connection").on_back(Message::Back)];
        lines.extend(self.addresses());
        ui::column(lines)
    }

    /// What the kernel knows, when iwd can't be used (or hasn't answered yet).
    fn kernel_page(&self) -> Node<Message> {
        let mut lines = vec![ui::header("Network")];
        match &self.status.primary {
            Some(connection) => {
                let row = match &connection.kind {
                    Kind::Wifi {
                        ssid,
                        strength,
                        frequency,
                    } => {
                        let subtitle = match band(*frequency) {
                            Some(band) => format!("Connected · {band}"),
                            None => "Connected".into(),
                        };
                        ui::list_item(ssid.clone())
                            .leading(ui::icon(Icon::Wifi {
                                strength: Some(f32::from(*strength) / 100.0),
                            }))
                            .subtitle(subtitle)
                            .selected(true)
                    }
                    _ => self.connection_row(connection).trailing(ui::empty()),
                };
                lines.push(ui::list([row]));
                lines.extend(self.addresses());
            }
            None => lines.push(ui::label("Not connected").tone(Tone::Dim)),
        }
        let hint = match self.wifi.as_ref().map(|wifi| wifi.access) {
            None => None,
            Some(Access::Ready) => Some("No Wi-Fi device"),
            Some(Access::Absent) => Some("Wi-Fi controls need iwd"),
            Some(Access::Denied) => Some("Wi-Fi controls need the wheel or network group"),
        };
        if let Some(hint) = hint {
            lines.push(ui::separator());
            lines.push(ui::label(hint).tone(Tone::Dim));
        }
        ui::column(lines)
    }
}

/// `5 GHz` for a frequency in MHz.
fn band(frequency: u32) -> Option<&'static str> {
    match frequency {
        0 => None,
        f if f >= 5925 => Some("6 GHz"),
        f if f >= 4900 => Some("5 GHz"),
        _ => Some("2.4 GHz"),
    }
}

fn reason(result: eave_iwd::zbus::Result<()>) -> Result<(), Failure> {
    match result {
        // Another join took its place; that one's result is the one to show.
        Err(error) if eave_iwd::superseded(&error) => Ok(()),
        Err(error) => {
            eprintln!("eave-network: {error}");
            Err(Failure {
                why: eave_iwd::reason(&error).to_string(),
                password: eave_iwd::wants_password(&error),
            })
        }
        Ok(()) => Ok(()),
    }
}

/// Why a command other than joining failed, in a few words.
fn problem(error: &eave_iwd::zbus::Error) -> &'static str {
    match error {
        eave_iwd::zbus::Error::MethodError(name, ..) => {
            match name.as_str().rsplit('.').next().unwrap_or("") {
                "AccessDenied" | "PermissionDenied" => "Not allowed to change Wi-Fi",
                "Busy" | "InProgress" => "Wi-Fi is busy. Try again in a moment",
                "NotAvailable" | "NotSupported" => "Wi-Fi can't do that right now",
                _ => "Wi-Fi didn't respond",
            }
        }
        eave_iwd::zbus::Error::InputOutput(_) => "Couldn't reach iwd",
        _ => "Wi-Fi didn't answer in time",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network(state: State, primary: Option<Connection>) -> Network {
        let (mut network, _) = Network::new(Config::default());
        network.update(Message::Read(Status { state, primary }));
        network
    }

    fn wifi() -> Connection {
        Connection {
            interface: "wlan0".into(),
            kind: Kind::Wifi {
                ssid: "home".into(),
                strength: 70,
                frequency: 5180,
            },
            address: Some("192.168.1.15/24".into()),
            gateway: Some("192.168.1.1".into()),
        }
    }

    #[test]
    fn a_popover_even_offline() {
        assert!(network(State::Offline, None).popover().is_some());
        assert!(network(State::Online, Some(wifi())).popover().is_some());
    }

    fn icon(network: &Network) -> Option<Icon> {
        match network.item().into_parts().content?.into_kind() {
            ui::Kind::Icon { icon, .. } => Some(icon),
            _ => None,
        }
    }

    #[test]
    fn shown_while_offline() {
        assert_eq!(
            icon(&network(State::Offline, None)),
            Some(Icon::Wifi { strength: None })
        );
    }

    #[test]
    fn wifi_shows_its_signal() {
        assert_eq!(
            icon(&network(State::Online, Some(wifi()))),
            Some(Icon::Wifi {
                strength: Some(0.7)
            })
        );
    }

    #[test]
    fn passwords_stay_out_of_logs() {
        let message = Message::Input(Secret("hunter22".into()));
        assert!(!format!("{message:?}").contains("hunter22"));
    }

    #[test]
    fn closing_forgets_the_password() {
        let mut network = network(State::Online, Some(wifi()));
        network.update(Message::Opened);
        network.update(Message::Input(Secret("hunter22".into())));
        network.update(Message::Closed);
        assert_eq!(network.password, Secret::default());
        assert_eq!(network.page, Page::List);
        assert!(!network.open);
    }

    fn path(path: &str) -> eave_iwd::zbus::zvariant::OwnedObjectPath {
        eave_iwd::zbus::zvariant::OwnedObjectPath::try_from(path).unwrap()
    }

    fn wlan(name: &str, security: Security, known: bool, connected: bool) -> Wlan {
        Wlan {
            path: path(&format!("/net/{name}")),
            name: name.into(),
            security,
            strength: 60,
            signal: -60,
            known: known.then(|| eave_iwd::Known {
                path: path(&format!("/known/{name}")),
                autoconnect: true,
            }),
            connected,
            connecting: false,
        }
    }

    fn titles(network: &Network) -> Vec<String> {
        network
            .actions()
            .into_iter()
            .map(|action| action.title().to_string())
            .collect()
    }

    #[test]
    fn actions_join_only_what_needs_no_password() {
        let mut network = network(State::Online, Some(wifi()));
        assert!(titles(&network).is_empty(), "nothing known about Wi-Fi yet");
        let station = Station {
            device: path("/dev"),
            interface: "wlan0".into(),
            address: String::new(),
            powered: true,
            state: Link::Connected,
            scanning: false,
            networks: vec![
                wlan("home", Security::Psk, true, true),
                wlan("office", Security::Psk, true, false),
                wlan("cafe", Security::Open, false, false),
                wlan("neighbour", Security::Psk, false, false),
            ],
        };
        network.update(Message::Wifi(Wifi {
            access: Access::Ready,
            station: Some(station.clone()),
        }));
        assert_eq!(
            titles(&network),
            [
                "Turn Wi-Fi off",
                "Restart Wi-Fi",
                "Disconnect from home",
                "Join office",
                "Join cafe"
            ]
        );
        network.update(Message::Wifi(Wifi {
            access: Access::Ready,
            station: Some(Station {
                powered: false,
                ..station
            }),
        }));
        assert_eq!(titles(&network), ["Turn Wi-Fi on"]);
    }

    fn station(state: Link) -> Station {
        Station {
            device: path("/dev"),
            interface: "wlan0".into(),
            address: String::new(),
            powered: true,
            state,
            scanning: false,
            networks: Vec::new(),
        }
    }

    #[test]
    fn iwd_overrules_a_lingering_route() {
        let mut network = network(State::Online, Some(wifi()));
        let strong = Some(Icon::Wifi {
            strength: Some(0.7),
        });
        assert_eq!(icon(&network), strong, "the kernel alone");
        for (link, shown) in [
            (Link::Connected, strong),
            (Link::Roaming, strong),
            (Link::Disconnected, Some(Icon::Wifi { strength: None })),
            (Link::Connecting, Some(Icon::Wifi { strength: None })),
        ] {
            network.update(Message::Wifi(Wifi {
                access: Access::Ready,
                station: Some(station(link)),
            }));
            assert_eq!(icon(&network), shown, "{link:?}");
        }
    }

    #[test]
    fn one_command_at_a_time() {
        let mut network = network(State::Online, Some(wifi()));
        network.update(Message::Wifi(Wifi {
            access: Access::Ready,
            station: Some(station(Link::Connected)),
        }));
        network.update(Message::Disconnect);
        assert!(network.busy);
        assert!(network.idle_station().is_none(), "the next waits");
        network.update(Message::Done(Err("Wi-Fi didn't respond".into())));
        assert!(!network.busy);
        assert_eq!(network.problem.as_deref(), Some("Wi-Fi didn't respond"));
        network.update(Message::Power(false));
        assert_eq!(
            network.problem, None,
            "a new command clears the old problem"
        );
    }

    #[test]
    fn bands_follow_the_frequency() {
        assert_eq!(band(2427), Some("2.4 GHz"));
        assert_eq!(band(5180), Some("5 GHz"));
        assert_eq!(band(5955), Some("6 GHz"));
        assert_eq!(band(0), None);
    }
}
