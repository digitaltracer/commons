//! Tauri commands: the UI's only way to reach the core.
use client_core::transport::Method;
use proto::model::{Conversation, ConversationKind, Member};
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
        .map_err(err)
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
) -> R<Uuid> {
    host.core
        .account(account_id)
        .map_err(err)?
        .send_message(conversation_id, body, thread_root_id, vec![])
        .map_err(err)
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
        .map_err(err)?;
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
        .map_err(err)?;
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
        .map_err(err)?;
    Ok(())
}

#[tauri::command]
async fn leave_channel(host: State<'_, Host>, account_id: Uuid, conversation_id: Uuid) -> R<()> {
    let a = host.core.account(account_id).map_err(err)?;
    let _: serde_json::Value = a
        .api(
            Method::Post,
            &format!("/conversations/{conversation_id}/leave"),
            &[],
            None::<&()>,
            None,
        )
        .await
        .map_err(err)?;
    Ok(())
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
