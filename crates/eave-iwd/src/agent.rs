//! Joining a network with a password. iwd asks for the password through an agent: an object the
//! caller serves on the bus, which iwd calls back while it connects. eave serves one, but never
//! makes iwd wait on the user: the password is already typed when [`connect_with`] runs, and the
//! agent hands it over once, for that network only.

use std::sync::Mutex;

use zbus::zvariant::OwnedObjectPath;

use crate::{NETWORK, Network, SERVICE};

const AGENT: &str = "/eave/iwd/agent";

/// The password for the network being joined, until iwd asks for it.
static WAITING: Mutex<Option<(OwnedObjectPath, String)>> = Mutex::new(None);

fn take(network: &OwnedObjectPath) -> Option<String> {
    let mut waiting = WAITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match waiting.take() {
        Some((path, passphrase)) if path == *network => Some(passphrase),
        other => {
            *waiting = other;
            None
        }
    }
}

fn clear() {
    *WAITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "net.connman.iwd.Agent.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Canceled(String),
}

fn canceled() -> AgentError {
    AgentError::Canceled("eave only answers passwords it was given".into())
}

struct Agent;

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl Agent {
    fn release(&self) {
        clear();
    }

    fn request_passphrase(&self, network: OwnedObjectPath) -> Result<String, AgentError> {
        take(&network).ok_or_else(canceled)
    }

    fn request_private_key_passphrase(
        &self,
        _network: OwnedObjectPath,
    ) -> Result<String, AgentError> {
        Err(canceled())
    }

    fn request_user_name_and_password(
        &self,
        _network: OwnedObjectPath,
    ) -> Result<(String, String), AgentError> {
        Err(canceled())
    }

    fn request_user_password(
        &self,
        _network: OwnedObjectPath,
        _user: String,
    ) -> Result<String, AgentError> {
        Err(canceled())
    }

    fn cancel(&self, _reason: String) {
        clear();
    }
}

/// Serves eave's agent and makes it the one iwd asks for joins eave starts. Every time, so it
/// holds after iwd restarts, or after the bus connection was opened again.
pub(crate) async fn register() -> zbus::Result<()> {
    let connection = eave_dbus::system().await?;
    // Served once per connection; `at` says false when it already is.
    connection.object_server().at(AGENT, Agent).await?;
    // iwd sends the request to the agent of whoever called Connect, so another program's agent
    // doesn't get it.
    let agent = OwnedObjectPath::try_from(AGENT)?;
    match connection
        .call_method(
            Some(SERVICE),
            "/net/connman/iwd",
            Some("net.connman.iwd.AgentManager"),
            "RegisterAgent",
            &(agent,),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(zbus::Error::MethodError(name, ..)) if name.as_str().ends_with("AlreadyExists") => {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Joins `network` with `passphrase`. iwd remembers it once it works.
pub async fn connect_with(network: &Network, passphrase: String) -> zbus::Result<()> {
    register().await?;
    let connection = eave_dbus::system().await?;
    *WAITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some((network.path.clone(), passphrase));
    let result = connection
        .call_method(Some(SERVICE), &network.path, Some(NETWORK), "Connect", &())
        .await;
    clear();
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_goes_once_to_its_network_only() {
        let home = OwnedObjectPath::try_from("/net/connman/iwd/0/2/home_psk").unwrap();
        let other = OwnedObjectPath::try_from("/net/connman/iwd/0/2/other_psk").unwrap();
        *WAITING.lock().unwrap() = Some((home.clone(), "secret".into()));
        assert_eq!(take(&other), None);
        assert_eq!(take(&home).as_deref(), Some("secret"));
        assert_eq!(take(&home), None, "only once");
    }
}
