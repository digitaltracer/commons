//! Tauri commands: the UI's only way to reach the core.
use client_core::transport::Method;
use proto::model::{Conversation, ConversationKind, Entity, Member, Preferences};
use proto::view::{AppEvent, SidebarView};
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
        subscribe_window,
        ack_window,
        reset_window,
        move_window,
        unsubscribe_window,
        send_message,
        attach_file,
        edit_message,
        delete_message,
        toggle_reaction,
        mark_read,
        retry_send,
        discard_send,
        set_focus,
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
        reconnect_now,
        sign_in_again,
        save_file,
        ui_log,
        crate::spike::spike_config,
        crate::spike::spike_report,
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
) -> R<Uuid> {
    host.core
        .add_account(&server_url, &email, &password, display_name.as_deref())
        .await
        .map_err(refused)
}

#[tauri::command]
fn remove_account(host: State<'_, Host>, account_id: Uuid) -> R<()> {
    host.core.remove_account(account_id).map_err(err)
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
    let thread_root_id = resolve_root(&a, thread_root_id)?;
    a.send_message_with(
        conversation_id,
        body,
        thread_root_id,
        vec![],
        mentions.unwrap_or_default(),
    )
    .map_err(err)
}

/// Picks a file, uploads it and queues a message carrying it. `None` when the picker was
/// cancelled.
#[tauri::command]
async fn attach_file(
    app: tauri::AppHandle,
    host: State<'_, Host>,
    account_id: Uuid,
    conversation_id: Uuid,
    thread_root_id: Option<Uuid>,
) -> R<Option<Uuid>> {
    use tauri_plugin_dialog::DialogExt;
    let a = host.core.account(account_id).map_err(err)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Attach a file")
        .pick_file(move |picked| {
            let _ = tx.send(picked);
        });
    let Some(picked) = rx.await.map_err(err)? else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(err)?;
    let thread_root_id = resolve_root(&a, thread_root_id)?;
    a.attach_file(conversation_id, thread_root_id, &path)
        .await
        .map(Some)
        .map_err(|e| match e {
            client_core::account::SyncError::Transport(
                client_core::transport::TransportError::Status { body, .. },
            ) => body["message"]
                .as_str()
                .unwrap_or("the upload was refused")
                .to_string(),
            other => other.to_string(),
        })
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
    a.mark_read(conversation_id).await.map_err(err)
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
