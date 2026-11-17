//! Hosts every signed-in account and fans their events out to the UI.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use proto::api::{LoginRequest, RegisterRequest, SessionResponse};
use proto::view::{AppEvent, SidebarView};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::{Account, AccountConfig, AccountEvent, EventSink};
use crate::platform::{Notifier, SecretStore};
use crate::store::SqliteStore;
use crate::transport::{Method, Transport, call};

pub type TransportFactory = Arc<dyn Fn(&str) -> Arc<dyn Transport> + Send + Sync>;
pub type AppSink = Arc<dyn Fn(AppEvent) + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedAccount {
    id: Uuid,
    server_url: String,
    user: proto::model::User,
}

pub struct Core {
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
    notifier: Arc<dyn Notifier>,
    transports: TransportFactory,
    accounts: RwLock<HashMap<Uuid, Account>>,
    sink: AppSink,
    runtime: Option<tokio::runtime::Handle>,
    sidebar_pending: AtomicBool,
}

/// Sidebar view models are rebuilt at most this often, however fast the cache changes.
const SIDEBAR_COALESCE: Duration = Duration::from_millis(100);

impl Core {
    /// Opens the client state in `data_dir` and starts every saved account.
    pub fn open(
        data_dir: &Path,
        secrets: Arc<dyn SecretStore>,
        notifier: Arc<dyn Notifier>,
        transports: TransportFactory,
        sink: AppSink,
    ) -> anyhow::Result<Arc<Core>> {
        std::fs::create_dir_all(data_dir)?;
        let core = Arc::new(Core {
            data_dir: data_dir.to_path_buf(),
            secrets,
            notifier,
            transports,
            accounts: RwLock::new(HashMap::new()),
            sink,
            runtime: tokio::runtime::Handle::try_current().ok(),
            sidebar_pending: AtomicBool::new(false),
        });
        for saved in core.load_saved()? {
            let token = core.secrets.get(saved.id).unwrap_or(None);
            core.start_account(saved, token)?;
        }
        Ok(core)
    }

    fn saved_path(&self) -> PathBuf {
        self.data_dir.join("accounts.json")
    }

    fn load_saved(&self) -> anyhow::Result<Vec<SavedAccount>> {
        match std::fs::read(self.saved_path()) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        let accounts = self
            .accounts
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?;
        let saved: Vec<SavedAccount> = accounts
            .values()
            .map(|a| SavedAccount {
                id: a.id,
                server_url: a.server_url.clone(),
                user: a.user.clone(),
            })
            .collect();
        let tmp = self.saved_path().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
        std::fs::rename(tmp, self.saved_path())?;
        Ok(())
    }

    fn events(self: &Arc<Self>) -> EventSink {
        let weak = Arc::downgrade(self);
        Arc::new(move |account_id, ev| {
            if let Some(core) = weak.upgrade() {
                core.on_account_event(account_id, ev);
            }
        })
    }

    fn emit_sidebar(&self) {
        if let Ok(sidebar) = self.sidebar() {
            (self.sink)(AppEvent::Sidebar { sidebar });
        }
    }

    fn on_account_event(self: &Arc<Self>, account_id: Uuid, ev: AccountEvent) {
        match ev {
            AccountEvent::Sidebar => match &self.runtime {
                Some(rt) => {
                    if !self.sidebar_pending.swap(true, Ordering::SeqCst) {
                        let core = self.clone();
                        rt.spawn(async move {
                            tokio::time::sleep(SIDEBAR_COALESCE).await;
                            core.sidebar_pending.store(false, Ordering::SeqCst);
                            core.emit_sidebar();
                        });
                    }
                }
                None => self.emit_sidebar(),
            },
            AccountEvent::Drafts => {
                let drafts = self
                    .accounts
                    .read()
                    .map(|a| {
                        a.values()
                            .flat_map(|acc| acc.drafts().unwrap_or_default())
                            .collect()
                    })
                    .unwrap_or_default();
                (self.sink)(AppEvent::Drafts { drafts });
            }
            AccountEvent::Typing {
                conversation_id,
                names,
            } => (self.sink)(AppEvent::Typing {
                account_id,
                conversation_id,
                names,
            }),
            AccountEvent::Presence { online } => {
                (self.sink)(AppEvent::Presence { account_id, online })
            }
            AccountEvent::Notification(_) => {}
        }
    }

    fn start_account(
        self: &Arc<Self>,
        saved: SavedAccount,
        token: Option<String>,
    ) -> anyhow::Result<Account> {
        let dir = self.data_dir.join("accounts").join(saved.id.to_string());
        let store = Arc::new(SqliteStore::open(&dir.join("cache.sqlite"))?);
        let account = Account::new(
            AccountConfig {
                id: saved.id,
                server_url: saved.server_url.clone(),
                user: saved.user,
                data_dir: dir,
            },
            (self.transports)(&saved.server_url),
            store,
            token,
            self.notifier.clone(),
            self.events(),
        );
        account.start();
        self.accounts
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .insert(account.id, account.clone());
        Ok(account)
    }

    /// Signs in (or registers) on a server and starts a new account for it.
    pub async fn add_account(
        self: &Arc<Self>,
        server_url: &str,
        email: &str,
        password: &str,
        register: Option<&str>,
    ) -> anyhow::Result<Uuid> {
        let server_url = server_url.trim().trim_end_matches('/').to_string();
        let transport = (self.transports)(&server_url);
        let session: SessionResponse = match register {
            Some(display_name) => {
                call(
                    &*transport,
                    Method::Post,
                    "/auth/register",
                    &[],
                    Some(&RegisterRequest {
                        email: email.into(),
                        password: password.into(),
                        display_name: display_name.into(),
                    }),
                    None,
                    None,
                )
                .await?
            }
            None => {
                call(
                    &*transport,
                    Method::Post,
                    "/auth/login",
                    &[],
                    Some(&LoginRequest {
                        email: email.into(),
                        password: password.into(),
                    }),
                    None,
                    None,
                )
                .await?
            }
        };
        let existing = self
            .accounts
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .values()
            .find(|a| a.server_url == server_url && a.user.id == session.user.id)
            .map(|a| a.id);
        if let Some(id) = existing {
            self.remove_account(id)?;
        }
        let id = Uuid::now_v7();
        self.secrets.set(id, &session.token)?;
        self.start_account(
            SavedAccount {
                id,
                server_url,
                user: session.user,
            },
            Some(session.token),
        )?;
        self.save()?;
        self.on_account_event(id, AccountEvent::Sidebar);
        Ok(id)
    }

    /// Hosts an account built elsewhere (the M0 spike's local accounts). Not saved.
    pub fn attach(&self, account: Account) -> anyhow::Result<()> {
        self.accounts
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .insert(account.id, account);
        Ok(())
    }

    /// The event sink to give an account built outside [`Core::add_account`].
    pub fn account_events(self: &Arc<Self>) -> EventSink {
        self.events()
    }

    pub fn remove_account(self: &Arc<Self>, id: Uuid) -> anyhow::Result<()> {
        let removed = self
            .accounts
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .remove(&id);
        if let Some(a) = removed {
            a.stop();
            self.secrets.delete(id)?;
            let _ = std::fs::remove_dir_all(self.data_dir.join("accounts").join(id.to_string()));
        }
        self.save()?;
        self.on_account_event(id, AccountEvent::Sidebar);
        Ok(())
    }

    pub fn account(&self, id: Uuid) -> anyhow::Result<Account> {
        self.accounts
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown account"))
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.accounts
            .read()
            .map(|a| a.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn sidebar(&self) -> anyhow::Result<SidebarView> {
        let mut accounts: Vec<_> = self
            .accounts()
            .iter()
            .map(|a| a.view())
            .collect::<anyhow::Result<_>>()?;
        accounts.sort_by(|a, b| a.server_url.cmp(&b.server_url).then(a.email.cmp(&b.email)));
        Ok(SidebarView { accounts })
    }
}
