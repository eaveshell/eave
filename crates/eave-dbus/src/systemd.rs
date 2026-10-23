//! The user's service manager, systemd, on the session bus: what `systemctl --user` asks, without
//! running it. Starting and restarting units, their state, and the environment it gives them.
//! [`system`] asks the system's manager, on the system bus, what polkit lets the user ask it.

use futures_util::{StreamExt, stream};
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream};

use crate::{Signals, Stream, drained, properties, session};

pub mod system;

const SERVICE: &str = "org.freedesktop.systemd1";
const PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT: &str = "org.freedesktop.systemd1.Unit";

/// A unit as systemd has it now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unit {
    /// `loaded`, or `not-found` for a unit with no file.
    pub load: String,
    /// `active`, `activating`, `inactive`, `failed`…
    pub active: String,
    /// Its current run, in hex like `$INVOCATION_ID`; empty when it never ran.
    pub invocation: String,
}

impl Unit {
    pub fn exists(&self) -> bool {
        self.load == "loaded"
    }

    /// Starting, running or stopping: a oneshot that hasn't finished.
    pub fn busy(&self) -> bool {
        matches!(
            self.active.as_str(),
            "active" | "activating" | "deactivating" | "reloading" | "refreshing"
        )
    }

    pub fn failed(&self) -> bool {
        self.active == "failed"
    }
}

/// Whether `unit` is enabled: `enabled`, `disabled`, `static`… An error for a unit with no file.
pub async fn file_state(unit: &str) -> zbus::Result<String> {
    let reply = manager(&session().await?, "GetUnitFileState", &(unit,)).await?;
    reply.body().deserialize()
}

/// The environment systemd gives the units it starts, as `NAME=value`.
pub async fn environment() -> zbus::Result<Vec<String>> {
    let connection = session().await?;
    let manager = properties(&connection, SERVICE, PATH, MANAGER).await?;
    Ok(manager.get("Environment"))
}

/// Starts `unit`, like `systemctl --user start`. Returns once the job is queued.
pub async fn start(unit: &str) -> zbus::Result<()> {
    manager(&session().await?, "StartUnit", &(unit, "replace")).await?;
    Ok(())
}

/// Restarts `unit`, like `systemctl --user restart`.
pub async fn restart(unit: &str) -> zbus::Result<()> {
    manager(&session().await?, "RestartUnit", &(unit, "replace")).await?;
    Ok(())
}

/// `unit` as systemd has it now.
pub async fn unit(name: &str) -> zbus::Result<Unit> {
    let connection = session().await?;
    let path = path(&connection, name).await?;
    read(&connection, &path).await
}

/// `unit` now, then again whenever it changes, with no polling. Ends when there's no session bus
/// or no systemd on it.
pub fn watch(name: &'static str) -> impl Stream<Item = Unit> {
    struct State {
        watching: Option<(Connection, OwnedObjectPath, Signals)>,
        last: Option<Unit>,
    }

    let state = State {
        watching: None,
        last: None,
    };
    stream::unfold(state, move |mut state| async move {
        if state.watching.is_none() {
            match subscribe(name).await {
                Ok(watching) => state.watching = Some(watching),
                Err(error) => {
                    eprintln!("eave-dbus: systemd: {name}: {error}");
                    return None;
                }
            }
        }
        loop {
            let (connection, path, signals) = state.watching.as_mut()?;
            match read(connection, path).await {
                Ok(unit) if state.last.as_ref() != Some(&unit) => {
                    state.last = Some(unit.clone());
                    return Some((unit, state));
                }
                Ok(_) => {}
                Err(error) => eprintln!("eave-dbus: systemd: {name}: {error}"),
            }
            // Any change: read it again.
            let _ = signals.next().await?;
        }
    })
}

/// Asks systemd for its signals, which it sends only to those who ask, and listens for `name`'s
/// property changes.
async fn subscribe(name: &str) -> zbus::Result<(Connection, OwnedObjectPath, Signals)> {
    let connection = session().await?;
    let path = path(&connection, name).await?;
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(SERVICE)?
        .path(path.clone())?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    let signals = drained(MessageStream::for_match_rule(rule, &connection, Some(16)).await?);
    manager(&connection, "Subscribe", &()).await?;
    Ok((connection, path, signals))
}

/// The unit's object, loading it if systemd hasn't yet.
async fn path(connection: &Connection, name: &str) -> zbus::Result<OwnedObjectPath> {
    let reply = manager(connection, "LoadUnit", &(name,)).await?;
    reply.body().deserialize()
}

async fn read(connection: &Connection, path: &OwnedObjectPath) -> zbus::Result<Unit> {
    let unit = properties(connection, SERVICE, path.as_str(), UNIT).await?;
    let invocation: Vec<u8> = unit.get("InvocationID");
    Ok(Unit {
        load: unit.get("LoadState"),
        active: unit.get("ActiveState"),
        invocation: hex(&invocation),
    })
}

async fn manager<B>(connection: &Connection, method: &str, body: &B) -> zbus::Result<zbus::Message>
where
    B: zbus::export::serde::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(Some(SERVICE), PATH, Some(MANAGER), method, body)
        .await
}

/// `$INVOCATION_ID`'s spelling of an id: lowercase hex, no dashes.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_ids_read_like_the_environment_variable() {
        assert_eq!(hex(&[0x0a, 0xff, 0x10]), "0aff10");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn a_oneshot_is_busy_until_it_finishes() {
        let state = |active: &str| Unit {
            load: "loaded".into(),
            active: active.into(),
            invocation: String::new(),
        };
        assert!(state("activating").busy());
        assert!(!state("inactive").busy());
        assert!(!state("failed").busy());
        assert!(state("failed").failed());
        assert!(!Unit::default().exists());
    }

    #[test]
    #[ignore = "needs a systemd user manager"]
    fn reads_real_units() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let manager = unit("default.target").await.unwrap();
            assert!(manager.exists(), "{manager:?}");
            assert_eq!(manager.active, "active");
            assert_eq!(manager.invocation.len(), 32, "{manager:?}");
            let missing = unit("eave-no-such.service").await.unwrap();
            assert!(!missing.exists());
            let first = std::pin::pin!(watch("default.target"))
                .next()
                .await
                .unwrap();
            assert_eq!(first, manager);
        });
    }
}
