//! Hosts every signed-in account and fans their events out to the UI.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use proto::api::{
    AcceptInvite, IdentityList, InvitePreview, InvitePreviewRequest, LoginRequest, Meta,
    OidcExchanged, RegisterRequest, SessionResponse,
};
use proto::view::{AppEvent, OpenedInvite, SidebarView, UiPrefs};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::{Account, AccountConfig, AccountEvent, EventSink};
use crate::platform::{Notifier, SecretStore};
use crate::store::SqliteStore;
use crate::transport::{Method, Transport, TransportError, call};

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
    oidc_cancel: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
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
            oidc_cancel: Default::default(),
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

    fn ui_prefs_path(&self) -> PathBuf {
        self.data_dir.join("ui_prefs.json")
    }

    /// Pane sizes and other layout choices; defaults when none are saved or the file is unreadable.
    pub fn ui_prefs(&self) -> UiPrefs {
        std::fs::read(self.ui_prefs_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<UiPrefs>(&b).ok())
            .map(UiPrefs::clamped)
            .unwrap_or_default()
    }

    pub fn set_ui_prefs(&self, prefs: UiPrefs) -> anyhow::Result<UiPrefs> {
        let prefs = prefs.clamped();
        let tmp = self.ui_prefs_path().with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&prefs)?)?;
        std::fs::rename(tmp, self.ui_prefs_path())?;
        Ok(prefs)
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
            AccountEvent::Staged {
                conversation_id,
                thread_root_id,
                files,
            } => (self.sink)(AppEvent::Staged {
                account_id,
                conversation_id,
                thread_root_id,
                files,
            }),
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
        account.clear_staging_dir();
        account.start();
        self.accounts
            .write()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .insert(account.id, account.clone());
        Ok(account)
    }

    /// Signs in (or registers) on a server and starts a new account for it. With an invite,
    /// a new account joins its workspace as it registers, and an existing one accepts it.
    pub async fn add_account(
        self: &Arc<Self>,
        server_url: &str,
        email: &str,
        password: &str,
        register: Option<&str>,
        invite: Option<&str>,
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
                        invite: invite.map(str::to_string),
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
        if let (None, Some(token)) = (register, invite) {
            let accepted = call::<proto::model::Workspace>(
                &*transport,
                Method::Post,
                "/invites/accept",
                &[],
                Some(&AcceptInvite {
                    token: token.into(),
                }),
                Some(&session.token),
                None,
            )
            .await;
            if let Err(e) = accepted {
                let _ = call::<serde_json::Value>(
                    &*transport,
                    Method::Post,
                    "/auth/logout",
                    &[],
                    None::<&()>,
                    Some(&session.token),
                    None,
                )
                .await;
                return Err(e.into());
            }
        }
        self.adopt(server_url, session).await
    }

    /// Starts a new account for a fresh session, replacing this device's older account for
    /// the same user on the same server.
    async fn adopt(
        self: &Arc<Self>,
        server_url: String,
        session: SessionResponse,
    ) -> anyhow::Result<Uuid> {
        let existing = self
            .accounts
            .read()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .values()
            .find(|a| a.server_url == server_url && a.user.id == session.user.id)
            .map(|a| a.id);
        if let Some(id) = existing {
            self.remove_account(id).await?;
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

    /// Signs an account whose session ended back in, keeping its cache and outbox. Errors are
    /// worded for the person signing in.
    pub async fn sign_in_again(&self, account_id: Uuid, password: &str) -> anyhow::Result<()> {
        let account = self.account(account_id)?;
        let transport = (self.transports)(&account.server_url);
        let host = account
            .server_url
            .split("://")
            .last()
            .unwrap_or(&account.server_url)
            .to_string();
        let session: SessionResponse = call(
            &*transport,
            Method::Post,
            "/auth/login",
            &[],
            Some(&LoginRequest {
                email: account.user.email.clone(),
                password: password.into(),
            }),
            None,
            None,
        )
        .await
        .map_err(|e| match e.status() {
            Some(401) => anyhow::anyhow!(
                "That password didn't work for {}. Check it and try again.",
                account.user.email
            ),
            Some(429) => anyhow::anyhow!("Too many attempts. Wait a minute, then try again."),
            None => {
                anyhow::anyhow!("Couldn't reach {host}. Check your connection, then try again.")
            }
            Some(_) => e.into(),
        })?;
        if session.user.id != account.user.id {
            anyhow::bail!("that password signed in a different user");
        }
        self.secrets.set(account_id, &session.token)?;
        account.resume_session(session.token);
        Ok(())
    }

    /// What a server offers before signing in, such as its OIDC provider.
    pub async fn server_meta(&self, server_url: &str) -> anyhow::Result<Meta> {
        let transport = (self.transports)(server_url.trim().trim_end_matches('/'));
        Ok(call(
            &*transport,
            Method::Get,
            "/meta",
            &[],
            None::<&()>,
            None,
            None,
        )
        .await?)
    }

    /// What an invite is for, read from its server before anyone signs in. Errors are worded
    /// for the person.
    pub async fn invite_preview(
        &self,
        server_url: &str,
        token: &str,
    ) -> anyhow::Result<InvitePreview> {
        let transport = (self.transports)(server_url);
        call(
            &*transport,
            Method::Post,
            "/invites/preview",
            &[],
            Some(&InvitePreviewRequest {
                token: token.into(),
            }),
            None,
            None,
        )
        .await
        .map_err(|e| match &e {
            TransportError::Status { body, .. } if body["message"].is_string() => {
                anyhow::anyhow!("{}", body["message"].as_str().unwrap_or_default())
            }
            TransportError::Network(_) => anyhow::anyhow!(
                "Couldn't reach {}. Check your connection, then open the link again.",
                server_url.split("://").last().unwrap_or(server_url)
            ),
            TransportError::Status { .. } => {
                anyhow::anyhow!("The server couldn't check this invite. Try again later.")
            }
        })
    }

    /// This device's accounts on the server at `server_url`, by origin.
    pub fn accounts_on(&self, server_url: &str) -> Vec<Uuid> {
        let mut found: Vec<_> = self
            .accounts()
            .into_iter()
            .filter(|a| crate::deeplink::same_origin(&a.server_url, server_url))
            .map(|a| a.id)
            .collect();
        found.sort();
        found
    }

    /// Checks a `commons://` link and previews its invite. Nothing else is sent anywhere,
    /// and no account is touched: the result only fills in a form.
    pub async fn open_invite_link(&self, raw: &str) -> anyhow::Result<OpenedInvite> {
        let link = crate::deeplink::parse(raw)?;
        let preview = self.invite_preview(&link.server_url, &link.token).await?;
        Ok(OpenedInvite {
            accounts: self.accounts_on(&link.server_url),
            server_url: link.server_url,
            host: link.host,
            token: link.token,
            preview,
        })
    }

    /// A fresh cancel signal for the one provider flow that may run at a time; starting a
    /// flow cancels any earlier one still waiting for the browser.
    fn oidc_signal(&self) -> Arc<tokio::sync::Notify> {
        let n = Arc::new(tokio::sync::Notify::new());
        if let Ok(mut slot) = self.oidc_cancel.lock()
            && let Some(old) = slot.replace(n.clone())
        {
            old.notify_one();
        }
        n
    }

    /// Stops a provider flow that is waiting for the browser.
    pub fn cancel_oidc(&self) {
        if let Ok(mut slot) = self.oidc_cancel.lock()
            && let Some(n) = slot.take()
        {
            n.notify_one();
        }
    }

    async fn run_oidc(
        &self,
        server_url: &str,
        token: Option<&str>,
        invite: Option<String>,
        link: bool,
        open: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<OidcExchanged> {
        let transport = (self.transports)(server_url);
        let cancel = self.oidc_signal();
        let r = crate::oidc::run(
            &*transport,
            token,
            invite,
            link,
            open,
            &cancel,
            crate::oidc::TIMEOUT,
        )
        .await;
        if let Ok(mut slot) = self.oidc_cancel.lock()
            && slot.as_ref().is_some_and(|n| Arc::ptr_eq(n, &cancel))
        {
            slot.take();
        }
        r
    }

    /// Signs in (or creates an account) through the server's OIDC provider in the system
    /// browser, which `open` shows. An invite joins its workspace. Nothing is stored unless
    /// the whole flow succeeds.
    pub async fn add_account_oidc(
        self: &Arc<Self>,
        server_url: &str,
        invite: Option<&str>,
        open: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<Uuid> {
        let server_url = server_url.trim().trim_end_matches('/').to_string();
        let invite = invite.filter(|i| !i.trim().is_empty()).map(str::to_string);
        match self
            .run_oidc(&server_url, None, invite, false, open)
            .await?
        {
            OidcExchanged::Session { session } => self.adopt(server_url, session).await,
            OidcExchanged::Linked { .. } => {
                anyhow::bail!("the server linked instead of signing in")
            }
        }
    }

    /// Signs an account whose session ended back in through the provider, keeping its cache
    /// and outbox.
    pub async fn sign_in_again_oidc(
        &self,
        account_id: Uuid,
        open: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let account = self.account(account_id)?;
        let session = match self
            .run_oidc(&account.server_url, None, None, false, open)
            .await?
        {
            OidcExchanged::Session { session } => session,
            OidcExchanged::Linked { .. } => {
                anyhow::bail!("the server linked instead of signing in")
            }
        };
        if session.user.id != account.user.id {
            // Not this account: end the session it made rather than keep it.
            let transport = (self.transports)(&account.server_url);
            let _ = call::<serde_json::Value>(
                &*transport,
                Method::Post,
                "/auth/logout",
                &[],
                None::<&()>,
                Some(&session.token),
                None,
            )
            .await;
            anyhow::bail!(
                "That sign-in belongs to {}, not {}.",
                session.user.email,
                account.user.email
            );
        }
        self.secrets.set(account_id, &session.token)?;
        account.resume_session(session.token);
        Ok(())
    }

    /// Links the server's OIDC provider to a signed-in account, through the browser.
    pub async fn link_oidc(
        &self,
        account_id: Uuid,
        open: impl FnOnce(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<IdentityList> {
        let account = self.account(account_id)?;
        let token = self
            .secrets
            .get(account_id)?
            .ok_or_else(|| anyhow::anyhow!("This account is signed out. Sign in first."))?;
        match self
            .run_oidc(&account.server_url, Some(&token), None, true, open)
            .await?
        {
            OidcExchanged::Linked { identities } => Ok(identities),
            OidcExchanged::Session { .. } => {
                anyhow::bail!("the server signed in instead of linking")
            }
        }
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

    /// Signs the account out on its server, then removes it from this device. Removal goes
    /// ahead even when logout fails; the result is false then, because the server-side
    /// session may still be active.
    pub async fn remove_account(self: &Arc<Self>, id: Uuid) -> anyhow::Result<bool> {
        let server_url = self.account(id).ok().map(|a| a.server_url.clone());
        let token = self.secrets.get(id).map_err(|e| {
            tracing::warn!(account = %id, error = %e, "keychain read failed; cannot log out");
        });
        let revoked = match (server_url, token) {
            // The token could not be read, so its session could not be ended.
            (_, Err(())) => false,
            (Some(url), Ok(Some(token))) => {
                let transport = (self.transports)(&url);
                let logout = call::<serde_json::Value>(
                    &*transport,
                    Method::Post,
                    "/auth/logout",
                    &[],
                    None::<&()>,
                    Some(&token),
                    None,
                );
                match tokio::time::timeout(Duration::from_secs(10), logout).await {
                    Ok(Ok(_)) => true,
                    Ok(Err(e)) => {
                        tracing::warn!(account = %id, error = %e, "logout failed");
                        false
                    }
                    Err(_) => {
                        tracing::warn!(account = %id, "logout timed out");
                        false
                    }
                }
            }
            // No token: there is no session to end.
            _ => true,
        };
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
        Ok(revoked)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{MemorySecrets, RecordingNotifier};
    use crate::transport::HttpTransport;

    fn open(dir: &Path) -> Arc<Core> {
        Core::open(
            dir,
            Arc::new(MemorySecrets::default()),
            Arc::new(RecordingNotifier::default()),
            Arc::new(|u: &str| Arc::new(HttpTransport::new(u)) as Arc<dyn Transport>),
            Arc::new(|_| {}),
        )
        .unwrap()
    }

    #[test]
    fn ui_prefs_are_clamped_and_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("ui-prefs-{}", Uuid::now_v7()));
        {
            let core = open(&dir);
            assert_eq!(core.ui_prefs(), UiPrefs::default());
            let saved = core
                .set_ui_prefs(UiPrefs {
                    sidebar_width: Some(300),
                    thread_width: Some(5000),
                })
                .unwrap();
            assert_eq!(saved.thread_width, Some(560));
        }
        let core = open(&dir);
        assert_eq!(core.ui_prefs().sidebar(), 300);
        assert_eq!(core.ui_prefs().thread(), 560);
        std::fs::write(dir.join("ui_prefs.json"), b"not json").unwrap();
        assert_eq!(core.ui_prefs(), UiPrefs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
