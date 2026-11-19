//! Bindings to the Tauri IPC bridge (`window.__TAURI__.core`).
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn tauri_invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;

    /// A Tauri channel: the core streams batches to `onmessage`.
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    pub type Channel;

    #[wasm_bindgen(constructor, js_namespace = ["window", "__TAURI__", "core"])]
    pub fn new() -> Channel;

    #[wasm_bindgen(method, setter, js_name = onmessage)]
    pub fn set_onmessage(this: &Channel, f: &js_sys::Function);
}

fn to_js<T: Serialize>(v: &T) -> JsValue {
    v.serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .unwrap_or(JsValue::NULL)
}

fn error_text(e: JsValue) -> String {
    e.as_string()
        .or_else(|| js_sys::JSON::stringify(&e).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "unknown error".into())
}

/// Invokes a core command with named arguments.
pub async fn invoke<T: DeserializeOwned>(cmd: &str, args: serde_json::Value) -> Result<T, String> {
    let v = tauri_invoke(cmd, to_js(&args)).await.map_err(error_text)?;
    serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string())
}

/// Reports a UI-side problem to the shell's log.
pub fn warn(message: String) {
    web_sys::console::warn_1(&message.clone().into());
    wasm_bindgen_futures::spawn_local(async move {
        let _ = tauri_invoke("ui_log", to_js(&serde_json::json!({ "message": message }))).await;
    });
}

/// Fire-and-forget command; errors go to the log.
pub fn command(cmd: &'static str, args: serde_json::Value) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = invoke::<serde_json::Value>(cmd, args).await {
            warn(format!("{cmd}: {e}"));
        }
    });
}

/// Invokes a command whose argument `channel_arg` is a Tauri channel delivering `T`s.
pub async fn invoke_with_channel<T: DeserializeOwned + 'static, R: DeserializeOwned>(
    cmd: &str,
    args: serde_json::Value,
    channel_arg: &str,
    on_message: impl FnMut(T) + 'static,
) -> Result<R, String> {
    let channel = Channel::new();
    let mut on_message = on_message;
    let cb =
        Closure::<dyn FnMut(JsValue)>::new(
            move |v: JsValue| match serde_wasm_bindgen::from_value::<T>(v) {
                Ok(t) => on_message(t),
                Err(e) => warn(format!("bad channel message: {e}")),
            },
        );
    channel.set_onmessage(cb.as_ref().unchecked_ref());
    // The channel lives as long as the subscription; the core stops sending on unsubscribe.
    cb.forget();
    let obj = to_js(&args);
    js_sys::Reflect::set(&obj, &JsValue::from_str(channel_arg), &channel).map_err(error_text)?;
    let v = tauri_invoke(cmd, obj).await.map_err(error_text)?;
    serde_wasm_bindgen::from_value(v).map_err(|e| e.to_string())
}

pub fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

pub fn next_frame(f: impl FnOnce() + 'static) {
    let cb = Closure::once_into_js(f);
    if let Some(w) = web_sys::window() {
        let _ = w.request_animation_frame(cb.unchecked_ref());
    }
}

// web-sys has no ResizeObserver feature enabled here, so bind the one call we need.
#[wasm_bindgen]
extern "C" {
    pub type ResizeObserver;

    #[wasm_bindgen(constructor)]
    pub fn new(callback: &js_sys::Function) -> ResizeObserver;

    #[wasm_bindgen(method)]
    pub fn observe(this: &ResizeObserver, target: &web_sys::Element);

    #[wasm_bindgen(method)]
    pub fn disconnect(this: &ResizeObserver);
}

/// Calls `f` whenever any of `targets` changes size, until the returned observer disconnects.
pub fn on_resize(targets: &[&web_sys::Element], f: impl FnMut() + 'static) -> ResizeObserver {
    let cb = Closure::<dyn FnMut()>::new(f);
    let ro = ResizeObserver::new(cb.as_ref().unchecked_ref());
    // The observer owns the callback from here; it stops firing once disconnected.
    cb.forget();
    for t in targets {
        ro.observe(t);
    }
    ro
}
