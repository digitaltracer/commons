//! The Commons desktop shell. It hosts the core in-process and bridges it to the Leptos views
//! over Tauri commands and channels. The webview never sees tokens, SQL or sockets.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod files;
mod spike;
mod updater;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use client_core::Core;
use client_core::platform::{FileSecrets, KeyringSecrets, Notifier, SecretStore};
use client_core::transport::{HttpTransport, Transport};
use proto::view::AppEvent;
use tauri::Manager;
use tauri::ipc::Channel;
use tauri_plugin_window_state::StateFlags;

pub struct Host {
    pub core: Arc<Core>,
    pub app_channels: Arc<Mutex<Vec<Channel<AppEvent>>>>,
    pub spike: Option<spike::Spike>,
    /// Paths of files dropped on the window, until the UI says which composer takes them.
    pub drops: Mutex<Drops>,
}

#[derive(Default)]
pub struct Drops {
    next: u64,
    paths: HashMap<u64, Vec<PathBuf>>,
}

/// Drops the UI hasn't claimed yet; older ones are forgotten.
const KEPT_DROPS: usize = 8;

impl Host {
    fn broadcast(&self, ev: AppEvent) {
        if let Ok(mut chans) = self.app_channels.lock() {
            chans.retain(|c| c.send(ev.clone()).is_ok());
        }
    }

    fn keep_drop(&self, paths: Vec<PathBuf>) -> Option<u64> {
        let mut d = self.drops.lock().ok()?;
        d.next += 1;
        let id = d.next;
        d.paths.insert(id, paths);
        d.paths.retain(|k, _| *k + KEPT_DROPS as u64 > id);
        Some(id)
    }

    pub fn take_drop(&self, id: u64) -> Option<Vec<PathBuf>> {
        self.drops.lock().ok()?.paths.remove(&id)
    }
}

/// Native file drops. Tauri handles them, so the webview's own drop events carry no files;
/// the UI hears where the files landed and claims them by id, and never sees their paths.
fn on_drag(window: &tauri::Window, event: &tauri::DragDropEvent) {
    let Some(host) = window.app_handle().try_state::<Host>() else {
        return;
    };
    match event {
        tauri::DragDropEvent::Enter { paths, .. } if !paths.is_empty() => {
            host.broadcast(AppEvent::FileDrag { active: true });
        }
        tauri::DragDropEvent::Leave => host.broadcast(AppEvent::FileDrag { active: false }),
        tauri::DragDropEvent::Drop { paths, position } => {
            host.broadcast(AppEvent::FileDrag { active: false });
            if paths.is_empty() {
                return;
            }
            let scale = window.scale_factor().unwrap_or(1.0);
            if let Some(drop_id) = host.keep_drop(paths.clone()) {
                host.broadcast(AppEvent::FilesDropped {
                    drop_id,
                    x: position.x / scale,
                    y: position.y / scale,
                });
            }
        }
        _ => {}
    }
}

/// OS notifications. macOS applies the user's Focus (do-not-disturb) setting to them.
struct OsNotifier(tauri::AppHandle);

impl Notifier for OsNotifier {
    fn show(&self, _: uuid::Uuid, title: &str, body: &str, _: &proto::model::Notification) {
        use tauri_plugin_notification::NotificationExt;
        if let Err(e) = self
            .0
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            tracing::warn!(error = %e, "notification failed");
        }
    }
}

/// Where session tokens live: the keychain, except in debug builds, whose ad-hoc signature
/// changes per build so the keychain asks for a password on every launch. Override with
/// `COMMONS_SECRETS=keychain|file`.
fn secret_store(data_dir: &std::path::Path) -> anyhow::Result<Arc<dyn SecretStore>> {
    let file = match std::env::var("COMMONS_SECRETS").ok().as_deref() {
        None | Some("") => cfg!(debug_assertions),
        Some("file") => true,
        Some("keychain") => false,
        Some(other) => anyhow::bail!("COMMONS_SECRETS must be keychain or file, not {other:?}"),
    };
    Ok(if file {
        let store = FileSecrets::new(data_dir.join("secrets.json"));
        tracing::info!(path = %store.path().display(), "session tokens: file store");
        Arc::new(store)
    } else {
        tracing::info!("session tokens: OS keychain");
        Arc::new(KeyringSecrets::new("org.commons.desktop"))
    })
}

/// What the window keeps between launches: size, position and maximized/fullscreen. Not
/// decorations or visibility, which would fight the overlay title bar or reopen it hidden.
const WINDOW_STATE: StateFlags = StateFlags::SIZE
    .union(StateFlags::POSITION)
    .union(StateFlags::MAXIMIZED)
    .union(StateFlags::FULLSCREEN);

/// macOS applies a window's minimum size only to drags. A frame restored from an old or
/// edited state file, or set from code, is raised back to the configured minimum.
fn keep_minimum(window: &tauri::Window, size: tauri::PhysicalSize<u32>) {
    let app = window.app_handle();
    let Some(cfg) = app
        .config()
        .app
        .windows
        .iter()
        .find(|c| c.label == window.label())
    else {
        return;
    };
    let (min_w, min_h) = (cfg.min_width.unwrap_or(0.0), cfg.min_height.unwrap_or(0.0));
    let size = size.to_logical::<f64>(window.scale_factor().unwrap_or(1.0));
    if size.width < min_w || size.height < min_h {
        let _ = window.set_size(tauri::LogicalSize::new(
            size.width.max(min_w),
            size.height.max(min_h),
        ));
    }
}

/// Two rustls backends are linked (ring from the updater, aws-lc-rs from reqwest), so rustls cannot choose one itself.
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    install_crypto_provider();
    let spike_mode = std::env::var("COMMONS_SPIKE").is_ok_and(|v| v == "1");
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(WINDOW_STATE)
                .build(),
        )
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::Resized(size) => keep_minimum(window, *size),
            tauri::WindowEvent::DragDrop(drag) => on_drag(window, drag),
            _ => {}
        })
        .register_asynchronous_uri_scheme_protocol(files::SCHEME, |ctx, request, responder| {
            let Some(host) = ctx.app_handle().try_state::<Host>() else {
                responder.respond(files::status(503));
                return;
            };
            let core = host.core.clone();
            let path = request.uri().path().to_string();
            tauri::async_runtime::spawn(async move {
                responder.respond(files::respond(core, &path).await);
            });
        })
        .setup(move |app| {
            let runtime = tokio::runtime::Runtime::new()?;
            let handle = runtime.handle().clone();
            tauri::async_runtime::set(handle.clone());
            // The runtime lives for the life of the app.
            std::mem::forget(runtime);
            let data_dir = if spike_mode {
                std::env::temp_dir().join(format!("commons-spike-{}", uuid::Uuid::now_v7()))
            } else {
                app.path().app_data_dir()?
            };
            let channels: Arc<Mutex<Vec<Channel<AppEvent>>>> = Arc::default();
            let fanout = channels.clone();
            let sink = Arc::new(move |ev: AppEvent| {
                if let Ok(mut chans) = fanout.lock() {
                    chans.retain(|c| c.send(ev.clone()).is_ok());
                }
            });
            let notifier: Arc<dyn Notifier> = Arc::new(OsNotifier(app.handle().clone()));
            let _guard = handle.enter();
            let core = Core::open(
                &data_dir,
                secret_store(&data_dir)?,
                notifier,
                Arc::new(|url: &str| Arc::new(HttpTransport::new(url)) as Arc<dyn Transport>),
                sink,
            )?;
            let spike = if spike_mode {
                // Frame timing needs a visible window: WebKit stops animation frames when hidden.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.set_always_on_top(true);
                    let _ = w.set_focus();
                }
                Some(spike::start(&core, &data_dir, app.handle().clone())?)
            } else {
                None
            };
            app.manage(Host {
                core,
                app_channels: channels,
                spike,
                drops: Mutex::default(),
            });
            updater::start(app.handle(), !spike_mode)?;
            Ok(())
        })
        .invoke_handler(commands::handler())
        .run(tauri::generate_context!())
        .expect("error while running Commons");
}

#[cfg(test)]
mod tests {
    // Without a chosen provider each of these panics, as the first HTTPS connection did.
    #[test]
    fn tls_clients_build_after_startup() {
        super::install_crypto_provider();
        let _http = client_core::transport::HttpTransport::new("https://commons.invalid");
        let _socket = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
    }
}
