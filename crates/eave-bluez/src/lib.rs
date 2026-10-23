//! Bluetooth controls for eave plugins, through BlueZ's D-Bus API
//! ([D-103](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)): the adapter, the
//! devices paired and nearby with their battery, looking for devices, connecting, disconnecting,
//! pairing (answering the device through an agent eave serves, [`agent`]) and forgetting.
//!
//! Without BlueZ, or without permission to use it, [`watch`] says so instead of failing.

use std::collections::HashMap;

use eave_dbus::{Snapshot, Stream};
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub mod agent;

pub use agent::{Answer, Request, answer, requests};
pub use zbus;

const SERVICE: &str = "org.bluez";
const ADAPTER: &str = "org.bluez.Adapter1";
const DEVICE: &str = "org.bluez.Device1";
const BATTERY: &str = "org.bluez.Battery1";

/// What BlueZ says about Bluetooth.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bluetooth {
    pub access: Access,
    /// The first adapter, when there is one.
    pub adapter: Option<Adapter>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Access {
    /// BlueZ answers.
    Ready,
    /// BlueZ isn't running.
    #[default]
    Absent,
    /// BlueZ is running, but its bus policy turns us away.
    Denied,
}

/// A Bluetooth adapter, and the devices it knows.
#[derive(Debug, Clone, PartialEq)]
pub struct Adapter {
    pub path: OwnedObjectPath,
    pub address: String,
    pub powered: bool,
    /// Off, and kept off by rfkill: turning it on fails until that's lifted.
    pub blocked: bool,
    pub discovering: bool,
    /// Paired ones first, by name; then the rest, strongest first.
    pub devices: Vec<Device>,
}

/// A device the adapter knows: paired, or seen nearby.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub path: OwnedObjectPath,
    /// Its name, or its address when it has none.
    pub name: String,
    /// Whether it said its name. Nearby devices that didn't are rarely what's being looked for.
    pub named: bool,
    pub address: String,
    pub kind: Kind,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// Percent, when it reports its battery.
    pub battery: Option<u8>,
    /// Signal in dBm, while looking for devices.
    pub rssi: Option<i16>,
}

/// What a device is, from the icon name BlueZ gives it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    Headphones,
    Speaker,
    Display,
    Keyboard,
    Mouse,
    Phone,
    Computer,
    Gamepad,
    #[default]
    Other,
}

impl Kind {
    fn from_icon(icon: &str) -> Self {
        match icon {
            "audio-headset" | "audio-headphones" => Self::Headphones,
            "audio-card" | "multimedia-player" => Self::Speaker,
            "video-display" => Self::Display,
            "input-keyboard" => Self::Keyboard,
            "input-mouse" | "input-tablet" => Self::Mouse,
            "phone" => Self::Phone,
            "computer" => Self::Computer,
            "input-gaming" => Self::Gamepad,
            _ => Self::Other,
        }
    }
}

/// Bluetooth now, once: for a widget that wants to know what there is without watching it.
pub async fn read() -> Option<Bluetooth> {
    use futures_util::StreamExt;
    std::pin::pin!(watch()).next().await
}

/// Bluetooth now, then again after every change BlueZ signals. Quiet while nothing changes;
/// chatty while looking for devices.
pub fn watch() -> impl Stream<Item = Bluetooth> {
    eave_dbus::watch_all(SERVICE, snapshot)
}

async fn snapshot(connection: Connection) -> zbus::Result<Snapshot<Bluetooth>> {
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
                state: Bluetooth {
                    access,
                    adapter: None,
                },
                watch: Vec::new(),
            });
        }
    };
    Ok(Snapshot {
        state: Bluetooth {
            access: Access::Ready,
            adapter: Objects::from(objects).adapter(),
        },
        watch: Vec::new(),
    })
}

type Properties = HashMap<String, OwnedValue>;

/// BlueZ's objects, by path, then interface.
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

fn value<T: TryFrom<OwnedValue>>(properties: &Properties, name: &str) -> Option<T> {
    let value = properties.get(name)?.try_clone().ok()?;
    T::try_from(value).ok()
}

fn get<T: TryFrom<OwnedValue> + Default>(properties: &Properties, name: &str) -> T {
    value(properties, name).unwrap_or_default()
}

impl Objects {
    /// The first adapter, by path, so it's the same one every time, and its devices.
    fn adapter(&self) -> Option<Adapter> {
        let (path, properties) = self
            .0
            .iter()
            .filter_map(|(path, interfaces)| Some((path, interfaces.get(ADAPTER)?)))
            .min_by_key(|(path, _)| path.as_str())?;
        let mut devices: Vec<Device> = self
            .0
            .iter()
            .filter_map(|(device, interfaces)| {
                let properties = interfaces.get(DEVICE)?;
                let adapter: OwnedObjectPath = value(properties, "Adapter")?;
                (adapter.as_str() == path).then(|| self.device(device, properties))?
            })
            .collect();
        devices.sort_by(|a, b| {
            b.paired
                .cmp(&a.paired)
                .then_with(|| match a.paired {
                    true => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    false => b.rssi.cmp(&a.rssi),
                })
                .then_with(|| a.address.cmp(&b.address))
        });
        Some(Adapter {
            path: OwnedObjectPath::try_from(path.as_str()).ok()?,
            address: get(properties, "Address"),
            powered: get(properties, "Powered"),
            blocked: get::<String>(properties, "PowerState") == "off-blocked",
            discovering: get(properties, "Discovering"),
            devices,
        })
    }

    fn device(&self, path: &str, properties: &Properties) -> Option<Device> {
        let address: String = get(properties, "Address");
        let name: Option<String> = value(properties, "Name");
        let alias: Option<String> = value(properties, "Alias");
        let battery = self
            .0
            .get(path)
            .and_then(|interfaces| interfaces.get(BATTERY))
            .and_then(|battery| value::<u8>(battery, "Percentage"));
        Some(Device {
            path: OwnedObjectPath::try_from(path).ok()?,
            named: name.is_some(),
            name: alias.or(name).unwrap_or_else(|| address.clone()),
            address,
            kind: Kind::from_icon(&get::<String>(properties, "Icon")),
            paired: get(properties, "Paired"),
            trusted: get(properties, "Trusted"),
            connected: get(properties, "Connected"),
            battery,
            rssi: value(properties, "RSSI"),
        })
    }
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

/// Turns the adapter on or off.
pub async fn set_powered(adapter: &Adapter, on: bool) -> zbus::Result<()> {
    set(&adapter.path, ADAPTER, "Powered", on).await
}

/// Starts looking for devices. BlueZ stops on its own when this program leaves, and reports what
/// it finds as it goes.
pub async fn start_discovery(adapter: &Adapter) -> zbus::Result<()> {
    call(&adapter.path, ADAPTER, "StartDiscovery").await
}

pub async fn stop_discovery(adapter: &Adapter) -> zbus::Result<()> {
    call(&adapter.path, ADAPTER, "StopDiscovery").await
}

/// Connects a paired device. Answers once it's connected, or couldn't be: seconds, sometimes.
/// Connects a device. Already connected is done; already connecting is left to finish, and
/// its `Connected` says when it has.
pub async fn connect(device: &Device) -> zbus::Result<()> {
    match call(&device.path, DEVICE, "Connect").await {
        Err(error) if matches!(error_name(&error), Some("AlreadyConnected" | "InProgress")) => {
            Ok(())
        }
        result => result,
    }
}

/// Disconnects a device. Already disconnected is done.
pub async fn disconnect(device: &Device) -> zbus::Result<()> {
    match call(&device.path, DEVICE, "Disconnect").await {
        Err(error) if error_name(&error) == Some("NotConnected") => Ok(()),
        result => result,
    }
}

/// BlueZ's short name for an error, like `Failed` or `InProgress`.
fn error_name(error: &zbus::Error) -> Option<&str> {
    match error {
        zbus::Error::MethodError(name, ..) => name.as_str().rsplit('.').next(),
        _ => None,
    }
}

/// Pairs a device, trusts it so it can connect on its own later, and connects it. BlueZ asks
/// eave's agent for whatever the pairing needs ([`requests`]), and waits for the [`answer`].
pub async fn pair(device: &Device) -> zbus::Result<()> {
    agent::pair(device).await?;
    set(&device.path, DEVICE, "Trusted", true).await?;
    connect(device).await
}

/// Stops pairing a device.
pub async fn cancel_pairing(device: &Device) -> zbus::Result<()> {
    agent::cancel(&device.path);
    call(&device.path, DEVICE, "CancelPairing").await
}

/// Forgets a device: unpairs it, and it's gone from the list until it's seen again.
pub async fn forget(adapter: &Adapter, device: &Device) -> zbus::Result<()> {
    let connection = eave_dbus::system().await?;
    connection
        .call_method(
            Some(SERVICE),
            &adapter.path,
            Some(ADAPTER),
            "RemoveDevice",
            &(&device.path,),
        )
        .await?;
    Ok(())
}

/// A short reason a BlueZ call failed, fit to show under a device.
pub fn reason(error: &zbus::Error) -> &'static str {
    let (name, message) = match error {
        zbus::Error::MethodError(name, message, ..) => (name.as_str(), message.as_deref()),
        zbus::Error::InputOutput(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            return "The device didn't answer in time";
        }
        _ => return "Couldn't reach Bluetooth",
    };
    match name.rsplit('.').next().unwrap_or("") {
        "AuthenticationCanceled" | "Canceled" | "Rejected" => "Pairing was cancelled",
        "AuthenticationRejected" => "The device turned pairing down",
        "AuthenticationTimeout" => "The device didn't answer in time",
        "AuthenticationFailed" => "Pairing failed. Try again",
        "AlreadyExists" => "Already paired",
        "InProgress" | "Busy" => "Already connecting",
        "NotReady" => "Bluetooth is off",
        "DoesNotExist" => "The device is gone",
        "NotAuthorized" | "AccessDenied" => "Not allowed to change Bluetooth",
        "NotSupported" => "This device isn't supported",
        "ConnectionAttemptFailed" => "Couldn't connect. Is it on and in range?",
        "Failed" if message.is_some_and(|m| m.contains("rfkill")) => "Bluetooth is blocked",
        "Failed" => "Couldn't connect. Is it on and in range?",
        _ => "Something went wrong",
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

    fn device(name: Option<&str>, address: &str, paired: bool, rssi: Option<i16>) -> Properties {
        let mut properties = Properties::from([
            ("Adapter".into(), value(object("/org/bluez/hci0"))),
            ("Address".into(), value(address.to_string())),
            ("Alias".into(), value(name.unwrap_or(address).to_string())),
            ("Icon".into(), value("audio-headphones")),
            ("Paired".into(), value(paired)),
            ("Trusted".into(), value(paired)),
            ("Connected".into(), value(false)),
        ]);
        if let Some(name) = name {
            properties.insert("Name".into(), value(name.to_string()));
        }
        if let Some(rssi) = rssi {
            properties.insert("RSSI".into(), value(rssi));
        }
        properties
    }

    fn fixture() -> Objects {
        let mut objects = HashMap::new();
        objects.insert(
            "/org/bluez/hci0".to_string(),
            HashMap::from([(
                ADAPTER.to_string(),
                Properties::from([
                    ("Address".into(), value("C8:89:F3:DA:E4:6E")),
                    ("Powered".into(), value(true)),
                    ("PowerState".into(), value("on")),
                    ("Discovering".into(), value(true)),
                ]),
            )]),
        );
        let mut buds = HashMap::from([(
            DEVICE.to_string(),
            device(Some("Buds"), "AA:00:00:00:00:01", true, None),
        )]);
        buds.insert(
            BATTERY.to_string(),
            Properties::from([("Percentage".into(), value(80u8))]),
        );
        objects.insert("/org/bluez/hci0/dev_AA_00_00_00_00_01".into(), buds);
        for (path, properties) in [
            (
                "/org/bluez/hci0/dev_AA_00_00_00_00_02",
                device(Some("Anker"), "AA:00:00:00:00:02", true, None),
            ),
            (
                "/org/bluez/hci0/dev_AA_00_00_00_00_03",
                device(Some("Far"), "AA:00:00:00:00:03", false, Some(-90)),
            ),
            (
                "/org/bluez/hci0/dev_AA_00_00_00_00_04",
                device(None, "AA:00:00:00:00:04", false, Some(-40)),
            ),
        ] {
            objects.insert(
                path.into(),
                HashMap::from([(DEVICE.to_string(), properties)]),
            );
        }
        Objects(objects)
    }

    #[test]
    fn reads_the_adapter_and_its_devices_in_order() {
        let adapter = fixture().adapter().unwrap();
        assert_eq!(adapter.path.as_str(), "/org/bluez/hci0");
        assert!(adapter.powered && adapter.discovering && !adapter.blocked);
        let names: Vec<_> = adapter.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["Anker", "Buds", "AA:00:00:00:00:04", "Far"],
            "paired by name, then the strongest"
        );
        let buds = &adapter.devices[1];
        assert_eq!(buds.battery, Some(80));
        assert_eq!(buds.kind, Kind::Headphones);
        assert!(buds.named && buds.paired);
        assert!(!adapter.devices[2].named);
        assert_eq!(adapter.devices[3].rssi, Some(-90));
    }

    #[test]
    fn no_adapter_no_devices() {
        assert_eq!(Objects(HashMap::new()).adapter(), None);
    }

    fn error(name: &str, message: Option<&str>) -> zbus::Error {
        zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from(name).unwrap(),
            message.map(str::to_owned),
            zbus::Message::method_call("/", "x")
                .unwrap()
                .build(&())
                .unwrap(),
        )
    }

    #[test]
    fn errors_read_as_reasons() {
        assert_eq!(
            reason(&error("org.bluez.Error.AuthenticationFailed", None)),
            "Pairing failed. Try again"
        );
        assert_eq!(
            reason(&error(
                "org.bluez.Error.Failed",
                Some("br-connection-page-timeout")
            )),
            "Couldn't connect. Is it on and in range?"
        );
        assert_eq!(
            reason(&error(
                "org.bluez.Error.Failed",
                Some("Blocked through rfkill")
            )),
            "Bluetooth is blocked"
        );
    }

    /// Prints what BlueZ says on this machine. Reads only.
    /// `cargo test -p eave-bluez -- --ignored --nocapture`
    #[test]
    #[ignore = "needs BlueZ"]
    fn live() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async { println!("{:#?}", read().await) });
    }
}
