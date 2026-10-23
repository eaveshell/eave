//! The system's service manager, on the system bus: units polkit lets the user start, such as
//! eave's own boot helpers ([D-131](https://github.com/eaveshell/eave/blob/main/docs/decisions.md)).

use futures_util::StreamExt;
use zbus::MatchRule;
use zbus::MessageStream;
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedObjectPath;

use super::{MANAGER, SERVICE, Unit, manager, path, read};
use crate::system as bus;

/// `unit` as the system's systemd has it now. A unit with no file is there, `not-found`.
pub async fn unit(name: &str) -> zbus::Result<Unit> {
    let connection = bus().await?;
    let path = path(&connection, name).await?;
    read(&connection, &path).await
}

/// Starts `unit`, like `systemctl start`, and returns once its job is done: `Ok` when it
/// succeeded, an error naming systemd's result (`failed`, `canceled`…) when it didn't.
pub async fn start(unit: &str) -> zbus::Result<()> {
    let connection = bus().await?;
    // Listen before starting, so a quick job's end isn't missed.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(SERVICE)?
        .interface(MANAGER)?
        .member("JobRemoved")?
        .build();
    // Every job's end comes, and the call below can wait on a password: read them as they come.
    let mut signals =
        crate::drained(MessageStream::for_match_rule(rule, &connection, Some(16)).await?);
    // The shared connection may have subscribed already, which systemd answers with an error.
    if let Err(error) = manager(&connection, "Subscribe", &()).await
        && !already_subscribed(&error)
    {
        return Err(error);
    }
    let reply = manager(&connection, "StartUnit", &(unit, "replace")).await?;
    let job: OwnedObjectPath = reply.body().deserialize()?;
    while let Some(message) = signals.next().await {
        let Ok((_, path, _, result)) = message?
            .body()
            .deserialize::<(u32, OwnedObjectPath, String, String)>()
        else {
            continue;
        };
        if path == job {
            return match result.as_str() {
                "done" => Ok(()),
                _ => Err(zbus::Error::Failure(format!("{unit}: {result}"))),
            };
        }
    }
    Err(zbus::Error::Failure(format!("{unit}: lost the system bus")))
}

fn already_subscribed(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, ..)
        if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs the system's systemd"]
    fn reads_real_system_units() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(unit("basic.target").await.unwrap().exists());
            assert!(!unit("eave-no-such.service").await.unwrap().exists());
        });
    }
}
