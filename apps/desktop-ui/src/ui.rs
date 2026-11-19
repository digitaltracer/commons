//! Small shared pieces: icons, avatars, time labels and text offsets.
use leptos::prelude::*;
use uuid::Uuid;

/// Stroke icons drawn as inline SVG paths (24×24 box).
#[derive(Clone, Copy)]
pub enum Icon {
    Hash,
    Lock,
    Search,
    Compose,
    Plus,
    Chevron,
    Close,
    Members,
    Smile,
    Reply,
    Edit,
    Trash,
    Download,
    File,
    Attach,
    At,
    Send,
    Bold,
    Italic,
    Code,
    CodeBlock,
    Strike,
    Link,
    List,
    NumberedList,
    Bell,
    BellOff,
    Clock,
    Offline,
    Refresh,
    Alert,
    Info,
    Check,
    Server,
    Leave,
    Settings,
    Shield,
}

fn path(i: Icon) -> &'static str {
    match i {
        Icon::Hash => "M4 9h16M4 15h16M10 3 8 21M16 3l-2 18",
        Icon::Lock => "M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4",
        Icon::Search => "M18 11a7 7 0 1 1-14 0 7 7 0 0 1 14 0zM20 20l-3.5-3.5",
        Icon::Compose => {
            "M12 4H6a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-6M17 3l4 4-9 9H8v-4z"
        }
        Icon::Plus => "M12 5v14M5 12h14",
        Icon::Chevron => "m6 9 6 6 6-6",
        Icon::Close => "M6 6l12 12M18 6 6 18",
        Icon::Members => {
            "M12.5 8a3.5 3.5 0 1 1-7 0 3.5 3.5 0 0 1 7 0zM2.5 20a6.5 6.5 0 0 1 13 0M16 4.5a3.5 3.5 0 0 1 0 7M18 14a6 6 0 0 1 3.5 6"
        }
        Icon::Smile => {
            "M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0zM8.5 14.5a4.5 4.5 0 0 0 7 0M9 9.5h.01M15 9.5h.01"
        }
        Icon::Reply => "M21 12a8 8 0 0 1-11.6 7.1L4 20l1.1-4.4A8 8 0 1 1 21 12z",
        Icon::Edit => "M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4",
        Icon::Trash => "M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13",
        Icon::Download => "M12 4v11M7 10l5 5 5-5M5 20h14",
        Icon::File => "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5",
        Icon::Attach => {
            "M21 11.5 12.5 20a5 5 0 0 1-7-7l8.5-8.5a3.5 3.5 0 0 1 5 5L10.5 18a2 2 0 0 1-3-3l7.5-7.5"
        }
        Icon::At => "M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0zM16 8v5a3 3 0 0 0 6 0v-1a10 10 0 1 0-4 8",
        Icon::Send => "M12 19V5M6 11l6-6 6 6",
        Icon::Bold => "M7 5h6a3.5 3.5 0 0 1 0 7H7zM7 12h7a3.5 3.5 0 0 1 0 7H7z",
        Icon::Italic => "M19 4h-9M14 20H5M15 4 9 20",
        Icon::Code => "m8 7-5 5 5 5M16 7l5 5-5 5",
        Icon::CodeBlock => "M4 4h16v16H4zM9 9l-2 3 2 3M15 9l2 3-2 3",
        Icon::Strike => {
            "M5 12h14M16 6.5A4 3 0 0 0 12 5c-2.5 0-4 1.3-4 3M8 17.5a4 3 0 0 0 4 1.5c2.5 0 4-1.3 4-3"
        }
        Icon::Link => {
            "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1"
        }
        Icon::List => "M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01",
        Icon::NumberedList => "M10 6h10M10 12h10M10 18h10M4 5h1v4M4 15h2l-2 3h2",
        Icon::Bell => "M6 16V11a6 6 0 0 1 12 0v5l2 2H4zM10 20a2 2 0 0 0 4 0",
        Icon::BellOff => "M6 16V11a6 6 0 0 1 9.5-4.9M18 11v5l2 2H8M10 20a2 2 0 0 0 4 0M3 3l18 18",
        Icon::Clock => "M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0zM12 7v5l3 2",
        Icon::Offline => {
            "M3 3l18 18M8.5 16.5a5 5 0 0 1 7 0M5 12.5a10 10 0 0 1 4-2.3M19 12.5a10 10 0 0 0-3-2M2 8.8a15 15 0 0 1 4.4-2.6M22 8.8A15 15 0 0 0 11 5M12 20h.01"
        }
        Icon::Refresh => "M20 11a8 8 0 1 0-2.3 5.7M20 5v6h-6",
        Icon::Alert => "M12 6v7M12 17h.01",
        Icon::Info => "M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0zM12 11v5M12 8h.01",
        Icon::Check => "m5 12 5 5 9-10",
        Icon::Server => "M4 4h16v7H4zM4 13h16v7H4zM8 7.5h.01M8 16.5h.01",
        Icon::Leave => "M15 4h4a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1h-4M10 17l-5-5 5-5M5 12h11",
        Icon::Settings => "M4 7h9M17 7h3M15 5v4M4 17h3M11 17h9M9 15v4",
        Icon::Shield => "M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z",
    }
}

/// An icon at the default 16 px, or `size` px.
pub fn icon(i: Icon) -> impl IntoView {
    sized(i, 16)
}

pub fn sized(i: Icon, size: u32) -> impl IntoView {
    view! {
        <svg class="i" viewBox="0 0 24 24" aria-hidden="true"
             style=format!("width: {size}px; height: {size}px")>
            <path d=path(i)></path>
        </svg>
    }
}

const PALETTE: [&str; 7] = [
    "#2F6F5E", "#3E6A8F", "#85489A", "#8A5A14", "#A3483F", "#4A5AA8", "#6B6F6D",
];

/// A stable colour for a person or workspace.
pub fn color(id: Uuid) -> &'static str {
    let n = id.as_u128();
    PALETTE[((n ^ (n >> 64)) % PALETTE.len() as u128) as usize]
}

/// "PN" for "Priya Nair", "AL" for "alice".
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let s: String = match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [first, .., last] => first.chars().take(1).chain(last.chars().take(1)).collect(),
    };
    s.to_uppercase()
}

/// "La" for "Lattice": workspace tiles use a capital and a small letter.
pub fn tile(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.as_slice() {
        [] => "?".into(),
        [one] => {
            let mut c = one.chars();
            let first: String = c
                .next()
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_default();
            let second: String = c
                .next()
                .map(|c| c.to_lowercase().collect())
                .unwrap_or_default();
            first + &second
        }
        [a, b, ..] => a
            .chars()
            .take(1)
            .chain(b.chars().take(1))
            .collect::<String>()
            .to_uppercase(),
    }
}

/// An avatar tile; `size` is the CSS class (`""`, `"s"` or `"m"`).
pub fn avatar(id: Uuid, name: &str, size: &'static str) -> impl IntoView + use<> {
    view! {
        <span class=format!("av {size}") style=format!("background: {}", color(id)) aria-hidden="true">
            {initials(name)}
        </span>
    }
}

fn date(ms: i64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// "10:02 AM".
pub fn clock(ms: i64) -> String {
    let d = date(ms);
    let h = d.get_hours();
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{:02} {ap}", d.get_minutes())
}

fn day_index(ms: i64) -> i64 {
    let d = date(ms);
    // Local midnight, in days.
    let local = ms - (d.get_timezone_offset() * 60_000.0) as i64;
    local.div_euclid(86_400_000)
}

fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// "Today", "Yesterday" or "Mon, Nov 12".
pub fn day_label(ms: i64) -> String {
    let diff = day_index(now_ms()) - day_index(ms);
    match diff {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            let d = date(ms);
            let mut s = format!(
                "{}, {} {}",
                DAYS[d.get_day() as usize % 7],
                MONTHS[d.get_month() as usize % 12],
                d.get_date()
            );
            if d.get_full_year() != date(now_ms()).get_full_year() {
                s.push_str(&format!(", {}", d.get_full_year()));
            }
            s
        }
    }
}

pub fn same_day(a: i64, b: i64) -> bool {
    day_index(a) == day_index(b)
}

/// "Today 10:14 AM", "Yesterday 4:40 PM" or "Nov 12".
pub fn when(ms: i64) -> String {
    match day_index(now_ms()) - day_index(ms) {
        0 => format!("Today {}", clock(ms)),
        1 => format!("Yesterday {}", clock(ms)),
        _ => {
            let d = date(ms);
            format!("{} {}", MONTHS[d.get_month() as usize % 12], d.get_date())
        }
    }
}

/// "1.2 MB".
pub fn size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{bytes} B")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    }
}

/// "PNG" from "image/png" or a file name.
pub fn kind_label(mime: &str, name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_uppercase())
        .filter(|e| e.len() <= 5)
        .unwrap_or_else(|| mime.rsplit('/').next().unwrap_or("File").to_uppercase())
}

/// Host part of a server URL, for labels.
pub fn host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// Copies text to the clipboard. Returns false when the webview refused.
pub async fn copy_text(text: &str) -> bool {
    let Some(w) = web_sys::window() else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(w.navigator().clipboard().write_text(text))
        .await
        .is_ok()
}

/// Byte offset of a UTF-16 index, as text fields report caret positions.
pub fn utf16_to_byte(s: &str, n: u32) -> usize {
    let mut units = 0u32;
    for (i, c) in s.char_indices() {
        if units >= n {
            return i;
        }
        units += c.len_utf16() as u32;
    }
    s.len()
}

pub fn byte_to_utf16(s: &str, b: usize) -> u32 {
    s[..b.min(s.len())].encode_utf16().count() as u32
}
