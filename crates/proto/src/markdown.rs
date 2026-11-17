//! Restricted Markdown (decision 15), rendered to a node tree with no raw-HTML variant.
//! Clients turn these nodes into text nodes and a fixed set of elements; HTML in the source
//! comes out as literal text.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::Entity;

/// What a mention span points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    User(Uuid),
    Channel(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    Text(String),
    Code(String),
    Strong(Vec<Node>),
    Emphasis(Vec<Node>),
    Strike(Vec<Node>),
    /// Only `http`, `https` and `mailto` targets survive; others render as their text.
    Link {
        href: String,
        children: Vec<Node>,
    },
    Paragraph(Vec<Node>),
    CodeBlock(String),
    Quote(Vec<Node>),
    List(Vec<Vec<Node>>),
    LineBreak,
    /// A mention or channel link from the message's entities; `text` is the source text.
    Mention {
        target: Target,
        text: String,
    },
}

const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

/// Renders `source` with its mention and channel-link entities as atomic [`Node::Mention`]
/// spans, so Markdown inside a name is never interpreted. Entities that are out of range,
/// overlap, or split a character are ignored and their text renders as usual.
pub fn render_with_entities(source: &str, entities: &[Entity]) -> Vec<Node> {
    let mut spans: Vec<(usize, usize, Target)> = entities
        .iter()
        .filter_map(|e| match e {
            Entity::Mention {
                user_id,
                start,
                end,
            } => Some((*start as usize, *end as usize, Target::User(*user_id))),
            Entity::ChannelLink {
                conversation_id,
                start,
                end,
            } => Some((
                *start as usize,
                *end as usize,
                Target::Channel(*conversation_id),
            )),
            Entity::Emoji { .. } => None,
        })
        .filter(|(s, e, _)| s < e && source.get(*s..*e).is_some())
        .collect();
    spans.sort_by_key(|(s, _, _)| *s);
    let mut kept: Vec<(usize, usize, Target)> = vec![];
    for sp in spans {
        if kept.last().is_none_or(|(_, e, _)| sp.0 >= *e) {
            kept.push(sp);
        }
    }
    if kept.is_empty() {
        return render(source);
    }
    // Swap each span for a private-use placeholder, render, then swap them back.
    let clean = |t: &str| t.replace([OPEN, CLOSE], "\u{FFFD}");
    let mut text = String::new();
    let mut last = 0;
    for (i, (s, e, _)) in kept.iter().enumerate() {
        text.push_str(&clean(&source[last..*s]));
        text.push(OPEN);
        text.push_str(&i.to_string());
        text.push(CLOSE);
        last = *e;
    }
    text.push_str(&clean(&source[last..]));
    let originals: Vec<(String, Target)> = kept
        .iter()
        .map(|(s, e, t)| (source[*s..*e].to_string(), *t))
        .collect();
    restore(render(&text), &originals)
}

fn restore(nodes: Vec<Node>, spans: &[(String, Target)]) -> Vec<Node> {
    let mut out = vec![];
    for n in nodes {
        match n {
            Node::Text(t) => split_text(&t, spans, &mut out),
            Node::Code(t) => out.push(Node::Code(unplace(&t, spans))),
            Node::CodeBlock(t) => out.push(Node::CodeBlock(unplace(&t, spans))),
            Node::Strong(c) => out.push(Node::Strong(restore(c, spans))),
            Node::Emphasis(c) => out.push(Node::Emphasis(restore(c, spans))),
            Node::Strike(c) => out.push(Node::Strike(restore(c, spans))),
            Node::Quote(c) => out.push(Node::Quote(restore(c, spans))),
            Node::Paragraph(c) => out.push(Node::Paragraph(restore(c, spans))),
            Node::Link { href, children } => out.push(Node::Link {
                href,
                children: restore(children, spans),
            }),
            Node::List(items) => out.push(Node::List(
                items.into_iter().map(|i| restore(i, spans)).collect(),
            )),
            other => out.push(other),
        }
    }
    out
}

/// Each placeholder in `t`, as (byte range, span index).
fn placeholders(t: &str) -> Vec<(usize, usize, usize)> {
    let mut found = vec![];
    let mut rest = 0;
    while let Some(o) = t[rest..].find(OPEN).map(|i| i + rest) {
        let digits = o + OPEN.len_utf8();
        let Some(c) = t[digits..].find(CLOSE).map(|i| i + digits) else {
            break;
        };
        if let Ok(i) = t[digits..c].parse::<usize>() {
            found.push((o, c + CLOSE.len_utf8(), i));
        }
        rest = c + CLOSE.len_utf8();
    }
    found
}

fn split_text(t: &str, spans: &[(String, Target)], out: &mut Vec<Node>) {
    let mut last = 0;
    for (s, e, i) in placeholders(t) {
        let Some((text, target)) = spans.get(i) else {
            continue;
        };
        if s > last {
            out.push(Node::Text(t[last..s].to_string()));
        }
        out.push(Node::Mention {
            target: *target,
            text: text.clone(),
        });
        last = e;
    }
    if last < t.len() {
        out.push(Node::Text(t[last..].to_string()));
    }
}

fn unplace(t: &str, spans: &[(String, Target)]) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (s, e, i) in placeholders(t) {
        out.push_str(&t[last..s]);
        out.push_str(spans.get(i).map_or("", |(text, _)| text));
        last = e;
    }
    out.push_str(&t[last..]);
    out
}

pub fn render(source: &str) -> Vec<Node> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut stack: Vec<(Option<Frame>, Vec<Node>)> = vec![(None, Vec::new())];
    let mut list_items: Vec<Vec<Vec<Node>>> = Vec::new();
    let mut code_block: Option<String> = None;

    for event in Parser::new_ext(source, opts) {
        if let Some(buf) = code_block.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let text = code_block.take().unwrap_or_default();
                    push(&mut stack, Node::CodeBlock(text));
                }
                Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => buf.push_str(&t),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(_)) => code_block = Some(String::new()),
            Event::Start(Tag::List(_)) => list_items.push(Vec::new()),
            Event::End(TagEnd::List(_)) => {
                let items = list_items.pop().unwrap_or_default();
                push(&mut stack, Node::List(items));
            }
            Event::Start(tag) => {
                let frame = match tag {
                    Tag::Paragraph => Frame::Paragraph,
                    Tag::Strong => Frame::Strong,
                    Tag::Emphasis => Frame::Emphasis,
                    Tag::Strikethrough => Frame::Strike,
                    Tag::BlockQuote(_) => Frame::Quote,
                    Tag::Item => Frame::Item,
                    Tag::Link { dest_url, .. } => Frame::Link(dest_url.to_string()),
                    // Headings, tables, images and HTML blocks flatten into their text.
                    _ => Frame::Transparent,
                };
                stack.push((Some(frame), Vec::new()));
            }
            Event::End(_) => {
                let Some((Some(frame), children)) = stack.pop() else {
                    continue;
                };
                match frame {
                    Frame::Paragraph => push(&mut stack, Node::Paragraph(children)),
                    Frame::Strong => push(&mut stack, Node::Strong(children)),
                    Frame::Emphasis => push(&mut stack, Node::Emphasis(children)),
                    Frame::Strike => push(&mut stack, Node::Strike(children)),
                    Frame::Quote => push(&mut stack, Node::Quote(children)),
                    Frame::Item => match list_items.last_mut() {
                        Some(items) => items.push(children),
                        None => extend(&mut stack, children),
                    },
                    Frame::Link(href) if safe_href(&href) => {
                        push(&mut stack, Node::Link { href, children })
                    }
                    Frame::Link(_) | Frame::Transparent => extend(&mut stack, children),
                }
            }
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                push(&mut stack, Node::Text(t.to_string()))
            }
            Event::Code(t) => push(&mut stack, Node::Code(t.to_string())),
            Event::SoftBreak | Event::HardBreak => push(&mut stack, Node::LineBreak),
            _ => {}
        }
    }
    while stack.len() > 1 {
        let (_, children) = stack.pop().unwrap_or_default();
        extend(&mut stack, children);
    }
    stack.pop().map(|(_, n)| n).unwrap_or_default()
}

enum Frame {
    Paragraph,
    Strong,
    Emphasis,
    Strike,
    Quote,
    Item,
    Link(String),
    Transparent,
}

fn push(stack: &mut [(Option<Frame>, Vec<Node>)], node: Node) {
    if let Some((_, children)) = stack.last_mut() {
        children.push(node);
    }
}

fn extend(stack: &mut [(Option<Frame>, Vec<Node>)], nodes: Vec<Node>) {
    if let Some((_, children)) = stack.last_mut() {
        children.extend(nodes);
    }
}

fn safe_href(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// Plain text of a body, for previews and notifications.
pub fn plain_text(source: &str) -> String {
    fn walk(nodes: &[Node], out: &mut String) {
        for n in nodes {
            match n {
                Node::Text(t) | Node::Code(t) | Node::CodeBlock(t) => out.push_str(t),
                Node::Mention { text, .. } => out.push_str(text),
                Node::LineBreak => out.push(' '),
                Node::Strong(c)
                | Node::Emphasis(c)
                | Node::Strike(c)
                | Node::Quote(c)
                | Node::Paragraph(c)
                | Node::Link { children: c, .. } => {
                    walk(c, out);
                    if matches!(n, Node::Paragraph(_)) {
                        out.push(' ');
                    }
                }
                Node::List(items) => {
                    for i in items {
                        walk(i, out);
                        out.push(' ');
                    }
                }
            }
        }
    }
    let mut out = String::new();
    walk(&render(source), &mut out);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(nodes: &[Node]) -> Vec<String> {
        let mut out = Vec::new();
        fn go(nodes: &[Node], out: &mut Vec<String>) {
            for n in nodes {
                match n {
                    Node::Text(t) | Node::Code(t) | Node::CodeBlock(t) => out.push(t.clone()),
                    Node::Mention { text, .. } => out.push(format!("[{text}]")),
                    Node::Strong(c)
                    | Node::Emphasis(c)
                    | Node::Strike(c)
                    | Node::Quote(c)
                    | Node::Paragraph(c)
                    | Node::Link { children: c, .. } => go(c, out),
                    Node::List(items) => items.iter().for_each(|i| go(i, out)),
                    Node::LineBreak => {}
                }
            }
        }
        go(nodes, &mut out);
        out
    }

    #[test]
    fn script_tag_renders_as_text() {
        let nodes = render("hello <script>alert(1)</script> world");
        let joined = texts(&nodes).join("");
        assert!(joined.contains("<script>alert(1)</script>"), "{nodes:?}");
    }

    #[test]
    fn html_block_renders_as_text() {
        let nodes = render("<div onclick=\"x()\">hi</div>\n\n<img src=x onerror=alert(1)>");
        let joined = texts(&nodes).join("");
        assert!(joined.contains("<div onclick=\"x()\">"));
        assert!(joined.contains("<img src=x onerror=alert(1)>"));
    }

    #[test]
    fn javascript_links_are_dropped() {
        let nodes = render("[click](javascript:alert(1)) [ok](https://example.org)");
        let Node::Paragraph(children) = &nodes[0] else {
            panic!("{nodes:?}")
        };
        let links: Vec<_> = children
            .iter()
            .filter_map(|n| match n {
                Node::Link { href, .. } => Some(href.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(links, vec!["https://example.org".to_string()]);
    }

    fn mention(user: u128, start: u32, end: u32) -> Entity {
        Entity::Mention {
            user_id: Uuid::from_u128(user),
            start,
            end,
        }
    }

    #[test]
    fn mentions_are_atomic_spans() {
        let src = "hi @Ann_Lee_ and **@Bo**, `@Ann_Lee_` stays code";
        let nodes = render_with_entities(
            src,
            &[mention(1, 3, 12), mention(2, 19, 22), mention(1, 27, 36)],
        );
        assert_eq!(
            texts(&nodes).join(""),
            "hi [@Ann_Lee_] and [@Bo], @Ann_Lee_ stays code"
        );
        let Node::Paragraph(p) = &nodes[0] else {
            panic!("{nodes:?}")
        };
        assert_eq!(
            p[1],
            Node::Mention {
                target: Target::User(Uuid::from_u128(1)),
                text: "@Ann_Lee_".into()
            }
        );
        assert!(matches!(&p[3], Node::Strong(c) if matches!(c[0], Node::Mention { .. })));
    }

    #[test]
    fn bad_entities_are_ignored() {
        let src = "é @x";
        // Out of range, inside a character, and overlapping.
        let nodes = render_with_entities(
            src,
            &[
                mention(1, 3, 99),
                mention(1, 1, 3),
                mention(2, 3, 5),
                mention(3, 4, 5),
            ],
        );
        assert_eq!(texts(&nodes).join(""), "é [@x]");
        let raw = render_with_entities("a \u{E000}0\u{E001} b", &[mention(1, 0, 1)]);
        assert_eq!(
            texts(&raw).join(""),
            "[a] \u{FFFD}0\u{FFFD} b",
            "stray markers are neutralised"
        );
    }

    #[test]
    fn basic_formatting() {
        let nodes = render("**b** _i_ `c`");
        assert!(matches!(&nodes[0], Node::Paragraph(c) if matches!(c[0], Node::Strong(_))));
        assert_eq!(plain_text("**b** _i_ `c`"), "b i c");
    }
}
