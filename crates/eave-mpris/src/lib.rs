//! Media players for eave plugins, through MPRIS on the session bus
//! ([D-56](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)): what each player is
//! playing, and play/pause, next, previous and seeking.
//!
//! Every player owns a bus name under `org.mpris.MediaPlayer2.` and serves one object,
//! `/org/mpris/MediaPlayer2`. [`watch`] follows players coming and going and every property they
//! change, with no polling. The one thing players don't signal is the position as it advances, so
//! [`Player::position_at`] works it out from when it was read.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eave_dbus::zbus::message::Type as MessageType;
use eave_dbus::zbus::names::BusName;
use eave_dbus::zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use eave_dbus::zbus::{Connection, MatchRule, MessageStream};
use eave_dbus::{Signals, drained};
use futures_util::{FutureExt, StreamExt, stream};

pub use eave_dbus::{Stream, zbus};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// A media player and what it's playing.
#[derive(Debug, Clone)]
pub struct Player {
    /// Its name on the bus, which the commands take: `org.mpris.MediaPlayer2.spotify`.
    pub bus_name: String,
    /// Its own name for itself, like `Spotify` or `Mozilla Firefox`.
    pub identity: String,
    /// Its desktop entry, for its icon: `spotify`, `firefox`. Empty when it doesn't say.
    pub desktop_entry: String,
    pub status: Status,
    pub title: String,
    /// The artists, joined with commas.
    pub artist: String,
    pub album: String,
    /// The track's cover art, as a URL: `file://` or `https://` mostly. Empty when there's none.
    pub art_url: String,
    /// The track's length, when the player knows it.
    pub length: Option<Duration>,
    /// The track's id, which seeking needs. `None` when the player gives none.
    pub track_id: Option<OwnedObjectPath>,
    /// Where it was in the track at `read_at`.
    pub position: Duration,
    pub read_at: Instant,
    /// How fast it plays: 1.0 is normal speed.
    pub rate: f64,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_seek: bool,
}

/// Equal when they show the same thing: `read_at` alone changing isn't a change, but a new
/// position (after a seek) is.
impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self.bus_name == other.bus_name
            && self.identity == other.identity
            && self.desktop_entry == other.desktop_entry
            && self.status == other.status
            && self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.art_url == other.art_url
            && self.length == other.length
            && self.track_id == other.track_id
            && self.position == other.position
            && self.rate == other.rate
            && self.can_go_next == other.can_go_next
            && self.can_go_previous == other.can_go_previous
            && self.can_play == other.can_play
            && self.can_pause == other.can_pause
            && self.can_seek == other.can_seek
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Playing,
    Paused,
    Stopped,
}

impl Status {
    fn from_mpris(status: &str) -> Self {
        match status {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

impl Player {
    pub fn playing(&self) -> bool {
        self.status == Status::Playing
    }

    /// Where it is in the track at `now`: where it was, moved on by the time since if it's
    /// playing, and never past the end.
    pub fn position_at(&self, now: Instant) -> Duration {
        let mut position = self.position;
        if self.playing() && self.rate > 0.0 {
            let elapsed = now.saturating_duration_since(self.read_at);
            position += elapsed.mul_f64(self.rate);
        }
        match self.length {
            Some(length) => position.min(length),
            None => position,
        }
    }
}

/// Every player, sorted by bus name, then again whenever one comes, goes or changes. Unchanged
/// lists aren't yielded. Without a session bus the stream ends.
pub fn watch() -> impl Stream<Item = Vec<Player>> {
    struct State {
        signals: Option<(Connection, stream::Select<Signals, Signals>)>,
        last: Option<Vec<Player>>,
        stale: bool,
    }

    let state = State {
        signals: None,
        last: None,
        stale: true,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if state.signals.is_none() {
                match subscribe().await {
                    Ok(signals) => state.signals = Some(signals),
                    Err(error) => {
                        eprintln!("eave-mpris: {error}");
                        return None;
                    }
                }
            }
            let (connection, signals) = state.signals.as_mut()?;

            if state.stale {
                // Subscribed first, so a change during the snapshot still marks it stale.
                match snapshot(connection).await {
                    Ok(players) => {
                        state.stale = false;
                        if state.last.as_ref() != Some(&players) {
                            state.last = Some(players.clone());
                            return Some((players, state));
                        }
                    }
                    Err(error) => eprintln!("eave-mpris: {error}"),
                }
            }

            let _signal = signals.next().await?;
            // A change usually comes as a burst of signals; take them all, let the player
            // settle, then read again.
            while let Some(Some(_)) = signals.next().now_or_never() {}
            tokio::time::sleep(Duration::from_millis(50)).await;
            while let Some(Some(_)) = signals.next().now_or_never() {}
            state.stale = true;
        }
    })
}

/// Players' names coming and going, and every signal from a player's object: its property
/// changes and `Seeked`.
async fn subscribe() -> zbus::Result<(Connection, stream::Select<Signals, Signals>)> {
    let connection = eave_dbus::session().await?;
    let names = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg0ns(ROOT)?
        .build();
    let players = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .path(PATH)?
        .build();
    // Read as they come, even during a snapshot, so a chatty player can't stall the connection.
    let names = drained(MessageStream::for_match_rule(names, &connection, Some(16)).await?);
    let players = drained(MessageStream::for_match_rule(players, &connection, Some(64)).await?);
    Ok((connection, stream::select(names, players)))
}

async fn snapshot(connection: &Connection) -> zbus::Result<Vec<Player>> {
    let dbus = zbus::fdo::DBusProxy::new(connection).await?;
    let mut names: Vec<String> = dbus
        .list_names()
        .await?
        .into_iter()
        .map(|name| name.to_string())
        .filter(|name| is_player(name))
        .collect();
    names.sort();
    let mut players = Vec::with_capacity(names.len());
    for name in names {
        // A player can leave between listing and reading it; it's simply not there.
        match read(connection, name).await {
            Ok(player) => players.push(player),
            Err(error) => eprintln!("eave-mpris: {error}"),
        }
    }
    Ok(players)
}

/// playerctld isn't a player: it mirrors whichever one played last, which would show it twice.
fn is_player(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest != "playerctld")
}

type Properties = HashMap<String, OwnedValue>;

async fn read(connection: &Connection, bus_name: String) -> zbus::Result<Player> {
    let player = get_all(connection, &bus_name, PLAYER).await?;
    let read_at = Instant::now();
    // The root interface only names the player; a player that leaves it out still plays.
    let root = get_all(connection, &bus_name, ROOT)
        .await
        .unwrap_or_default();
    Ok(parse(bus_name, &root, &player, read_at))
}

async fn get_all(
    connection: &Connection,
    bus_name: &str,
    interface: &'static str,
) -> zbus::Result<Properties> {
    let proxy = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(bus_name.to_owned())?
        .path(PATH)?
        .build()
        .await?;
    let interface = zbus::names::InterfaceName::try_from(interface)?;
    Ok(proxy.get_all(interface).await?)
}

fn parse(bus_name: String, root: &Properties, player: &Properties, read_at: Instant) -> Player {
    let metadata: Properties = player
        .get("Metadata")
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Properties::try_from(value).ok())
        .unwrap_or_default();
    let identity = match get::<String>(root, "Identity") {
        identity if identity.is_empty() => fallback_identity(&bus_name),
        identity => identity,
    };
    let rate = match get::<f64>(player, "Rate") {
        rate if rate > 0.0 => rate,
        _ => 1.0,
    };
    Player {
        identity,
        desktop_entry: get(root, "DesktopEntry"),
        status: Status::from_mpris(&get::<String>(player, "PlaybackStatus")),
        title: get::<String>(&metadata, "xesam:title").trim().to_string(),
        artist: artists(&metadata),
        album: get::<String>(&metadata, "xesam:album").trim().to_string(),
        art_url: get::<String>(&metadata, "mpris:artUrl").trim().to_string(),
        length: micros(metadata.get("mpris:length")).filter(|length| !length.is_zero()),
        track_id: track_id(&metadata),
        position: micros(player.get("Position")).unwrap_or_default(),
        read_at,
        rate,
        can_go_next: get(player, "CanGoNext"),
        can_go_previous: get(player, "CanGoPrevious"),
        can_play: get(player, "CanPlay"),
        can_pause: get(player, "CanPause"),
        can_seek: get(player, "CanSeek"),
        bus_name,
    }
}

fn get<T: TryFrom<OwnedValue> + Default>(properties: &Properties, name: &str) -> T {
    properties
        .get(name)
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| T::try_from(value).ok())
        .unwrap_or_default()
}

/// `xesam:artist` is a list, but some players send a single string.
fn artists(metadata: &Properties) -> String {
    let Some(value) = metadata.get("xesam:artist") else {
        return String::new();
    };
    let names: Vec<String> = match &**value {
        Value::Str(name) => vec![name.to_string()],
        _ => value
            .try_clone()
            .ok()
            .and_then(|value| Vec::<String>::try_from(value).ok())
            .unwrap_or_default(),
    };
    names
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A time in microseconds. The spec says signed, but some players send unsigned.
fn micros(value: Option<&OwnedValue>) -> Option<Duration> {
    let micros = match &**value? {
        Value::I64(micros) => u64::try_from(*micros).unwrap_or(0),
        Value::U64(micros) => *micros,
        Value::I32(micros) => u64::try_from(*micros).unwrap_or(0),
        Value::U32(micros) => u64::from(*micros),
        _ => return None,
    };
    Some(Duration::from_micros(micros))
}

/// `mpris:trackid` is an object path, but some players send a string. `/org/mpris/MediaPlayer2/
/// TrackList/NoTrack` means none.
fn track_id(metadata: &Properties) -> Option<OwnedObjectPath> {
    let path = match &**metadata.get("mpris:trackid")? {
        Value::ObjectPath(path) => OwnedObjectPath::from(path.to_owned()),
        Value::Str(path) => OwnedObjectPath::try_from(path.as_str()).ok()?,
        _ => return None,
    };
    (path.as_str() != "/org/mpris/MediaPlayer2/TrackList/NoTrack").then_some(path)
}

/// `org.mpris.MediaPlayer2.vlc.instance42` reads as `vlc`.
fn fallback_identity(bus_name: &str) -> String {
    let rest = bus_name.strip_prefix(PREFIX).unwrap_or(bus_name);
    rest.split('.').next().unwrap_or(rest).to_string()
}

async fn call(bus_name: &str, method: &'static str) -> zbus::Result<()> {
    let connection = eave_dbus::session().await?;
    let destination = BusName::try_from(bus_name)?;
    connection
        .call_method(Some(destination), PATH, Some(PLAYER), method, &())
        .await?;
    Ok(())
}

pub async fn play_pause(bus_name: &str) -> zbus::Result<()> {
    call(bus_name, "PlayPause").await
}

pub async fn next(bus_name: &str) -> zbus::Result<()> {
    call(bus_name, "Next").await
}

pub async fn previous(bus_name: &str) -> zbus::Result<()> {
    call(bus_name, "Previous").await
}

/// Moves to `position` in the track `track_id`. The player ignores it if the track has changed
/// since.
pub async fn set_position(
    bus_name: &str,
    track_id: &OwnedObjectPath,
    position: Duration,
) -> zbus::Result<()> {
    let micros = i64::try_from(position.as_micros()).unwrap_or(i64::MAX);
    let connection = eave_dbus::session().await?;
    let destination = BusName::try_from(bus_name)?;
    connection
        .call_method(
            Some(destination),
            PATH,
            Some(PLAYER),
            "SetPosition",
            &(track_id, micros),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::ObjectPath;

    fn owned(value: Value<'_>) -> OwnedValue {
        value.try_into_owned().unwrap()
    }

    fn metadata(entries: Vec<(&str, Value<'static>)>) -> OwnedValue {
        let map: HashMap<String, Value<'static>> = entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        owned(Value::from(map))
    }

    fn player(entries: Vec<(&str, OwnedValue)>) -> Properties {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    }

    #[test]
    fn only_names_under_the_prefix_are_players() {
        assert!(is_player("org.mpris.MediaPlayer2.spotify"));
        assert!(is_player("org.mpris.MediaPlayer2.vlc.instance42"));
        assert!(!is_player("org.mpris.MediaPlayer2."));
        assert!(!is_player("org.mpris.MediaPlayer2"));
        assert!(!is_player("org.freedesktop.Notifications"));
        assert!(!is_player("org.mpris.MediaPlayer2.playerctld"));
    }

    #[test]
    fn reads_a_track() {
        let track = ObjectPath::try_from("/com/spotify/track/1").unwrap();
        let properties = player(vec![
            ("PlaybackStatus", owned(Value::from("Playing"))),
            ("Position", owned(Value::I64(12_000_000))),
            ("CanGoNext", owned(Value::Bool(true))),
            ("CanSeek", owned(Value::Bool(true))),
            (
                "Metadata",
                metadata(vec![
                    ("xesam:title", Value::from(" Midnight City ")),
                    ("xesam:artist", Value::from(vec!["M83", "Someone"])),
                    ("xesam:album", Value::from("Hurry Up, We're Dreaming")),
                    ("mpris:length", Value::I64(243_000_000)),
                    ("mpris:trackid", Value::ObjectPath(track)),
                    ("mpris:artUrl", Value::from("file:///tmp/cover.jpg")),
                ]),
            ),
        ]);
        let root = player(vec![
            ("Identity", owned(Value::from("Spotify"))),
            ("DesktopEntry", owned(Value::from("spotify"))),
        ]);
        let parsed = parse(
            "org.mpris.MediaPlayer2.spotify".into(),
            &root,
            &properties,
            Instant::now(),
        );
        assert_eq!(parsed.identity, "Spotify");
        assert_eq!(parsed.desktop_entry, "spotify");
        assert_eq!(parsed.status, Status::Playing);
        assert_eq!(parsed.title, "Midnight City");
        assert_eq!(parsed.artist, "M83, Someone");
        assert_eq!(parsed.album, "Hurry Up, We're Dreaming");
        assert_eq!(parsed.art_url, "file:///tmp/cover.jpg");
        assert_eq!(parsed.length, Some(Duration::from_secs(243)));
        assert_eq!(parsed.position, Duration::from_secs(12));
        assert_eq!(parsed.track_id.unwrap().as_str(), "/com/spotify/track/1");
        assert!(parsed.can_go_next && parsed.can_seek && !parsed.can_go_previous);
        assert_eq!(parsed.rate, 1.0);
    }

    #[test]
    fn reads_what_lenient_players_send() {
        let properties = player(vec![(
            "Metadata",
            metadata(vec![
                ("xesam:artist", Value::from("Solo")),
                ("mpris:length", Value::U64(5_000_000)),
                (
                    "mpris:trackid",
                    Value::from("/org/mpris/MediaPlayer2/TrackList/NoTrack"),
                ),
            ]),
        )]);
        let parsed = parse(
            "org.mpris.MediaPlayer2.vlc.instance42".into(),
            &Properties::new(),
            &properties,
            Instant::now(),
        );
        assert_eq!(parsed.identity, "vlc");
        assert_eq!(parsed.artist, "Solo");
        assert_eq!(parsed.length, Some(Duration::from_secs(5)));
        assert_eq!(parsed.track_id, None);
        assert_eq!(parsed.status, Status::Stopped);
    }

    #[test]
    fn the_position_moves_on_only_while_playing() {
        let start = Instant::now();
        let mut player = parse(
            "org.mpris.MediaPlayer2.x".into(),
            &Properties::new(),
            &player(vec![("PlaybackStatus", owned(Value::from("Playing")))]),
            start,
        );
        player.position = Duration::from_secs(10);
        player.length = Some(Duration::from_secs(60));
        let later = start + Duration::from_secs(5);
        assert_eq!(player.position_at(later), Duration::from_secs(15));

        player.rate = 2.0;
        assert_eq!(player.position_at(later), Duration::from_secs(20));

        // Never past the end.
        assert_eq!(
            player.position_at(start + Duration::from_secs(600)),
            Duration::from_secs(60)
        );

        player.status = Status::Paused;
        assert_eq!(player.position_at(later), Duration::from_secs(10));
    }

    #[test]
    fn reading_again_later_is_no_change() {
        let read = |at| {
            parse(
                "org.mpris.MediaPlayer2.x".into(),
                &Properties::new(),
                &Properties::new(),
                at,
            )
        };
        let now = Instant::now();
        assert_eq!(read(now), read(now + Duration::from_secs(1)));
    }
}

#[cfg(test)]
mod live {
    /// `cargo test -p eave-mpris -- --ignored --nocapture` prints the players on this session.
    #[test]
    #[ignore = "reads the real session bus"]
    fn reads_this_session() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let players = runtime.block_on(async {
            use futures_util::StreamExt;
            std::pin::pin!(super::watch()).next().await
        });
        println!("{players:#?}");
    }
}
