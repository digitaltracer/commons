//! The Commons desktop shell. It hosts the core in-process and bridges it to the Leptos views
//! over Tauri commands and channels. The webview never sees tokens, SQL or sockets.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod spike;

use std::sync::{Arc, Mutex};

use client_core::Core;
use client_core::platform::{KeyringSecrets, Notifier};
use client_core::transport::{HttpTransport, Transport};
use proto::view::AppEvent;
use tauri::Manager;
use tauri::ipc::Channel;

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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let spike_mode = std::env::var("COMMONS_SPIKE").is_ok_and(|v| v == "1");
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
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
                Arc::new(KeyringSecrets::new("org.commons.desktop")),
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
            Ok(())
        })
        .invoke_handler(commands::handler())
        .run(tauri::generate_context!())
        .expect("error while running Commons");
}
