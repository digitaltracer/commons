//! Restricted Markdown (decision 15), rendered to a node tree with no raw-HTML variant.
//! Clients turn these nodes into text nodes and a fixed set of elements; HTML in the source
//! comes out as literal text.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

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

    #[test]
    fn basic_formatting() {
        let nodes = render("**b** _i_ `c`");
        assert!(matches!(&nodes[0], Node::Paragraph(c) if matches!(c[0], Node::Strong(_))));
        assert_eq!(plain_text("**b** _i_ `c`"), "b i c");
    }
}
