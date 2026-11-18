//! Renders restricted Markdown as text nodes and a fixed set of elements. Never raw HTML.
use leptos::prelude::*;
use proto::markdown::{Node, Target, render_with_entities};
use proto::model::Entity;
use uuid::Uuid;

use crate::state::{Overlay, Selection, ctx};

/// Who is reading, so mentions of them stand out and clicks know the account.
#[derive(Clone, Copy)]
pub struct Reader {
    pub account_id: Uuid,
    pub me: Uuid,
}

/// A message body. `edited` puts "(edited)" after the last word, inside the last paragraph.
pub fn body(source: &str, entities: &[Entity], reader: Reader, edited: bool) -> AnyView {
    let mut list = render_with_entities(source, entities);
    let last = (edited && matches!(list.last(), Some(Node::Paragraph(_))))
        .then(|| list.pop())
        .flatten();
    let mark = || view! { <span class="edited">" (edited)"</span> };
    match last {
        Some(Node::Paragraph(c)) => {
            view! { {nodes(list, reader)}<p>{nodes(c, reader)}{mark()}</p> }.into_any()
        }
        _ => view! { {nodes(list, reader)}{edited.then(mark)} }.into_any(),
    }
}

fn nodes(list: Vec<Node>, r: Reader) -> AnyView {
    list.into_iter()
        .map(|n| node(n, r))
        .collect_view()
        .into_any()
}

fn node(n: Node, r: Reader) -> AnyView {
    match n {
        Node::Text(t) => t.into_any(),
        Node::Code(t) => view! { <code class="ic">{t}</code> }.into_any(),
        Node::Strong(c) => view! { <strong>{nodes(c, r)}</strong> }.into_any(),
        Node::Emphasis(c) => view! { <em>{nodes(c, r)}</em> }.into_any(),
        Node::Strike(c) => view! { <s>{nodes(c, r)}</s> }.into_any(),
        Node::Link { href, children } => {
            view! { <a href=href target="_blank" rel="noopener noreferrer">{nodes(children, r)}</a> }
                .into_any()
        }
        Node::Paragraph(c) => view! { <p>{nodes(c, r)}</p> }.into_any(),
        Node::CodeBlock { lang, text } => code_block(lang, text),
        Node::Quote(c) => view! { <blockquote>{nodes(c, r)}</blockquote> }.into_any(),
        Node::List { start, items } => {
            let items = items
                .into_iter()
                .map(|i| view! { <li>{nodes(i, r)}</li> })
                .collect_view();
            match start {
                Some(n) => view! { <ol start=n.to_string()>{items}</ol> }.into_any(),
                None => view! { <ul>{items}</ul> }.into_any(),
            }
        }
        Node::LineBreak => view! { <br /> }.into_any(),
        Node::Gap(n) => {
            // One body line per empty line typed (`.body` line-height is 1.45).
            let style = format!("height: calc({n} * 1.45em)");
            view! { <div class="gap" style=style aria-hidden="true"></div> }.into_any()
        }
        Node::Mention { target, text } => mention(target, text, r),
    }
}

/// A code block with its language and a Copy button for the raw text.
fn code_block(lang: Option<String>, text: String) -> AnyView {
    let copied = RwSignal::new(false);
    let raw = StoredValue::new(text.clone());
    let copy = move |_| {
        leptos::task::spawn_local(async move {
            if crate::ui::copy_text(&raw.get_value()).await {
                copied.set(true);
                set_timeout(
                    move || copied.set(false),
                    std::time::Duration::from_millis(1500),
                );
            }
        });
    };
    view! {
        <div class="cb">
            <div class="cbh">
                <span>{lang}</span>
                <button type="button" class="cbcopy" on:click=copy>
                    {move || if copied.get() { "Copied" } else { "Copy" }}
                </button>
            </div>
            <pre class="pre"><code>{text}</code></pre>
        </div>
    }
    .into_any()
}

fn mention(target: Target, text: String, r: Reader) -> AnyView {
    let c = ctx();
    match target {
        Target::User(user_id) => {
            let me = user_id == r.me;
            view! {
                <button type="button" class="men" class:me=me
                    on:click=move |_| c.overlay.set(Overlay::Person { account_id: r.account_id, user_id })>
                    {text}
                </button>
            }
            .into_any()
        }
        Target::Channel(conversation_id) => {
            let known = c
                .conversation(Selection {
                    account_id: r.account_id,
                    conversation_id,
                })
                .is_some();
            view! {
                <button type="button" class="men" disabled=!known
                    on:click=move |_| c.open(r.account_id, conversation_id)>{text}</button>
            }
            .into_any()
        }
    }
}

/// True when the entities mention `me`.
pub fn mentions_me(entities: &[Entity], me: Uuid) -> bool {
    entities
        .iter()
        .any(|e| matches!(e, Entity::Mention { user_id, .. } if *user_id == me))
}
