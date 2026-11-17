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

pub fn body(source: &str, entities: &[Entity], reader: Reader) -> AnyView {
    nodes(render_with_entities(source, entities), reader)
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
        Node::CodeBlock(t) => view! { <pre class="pre"><code>{t}</code></pre> }.into_any(),
        Node::Quote(c) => view! { <blockquote>{nodes(c, r)}</blockquote> }.into_any(),
        Node::List(items) => view! {
            <ul>{items.into_iter().map(|i| view! { <li>{nodes(i, r)}</li> }).collect_view()}</ul>
        }
        .into_any(),
        Node::LineBreak => view! { <br /> }.into_any(),
        Node::Mention { target, text } => mention(target, text, r),
    }
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
