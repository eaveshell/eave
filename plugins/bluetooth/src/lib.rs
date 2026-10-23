//! Bluetooth, through BlueZ ([D-103](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)):
//! the rune, with a dot on each side while a device is connected, and dimmed while Bluetooth is
//! off. Just the rune: what's connected, and its battery, are in the popover.
//!
//! The popover turns Bluetooth on and off, lists the paired devices to connect and disconnect, and
//! looks for devices nearby while it's open, to pair them: it shows the code to confirm, or asks
//! for the one to type, through an agent eave serves. Each paired device has a details page, to
//! forget it.
//!
//! In the dormer it turns Bluetooth on and off, and connects and disconnects paired devices.

use std::fmt;

use eave_api::ui::{self, Action, Icon, Item, Node, Panel, Tone};
use eave_api::{Effect, Registry, Subscription, Task, Widget};
use eave_bluez::{Access, Adapter, Answer, Bluetooth as State, Device, Kind, Request};
use serde::Deserialize;

pub fn register(registry: &mut Registry) {
    registry.widget::<Bluetooth>("bluetooth");
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// When the item shows.
    show: Show,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Show {
    /// Always, dimmed while Bluetooth is off.
    #[default]
    Always,
    /// While Bluetooth is on.
    On,
    /// While a device is connected.
    Connected,
}

pub struct Bluetooth {
    show: Show,
    /// What BlueZ says, watched all the time: it's quiet unless something changes, and the bar
    /// shows what's connected.
    state: Option<State>,
    open: bool,
    page: Page,
    /// Looking for devices, started by us when the popover opened.
    looking: bool,
    /// The device being connected, disconnected or paired, by object path.
    busy: Option<(String, Busy)>,
    /// The last device that couldn't be connected or paired, and why.
    failed: Option<(String, String)>,
    /// What BlueZ asks while pairing.
    request: Option<Request>,
    /// A PIN or passkey typed on the pairing page; cleared on leaving it.
    typed: Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    Connecting,
    Disconnecting,
    Pairing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    List,
    /// A paired device's details, by object path.
    Details(String),
    /// A device being paired, by object path.
    Pair(String),
}

/// A PIN or passkey, kept out of logs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Read(State),
    Opened,
    Closed,
    Power(bool),
    /// A device's row was clicked, by object path: connects, disconnects or pairs it.
    Press(String),
    Connect(String),
    Disconnect(String),
    /// Connecting, disconnecting or pairing a device ended, by object path: how, or why not.
    Ended(String, Result<(), String>),
    /// BlueZ asks something, or stopped asking.
    Asked(Option<Request>),
    /// Yes or no to what BlueZ asks.
    Answer(bool),
    Input(Secret),
    Submit,
    Details(String),
    Back,
    Forget(String),
    /// A command finished; what it changed arrives from BlueZ.
    Done,
}

impl Widget for Bluetooth {
    type Config = Config;
    type Message = Message;

    fn new(config: Config) -> (Self, Task<Message>) {
        let bluetooth = Self {
            show: config.show,
            state: None,
            open: false,
            page: Page::List,
            looking: false,
            busy: None,
            failed: None,
            request: None,
            typed: Secret::default(),
        };
        (bluetooth, Task::none())
    }

    fn update(&mut self, message: Message) -> Effect<Message> {
        match message {
            Message::Read(state) => {
                self.state = Some(state);
                // Connected or not as asked: done, however the call is doing. BlueZ can take
                // long to answer a connect after the device has already come.
                if let Some((path, busy)) = &self.busy {
                    let done = self.device(path).is_some_and(|device| match busy {
                        Busy::Connecting => device.connected,
                        Busy::Disconnecting => !device.connected,
                        Busy::Pairing => false,
                    });
                    if done {
                        self.busy = None;
                    }
                }
                if !self.adapter().is_some_and(|adapter| adapter.powered) {
                    self.looking = false;
                }
                return self.look();
            }
            Message::Opened => {
                self.open = true;
                self.page = Page::List;
                self.failed = None;
                return self.look();
            }
            Message::Closed => {
                self.open = false;
                let cancel = self.leave_pairing();
                self.page = Page::List;
                let looked = std::mem::take(&mut self.looking);
                let stop = match self.adapter().filter(|_| looked).cloned() {
                    Some(adapter) => run(async move { eave_bluez::stop_discovery(&adapter).await }),
                    None => Task::none(),
                };
                return Effect::task(Task::batch([cancel, stop]));
            }
            Message::Power(on) => {
                let Some(adapter) = self.adapter().cloned() else {
                    return Effect::none();
                };
                self.failed = None;
                let path = adapter.path.to_string();
                return Task::future(async move { eave_bluez::set_powered(&adapter, on).await })
                    .map(move |result| Message::Ended(path.clone(), reason(result)))
                    .into();
            }
            Message::Press(path) => {
                let Some(device) = self.device(&path) else {
                    return Effect::none();
                };
                return match (device.paired, device.connected) {
                    (true, true) => self.update(Message::Disconnect(path)),
                    (true, false) => self.update(Message::Connect(path)),
                    (false, _) => self.pair(path),
                };
            }
            Message::Connect(path) => {
                let Some(device) = self.device(&path).cloned() else {
                    return Effect::none();
                };
                self.start(&path, Busy::Connecting);
                return Task::future(async move { eave_bluez::connect(&device).await })
                    .map(move |result| Message::Ended(path.clone(), reason(result)))
                    .into();
            }
            Message::Disconnect(path) => {
                let Some(device) = self.device(&path).cloned() else {
                    return Effect::none();
                };
                self.start(&path, Busy::Disconnecting);
                return Task::future(async move { eave_bluez::disconnect(&device).await })
                    .map(move |result| Message::Ended(path.clone(), reason(result)))
                    .into();
            }
            Message::Ended(path, result) => {
                if self.busy.as_ref().is_some_and(|(busy, _)| *busy == path) {
                    self.busy = None;
                }
                if self.page == Page::Pair(path.clone()) {
                    self.page = Page::List;
                    self.typed = Secret::default();
                }
                if let Err(why) = result {
                    self.failed = Some((path, why));
                }
            }
            Message::Asked(request) => self.request = request,
            Message::Answer(yes) => {
                eave_bluez::answer(if yes { Answer::Yes } else { Answer::No });
            }
            Message::Input(typed) => self.typed = typed,
            Message::Submit => {
                let Secret(typed) = std::mem::take(&mut self.typed);
                if !typed.is_empty() {
                    eave_bluez::answer(Answer::Text(typed));
                }
            }
            Message::Details(path) => self.page = Page::Details(path),
            Message::Back => {
                let cancel = self.leave_pairing();
                self.page = Page::List;
                return cancel.into();
            }
            Message::Forget(path) => {
                let (Some(adapter), Some(device)) =
                    (self.adapter().cloned(), self.device(&path).cloned())
                else {
                    return Effect::none();
                };
                self.page = Page::List;
                return command(async move { eave_bluez::forget(&adapter, &device).await });
            }
            Message::Done => {}
        }
        Effect::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        let bluez = Subscription::run(eave_bluez::watch).map(Message::Read);
        if !matches!(self.busy, Some((_, Busy::Pairing))) {
            return bluez;
        }
        let requests = Subscription::run(eave_bluez::requests).map(Message::Asked);
        Subscription::batch([bluez, requests])
    }

    fn item(&self) -> Item<Message> {
        let Some(state) = &self.state else {
            return Item::hidden();
        };
        let adapter = state.adapter.as_ref();
        // No adapter: a machine without Bluetooth.
        if state.access == Access::Ready && adapter.is_none() {
            return Item::hidden();
        }
        let powered = adapter.is_some_and(|adapter| adapter.powered);
        let connected = self.connected().next();
        let shown = match self.show {
            Show::Always => true,
            Show::On => powered,
            Show::Connected => connected.is_some(),
        };
        if !shown {
            return Item::hidden();
        }
        let content = ui::icon(Icon::Bluetooth {
            connected: connected.is_some(),
        });
        Item::new(if powered {
            content
        } else {
            content.tone(Tone::Dim)
        })
    }

    fn popover(&self) -> Option<Panel<Message>> {
        let state = self.state.as_ref()?;
        // The list is the first page; a device's details or pairing, one further in.
        let (order, content) = match (&self.page, state.adapter.as_ref()) {
            (_, None) => (0, absent(state.access)),
            (Page::List, Some(adapter)) => (0, self.list_page(adapter)),
            (Page::Details(path), Some(adapter)) => match self.device(path) {
                Some(device) => (1, self.details_page(device)),
                None => (0, self.list_page(adapter)),
            },
            (Page::Pair(path), Some(adapter)) => match self.device(path) {
                Some(device) => (1, self.pair_page(device)),
                None => (0, self.list_page(adapter)),
            },
        };
        let panel = Panel::new(ui::page(order, content))
            .on_open(Message::Opened)
            .on_close(Message::Closed);
        Some(panel)
    }

    /// Bluetooth on or off, and connecting or disconnecting each paired device.
    fn actions(&self) -> Vec<Action<Message>> {
        let Some(adapter) = self.adapter() else {
            return Vec::new();
        };
        let words = ["bluetooth", "wireless", "device"];
        if !adapter.powered {
            return vec![
                ui::action("Turn Bluetooth on", Message::Power(true))
                    .id("bluetooth-on")
                    .keywords(words.into_iter().chain(["enable"]))
                    .leading(ui::icon(Icon::Bluetooth { connected: false })),
            ];
        }
        let mut actions = vec![
            ui::action("Turn Bluetooth off", Message::Power(false))
                .id("bluetooth-off")
                .keywords(words.into_iter().chain(["disable", "airplane"]))
                .leading(ui::icon(Icon::Bluetooth { connected: false })),
        ];
        for device in adapter.devices.iter().filter(|device| device.paired) {
            let path = device.path.to_string();
            if self.busy.as_ref().is_some_and(|(busy, _)| *busy == path) {
                continue;
            }
            let (title, message, word) = if device.connected {
                (
                    format!("Disconnect {}", device.name),
                    Message::Disconnect(path),
                    "disconnect",
                )
            } else {
                (
                    format!("Connect {}", device.name),
                    Message::Connect(path),
                    "connect",
                )
            };
            actions.push(
                ui::action(title, message)
                    .subtitle("Bluetooth")
                    .keywords(words.into_iter().chain([word]))
                    .leading(ui::icon(kind_icon(device.kind))),
            );
        }
        actions
    }
}

impl Bluetooth {
    /// The adapter, when BlueZ can be used.
    fn adapter(&self) -> Option<&Adapter> {
        let state = self.state.as_ref()?;
        (state.access == Access::Ready).then_some(state.adapter.as_ref()?)
    }

    fn device(&self, path: &str) -> Option<&Device> {
        self.adapter()?
            .devices
            .iter()
            .find(|device| device.path.as_str() == path)
    }

    fn connected(&self) -> impl Iterator<Item = &Device> {
        self.adapter()
            .into_iter()
            .flat_map(|adapter| &adapter.devices)
            .filter(|device| device.connected)
    }

    /// Starts looking for devices, while the popover is open and Bluetooth is on.
    fn look(&mut self) -> Effect<Message> {
        if !self.open || self.looking {
            return Effect::none();
        }
        let Some(adapter) = self.adapter().filter(|adapter| adapter.powered).cloned() else {
            return Effect::none();
        };
        self.looking = true;
        command(async move { eave_bluez::start_discovery(&adapter).await })
    }

    fn start(&mut self, path: &str, busy: Busy) {
        self.busy = Some((path.to_string(), busy));
        self.failed = None;
    }

    fn pair(&mut self, path: String) -> Effect<Message> {
        let Some(device) = self.device(&path).cloned() else {
            return Effect::none();
        };
        self.start(&path, Busy::Pairing);
        self.request = None;
        self.typed = Secret::default();
        self.page = Page::Pair(path.clone());
        Task::future(async move { eave_bluez::pair(&device).await })
            .map(move |result| Message::Ended(path.clone(), reason(result)))
            .into()
    }

    /// Leaving the pairing page, or the popover, stops the pairing.
    fn leave_pairing(&mut self) -> Task<Message> {
        self.typed = Secret::default();
        let Page::Pair(path) = &self.page else {
            return Task::none();
        };
        let pairing = matches!(&self.busy, Some((busy, Busy::Pairing)) if busy == path);
        let Some(device) = self.device(path).filter(|_| pairing).cloned() else {
            return Task::none();
        };
        run(async move { eave_bluez::cancel_pairing(&device).await })
    }

    fn list_page(&self, adapter: &Adapter) -> Node<Message> {
        let header = ui::header("Bluetooth").trailing(ui::toggle(
            adapter.powered,
            Message::Power(!adapter.powered),
        ));
        let mut lines = vec![header];
        let failed = self
            .failed
            .as_ref()
            .filter(|(path, _)| *path == adapter.path.as_str());
        if let Some((_, why)) = failed {
            lines.push(ui::label(why.clone()).tone(Tone::Critical));
        }
        if !adapter.powered {
            let off = if adapter.blocked {
                "Bluetooth is blocked by rfkill"
            } else {
                "Bluetooth is off"
            };
            lines.push(ui::label(off).tone(Tone::Dim));
            return ui::column(lines);
        }
        let (paired, nearby): (Vec<&Device>, Vec<&Device>) =
            adapter.devices.iter().partition(|device| device.paired);
        if !paired.is_empty() {
            lines.push(ui::section("My devices"));
            lines.push(ui::list(paired.into_iter().map(|device| self.row(device))));
        }
        let nearby: Vec<_> = nearby.into_iter().filter(|device| device.named).collect();
        let scanning = if adapter.discovering {
            ui::label("Looking…").tone(Tone::Dim)
        } else {
            ui::empty()
        };
        lines.push(ui::section("Nearby").trailing(scanning));
        if nearby.is_empty() {
            let empty = if adapter.discovering {
                "Looking for devices…"
            } else {
                "No devices nearby"
            };
            lines.push(ui::label(empty).tone(Tone::Dim));
        } else {
            lines.push(ui::list(nearby.into_iter().map(|device| self.row(device))));
        }
        ui::column(lines)
    }

    /// A device in the list: what it is, its name and state, and for a paired one an info button.
    fn row(&self, device: &Device) -> Node<Message> {
        let path = device.path.to_string();
        let busy = self
            .busy
            .as_ref()
            .filter(|(busy, _)| *busy == path)
            .map(|(_, busy)| *busy);
        let failed = self
            .failed
            .as_ref()
            .filter(|(failed, _)| *failed == path)
            .map(|(_, why)| why.clone());
        let subtitle = match (busy, failed) {
            (Some(Busy::Connecting), _) => Some("Connecting…".to_string()),
            (Some(Busy::Disconnecting), _) => Some("Disconnecting…".to_string()),
            (Some(Busy::Pairing), _) => Some("Pairing…".to_string()),
            (None, Some(why)) => Some(why),
            (None, None) if device.connected => Some(match device.battery {
                Some(battery) => format!("Connected · {battery}%"),
                None => "Connected".into(),
            }),
            (None, None) => None,
        };
        let mut row = ui::list_item(device.name.clone())
            .leading(ui::icon(kind_icon(device.kind)))
            .selected(device.connected);
        if let Some(subtitle) = subtitle {
            row = row.subtitle(subtitle);
        }
        if device.paired {
            row = row.trailing(ui::button(
                ui::icon(Icon::Info),
                Message::Details(path.clone()),
            ));
        }
        if busy.is_none() {
            row = row.on_press(Message::Press(path));
        }
        row
    }

    fn details_page(&self, device: &Device) -> Node<Message> {
        let path = device.path.to_string();
        let mut lines = vec![ui::header(device.name.clone()).on_back(Message::Back)];
        if let Some(battery) = device.battery {
            lines.push(ui::detail(
                "Battery",
                ui::row([
                    ui::meter(f32::from(battery) / 100.0),
                    ui::label(format!("{battery}%")),
                ]),
            ));
        }
        let state = if device.connected {
            "Connected"
        } else {
            "Not connected"
        };
        lines.push(ui::detail("State", ui::label(state)));
        lines.push(ui::detail("Address", ui::label(device.address.clone())));
        let busy = self.busy.as_ref().is_some_and(|(busy, _)| *busy == path);
        let mut buttons = Vec::new();
        if !busy {
            buttons.push(if device.connected {
                ui::button(ui::label("Disconnect"), Message::Disconnect(path.clone()))
            } else {
                ui::button(ui::label("Connect"), Message::Connect(path.clone()))
            });
        }
        buttons.push(ui::fill());
        buttons.push(ui::button(
            ui::label("Forget").tone(Tone::Critical),
            Message::Forget(path),
        ));
        lines.push(ui::separator());
        lines.push(ui::row(buttons));
        ui::column(lines)
    }

    /// What pairing `device` needs: a code to compare or type, or a wait.
    fn pair_page(&self, device: &Device) -> Node<Message> {
        let mut lines = vec![ui::header(device.name.clone()).on_back(Message::Back)];
        let request = self
            .request
            .as_ref()
            .filter(|request| request.device() == &device.path);
        let cancel = || ui::button(ui::label("Cancel"), Message::Back);
        match request {
            None => {
                lines.push(ui::label("Pairing…").tone(Tone::Dim));
                lines.push(ui::row([ui::fill(), cancel()]));
            }
            Some(Request::Confirm { passkey, .. }) => {
                lines.push(ui::label("Check that the device shows this code.").tone(Tone::Dim));
                lines.push(ui::center(ui::display(ui::label(format!("{passkey:06}")))));
                lines.push(ui::row([
                    ui::fill(),
                    ui::button(ui::label("Cancel"), Message::Answer(false)),
                    ui::button(ui::label("Pair"), Message::Answer(true)).prominent(),
                ]));
            }
            Some(Request::Display { code, .. }) => {
                lines.push(ui::label("Type this code on the device, then Enter.").tone(Tone::Dim));
                lines.push(ui::center(ui::display(ui::label(code.clone()))));
                lines.push(ui::row([ui::fill(), cancel()]));
            }
            Some(Request::PinCode { .. } | Request::Passkey { .. }) => {
                let hint = match request {
                    Some(Request::PinCode { .. }) => {
                        "Type the PIN the device shows, or came with (often 0000)."
                    }
                    _ => "Type the code the device shows.",
                };
                lines.push(ui::label(hint).tone(Tone::Dim));
                lines.push(
                    ui::text_input("Code", self.typed.0.clone(), |text| {
                        Message::Input(Secret(text))
                    })
                    .on_submit(Message::Submit),
                );
                lines.push(ui::row([
                    ui::fill(),
                    cancel(),
                    ui::button(ui::label("Pair"), Message::Submit).prominent(),
                ]));
            }
            Some(Request::Authorize { .. }) => {
                lines.push(ui::label("Let this device pair?").tone(Tone::Dim));
                lines.push(ui::row([
                    ui::fill(),
                    ui::button(ui::label("Cancel"), Message::Answer(false)),
                    ui::button(ui::label("Pair"), Message::Answer(true)).prominent(),
                ]));
            }
        }
        ui::column(lines)
    }
}

/// The popover without an adapter to use: why.
fn absent(access: Access) -> Node<Message> {
    let why = match access {
        Access::Ready => "No Bluetooth adapter",
        Access::Absent => "Bluetooth needs BlueZ (bluetooth.service)",
        Access::Denied => "Not allowed to use BlueZ",
    };
    ui::column([ui::header("Bluetooth"), ui::label(why).tone(Tone::Dim)])
}

fn kind_icon(kind: Kind) -> Icon {
    match kind {
        Kind::Headphones => Icon::Headphones,
        Kind::Speaker => Icon::Speaker,
        Kind::Display => Icon::Display,
        _ => Icon::Bluetooth { connected: false },
    }
}

fn reason(result: eave_bluez::zbus::Result<()>) -> Result<(), String> {
    result.map_err(|error| {
        eprintln!("eave-bluetooth: {error}");
        eave_bluez::reason(&error).to_string()
    })
}

fn command(
    work: impl Future<Output = eave_bluez::zbus::Result<()>> + Send + 'static,
) -> Effect<Message> {
    run(work).into()
}

fn run(work: impl Future<Output = eave_bluez::zbus::Result<()>> + Send + 'static) -> Task<Message> {
    Task::future(async move {
        if let Err(error) = work.await {
            eprintln!("eave-bluetooth: {error}");
        }
        Message::Done
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eave_bluez::zbus::zvariant::OwnedObjectPath;

    fn path(path: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(path).unwrap()
    }

    fn device(name: &str, paired: bool, connected: bool) -> Device {
        Device {
            path: path(&format!("/org/bluez/hci0/dev_{name}")),
            name: name.into(),
            named: true,
            address: "AA:BB:CC:DD:EE:FF".into(),
            kind: Kind::Headphones,
            paired,
            trusted: paired,
            connected,
            battery: connected.then_some(80),
            rssi: None,
        }
    }

    fn state(powered: bool, devices: Vec<Device>) -> State {
        State {
            access: Access::Ready,
            adapter: Some(Adapter {
                path: path("/org/bluez/hci0"),
                address: String::new(),
                powered,
                blocked: false,
                discovering: false,
                devices,
            }),
        }
    }

    fn bluetooth(config: Config, state: State) -> Bluetooth {
        let (mut bluetooth, _) = Bluetooth::new(config);
        bluetooth.update(Message::Read(state));
        bluetooth
    }

    fn titles(bluetooth: &Bluetooth) -> Vec<String> {
        bluetooth
            .actions()
            .into_iter()
            .map(|action| action.title().to_string())
            .collect()
    }

    #[test]
    fn hidden_without_an_adapter() {
        let (bluetooth, _) = Bluetooth::new(Config::default());
        assert!(bluetooth.item().into_parts().content.is_none());
        let none = bluetooth_without_adapter();
        assert!(none.item().into_parts().content.is_none());
        assert!(none.popover().is_some(), "the popover says why");
    }

    fn bluetooth_without_adapter() -> Bluetooth {
        bluetooth(
            Config::default(),
            State {
                access: Access::Ready,
                adapter: None,
            },
        )
    }

    #[test]
    fn the_rune_shows_what_is_connected() {
        let off = bluetooth(Config::default(), state(false, vec![]));
        assert!(matches!(
            off.item().into_parts().content.unwrap().into_kind(),
            ui::Kind::Icon {
                icon: Icon::Bluetooth { connected: false },
                tone: Some(Tone::Dim),
                ..
            }
        ));
        let on = bluetooth(
            Config::default(),
            state(true, vec![device("Buds", true, true)]),
        );
        assert!(matches!(
            on.item().into_parts().content.unwrap().into_kind(),
            ui::Kind::Icon {
                icon: Icon::Bluetooth { connected: true },
                ..
            }
        ));
    }

    #[test]
    fn shows_only_when_told() {
        let config = |show| Config { show };
        let off = || state(false, vec![]);
        assert!(
            bluetooth(config(Show::On), off())
                .item()
                .into_parts()
                .content
                .is_none()
        );
        let idle = || state(true, vec![device("Buds", true, false)]);
        assert!(
            bluetooth(config(Show::On), idle())
                .item()
                .into_parts()
                .content
                .is_some()
        );
        assert!(
            bluetooth(config(Show::Connected), idle())
                .item()
                .into_parts()
                .content
                .is_none()
        );
        let parsed: Config = toml::from_str("show = \"connected\"").unwrap();
        assert_eq!(parsed.show, Show::Connected);
    }

    #[test]
    fn actions_connect_and_disconnect_paired_devices() {
        let mut bluetooth = bluetooth(
            Config::default(),
            state(
                true,
                vec![
                    device("Buds", true, true),
                    device("Speaker", true, false),
                    device("Stranger", false, false),
                ],
            ),
        );
        assert_eq!(
            titles(&bluetooth),
            ["Turn Bluetooth off", "Disconnect Buds", "Connect Speaker"]
        );
        bluetooth.update(Message::Read(state(false, vec![])));
        assert_eq!(titles(&bluetooth), ["Turn Bluetooth on"]);
    }

    #[test]
    fn a_row_press_does_what_the_device_needs() {
        let mut bluetooth = bluetooth(
            Config::default(),
            state(
                true,
                vec![device("Buds", true, false), device("New", false, false)],
            ),
        );
        bluetooth.update(Message::Press("/org/bluez/hci0/dev_Buds".into()));
        assert_eq!(
            bluetooth.busy,
            Some(("/org/bluez/hci0/dev_Buds".into(), Busy::Connecting))
        );
        bluetooth.update(Message::Press("/org/bluez/hci0/dev_New".into()));
        assert_eq!(bluetooth.page, Page::Pair("/org/bluez/hci0/dev_New".into()));
        assert!(matches!(bluetooth.busy, Some((_, Busy::Pairing))));
        // Pairing failed: back to the list, saying why.
        bluetooth.update(Message::Ended(
            "/org/bluez/hci0/dev_New".into(),
            Err("Pairing failed. Try again".into()),
        ));
        assert_eq!(bluetooth.page, Page::List);
        assert_eq!(bluetooth.busy, None);
        assert!(bluetooth.failed.is_some());
    }

    #[test]
    fn looks_for_devices_only_while_open_and_on() {
        let mut bluetooth = bluetooth(Config::default(), state(true, vec![]));
        assert!(!bluetooth.looking);
        bluetooth.update(Message::Opened);
        assert!(bluetooth.looking);
        bluetooth.update(Message::Closed);
        assert!(!bluetooth.looking);
        let mut off = self::bluetooth(Config::default(), state(false, vec![]));
        off.update(Message::Opened);
        assert!(!off.looking);
        // Turned on while open: it starts looking.
        off.update(Message::Read(state(true, vec![])));
        assert!(off.looking);
    }

    #[test]
    fn codes_stay_out_of_logs() {
        let message = Message::Input(Secret("0000".into()));
        assert!(!format!("{message:?}").contains("0000"));
    }
}
