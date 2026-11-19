//! The composer's highlight layer: the draft drawn again behind its transparent textarea, in
//! the same font, padding and wrapping, with colour and background only. Nothing here changes
//! glyph widths or line heights, so the caret never drifts from the text it sits in.
use leptos::html::{Div, Textarea};
use leptos::prelude::*;
use proto::model::Entity;
use wasm_bindgen::JsCast;

use crate::compose::{self, Piece, Run};

/// The layer's content for `text`. The trailing characters give the layer the textarea's last
/// empty line, and one line more, so its scroll range never falls short of the textarea's.
pub fn content(text: &str, entities: &[Entity]) -> impl IntoView + use<> {
    let ranges = proto::markdown::highlight(text, entities);
    let pieces = compose::paint(text.len(), &ranges);
    let run = |(r, bits): Run| {
        let t = text[r].to_string();
        if bits == 0 {
            t.into_any()
        } else {
            view! { <span class=compose::classes(bits)>{t}</span> }.into_any()
        }
    };
    let views = pieces
        .into_iter()
        .map(|p| match p {
            Piece::Runs(v) => v.into_iter().map(run).collect_view().into_any(),
            Piece::Block(v) => {
                view! { <span class="hcb">{v.into_iter().map(run).collect_view()}</span> }
                    .into_any()
            }
        })
        .collect_view();
    view! { {views}"\u{200B}\n\u{200B}" }
}

/// Puts the layer on the textarea's content box and scroll offset. The textarea's width can
/// shrink by a scrollbar, so the layer takes its client size rather than the wrapper's.
pub fn sync(area: NodeRef<Textarea>, layer: NodeRef<Div>) {
    let (Some(ta), Some(l)) = (area.get_untracked(), layer.get_untracked()) else {
        return;
    };
    let style = web_sys::HtmlElement::style(&l);
    let _ = style.set_property("width", &format!("{}px", ta.client_width()));
    let _ = style.set_property("height", &format!("{}px", ta.client_height()));
    // web-sys rounds scrollTop to an integer; at fractional device scales that is half a pixel.
    if let Ok(top) = js_sys::Reflect::get(&ta, &"scrollTop".into()) {
        let _ = js_sys::Reflect::set(&l, &"scrollTop".into(), &top);
    }
}

fn scroll_top(el: &web_sys::Element) -> f64 {
    js_sys::Reflect::get(el, &"scrollTop".into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

/// Draws a full-width shaded box behind each code block, from its first line to its last.
/// The boxes sit under the text and outside its flow, so they can't move a glyph.
pub fn shade(layer: NodeRef<Div>, boxes: NodeRef<Div>) {
    let (Some(l), Some(bg)) = (layer.get_untracked(), boxes.get_untracked()) else {
        return;
    };
    while let Some(c) = bg.first_child() {
        let _ = bg.remove_child(&c);
    }
    let blocks = l.get_elements_by_class_name("hcb");
    if blocks.length() == 0 {
        return;
    }
    let origin = l.get_bounding_client_rect().top() - scroll_top(&l);
    for i in 0..blocks.length() {
        let Some(block) = blocks.item(i) else {
            continue;
        };
        let rects = block.get_client_rects();
        let (Some(first), Some(last)) =
            (rects.item(0), rects.item(rects.length().saturating_sub(1)))
        else {
            continue;
        };
        let Ok(b) = document().create_element("div") else {
            continue;
        };
        b.set_class_name("hcbg");
        let _ = b.set_attribute(
            "style",
            &format!(
                "top: {}px; height: {}px",
                first.top() - origin,
                last.bottom() - first.top()
            ),
        );
        let _ = bg.append_child(&b);
    }
}

/// Types `s` over the textarea's selection the way the keyboard would, so the edit joins the
/// field's own undo stack. False when the webview refused, and the caller sets the value.
pub fn insert_text(s: &str) -> bool {
    !s.is_empty()
        && document()
            .dyn_into::<web_sys::HtmlDocument>()
            .ok()
            .and_then(|d| {
                d.exec_command_with_show_ui_and_value("insertText", false, s)
                    .ok()
            })
            .unwrap_or(false)
}
