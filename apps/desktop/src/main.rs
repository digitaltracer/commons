//! The Commons desktop shell. It hosts the core in-process and bridges it to the Leptos views
//! over Tauri commands and channels. The webview never sees tokens, SQL or sockets.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod files;
mod spike;
mod updater;

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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let spike_mode = std::env::var("COMMONS_SPIKE").is_ok_and(|v| v == "1");
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(WINDOW_STATE)
                .build(),
        )
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Resized(size) = event {
                keep_minimum(window, *size);
            }
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
            });
            updater::start(app.handle(), !spike_mode)?;
            Ok(())
        })
        .invoke_handler(commands::handler())
        .run(tauri::generate_context!())
        .expect("error while running Commons");
}
