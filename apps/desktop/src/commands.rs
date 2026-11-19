//! Tauri commands: the UI's only way to reach the core.
use client_core::transport::Method;
use proto::model::{Conversation, ConversationKind, Entity, Member, Preferences};
use proto::view::{AppEvent, SidebarView, StagedFile};
use proto::window::{WindowBatch, WindowSpec};
use tauri::State;
use tauri::ipc::Channel;
use uuid::Uuid;

use crate::Host;

type R<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub fn handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        subscribe_app,
        add_account,
        remove_account,
        create_invite,
        accept_invite,
        subscribe_window,
        ack_window,
        reset_window,
        move_window,
        unsubscribe_window,
        send_message,
        staged_files,
        pick_files,
        stage_pasted,
        stage_dropped,
        retry_staged,
        remove_staged,
        edit_message,
        delete_message,
        toggle_reaction,
        mark_read,
        retry_send,
        discard_send,
        set_focus,
        set_window_active,
        typing,
        create_workspace,
        create_channel,
        browse_channels,
        join_channel,
        leave_channel,
        workspace_members,
        open_dm,
        search,
        conversation_members,
        add_member,
        remove_member,
        online,
        preferences,
        set_preferences,
        set_muted,
        recent_emoji,
        composer_draft,
        save_composer_draft,
        ui_prefs,
        set_ui_prefs,
        reconnect_now,
        sign_in_again,
        server_meta,
        take_invite_link,
        oidc_sign_in,
        oidc_sign_in_again,
        oidc_link,
        oidc_cancel,
        identities,
        unlink_identity,
        admin_members,
        set_member_role,
        remove_workspace_member,
        list_invites,
        revoke_invite,
        save_file,
        ui_log,
        crate::spike::spike_config,
        crate::spike::spike_report,
        crate::updater::update_ready,
        crate::updater::restart_to_update,
    ]
}

#[tauri::command]
fn subscribe_app(host: State<'_, Host>, on_event: Channel<AppEvent>) -> R<SidebarView> {
    host.app_channels.lock().map_err(err)?.push(on_event);
    host.core.sidebar().map_err(err)
}

#[tauri::command]
async fn add_account(
    host: State<'_, Host>,
    server_url: String,
    email: String,
    password: String,
    display_name: Option<String>,
    invite: Option<String>,
) -> R<Uuid> {
    host.core
        .add_account(
            &server_url,
            &email,
            &password,
            display_name.as_deref(),
            invite.as_deref(),
        )
        .await
        .map_err(refused)
}

/// Creates a single-use invite link to a workspace for one email (owners and admins).
#[tauri::command]
async fn create_invite(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    email: String,
    role: proto::model::Role,
    expires_in_days: u32,
) -> R<proto::api::InviteCreated> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(
        Method::Post,
        &format!("/workspaces/{workspace_id}/invites"),
        &[],
        Some(&proto::api::CreateInvite {
            email,
            role,
            expires_in_days: Some(expires_in_days),
        }),
        None,
    )
    .await
    .map_err(refusal)
}

/// Joins the invite's workspace as this account. The workspace arrives on the user stream.
#[tauri::command]
async fn accept_invite(
    host: State<'_, Host>,
    account_id: Uuid,
    token: String,
) -> R<proto::model::Workspace> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(
        Method::Post,
        "/invites/accept",
        &[],
        Some(&proto::api::AcceptInvite { token }),
        None,
    )
    .await
    .map_err(refusal)
}

/// True when the server ended the session; false when it may still be active there.
#[tauri::command]
async fn remove_account(host: State<'_, Host>, account_id: Uuid) -> R<bool> {
    host.core.remove_account(account_id).await.map_err(err)
}

#[tauri::command]
fn subscribe_window(
    host: State<'_, Host>,
    spec: WindowSpec,
    on_batch: Channel<WindowBatch>,
) -> R<u64> {
    let account = host.core.account(spec.account_id).map_err(err)?;
    let spec = WindowSpec {
        thread_root_id: resolve_root(&account, spec.thread_root_id)?,
        ..spec
    };
    Ok(account.subscribe_window(spec, Box::new(move |b| on_batch.send(b).is_ok())))
}

#[tauri::command]
fn ack_window(
    host: State<'_, Host>,
    account_id: Uuid,
    sub_id: u64,
    generation: u64,
    revision: u64,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.windows.ack(sub_id, generation, revision, &*a.0);
    Ok(())
}

#[tauri::command]
fn reset_window(host: State<'_, Host>, account_id: Uuid, sub_id: u64) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.windows.reset(sub_id, &*a.0);
    Ok(())
}

#[tauri::command]
fn move_window(host: State<'_, Host>, account_id: Uuid, sub_id: u64, spec: WindowSpec) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let spec = WindowSpec {
        thread_root_id: resolve_root(&a, spec.thread_root_id)?,
        ..spec
    };
    a.windows.update(sub_id, spec, &*a.0);
    Ok(())
}

#[tauri::command]
fn unsubscribe_window(host: State<'_, Host>, account_id: Uuid, sub_id: u64) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .windows
        .unsubscribe(sub_id);
    Ok(())
}

/// Sends the composer's text with every file staged in it. `thread_root_id` is the composer's
/// key as the UI has it; the message goes to its resolved root.
#[tauri::command]
fn send_message(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    body: String,
    thread_root_id: Option<Uuid>,
    mentions: Option<Vec<Entity>>,
) -> R<Uuid> {
    let a = host.core.account(account_id).map_err(err)?;
    let root = resolve_root(&a, thread_root_id)?;
    a.send_with_staged(
        (conversation_id, thread_root_id),
        body,
        root,
        mentions.unwrap_or_default(),
    )
    .map_err(err)
}

#[tauri::command]
fn staged_files(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
) -> R<Vec<StagedFile>> {
    Ok(host
        .core
        .account(account_id)
        .map_err(err)?
        .staged((conversation_id, thread_root_id)))
}

/// Picks files with the native dialog and stages them in the composer. Their paths never
/// reach the webview. Returns how many were staged.
#[tauri::command]
async fn pick_files(
    app: tauri::AppHandle,
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
) -> R<usize> {
    use tauri_plugin_dialog::DialogExt;
    let a = host.core.account(account_id).map_err(err)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Attach files")
        .pick_files(move |picked| {
            let _ = tx.send(picked);
        });
    let picked = rx.await.map_err(err)?.unwrap_or_default();
    stage_paths(
        &a,
        (conversation_id, thread_root_id),
        picked.into_iter().filter_map(|p| p.into_path().ok()),
    )
}

fn stage_paths(
    a: &client_core::Account,
    key: client_core::account::DraftKey,
    paths: impl IntoIterator<Item = std::path::PathBuf>,
) -> R<usize> {
    let mut n = 0;
    for p in paths {
        a.stage_file(key, &p).map_err(err)?;
        n += 1;
    }
    Ok(n)
}

/// Stages files dropped on the window, claimed by the id the drop event carried.
#[tauri::command]
async fn stage_dropped(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
    drop_id: u64,
) -> R<usize> {
    let a = host.core.account(account_id).map_err(err)?;
    let paths = host
        .take_drop(drop_id)
        .ok_or_else(|| "those files are no longer available".to_string())?;
    stage_paths(&a, (conversation_id, thread_root_id), paths)
}

/// Stages pasted file bytes. The body is the raw bytes; the composer and the file's name come
/// in headers, the name percent-encoded.
#[tauri::command]
async fn stage_pasted(host: State<'_, Host>, request: tauri::ipc::Request<'_>) -> R<Uuid> {
    let header = |name: &str| {
        request
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let id = |name: &str| -> R<Option<Uuid>> {
        header(name)
            .filter(|v| !v.is_empty())
            .map(|v| Uuid::parse_str(&v).map_err(err))
            .transpose()
    };
    let account_id = id("x-account")?.ok_or("no account")?;
    let conversation_id = id("x-conversation")?.ok_or("no conversation")?;
    let thread_root_id = id("x-thread")?;
    let name = percent_decode(&header("x-name").unwrap_or_default());
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected the file's bytes".into());
    };
    host.core
        .account(account_id)
        .map_err(err)?
        .stage_bytes((conversation_id, thread_root_id), &name, bytes)
        .map_err(err)
}

/// Decodes `%XX` escapes as UTF-8; anything malformed stays as it is.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tauri::command]
async fn retry_staged(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
    id: Uuid,
) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .retry_staged((conversation_id, thread_root_id), id);
    Ok(())
}

#[tauri::command]
fn remove_staged(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
    id: Uuid,
) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .remove_staged((conversation_id, thread_root_id), id);
    Ok(())
}

/// Asks where to save an attachment, then fetches it through the core, which checks access.
/// `false` when the dialog was cancelled.
#[tauri::command]
async fn save_file(
    app: tauri::AppHandle,
    host: State<'_, Host>,
    account_id: Uuid,
    file_id: Uuid,
    name: String,
) -> R<bool> {
    use tauri_plugin_dialog::DialogExt;
    let a = host.core.account(account_id).map_err(err)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Save attachment")
        .set_file_name(&name)
        .save_file(move |picked| {
            let _ = tx.send(picked);
        });
    let Some(picked) = rx.await.map_err(err)? else {
        return Ok(false);
    };
    let path = picked.into_path().map_err(err)?;
    let (bytes, _) = a.fetch_file(file_id, false).await.map_err(refusal)?;
    tokio::fs::write(&path, bytes).await.map_err(err)?;
    Ok(true)
}

#[tauri::command]
fn retry_send(host: State<'_, Host>, account_id: Uuid, id: Uuid) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .retry(id)
        .map_err(err)
}

#[tauri::command]
fn discard_send(host: State<'_, Host>, account_id: Uuid, id: Uuid) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .discard(id)
        .map_err(err)
}

#[tauri::command]
fn set_focus(host: State<'_, Host>, account_id: Uuid, conversation_id: Option<Uuid>) -> R<()> {
    for a in host.core.accounts() {
        a.set_focus(if a.id == account_id {
            conversation_id
        } else {
            None
        });
    }
    Ok(())
}

/// The window gained or lost focus or visibility.
#[tauri::command]
fn set_window_active(host: State<'_, Host>, active: bool) -> R<()> {
    for a in host.core.accounts() {
        a.set_window_active(active);
    }
    Ok(())
}

#[tauri::command]
fn typing(host: State<'_, Host>, account_id: Uuid, conversation_id: Uuid) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .typing(conversation_id);
    Ok(())
}

#[tauri::command]
async fn create_workspace(host: State<'_, Host>, account_id: Uuid, name: String) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: proto::model::Workspace = a
        .api(
            client_core::transport::Method::Post,
            "/workspaces",
            &[],
            Some(&proto::api::CreateWorkspace { name }),
            None,
        )
        .await
        .map_err(refusal)?;
    // The new workspace arrives on the user stream.
    Ok(())
}

#[tauri::command]
fn ui_log(message: String) {
    tracing::warn!(target: "ui", "{message}");
}

#[tauri::command]
async fn create_channel(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    name: String,
    private: bool,
) -> R<Conversation> {
    let a = host.core.account(account_id).map_err(err)?;
    let kind = if private {
        ConversationKind::Private
    } else {
        ConversationKind::Public
    };
    a.api(
        Method::Post,
        &format!("/workspaces/{workspace_id}/channels"),
        &[],
        Some(&proto::api::CreateChannel { name, kind }),
        None,
    )
    .await
    .map_err(err)
}

#[tauri::command]
async fn browse_channels(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
) -> R<Vec<Conversation>> {
    let a = host.core.account(account_id).map_err(err)?;
    let list: proto::api::ConversationList = a
        .api(
            Method::Get,
            &format!("/workspaces/{workspace_id}/channels"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(list.conversations)
}

#[tauri::command]
async fn join_channel(host: State<'_, Host>, account_id: Uuid, conversation_id: Uuid) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Post,
            &format!("/conversations/{conversation_id}/join"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(())
}

#[tauri::command]
async fn leave_channel(host: State<'_, Host>, account_id: Uuid, conversation_id: Uuid) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.leave(conversation_id).await.map_err(refusal)
}

/// The server's own wording for a refused request, when it gave one.
fn refusal(e: client_core::account::SyncError) -> String {
    match e {
        client_core::account::SyncError::Transport(
            client_core::transport::TransportError::Status { body, .. },
        ) if body["message"].is_string() => body["message"].as_str().unwrap_or_default().into(),
        other => other.to_string(),
    }
}

/// [`refusal`] for core calls that return `anyhow` errors.
fn refused(e: anyhow::Error) -> String {
    match e.downcast_ref::<client_core::transport::TransportError>() {
        Some(client_core::transport::TransportError::Status { body, .. })
            if body["message"].is_string() =>
        {
            body["message"].as_str().unwrap_or_default().into()
        }
        _ => e.to_string(),
    }
}

#[tauri::command]
async fn conversation_members(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
) -> R<Vec<Member>> {
    let a = host.core.account(account_id).map_err(err)?;
    a.conversation_members(conversation_id)
        .await
        .map_err(refusal)
}

#[tauri::command]
async fn add_member(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    user_id: Uuid,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.add_member(conversation_id, user_id)
        .await
        .map_err(refusal)
}

#[tauri::command]
async fn remove_member(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    user_id: Uuid,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.remove_member(conversation_id, user_id)
        .await
        .map_err(refusal)
}

#[tauri::command]
fn online(host: State<'_, Host>, account_id: Uuid) -> R<Vec<Uuid>> {
    Ok(host.core.account(account_id).map_err(err)?.online())
}

#[tauri::command]
fn preferences(host: State<'_, Host>, account_id: Uuid) -> R<Preferences> {
    host.core
        .account(account_id)
        .map_err(err)?
        .preferences()
        .map_err(err)
}

#[tauri::command]
async fn set_preferences(
    host: State<'_, Host>,
    account_id: Uuid,
    preferences: Preferences,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.set_preferences(preferences).await.map_err(refusal)
}

#[tauri::command]
async fn set_muted(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    muted: bool,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.set_muted(conversation_id, muted).await.map_err(refusal)
}

#[tauri::command]
fn recent_emoji(host: State<'_, Host>, account_id: Uuid) -> R<Vec<String>> {
    host.core
        .account(account_id)
        .map_err(err)?
        .recent_emoji()
        .map_err(err)
}

#[tauri::command]
fn composer_draft(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
) -> R<Option<String>> {
    host.core
        .account(account_id)
        .map_err(err)?
        .composer_draft(conversation_id, thread_root_id)
        .map_err(err)
}

#[tauri::command]
fn save_composer_draft(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
    text: String,
) -> R<()> {
    host.core
        .account(account_id)
        .map_err(err)?
        .set_composer_draft(conversation_id, thread_root_id, &text)
        .map_err(err)
}

#[tauri::command]
fn ui_prefs(host: State<'_, Host>) -> proto::view::UiPrefs {
    host.core.ui_prefs()
}

#[tauri::command]
fn set_ui_prefs(host: State<'_, Host>, prefs: proto::view::UiPrefs) -> R<proto::view::UiPrefs> {
    host.core.set_ui_prefs(prefs).map_err(err)
}

#[tauri::command]
fn reconnect_now(host: State<'_, Host>, account_id: Uuid) -> R<()> {
    host.core.account(account_id).map_err(err)?.reconnect_now();
    Ok(())
}

#[tauri::command]
async fn sign_in_again(host: State<'_, Host>, account_id: Uuid, password: String) -> R<()> {
    host.core
        .sign_in_again(account_id, &password)
        .await
        .map_err(refused)
}

#[tauri::command]
fn workspace_members(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
) -> R<Vec<Member>> {
    host.core
        .account(account_id)
        .map_err(err)?
        .store
        .members(workspace_id)
        .map_err(err)
}

#[tauri::command]
async fn open_dm(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    user_ids: Vec<Uuid>,
) -> R<Conversation> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(
        Method::Post,
        &format!("/workspaces/{workspace_id}/dms"),
        &[],
        Some(&proto::api::OpenDm { user_ids }),
        None,
    )
    .await
    .map_err(err)
}

#[tauri::command]
async fn search(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    query: String,
    offset: Option<u32>,
) -> R<Vec<proto::view::SearchHitView>> {
    let a = host.core.account(account_id).map_err(err)?;
    a.search(workspace_id, &query, offset.unwrap_or(0))
        .await
        .map_err(refusal)
}

#[tauri::command]
fn edit_message(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    message_id: Uuid,
    base_revision: i64,
    body: String,
) -> R<Uuid> {
    let a = host.core.account(account_id).map_err(err)?;
    let id = resolve(&a, message_id)?;
    a.edit_message(conversation_id, id, base_revision, body)
        .map_err(err)
}

#[tauri::command]
fn delete_message(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    message_id: Uuid,
) -> R<Uuid> {
    let a = host.core.account(account_id).map_err(err)?;
    let id = resolve(&a, message_id)?;
    a.delete_message(conversation_id, id).map_err(err)
}

#[tauri::command]
fn toggle_reaction(
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    message_id: Uuid,
    emoji: String,
) -> R<Uuid> {
    let a = host.core.account(account_id).map_err(err)?;
    let id = resolve(&a, message_id)?;
    a.toggle_reaction(conversation_id, id, emoji).map_err(err)
}

#[tauri::command]
async fn mark_read(host: State<'_, Host>, account_id: Uuid, conversation_id: Uuid) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    a.mark_seen(conversation_id).await.map_err(err)
}

/// A thread root as the UI names it (a client id) to the server id.
fn resolve_root(a: &client_core::Account, root: Option<Uuid>) -> R<Option<Uuid>> {
    root.map(|r| resolve(a, r)).transpose()
}

/// Views key messages by client id; commands need the server id.
fn resolve(a: &client_core::Account, key: Uuid) -> R<Uuid> {
    a.store
        .message_by_client_id(key)
        .map_err(err)?
        .map(|m| m.id)
        .ok_or_else(|| "that message has not been sent yet".to_string())
}

/// Opens a URL in the system browser.
fn open_browser(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url).spawn()?;
    Ok(())
}

/// What a server offers before signing in, such as its OIDC provider.
#[tauri::command]
async fn server_meta(host: State<'_, Host>, server_url: String) -> R<proto::api::Meta> {
    host.core.server_meta(&server_url).await.map_err(refused)
}

/// The `commons://` link the app was last opened with, checked and previewed; None when
/// there is none. Taking it clears it.
#[tauri::command]
async fn take_invite_link(host: State<'_, Host>) -> R<Option<proto::view::OpenedInvite>> {
    let raw = host.opened_link.lock().map_err(err)?.take();
    let Some(raw) = raw else {
        return Ok(None);
    };
    host.core
        .open_invite_link(&raw)
        .await
        .map(Some)
        .map_err(err)
}

/// Signs in or creates an account through the server's provider, in the system browser.
#[tauri::command]
async fn oidc_sign_in(
    host: State<'_, Host>,
    server_url: String,
    invite: Option<String>,
) -> R<Uuid> {
    host.core
        .add_account_oidc(&server_url, invite.as_deref(), open_browser)
        .await
        .map_err(refused)
}

#[tauri::command]
async fn oidc_sign_in_again(host: State<'_, Host>, account_id: Uuid) -> R<()> {
    host.core
        .sign_in_again_oidc(account_id, open_browser)
        .await
        .map_err(refused)
}

#[tauri::command]
async fn oidc_link(host: State<'_, Host>, account_id: Uuid) -> R<proto::api::IdentityList> {
    host.core
        .link_oidc(account_id, open_browser)
        .await
        .map_err(refused)
}

/// Stops a provider sign-in that is waiting for the browser.
#[tauri::command]
fn oidc_cancel(host: State<'_, Host>) {
    host.core.cancel_oidc();
}

#[tauri::command]
async fn identities(host: State<'_, Host>, account_id: Uuid) -> R<proto::api::IdentityList> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(Method::Get, "/me/identities", &[], None::<&()>, None)
        .await
        .map_err(refusal)
}

#[tauri::command]
async fn unlink_identity(host: State<'_, Host>, account_id: Uuid, identity_id: Uuid) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Delete,
            &format!("/me/identities/{identity_id}"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(())
}

/// Members with role, join date and online state (owners and admins).
#[tauri::command]
async fn admin_members(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
) -> R<proto::api::AdminMemberList> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(
        Method::Get,
        &format!("/workspaces/{workspace_id}/admin/members"),
        &[],
        None::<&()>,
        None,
    )
    .await
    .map_err(refusal)
}

#[tauri::command]
async fn set_member_role(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    user_id: Uuid,
    role: proto::model::Role,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Patch,
            &format!("/workspaces/{workspace_id}/members/{user_id}"),
            &[],
            Some(&proto::api::SetRole { role }),
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(())
}

/// Removes someone from the workspace, which ends their access to all of it.
#[tauri::command]
async fn remove_workspace_member(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    user_id: Uuid,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Delete,
            &format!("/workspaces/{workspace_id}/members/{user_id}"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(())
}

#[tauri::command]
async fn list_invites(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
) -> R<proto::api::InviteList> {
    let a = host.core.account(account_id).map_err(err)?;
    a.api(
        Method::Get,
        &format!("/workspaces/{workspace_id}/invites"),
        &[],
        None::<&()>,
        None,
    )
    .await
    .map_err(refusal)
}

#[tauri::command]
async fn revoke_invite(
    host: State<'_, Host>,
    account_id: Uuid,
    workspace_id: Uuid,
    invite_id: Uuid,
) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Delete,
            &format!("/workspaces/{workspace_id}/invites/{invite_id}"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(refusal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn pasted_names_decode() {
        assert_eq!(
            percent_decode("Screen%20Shot%20%E2%9C%93.png"),
            "Screen Shot ✓.png"
        );
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }
}
