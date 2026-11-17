//! Renders restricted Markdown as text nodes and a fixed set of elements. Never raw HTML.
use leptos::prelude::*;
use proto::markdown::{Node, render};

pub fn body(source: &str) -> AnyView {
    nodes(render(source))
}

fn nodes(list: Vec<Node>) -> AnyView {
    list.into_iter().map(node).collect_view().into_any()
}

fn node(n: Node) -> AnyView {
    match n {
        Node::Text(t) => t.into_any(),
        Node::Code(t) => view! { <code>{t}</code> }.into_any(),
        Node::Strong(c) => view! { <strong>{nodes(c)}</strong> }.into_any(),
        Node::Emphasis(c) => view! { <em>{nodes(c)}</em> }.into_any(),
        Node::Strike(c) => view! { <s>{nodes(c)}</s> }.into_any(),
        Node::Link { href, children } => {
            view! { <a href=href target="_blank" rel="noopener noreferrer">{nodes(children)}</a> }
                .into_any()
        }
        Node::Paragraph(c) => view! { <p>{nodes(c)}</p> }.into_any(),
        Node::CodeBlock(t) => view! { <pre><code>{t}</code></pre> }.into_any(),
        Node::Quote(c) => view! { <blockquote>{nodes(c)}</blockquote> }.into_any(),
        Node::List(items) => view! {
            <ul>{items.into_iter().map(|i| view! { <li>{nodes(i)}</li> }).collect_view()}</ul>
        }
        .into_any(),
        Node::LineBreak => view! { <br /> }.into_any(),
    }
}
