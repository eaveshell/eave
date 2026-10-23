//! D-Bus for eave plugins ([D-7](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).
//!
//! One system bus connection and one session bus connection, each shared by every widget. [`watch`] gives a widget a snapshot of a
//! service's state and a fresh one whenever the service reports a property change on an object
//! the snapshot depends on, with no polling:
//!
//! ```ignore
//! fn subscription(&self) -> Subscription<Message> {
//!     Subscription::run(|| eave_dbus::watch("org.freedesktop.UPower", snapshot)).map(Message::Battery)
//! }
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as Lock};
use std::time::Duration;

use futures_util::future::{self, Either};
use futures_util::{StreamExt, stream};
use tokio::sync::{Mutex, Notify};
use zbus::message::Type as MessageType;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

pub use futures_util::Stream;
pub use zbus;

pub mod portal;
pub mod systemd;

/// The longest any call waits for its answer. Without one, a service that never answers (or a
/// connection that stopped reading) holds the caller forever. Longer than anything eave asks a
/// person through a service, like BlueZ's 60 s for confirming a pairing code.
const METHOD_TIMEOUT: Duration = Duration::from_secs(75);

/// The longest a snapshot may take before the connection is taken for broken and opened again.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

static SYSTEM: Mutex<Option<Connection>> = Mutex::const_new(None);
static SESSION: Mutex<Option<Connection>> = Mutex::const_new(None);

/// The shared system bus connection, opened on first use, and again after it broke.
pub async fn system() -> zbus::Result<Connection> {
    shared(&SYSTEM, zbus::connection::Builder::system).await
}

/// The shared session bus connection, opened on first use: media players, notifications, the
/// tray.
pub async fn session() -> zbus::Result<Connection> {
    shared(&SESSION, zbus::connection::Builder::session).await
}

async fn shared(
    slot: &Mutex<Option<Connection>>,
    builder: fn() -> zbus::Result<zbus::connection::Builder<'static>>,
) -> zbus::Result<Connection> {
    let mut slot = slot.lock().await;
    if let Some(connection) = &*slot {
        return Ok(connection.clone());
    }
    let connection = builder()?.method_timeout(METHOD_TIMEOUT).build().await?;
    *slot = Some(connection.clone());
    Ok(connection)
}

/// Drops `connection` as the shared one, if it still is, so the next caller opens a new one.
async fn forget(connection: &Connection) {
    for slot in [&SYSTEM, &SESSION] {
        let mut slot = slot.lock().await;
        let same = slot
            .as_ref()
            .is_some_and(|shared| shared.unique_name() == connection.unique_name());
        if same {
            *slot = None;
        }
    }
}

/// `signals`, read as they come by a task of their own, whether or not anyone is reading the
/// stream it gives. zbus stops reading the connection while a stream's queue is full, and that
/// includes the replies to calls: a stream left unread while its reader waits on a call would
/// hold every caller on the connection for good. Ends when `signals` does; dropping it stops the
/// task.
pub fn drained(signals: MessageStream) -> Signals {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = Aborting(tokio::spawn(async move {
        let mut signals = signals;
        while let Some(message) = signals.next().await {
            if sender.send(message).is_err() {
                return;
            }
        }
    }));
    Box::pin(stream::unfold(
        (receiver, task),
        |(mut receiver, task)| async move {
            let message = receiver.recv().await?;
            Some((message, (receiver, task)))
        },
    ))
}

/// Signals read by [`drained`].
pub type Signals = std::pin::Pin<Box<dyn Stream<Item = zbus::Result<zbus::Message>> + Send>>;

/// A task that stops when this is dropped.
struct Aborting(tokio::task::JoinHandle<()>);

impl Drop for Aborting {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What a snapshot function returns: the state, and the objects whose changes make it stale.
pub struct Snapshot<S> {
    pub state: S,
    pub watch: Vec<OwnedObjectPath>,
}

/// A snapshot of `service`, then a new one after every property change on the objects the last
/// snapshot listed, and whenever the service starts, stops or restarts. Unchanged snapshots
/// aren't yielded, so a chatty service doesn't cause redraws. Errors are logged and retried on the
/// service's next signal. If the service isn't installed at all, the stream ends. If the bus
/// connection breaks, it's opened again and the watch goes on.
pub fn watch<S, F, Fut>(service: &'static str, snapshot: F) -> impl Stream<Item = S>
where
    S: PartialEq + Clone,
    F: Fn(Connection) -> Fut,
    Fut: Future<Output = zbus::Result<Snapshot<S>>>,
{
    watching(service, snapshot, false)
}

/// Like [`watch`], but any signal from `service` makes the snapshot stale: objects coming and
/// going (`InterfacesAdded`, `InterfacesRemoved`) as well as every property change. For services
/// whose objects change often, like Wi-Fi networks, where listing them all would be the snapshot.
pub fn watch_all<S, F, Fut>(service: &'static str, snapshot: F) -> impl Stream<Item = S>
where
    S: PartialEq + Clone,
    F: Fn(Connection) -> Fut,
    Fut: Future<Output = zbus::Result<Snapshot<S>>>,
{
    watching(service, snapshot, true)
}

/// What a watch's signal reader tells the watch.
struct Shared {
    /// Every signal counts, not only those on watched objects.
    everything: bool,
    stale: AtomicBool,
    /// The connection broke: the reader has stopped.
    closed: AtomicBool,
    watched: Lock<Vec<OwnedObjectPath>>,
    changed: Notify,
}

impl Shared {
    fn new(everything: bool) -> Self {
        Self {
            everything,
            stale: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            watched: Lock::new(Vec::new()),
            changed: Notify::new(),
        }
    }

    fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    fn offer(&self, message: zbus::Result<zbus::Message>) {
        let relevant = self.everything || {
            let watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
            is_relevant(message, &watched)
        };
        if relevant {
            self.mark_stale();
        }
    }

    fn take_stale(&self) -> bool {
        self.stale.swap(false, Ordering::AcqRel)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_one();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// A watch's connection, and the task reading its signals.
struct Watcher {
    connection: Connection,
    shared: Arc<Shared>,
    _reader: Aborting,
}

fn watching<S, F, Fut>(
    service: &'static str,
    snapshot: F,
    everything: bool,
) -> impl Stream<Item = S>
where
    S: PartialEq + Clone,
    F: Fn(Connection) -> Fut,
    Fut: Future<Output = zbus::Result<Snapshot<S>>>,
{
    struct State<S, F> {
        snapshot: F,
        watcher: Option<Watcher>,
        /// A watcher started once, so a failure now is the bus coming back, not missing.
        started: bool,
        retry: Duration,
        last: Option<S>,
    }

    let state = State {
        snapshot,
        watcher: None,
        started: false,
        retry: RETRY,
        last: None,
    };
    stream::unfold(state, move |mut state| async move {
        loop {
            if state.watcher.is_none() {
                match subscribe(service, everything).await {
                    Ok(watcher) => {
                        state.watcher = Some(watcher);
                        state.started = true;
                    }
                    Err(error) if !state.started => {
                        // No bus at all: nothing will ever change, so end quietly.
                        eprintln!("eave-dbus: {service}: {error}");
                        return None;
                    }
                    Err(error) => {
                        eprintln!("eave-dbus: {service}: {error}");
                        state.retry = backoff(state.retry).await;
                        continue;
                    }
                }
            }
            let (connection, shared) = state
                .watcher
                .as_ref()
                .map(|watcher| (watcher.connection.clone(), watcher.shared.clone()))?;

            if shared.take_stale() {
                // Subscribed first, so a change during the snapshot still marks it stale.
                match tokio::time::timeout(SNAPSHOT_TIMEOUT, (state.snapshot)(connection.clone()))
                    .await
                {
                    Ok(Ok(snapshot)) => {
                        state.retry = RETRY;
                        *shared.watched.lock().unwrap_or_else(|p| p.into_inner()) =
                            snapshot.watch;
                        if state.last.as_ref() != Some(&snapshot.state) {
                            state.last = Some(snapshot.state.clone());
                            return Some((snapshot.state, state));
                        }
                    }
                    Ok(Err(error)) if state.last.is_none() && is_not_installed(&error) => {
                        // Nothing provides it and nothing can start it: end, so the widget
                        // keeps its "absent" state instead of waiting forever.
                        eprintln!("eave-dbus: {service} isn't installed");
                        return None;
                    }
                    Ok(Err(error)) => eprintln!("eave-dbus: {service}: {error}"),
                    Err(_) => {
                        // Not even an error: the connection has stopped answering. Start over
                        // on a new one.
                        eprintln!("eave-dbus: {service}: no answer, reconnecting");
                        forget(&connection).await;
                        state.watcher = None;
                        state.retry = backoff(state.retry).await;
                        continue;
                    }
                }
            }

            shared.changed.notified().await;
            if shared.is_closed() {
                eprintln!("eave-dbus: {service}: lost the bus, reconnecting");
                forget(&connection).await;
                state.watcher = None;
                state.retry = backoff(state.retry).await;
                continue;
            }
            // A change usually comes as a burst of signals, and some services update several
            // objects one after another: let them settle before reading again.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
}

/// The first wait before trying a broken connection again.
const RETRY: Duration = Duration::from_secs(1);

/// Waits `retry`, and returns the next wait: twice as long, up to 30 s.
async fn backoff(retry: Duration) -> Duration {
    tokio::time::sleep(retry).await;
    (retry * 2).min(Duration::from_secs(30))
}

/// Starts reading `service`'s signals, and the bus's word on who owns its name, on a task of
/// their own (see [`drained`]).
async fn subscribe(service: &'static str, everything: bool) -> zbus::Result<Watcher> {
    let connection = system().await?;
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, service)?
        .build();
    let mut owners = MessageStream::for_match_rule(rule, &connection, Some(16)).await?;
    let mut signals = signals(&connection, service, everything).await?;
    let shared = Arc::new(Shared::new(everything));

    let reader = {
        let connection = connection.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            loop {
                let event = {
                    let owner = std::pin::pin!(owners.next());
                    let message = std::pin::pin!(next(&mut signals));
                    match future::select(owner, message).await {
                        Either::Left((owner, _)) => Event::Owner(owner.is_some()),
                        Either::Right((message, _)) => Event::Signal(message),
                    }
                };
                match event {
                    // The connection is gone.
                    Event::Owner(false) => break,
                    Event::Owner(true) => {
                        // Started, stopped or restarted: its signals now come from another
                        // sender, or none.
                        drop(signals.take());
                        signals = match self::signals(&connection, service, everything).await {
                            Ok(signals) => signals,
                            Err(error) => {
                                eprintln!("eave-dbus: {service}: {error}");
                                None
                            }
                        };
                        shared.mark_stale();
                    }
                    Event::Signal(Some(message)) => shared.offer(message),
                    Event::Signal(None) => signals = None,
                }
            }
            shared.close();
        })
    };
    Ok(Watcher {
        connection,
        shared,
        _reader: Aborting(reader),
    })
}

enum Event {
    /// The service's name changed hands; `false` when the bus connection ended instead.
    Owner(bool),
    Signal(Option<zbus::Result<zbus::Message>>),
}

/// The next of `signals`; never, when there are none.
async fn next(signals: &mut Option<MessageStream>) -> Option<zbus::Result<zbus::Message>> {
    match signals {
        Some(signals) => signals.next().await,
        None => std::future::pending().await,
    }
}

/// `service`'s signals, from whoever owns its name now, or `None` while nobody does. zbus can
/// only tell a signal's sender by its unique name, so a rule naming the well-known one would
/// bring every signal on the connection.
async fn signals(
    connection: &Connection,
    service: &'static str,
    everything: bool,
) -> zbus::Result<Option<MessageStream>> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let owner = match bus
        .get_name_owner(zbus::names::BusName::try_from(service)?)
        .await
    {
        Ok(owner) => owner,
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(owner.into_inner())?;
    let rule = if everything {
        rule.build()
    } else {
        rule.interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .build()
    };
    let signals = MessageStream::for_match_rule(rule, connection, Some(64)).await?;
    Ok(Some(signals))
}

/// The bus doesn't know the service and can't activate it. A service that is installed but not
/// running yet (`NameHasNoOwner`) isn't this: its signals arrive once it starts.
fn is_not_installed(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::FDO(error) => matches!(**error, zbus::fdo::Error::ServiceUnknown(_)),
        zbus::Error::MethodError(name, ..) => {
            name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
        }
        _ => false,
    }
}

/// The service is there, but we may not use it: its bus policy turns us away.
pub fn is_access_denied(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::FDO(error) => matches!(**error, zbus::fdo::Error::AccessDenied(_)),
        zbus::Error::MethodError(name, ..) => {
            name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"
        }
        _ => false,
    }
}

/// Nothing answers for the service right now: not installed, or not running.
pub fn is_unavailable(error: &zbus::Error) -> bool {
    let name = match error {
        zbus::Error::FDO(error) => {
            return matches!(
                **error,
                zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
            );
        }
        zbus::Error::MethodError(name, ..) => name.as_str(),
        _ => return false,
    };
    matches!(
        name,
        "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner"
    )
}

/// Every object `service` manages under `path`, with each interface's properties, in one call.
pub async fn managed_objects(
    connection: &Connection,
    service: &'static str,
    path: &'static str,
) -> zbus::Result<zbus::fdo::ManagedObjects> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination(service)?
        .path(path)?
        .build()
        .await?;
    Ok(proxy.get_managed_objects().await?)
}

fn is_relevant(message: zbus::Result<zbus::Message>, watched: &[OwnedObjectPath]) -> bool {
    let Ok(message) = message else {
        return true;
    };
    let header = message.header();
    header
        .path()
        .is_none_or(|path| watched.iter().any(|w| w.as_str() == path.as_str()))
}

/// Every property of `interface` on one object, read in one call.
pub async fn properties(
    connection: &Connection,
    service: &'static str,
    path: &str,
    interface: &'static str,
) -> zbus::Result<Properties> {
    let proxy = zbus::fdo::PropertiesProxy::builder(connection)
        .destination(service)?
        .path(ObjectPath::try_from(path)?)?
        .build()
        .await?;
    let interface = zbus::names::InterfaceName::try_from(interface)?;
    Ok(Properties(proxy.get_all(interface).await?))
}

/// Properties by name, read leniently: a missing or mistyped property reads as its default.
#[derive(Debug, Default)]
pub struct Properties(HashMap<String, OwnedValue>);

impl Properties {
    pub fn get<T>(&self, name: &str) -> T
    where
        T: TryFrom<OwnedValue> + Default,
    {
        self.0
            .get(name)
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| T::try_from(value).ok())
            .unwrap_or_default()
    }

    /// An object path property, `None` when unset (D-Bus uses `/` for "none").
    pub fn path(&self, name: &str) -> Option<OwnedObjectPath> {
        let path: OwnedObjectPath = self
            .0
            .get(name)
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| OwnedObjectPath::try_from(value).ok())?;
        (path.as_str() != "/").then_some(path)
    }

    pub fn value(&self, name: &str) -> Option<&OwnedValue> {
        self.0.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[zbus::interface(name = "eave.Test")]
    impl Echo {
        fn ping(&self) {}
    }

    /// Two ends of one connection, with no bus in between.
    async fn pair() -> (Connection, Connection) {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let guid = zbus::Guid::generate();
        let server = zbus::connection::Builder::unix_stream(a)
            .server(guid)
            .unwrap()
            .p2p()
            .build();
        let client = zbus::connection::Builder::unix_stream(b).p2p().build();
        let (server, client) = future::join(server, client).await;
        let server = server.unwrap();
        server.object_server().at("/eave/test", Echo).await.unwrap();
        (server, client.unwrap())
    }

    fn rule() -> MatchRule<'static> {
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .path("/eave/test")
            .unwrap()
            .build()
    }

    async fn flood(server: &Connection) {
        for _ in 0..200 {
            server
                .emit_signal(None::<()>, "/eave/test", "eave.Test", "Changed", &())
                .await
                .unwrap();
        }
    }

    async fn ping(client: &Connection) -> bool {
        let ping = client.call_method(
            None::<()>,
            "/eave/test",
            Some("eave.Test"),
            "Ping",
            &(),
        );
        tokio::time::timeout(Duration::from_secs(2), ping)
            .await
            .is_ok_and(|reply| reply.is_ok())
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn the_other_end_answers() {
        runtime().block_on(async {
            let (_server, client) = pair().await;
            assert!(ping(&client).await);
        });
    }

    #[test]
    fn an_unread_stream_stalls_the_connection() {
        // Why `drained` exists: zbus waits for room in a full queue before reading on.
        runtime().block_on(async {
            let (server, client) = pair().await;
            let _signals = MessageStream::for_match_rule(rule(), &client, Some(64))
                .await
                .unwrap();
            flood(&server).await;
            assert!(!ping(&client).await, "a full queue held the reply back");
        });
    }

    #[test]
    fn a_drained_stream_never_does() {
        runtime().block_on(async {
            let (server, client) = pair().await;
            let mut signals = drained(
                MessageStream::for_match_rule(rule(), &client, Some(64))
                    .await
                    .unwrap(),
            );
            flood(&server).await;
            assert!(ping(&client).await, "calls are answered while nobody reads");
            let all = (&mut signals).take(200).count();
            let count = tokio::time::timeout(Duration::from_secs(2), all).await;
            assert_eq!(count, Ok(200), "and no signal is lost");
        });
    }

    #[test]
    fn signals_on_unwatched_objects_dont_make_a_watch_stale() {
        let shared = Shared::new(false);
        assert!(shared.take_stale(), "stale until the first snapshot");
        *shared.watched.lock().unwrap() = vec![OwnedObjectPath::try_from("/a").unwrap()];
        let signal = |path: &str| {
            zbus::Message::signal(path, "eave.Test", "Changed")
                .unwrap()
                .build(&())
        };
        shared.offer(signal("/b"));
        assert!(!shared.take_stale());
        shared.offer(signal("/a"));
        assert!(shared.take_stale());
        assert!(!shared.take_stale(), "taken once");
    }
}
