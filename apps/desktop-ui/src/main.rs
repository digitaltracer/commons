//! Commons desktop views. Rendered in the system webview by the Tauri shell; the same views
//! are meant to become the browser client. All state and behaviour live in the core: these
//! views send commands and render view models.
mod app;
mod emoji;
mod ipc;
mod markdown;
mod messages;
mod panels;
mod search;
mod spike;
mod state;
mod switcher;
mod ui;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}
