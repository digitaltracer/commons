//! Restricted Markdown (decision 15), rendered to a node tree with no raw-HTML variant.
//! Clients turn these nodes into text nodes and a fixed set of elements; HTML in the source
//! comes out as literal text.
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::Entity;

mod highlight;
pub use highlight::{Style, highlight};

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
    /// `lang` is the first word of the fence's info string, when it looks like a language.
    CodeBlock {
        lang: Option<String>,
        text: String,
    },
    Quote(Vec<Node>),
    /// `start` is the first number of a numbered list; `None` for bullets.
    List {
        start: Option<u64>,
        items: Vec<Vec<Node>>,
    },
    LineBreak,
    /// Empty lines typed between two top-level blocks, at most [`MAX_GAP`].
    Gap(u8),
    /// A mention or channel link from the message's entities; `text` is the source text.
    Mention {
        target: Target,
        text: String,
    },
}

const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';

/// Most empty lines kept between two blocks; longer runs shrink to this.
pub const MAX_GAP: u8 = 3;

/// Renders `source` with its mention and channel-link entities as atomic [`Node::Mention`]
/// spans, so Markdown inside a name is never interpreted. Entities that are out of range,
/// overlap, or split a character are ignored and their text renders as usual.
pub fn render_with_entities(source: &str, entities: &[Entity]) -> Vec<Node> {
    let placed = place(source, entities);
    if placed.spans.is_empty() {
        return render(source);
    }
    let originals: Vec<(String, Target)> = placed
        .spans
        .iter()
        .map(|p| (source[p.src.clone()].to_string(), p.target))
        .collect();
    restore(render(&placed.text), &originals)
}

/// One entity swapped for a placeholder: where it is in the source and in the placed text.
struct PlacedSpan {
    src: std::ops::Range<usize>,
    text: std::ops::Range<usize>,
    target: Target,
}

/// The source with each usable entity swapped for a private-use placeholder, which the
/// parser treats as plain text.
struct Placed {
    text: String,
    spans: Vec<PlacedSpan>,
}

fn place(source: &str, entities: &[Entity]) -> Placed {
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
    // U+FFFD has the same UTF-8 length as the markers, so offsets outside spans keep their shift.
    let clean = |t: &str| t.replace([OPEN, CLOSE], "\u{FFFD}");
    let mut text = String::new();
    let mut placed = Vec::with_capacity(kept.len());
    let mut last = 0;
    for (i, (s, e, target)) in kept.into_iter().enumerate() {
        text.push_str(&clean(&source[last..s]));
        let at = text.len();
        text.push(OPEN);
        text.push_str(&i.to_string());
        text.push(CLOSE);
        placed.push(PlacedSpan {
            src: s..e,
            text: at..text.len(),
            target,
        });
        last = e;
    }
    text.push_str(&clean(&source[last..]));
    Placed {
        text,
        spans: placed,
    }
}

fn options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts
}

fn restore(nodes: Vec<Node>, spans: &[(String, Target)]) -> Vec<Node> {
    let mut out = vec![];
    for n in nodes {
        match n {
            Node::Text(t) => split_text(&t, spans, &mut out),
            Node::Code(t) => out.push(Node::Code(unplace(&t, spans))),
            Node::CodeBlock { lang, text } => out.push(Node::CodeBlock {
                lang,
                text: unplace(&text, spans),
            }),
            Node::Strong(c) => out.push(Node::Strong(restore(c, spans))),
            Node::Emphasis(c) => out.push(Node::Emphasis(restore(c, spans))),
            Node::Strike(c) => out.push(Node::Strike(restore(c, spans))),
            Node::Quote(c) => out.push(Node::Quote(restore(c, spans))),
            Node::Paragraph(c) => out.push(Node::Paragraph(restore(c, spans))),
            Node::Link { href, children } => out.push(Node::Link {
                href,
                children: restore(children, spans),
            }),
            Node::List { start, items } => out.push(Node::List {
                start,
                items: items.into_iter().map(|i| restore(i, spans)).collect(),
            }),
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
    let opts = options();
    let mut stack: Vec<(Option<Frame>, Vec<Node>)> = vec![(None, Vec::new())];
    let mut lists: Vec<(Option<u64>, Vec<Vec<Node>>)> = Vec::new();
    let mut code_block: Option<(Option<String>, String)> = None;
    // Tag nesting depth, and where the last top-level block ended, for the gaps between blocks.
    let mut depth = 0usize;
    let mut last_block_end: Option<usize> = None;

    for (event, range) in Parser::new_ext(source, opts).into_offset_iter() {
        let top_level_start = depth == 0 && matches!(event, Event::Start(_) | Event::Rule);
        if top_level_start {
            if let Some(end) = last_block_end {
                let lines = blank_lines_between(source, end, range.start);
                if lines > 0 {
                    push(&mut stack, Node::Gap(lines.min(MAX_GAP as usize) as u8));
                }
            }
            if matches!(event, Event::Rule) {
                last_block_end = Some(range.end);
            }
        }
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    last_block_end = Some(range.end);
                }
            }
            _ => {}
        }
        if let Some((_, buf)) = code_block.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, text) = code_block.take().unwrap_or_default();
                    push(&mut stack, Node::CodeBlock { lang, text });
                }
                Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => buf.push_str(&t),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => code_lang(&info),
                    CodeBlockKind::Indented => None,
                };
                code_block = Some((lang, String::new()));
            }
            Event::Start(Tag::List(start)) => lists.push((start, Vec::new())),
            Event::End(TagEnd::List(_)) => {
                let (start, items) = lists.pop().unwrap_or_default();
                push(&mut stack, Node::List { start, items });
            }
            Event::Start(tag) => {
                let frame = match tag {
                    Tag::Paragraph => Frame::Paragraph,
                    Tag::Heading { .. } => Frame::Heading,
                    Tag::HtmlBlock => Frame::HtmlBlock,
                    Tag::Strong => Frame::Strong,
                    Tag::Emphasis => Frame::Emphasis,
                    Tag::Strikethrough => Frame::Strike,
                    Tag::BlockQuote(_) => Frame::Quote,
                    Tag::Item => Frame::Item,
                    Tag::Link { dest_url, .. } => Frame::Link(dest_url.to_string()),
                    // Images flatten into their alt text.
                    _ => Frame::Transparent,
                };
                stack.push((Some(frame), Vec::new()));
            }
            Event::End(_) => {
                let Some((Some(frame), mut children)) = stack.pop() else {
                    continue;
                };
                match frame {
                    Frame::Paragraph => push(&mut stack, Node::Paragraph(children)),
                    // A heading is a bold line of its own, never merged into its neighbours.
                    Frame::Heading => {
                        push(&mut stack, Node::Paragraph(vec![Node::Strong(children)]))
                    }
                    // Raw HTML stays literal text, one line per source line.
                    Frame::HtmlBlock => {
                        if let Some(Node::Text(t)) = children.last_mut() {
                            t.truncate(t.trim_end_matches(['\n', '\r']).len());
                        }
                        push(&mut stack, Node::Paragraph(children))
                    }
                    Frame::Strong => push(&mut stack, Node::Strong(children)),
                    Frame::Emphasis => push(&mut stack, Node::Emphasis(children)),
                    Frame::Strike => push(&mut stack, Node::Strike(children)),
                    Frame::Quote => push(&mut stack, Node::Quote(children)),
                    Frame::Item => match lists.last_mut() {
                        Some((_, items)) => items.push(children),
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
    linkify(stack.pop().map(|(_, n)| n).unwrap_or_default())
}

enum Frame {
    Paragraph,
    Heading,
    HtmlBlock,
    Strong,
    Emphasis,
    Strike,
    Quote,
    Item,
    Link(String),
    Transparent,
}

/// Empty lines strictly between the block ending at `end` and the one starting at `start`.
fn blank_lines_between(source: &str, end: usize, start: usize) -> usize {
    let (Some(before), Some(between)) = (source.get(..end), source.get(end..start)) else {
        return 0;
    };
    // The previous block's range may or may not include its own line ending.
    let trailing = before.len() - before.trim_end_matches(['\n', '\r']).len();
    let newlines =
        before[before.len() - trailing..].matches('\n').count() + between.matches('\n').count();
    newlines.saturating_sub(1)
}

/// The fence's language, when its first word looks like one (`rust`, `c++`, `objective-c`).
fn code_lang(info: &str) -> Option<String> {
    let word = info.split_whitespace().next()?;
    let ok = word.len() <= 24
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+#._-".contains(c));
    ok.then(|| word.to_ascii_lowercase())
}

/// Turns bare `http(s)://` URLs in text into links. Code and existing links are left alone.
fn linkify(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(nodes.len());
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Node>| {
        if !text.is_empty() {
            split_urls(&std::mem::take(text), out);
        }
    };
    for n in nodes {
        let n = match n {
            Node::Text(t) => {
                text.push_str(&t);
                continue;
            }
            Node::Strong(c) => Node::Strong(linkify(c)),
            Node::Emphasis(c) => Node::Emphasis(linkify(c)),
            Node::Strike(c) => Node::Strike(linkify(c)),
            Node::Paragraph(c) => Node::Paragraph(linkify(c)),
            Node::Quote(c) => Node::Quote(linkify(c)),
            Node::List { start, items } => Node::List {
                start,
                items: items.into_iter().map(linkify).collect(),
            },
            other => other,
        };
        flush(&mut text, &mut out);
        out.push(n);
    }
    flush(&mut text, &mut out);
    out
}

fn split_urls(t: &str, out: &mut Vec<Node>) {
    let mut last = 0;
    for r in url_ranges(t) {
        if r.start > last {
            out.push(Node::Text(t[last..r.start].to_string()));
        }
        out.push(Node::Link {
            href: t[r.clone()].to_string(),
            children: vec![Node::Text(t[r.clone()].to_string())],
        });
        last = r.end;
    }
    if last < t.len() {
        out.push(Node::Text(t[last..].to_string()));
    }
}

/// Where the bare `http(s)://` URLs in `t` are.
fn url_ranges(t: &str) -> Vec<std::ops::Range<usize>> {
    let lower = t.to_ascii_lowercase();
    let mut found = vec![];
    let mut from = 0;
    while let Some(s) = ["https://", "http://"]
        .iter()
        .filter_map(|p| lower[from..].find(p).map(|i| i + from))
        .min()
    {
        from = s + 1;
        if t[..s]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let stop = t[s..]
            .find(|c: char| {
                c.is_whitespace()
                    || c.is_control()
                    || "<>\"`".contains(c)
                    || c == OPEN
                    || c == CLOSE
            })
            .map_or(t.len(), |i| i + s);
        let e = s + url_len(&t[s..stop]);
        if t[s..e]
            .split_once("://")
            .is_none_or(|(_, rest)| rest.is_empty())
        {
            continue;
        }
        found.push(s..e);
        from = e;
    }
    found
}

/// Length of `candidate` without trailing punctuation, keeping a `)` or `]` that closes one
/// opened inside the URL.
fn url_len(candidate: &str) -> usize {
    let mut u = candidate;
    loop {
        let Some(c) = u.chars().next_back() else {
            return 0;
        };
        let unbalanced = |open: char, close: char| {
            c == close && u.matches(open).count() < u.matches(close).count()
        };
        if ".,;:!?'*_".contains(c) || unbalanced('(', ')') || unbalanced('[', ']') {
            u = &u[..u.len() - c.len_utf8()];
        } else {
            return u.len();
        }
    }
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
                Node::Text(t) | Node::Code(t) | Node::CodeBlock { text: t, .. } => out.push_str(t),
                Node::Mention { text, .. } => out.push_str(text),
                Node::LineBreak => out.push(' '),
                Node::Gap(_) => {}
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
                Node::List { items, .. } => {
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
                    Node::Text(t) | Node::Code(t) | Node::CodeBlock { text: t, .. } => {
                        out.push(t.clone())
                    }
                    Node::Mention { text, .. } => out.push(format!("[{text}]")),
                    Node::Strong(c)
                    | Node::Emphasis(c)
                    | Node::Strike(c)
                    | Node::Quote(c)
                    | Node::Paragraph(c)
                    | Node::Link { children: c, .. } => go(c, out),
                    Node::List { items, .. } => items.iter().for_each(|i| go(i, out)),
                    Node::LineBreak | Node::Gap(_) => {}
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

    fn p(text: &str) -> Node {
        Node::Paragraph(vec![Node::Text(text.into())])
    }

    #[test]
    fn code_blocks_keep_lines_and_language() {
        let nodes = render("Repro:\n```Rust title\nfn main() {\n    let x = 1;\n}\n```\nafter");
        assert_eq!(
            nodes[1],
            Node::CodeBlock {
                lang: Some("rust".into()),
                text: "fn main() {\n    let x = 1;\n}\n".into()
            }
        );
        let odd = render("```<b>\nx\n```");
        assert!(
            matches!(&odd[0], Node::CodeBlock { lang: None, .. }),
            "{odd:?}"
        );
    }

    #[test]
    fn headings_are_bold_lines_of_their_own() {
        let nodes = render("# One\n# Two\nAfter");
        assert_eq!(
            nodes,
            vec![
                Node::Paragraph(vec![Node::Strong(vec![Node::Text("One".into())])]),
                Node::Paragraph(vec![Node::Strong(vec![Node::Text("Two".into())])]),
                p("After"),
            ]
        );
    }

    #[test]
    fn html_blocks_keep_their_lines_as_text() {
        let nodes = render("<config>\n  <a>1</a>\n</config>");
        assert_eq!(nodes, vec![p("<config>\n  <a>1</a>\n</config>")]);
    }

    #[test]
    fn numbered_lists_keep_their_start() {
        let nodes = render("Steps:\n1. pull\n2. run");
        assert!(
            matches!(&nodes[1], Node::List { start: Some(1), items } if items.len() == 2),
            "{nodes:?}"
        );
        let later = render("3. pull\n4. run");
        assert!(
            matches!(&later[0], Node::List { start: Some(3), .. }),
            "{later:?}"
        );
        let bullets = render("- a\n- b");
        assert!(matches!(&bullets[0], Node::List { start: None, .. }));
    }

    #[test]
    fn blank_lines_between_blocks_are_kept() {
        assert_eq!(
            render("a\nb")[0],
            Node::Paragraph(vec![
                Node::Text("a".into()),
                Node::LineBreak,
                Node::Text("b".into())
            ])
        );
        assert_eq!(render("a\n\nb"), vec![p("a"), Node::Gap(1), p("b")]);
        assert_eq!(render("a\n\n\n\nb"), vec![p("a"), Node::Gap(3), p("b")]);
        assert_eq!(render("a\n\n\n\n\n\n\nb")[1], Node::Gap(MAX_GAP));
        assert_eq!(render("a\r\n\r\nb"), vec![p("a"), Node::Gap(1), p("b")]);
        // A list right under a line, or a code block, gets no gap.
        assert!(
            !render("Steps:\n- a\n```\nx\n```")
                .iter()
                .any(|n| matches!(n, Node::Gap(_)))
        );
        // Blank lines inside a quote or list are not top-level gaps.
        assert!(
            !render("> a\n>\n> b")
                .iter()
                .any(|n| matches!(n, Node::Gap(_)))
        );
    }

    fn links(nodes: &[Node]) -> Vec<String> {
        let mut out = vec![];
        fn go(nodes: &[Node], out: &mut Vec<String>) {
            for n in nodes {
                match n {
                    Node::Link { href, .. } => out.push(href.clone()),
                    Node::Strong(c) | Node::Emphasis(c) | Node::Paragraph(c) | Node::Quote(c) => {
                        go(c, out)
                    }
                    Node::List { items, .. } => items.iter().for_each(|i| go(i, out)),
                    _ => {}
                }
            }
        }
        go(nodes, &mut out);
        out
    }

    #[test]
    fn bare_urls_become_links() {
        let nodes = render(
            "See https://example.org/a_b_c. (and http://x.dev/p(1)) or **https://b.io/x**, \
             not xhttps://no, `https://code.dev` or [named](https://n.dev) or https://",
        );
        assert_eq!(
            links(&nodes),
            vec![
                "https://example.org/a_b_c",
                "http://x.dev/p(1)",
                "https://b.io/x",
                "https://n.dev"
            ]
        );
        assert_eq!(
            plain_text("go to https://example.org."),
            "go to https://example.org."
        );
    }

    #[test]
    fn bare_urls_stop_at_mentions() {
        let src = "https://example.org/@Ann";
        let nodes = render_with_entities(src, &[mention(1, 20, 24)]);
        assert_eq!(links(&nodes), vec!["https://example.org/"]);
        assert_eq!(texts(&nodes).join(""), "https://example.org/[@Ann]");
    }

    #[test]
    fn basic_formatting() {
        let nodes = render("**b** _i_ `c`");
        assert!(matches!(&nodes[0], Node::Paragraph(c) if matches!(c[0], Node::Strong(_))));
        assert_eq!(plain_text("**b** _i_ `c`"), "b i c");
    }
}
