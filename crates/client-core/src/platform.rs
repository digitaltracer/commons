//! OS services behind traits: the keychain (or a file, for dev builds) and notifications.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// Session tokens in a JSON file readable only by the user (mode 0600), replaced atomically on
/// every write. For dev builds: their ad-hoc signature changes on every build, so the
/// keychain's "Always Allow" never sticks and every launch asks for a password.
pub struct FileSecrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileSecrets {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> anyhow::Result<HashMap<Uuid, String>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, tokens: &HashMap<Uuid, String>) -> anyhow::Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec(tokens)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut HashMap<Uuid, String>)) -> anyhow::Result<()> {
        let _held = self.lock.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        let mut tokens = self.read()?;
        f(&mut tokens);
        self.write(&tokens)
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, id: Uuid) -> anyhow::Result<Option<String>> {
        let _held = self.lock.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        Ok(self.read()?.remove(&id))
    }
    fn set(&self, id: Uuid, token: &str) -> anyhow::Result<()> {
        self.update(|t| {
            t.insert(id, token.to_string());
        })
    }
    fn delete(&self, id: Uuid) -> anyhow::Result<()> {
        self.update(|t| {
            t.remove(&id);
        })
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

#[cfg(test)]
mod tests {
    use super::{FileSecrets, SecretStore};
    use uuid::Uuid;

    #[test]
    fn file_secrets_round_trip_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let s = FileSecrets::new(&path);
        assert_eq!(s.get(a).unwrap(), None, "no file yet");
        s.set(a, "token-a").unwrap();
        s.set(b, "token-b").unwrap();
        s.set(a, "token-a2").unwrap();
        let again = FileSecrets::new(&path);
        assert_eq!(again.get(a).unwrap().as_deref(), Some("token-a2"));
        assert_eq!(again.get(b).unwrap().as_deref(), Some("token-b"));
        again.delete(a).unwrap();
        again.delete(a).unwrap();
        assert_eq!(s.get(a).unwrap(), None);
        assert_eq!(s.get(b).unwrap().as_deref(), Some("token-b"));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_secrets_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("secrets.json");
        FileSecrets::new(&path).set(Uuid::now_v7(), "t").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
