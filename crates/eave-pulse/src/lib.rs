//! Audio for eave plugins, over the PulseAudio native protocol. On PipeWire this is served by
//! `pipewire-pulse`, which every desktop setup runs ([D-19](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).
//!
//! One thread holds a subscription to sink and server events for every widget. Reads and commands
//! each use a short-lived connection on tokio's blocking pool, so the UI thread never waits on
//! the socket.

use std::ffi::CString;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::time::Duration;

use futures_util::{FutureExt, StreamExt, stream};
use pulseaudio::protocol::{self, ChannelVolume, Command, Volume};
use tokio::sync::broadcast;

pub use futures_util::Stream;

/// The audio server's devices: where sound goes, and where it comes from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Audio {
    pub sinks: Vec<Device>,
    /// Microphones and other inputs. Monitors of outputs are left out.
    pub sources: Vec<Device>,
    /// Index into `sinks` of the default sink.
    pub default_sink: Option<usize>,
    /// Index into `sources` of the default source.
    pub default_source: Option<usize>,
}

/// A sink (output) or source (input).
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub index: u32,
    pub direction: Direction,
    pub description: String,
    /// Percent of normal volume, per channel: 100 is 0 dB. Can go above 100.
    pub volume: u32,
    pub muted: bool,
    /// What it is, from its active port or its properties.
    pub form: Form,
    /// How it's connected, when that's worth saying: `Bluetooth`, `USB`, `HDMI`.
    pub connection: Option<&'static str>,
    /// The server's name for it, which stays while it's there and when it comes back, unlike
    /// `index`: `alsa_output.pci-0000_00_1f.3.analog-stereo`, `bluez_output.AC_12_34_56_78_9A.1`
    /// for a Bluetooth device, with its address. What `volume`'s bays are about (D-109).
    pub name: String,
    /// How many of its ports say something is plugged in: headphones in a jack, say. Rises when
    /// one is plugged in, even though the device was there all along.
    pub plugged: u8,
    /// Hardware (ALSA, Bluetooth, USB), not a virtual device an app made, like an equalizer's.
    pub hardware: bool,
    channels: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Output,
    Input,
}

/// What kind of device it is, for its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Form {
    #[default]
    Speaker,
    Headphones,
    Headset,
    /// A monitor or TV.
    Display,
    Microphone,
}

/// An app playing sound.
#[derive(Debug, Clone, PartialEq)]
pub struct Playback {
    pub index: u32,
    /// The app's name, like `Firefox`.
    pub app: String,
    /// What to find its icon by, like `firefox`.
    pub app_id: Option<String>,
    /// What it's playing, when it says.
    pub title: Option<String>,
    pub volume: u32,
    pub muted: bool,
    channels: u8,
}

/// A stereo output at `volume` percent, for plugin tests.
#[doc(hidden)]
pub fn test_device(volume: u32) -> Device {
    Device {
        index: 0,
        direction: Direction::Output,
        description: "Speakers".into(),
        volume,
        muted: false,
        form: Form::Speaker,
        connection: None,
        name: "speakers".into(),
        plugged: 0,
        hardware: true,
        channels: 2,
    }
}

impl Audio {
    pub fn default_sink(&self) -> Option<&Device> {
        self.default_sink.and_then(|index| self.sinks.get(index))
    }

    pub fn default_source(&self) -> Option<&Device> {
        self.default_source
            .and_then(|index| self.sources.get(index))
    }
}

/// What a change on the server was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    devices: bool,
    streams: bool,
}

impl Change {
    const ALL: Self = Self {
        devices: true,
        streams: true,
    };
}

/// The devices, then again after every change to them. Unchanged readings are skipped, and
/// changes that only concern apps' streams don't wake it.
pub fn watch() -> impl Stream<Item = Audio> {
    watching(|change| change.devices, || audio().boxed())
}

/// The apps playing sound, then again after every change to them. Meant to be watched only while
/// something shows them, like an open popover.
pub fn watch_streams() -> impl Stream<Item = Vec<Playback>> {
    watching(|change| change.streams, || streams().boxed())
}

type Reading<T> = futures_util::future::BoxFuture<'static, io::Result<T>>;

fn watching<T: Clone + PartialEq + Send + 'static>(
    wanted: fn(Change) -> bool,
    read: fn() -> Reading<T>,
) -> impl futures_util::Stream<Item = T> {
    struct State<T> {
        events: Option<broadcast::Receiver<Change>>,
        last: Option<T>,
        stale: bool,
    }

    let state = State {
        events: None,
        last: None,
        stale: true,
    };
    stream::unfold(state, move |mut state| async move {
        // Subscribed before reading, so a change during the read still marks it stale.
        let events = state.events.get_or_insert_with(|| bus().subscribe());
        loop {
            if state.stale {
                state.stale = false;
                match read().await {
                    Ok(reading) if state.last.as_ref() != Some(&reading) => {
                        state.last = Some(reading.clone());
                        return Some((reading, state));
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("eave-pulse: reading: {error}"),
                }
            }
            let mut take = |change: Result<Change, _>| match change {
                Ok(change) => state.stale |= wanted(change),
                Err(broadcast::error::RecvError::Lagged(_)) => state.stale = true,
                Err(broadcast::error::RecvError::Closed) => {}
            };
            match events.recv().await {
                Err(broadcast::error::RecvError::Closed) => return None,
                change => take(change),
            }
            // One volume drag is dozens of events; read once for the lot.
            while let Some(change) = events.recv().now_or_never() {
                take(change);
            }
        }
    })
    .boxed()
}

fn channel_volume(channels: u8, percent: u32) -> ChannelVolume {
    let raw = (u64::from(Volume::NORM.as_u32()) * u64::from(percent) / 100) as u32;
    let mut volume = ChannelVolume::empty();
    for _ in 0..channels.max(1) {
        volume.push(Volume::from_u32_clamped(raw));
    }
    volume
}

/// Sets every channel of `device` to `percent` of normal volume.
pub async fn set_volume(device: &Device, percent: u32) -> io::Result<()> {
    let params = protocol::SetDeviceVolumeParams {
        device_index: Some(device.index),
        device_name: None,
        volume: channel_volume(device.channels, percent),
    };
    command(match device.direction {
        Direction::Output => Command::SetSinkVolume(params),
        Direction::Input => Command::SetSourceVolume(params),
    })
    .await
}

pub async fn set_mute(device: &Device, mute: bool) -> io::Result<()> {
    let params = protocol::SetDeviceMuteParams {
        device_index: Some(device.index),
        device_name: None,
        mute,
    };
    command(match device.direction {
        Direction::Output => Command::SetSinkMute(params),
        Direction::Input => Command::SetSourceMute(params),
    })
    .await
}

/// Makes `device` where sound goes (an output) or comes from (an input).
pub async fn set_default(device: &Device) -> io::Result<()> {
    let name = CString::new(device.name.clone()).map_err(io::Error::other)?;
    command(match device.direction {
        Direction::Output => Command::SetDefaultSink(name),
        Direction::Input => Command::SetDefaultSource(name),
    })
    .await
}

pub async fn set_stream_volume(stream: &Playback, percent: u32) -> io::Result<()> {
    command(Command::SetSinkInputVolume(
        protocol::SetStreamVolumeParams {
            index: stream.index,
            volume: channel_volume(stream.channels, percent),
        },
    ))
    .await
}

pub async fn set_stream_mute(stream: &Playback, mute: bool) -> io::Result<()> {
    command(Command::SetSinkInputMute(protocol::SetStreamMuteParams {
        index: stream.index,
        mute,
    }))
    .await
}

async fn audio() -> io::Result<Audio> {
    blocking(|| {
        let (mut socket, version) = connect()?;
        let server: protocol::ServerInfo =
            roundtrip(&mut socket, version, 2, Command::GetServerInfo)?;
        let sinks: protocol::SinkInfoList =
            roundtrip(&mut socket, version, 3, Command::GetSinkInfoList)?;
        let sources: protocol::SourceInfoList =
            roundtrip(&mut socket, version, 4, Command::GetSourceInfoList)?;
        Ok(read_audio(&server, sinks, sources))
    })
    .await
}

async fn streams() -> io::Result<Vec<Playback>> {
    blocking(|| {
        let (mut socket, version) = connect()?;
        let inputs: protocol::SinkInputInfoList =
            roundtrip(&mut socket, version, 2, Command::GetSinkInputInfoList)?;
        Ok(inputs.into_iter().filter_map(read_stream).collect())
    })
    .await
}

fn text(bytes: Option<&[u8]>) -> Option<String> {
    let bytes = bytes?;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn lossy(text: &CString) -> String {
    text.to_string_lossy().into_owned()
}

/// What a device is, from its active port's type, else its form factor property.
fn form(
    port: Option<&protocol::port_info::PortInfo>,
    props: &protocol::Props,
    direction: Direction,
) -> Form {
    use protocol::port_info::PortType;
    let by_port = port.and_then(|port| match port.port_type {
        PortType::Headphones => Some(Form::Headphones),
        PortType::Headset | PortType::Handsfree | PortType::Handset | PortType::Earpiece => {
            Some(Form::Headset)
        }
        PortType::Hdmi | PortType::Tv | PortType::Video => Some(Form::Display),
        PortType::Speaker => Some(Form::Speaker),
        PortType::Mic => Some(Form::Microphone),
        _ => None,
    });
    let by_factor = || match text(props.get(protocol::Prop::DeviceFormFactor))?.as_str() {
        "headphone" => Some(Form::Headphones),
        "headset" | "hands-free" | "handset" => Some(Form::Headset),
        "tv" | "computer" => Some(Form::Display),
        "speaker" => Some(Form::Speaker),
        "microphone" | "webcam" => Some(Form::Microphone),
        _ => None,
    };
    by_port.or_else(by_factor).unwrap_or(match direction {
        Direction::Output => Form::Speaker,
        Direction::Input => Form::Microphone,
    })
}

fn connection(
    port: Option<&protocol::port_info::PortInfo>,
    props: &protocol::Props,
) -> Option<&'static str> {
    if port.is_some_and(|port| port.port_type == protocol::port_info::PortType::Hdmi) {
        return Some("HDMI");
    }
    match text(props.get(protocol::Prop::DeviceBus))?.as_str() {
        "bluetooth" => Some("Bluetooth"),
        "usb" => Some("USB"),
        _ => None,
    }
}

/// Whether a device is hardware: a sound card's, a Bluetooth or USB device's. Virtual ones (an
/// equalizer's, a combined or null sink) name no bus and aren't ALSA's or BlueZ's.
fn hardware(name: &str, props: &protocol::Props) -> bool {
    name.starts_with("alsa_")
        || name.starts_with("bluez_")
        || text(props.get(protocol::Prop::DeviceBus)).is_some()
}

#[allow(clippy::too_many_arguments)]
fn device(
    index: u32,
    direction: Direction,
    name: &CString,
    description: Option<&CString>,
    volume: &ChannelVolume,
    muted: bool,
    port: Option<&protocol::port_info::PortInfo>,
    ports: &[protocol::port_info::PortInfo],
    props: &protocol::Props,
) -> Device {
    let channels = volume.channels();
    let name = lossy(name);
    let plugged = ports
        .iter()
        .filter(|port| port.available == protocol::port_info::PortAvailable::Yes)
        .count();
    Device {
        index,
        direction,
        description: description.map_or_else(|| name.clone(), lossy),
        volume: percent(channels),
        muted,
        form: form(port, props, direction),
        connection: connection(port, props),
        hardware: hardware(&name, props),
        name,
        plugged: plugged.min(u8::MAX.into()) as u8,
        channels: channels.len() as u8,
    }
}

fn read_audio(
    server: &protocol::ServerInfo,
    sinks: protocol::SinkInfoList,
    sources: protocol::SourceInfoList,
) -> Audio {
    let sinks: Vec<Device> = sinks
        .iter()
        .map(|sink| {
            device(
                sink.index,
                Direction::Output,
                &sink.name,
                sink.description.as_ref(),
                &sink.cvolume,
                sink.muted,
                sink.ports.get(sink.active_port),
                &sink.ports,
                &sink.props,
            )
        })
        .collect();
    let sources: Vec<Device> = sources
        .iter()
        .filter(|source| source.monitor_of_sink_index.is_none())
        .map(|source| {
            device(
                source.index,
                Direction::Input,
                &source.name,
                source.description.as_ref(),
                &source.cvolume,
                source.muted,
                source.ports.get(source.active_port),
                &source.ports,
                &source.props,
            )
        })
        .collect();
    let find = |devices: &[Device], name: Option<&CString>| {
        let name = lossy(name?);
        devices.iter().position(|device| device.name == name)
    };
    Audio {
        default_sink: find(&sinks, server.default_sink_name.as_ref()),
        default_source: find(&sources, server.default_source_name.as_ref()),
        sinks,
        sources,
    }
}

fn read_stream(input: protocol::SinkInputInfo) -> Option<Playback> {
    use protocol::Prop;
    if !input.has_volume {
        return None;
    }
    let props = &input.props;
    let title = text(props.get(Prop::MediaName));
    let app = text(props.get(Prop::ApplicationName))
        .or_else(|| title.clone())
        .unwrap_or_else(|| lossy(&input.name));
    let app_id = text(props.get(Prop::ApplicationIconName))
        .or_else(|| text(props.get(Prop::ApplicationProcessBinary)));
    let channels = input.cvolume.channels();
    Some(Playback {
        index: input.index,
        // A title that only repeats the app's name says nothing.
        title: title.filter(|title| *title != app),
        app,
        app_id,
        volume: percent(channels),
        muted: input.muted,
        channels: channels.len() as u8,
    })
}

/// The loudest channel, as a rounded percent of normal volume, the way `pactl` reports it.
fn percent(channels: &[Volume]) -> u32 {
    let raw = channels.iter().map(Volume::as_u32).max().unwrap_or(0);
    let norm = Volume::NORM.as_u32();
    ((u64::from(raw) * 100 + u64::from(norm) / 2) / u64::from(norm)) as u32
}

async fn command(command: Command) -> io::Result<()> {
    blocking(move || {
        let (mut socket, version) = connect()?;
        ack(&mut socket, version, 2, command)
    })
    .await
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

type Socket = BufReader<UnixStream>;

/// A new, authenticated connection, and the protocol version it speaks.
fn connect() -> io::Result<(Socket, u16)> {
    let path = pulseaudio::socket_path_from_env()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no PulseAudio socket"))?;
    let mut socket = BufReader::new(UnixStream::connect(path)?);
    let cookie = pulseaudio::cookie_path_from_env()
        .and_then(|path| std::fs::read(path).ok())
        .unwrap_or_default();
    let auth = protocol::AuthParams {
        version: protocol::MAX_VERSION,
        supports_shm: false,
        supports_memfd: false,
        cookie,
    };
    let reply: protocol::AuthReply =
        roundtrip(&mut socket, protocol::MAX_VERSION, 0, Command::Auth(auth))?;
    let version = protocol::MAX_VERSION.min(reply.version);

    let mut props = protocol::Props::new();
    props.set(protocol::Prop::ApplicationName, c"eave");
    let _: protocol::SetClientNameReply =
        roundtrip(&mut socket, version, 1, Command::SetClientName(props))?;
    Ok((socket, version))
}

fn roundtrip<R: protocol::CommandReply>(
    socket: &mut Socket,
    version: u16,
    seq: u32,
    command: Command,
) -> io::Result<R> {
    protocol::write_command_message(socket.get_mut(), seq, &command, version)
        .map_err(protocol_error)?;
    let (_, reply) = protocol::read_reply_message::<R>(socket, version).map_err(protocol_error)?;
    Ok(reply)
}

fn ack(socket: &mut Socket, version: u16, seq: u32, command: Command) -> io::Result<()> {
    protocol::write_command_message(socket.get_mut(), seq, &command, version)
        .map_err(protocol_error)?;
    protocol::read_ack_message(socket).map_err(protocol_error)?;
    Ok(())
}

fn protocol_error(error: protocol::ProtocolError) -> io::Error {
    io::Error::other(error)
}

/// The one subscription, started by the first watcher. Each message says what changed.
fn bus() -> &'static broadcast::Sender<Change> {
    static BUS: OnceLock<broadcast::Sender<Change>> = OnceLock::new();
    BUS.get_or_init(|| {
        let (sender, _) = broadcast::channel(16);
        let events = sender.clone();
        std::thread::Builder::new()
            .name("eave-pulse".into())
            .spawn(move || listen(&events))
            .expect("spawning a thread");
        sender
    })
}

/// Blocks on the event socket, forever. Reconnects with a growing delay if the server goes away.
fn listen(events: &broadcast::Sender<Change>) {
    const MAX_DELAY: Duration = Duration::from_secs(30);
    let mut delay = Duration::from_secs(1);
    loop {
        match subscribe() {
            Ok((mut socket, version)) => {
                delay = Duration::from_secs(1);
                // Whatever happened while we were away.
                let _ = events.send(Change::ALL);
                loop {
                    match protocol::read_command_message(&mut socket, version) {
                        Ok((_, Command::SubscribeEvent(event))) => {
                            let _ = events.send(change(event.event_facility));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!("eave-pulse: event socket: {error}, reconnecting");
                            break;
                        }
                    }
                }
            }
            Err(error) => eprintln!("eave-pulse: {error}"),
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(MAX_DELAY);
    }
}

/// What an event from `facility` is about.
fn change(facility: protocol::SubscriptionEventFacility) -> Change {
    use protocol::SubscriptionEventFacility as Facility;
    match facility {
        Facility::SinkInput => Change {
            devices: false,
            streams: true,
        },
        // A stream follows its sink, and a new default moves streams along.
        Facility::Card => Change {
            devices: true,
            streams: false,
        },
        Facility::Sink | Facility::Source | Facility::Server => Change {
            devices: true,
            streams: facility != Facility::Source,
        },
        _ => Change::ALL,
    }
}

fn subscribe() -> io::Result<(Socket, u16)> {
    let (mut socket, version) = connect()?;
    use protocol::SubscriptionMask as Mask;
    // Cards for jacks: plugging headphones in changes the card's ports.
    let mask = Mask::SINK | Mask::SOURCE | Mask::SINK_INPUT | Mask::SERVER | Mask::CARD;
    ack(&mut socket, version, 2, Command::Subscribe(mask))?;
    Ok((socket, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_rounds_like_pactl() {
        let norm = Volume::NORM.as_u32();
        assert_eq!(percent(&[Volume::from_u32_clamped(norm)]), 100);
        assert_eq!(percent(&[Volume::from_u32_clamped(norm * 45 / 100)]), 45);
        assert_eq!(
            percent(&[
                Volume::from_u32_clamped(norm / 2),
                Volume::from_u32_clamped(norm)
            ]),
            100
        );
        assert_eq!(percent(&[]), 0);
    }

    #[test]
    fn stream_events_leave_devices_alone() {
        use protocol::SubscriptionEventFacility as Facility;
        assert!(!change(Facility::SinkInput).devices);
        assert!(change(Facility::SinkInput).streams);
        assert!(change(Facility::Sink).devices);
        assert!(!change(Facility::Source).streams);
    }

    fn props(pairs: &[(protocol::Prop, &str)]) -> protocol::Props {
        let mut props = protocol::Props::new();
        for (prop, value) in pairs {
            props.set(*prop, CString::new(*value).unwrap().as_c_str());
        }
        props
    }

    #[test]
    fn devices_get_their_form_from_the_port_then_the_props() {
        use protocol::Prop;
        let headphones = protocol::port_info::PortInfo {
            name: c"headphones".into(),
            port_type: protocol::port_info::PortType::Headphones,
            description: None,
            dir: protocol::port_info::PortDirection::Input,
            priority: 0,
            available: protocol::port_info::PortAvailable::Unknown,
            availability_group: None,
        };
        let none = protocol::Props::new();
        assert_eq!(
            form(Some(&headphones), &none, Direction::Output),
            Form::Headphones
        );
        let headset = props(&[
            (Prop::DeviceFormFactor, "headset"),
            (Prop::DeviceBus, "bluetooth"),
        ]);
        assert_eq!(form(None, &headset, Direction::Output), Form::Headset);
        assert_eq!(connection(None, &headset), Some("Bluetooth"));
        assert_eq!(form(None, &none, Direction::Input), Form::Microphone);
    }

    #[test]
    fn monitors_are_not_inputs_and_defaults_are_found_by_name() {
        let sink = protocol::SinkInfo {
            index: 7,
            name: c"speakers".into(),
            ..Default::default()
        };
        let mic = protocol::SourceInfo {
            index: 3,
            name: c"mic".into(),
            ..Default::default()
        };
        let monitor = protocol::SourceInfo {
            index: 4,
            name: c"speakers.monitor".into(),
            monitor_of_sink_index: Some(7),
            ..Default::default()
        };
        let server = protocol::ServerInfo {
            default_sink_name: Some(c"speakers".into()),
            default_source_name: Some(c"mic".into()),
            ..Default::default()
        };
        let audio = read_audio(&server, vec![sink], vec![monitor, mic]);
        assert_eq!(audio.sources.len(), 1);
        assert_eq!(audio.default_source().unwrap().index, 3);
        assert_eq!(audio.default_sink().unwrap().index, 7);
        assert_eq!(audio.sources[0].direction, Direction::Input);
    }

    #[test]
    fn streams_are_named_by_their_app() {
        use protocol::Prop;
        let input = protocol::SinkInputInfo {
            index: 1,
            name: c"playback".into(),
            has_volume: true,
            props: props(&[
                (Prop::ApplicationName, "Firefox"),
                (Prop::ApplicationIconName, "firefox"),
                (Prop::MediaName, "Firefox"),
            ]),
            ..Default::default()
        };
        let stream = read_stream(input).unwrap();
        assert_eq!(stream.app, "Firefox");
        assert_eq!(stream.app_id.as_deref(), Some("firefox"));
        assert_eq!(
            stream.title, None,
            "a title that repeats the app says nothing"
        );
    }
}
