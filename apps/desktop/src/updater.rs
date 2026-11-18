//! Updates, in release builds only: check at startup and every 6 hours, download in the
//! background, then offer "Restart to update". Nothing installs or restarts until the user asks.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::view::AppEvent;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::Host;

const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// A downloaded, signature-checked update waiting for the user.
#[derive(Default)]
pub struct Pending(Mutex<Option<(Update, Arc<Vec<u8>>)>>);

pub fn start(app: &AppHandle, enabled: bool) -> tauri::Result<()> {
    app.manage(Pending::default());
    if cfg!(debug_assertions) || !enabled {
        return Ok(());
    }
    app.plugin(tauri_plugin_updater::Builder::new().build())?;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            match download(&app).await {
                Ok(Some(version)) => {
                    tracing::info!(%version, "update downloaded");
                    let host = app.state::<Host>();
                    if let Ok(mut chans) = host.app_channels.lock() {
                        let ev = AppEvent::UpdateReady { version };
                        chans.retain(|c| c.send(ev.clone()).is_ok());
                    }
                    return;
                }
                Ok(None) => tracing::info!("no update"),
                Err(e) => tracing::warn!(error = %e, "update check failed"),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
    Ok(())
}

async fn download(app: &AppHandle) -> anyhow::Result<Option<String>> {
    let Some(update) = app.updater()?.check().await? else {
        return Ok(None);
    };
    let bytes = update.download(|_, _| {}, || {}).await?;
    let version = update.version.clone();
    if let Ok(mut p) = app.state::<Pending>().0.lock() {
        *p = Some((update, Arc::new(bytes)));
    }
    Ok(Some(version))
}

/// The version waiting to be installed, for a view that subscribed after it was announced.
#[tauri::command]
pub fn update_ready(pending: State<'_, Pending>) -> Option<String> {
    pending
        .0
        .lock()
        .ok()?
        .as_ref()
        .map(|(u, _)| u.version.clone())
}

/// Replaces the app bundle with the downloaded update and relaunches. Async, so it runs off
/// the main thread: an install that needs admin rights asks on the main thread and waits.
#[tauri::command]
pub async fn restart_to_update(app: AppHandle, pending: State<'_, Pending>) -> Result<(), String> {
    let ready = pending.0.lock().map_err(|e| e.to_string())?.clone();
    let (update, bytes) = ready.ok_or("no update is waiting")?;
    update.install(bytes.as_slice()).map_err(|e| {
        tracing::warn!(error = %e, "update install failed");
        e.to_string()
    })?;
    app.restart()
}
