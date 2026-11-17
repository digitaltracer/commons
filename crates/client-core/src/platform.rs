//! OS services behind traits: the keychain and notifications.
use std::collections::HashMap;
use std::sync::Mutex;

use proto::model::Notification;
use uuid::Uuid;

pub trait SecretStore: Send + Sync {
    fn get(&self, account_id: Uuid) -> anyhow::Result<Option<String>>;
    fn set(&self, account_id: Uuid, token: &str) -> anyhow::Result<()>;
    fn delete(&self, account_id: Uuid) -> anyhow::Result<()>;
}

/// Session tokens in the OS keychain.
pub struct KeyringSecrets {
    service: String,
}

impl KeyringSecrets {
    pub fn new(service: impl Into<String>) -> Self {
        KeyringSecrets {
            service: service.into(),
        }
    }
    fn entry(&self, id: Uuid) -> anyhow::Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, &id.to_string())?)
    }
}

impl SecretStore for KeyringSecrets {
    fn get(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        match self.entry(id)?.get_password() {
            Ok(t) => Ok(Some(t)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, id: Uuid, token: &str) -> anyhow::Result<()> {
        Ok(self.entry(id)?.set_password(token)?)
    }
    fn delete(&self, id: Uuid) -> anyhow::Result<()> {
        match self.entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<Uuid, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(&id)
            .cloned())
    }
    fn set(&self, id: Uuid, token: &str) -> anyhow::Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .insert(id, token.to_string());
        Ok(())
    }
    fn delete(&self, id: Uuid) -> anyhow::Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .remove(&id);
        Ok(())
    }
}

/// Shows an OS notification. The OS applies its own do-not-disturb / Focus setting.
pub trait Notifier: Send + Sync {
    fn show(&self, account_id: Uuid, title: &str, body: &str, notification: &Notification);
}

#[derive(Default)]
pub struct RecordingNotifier(pub Mutex<Vec<Notification>>);

impl Notifier for RecordingNotifier {
    fn show(&self, _account_id: Uuid, _title: &str, _body: &str, n: &Notification) {
        if let Ok(mut v) = self.0.lock() {
            v.push(n.clone());
        }
    }
}
