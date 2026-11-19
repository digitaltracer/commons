//! Byte ranges of a draft's Markdown, for the composer's highlight layer. Built on the same
//! parser, options and entity placement as [`super::render_with_entities`], so the composer
//! shows what the renderer will produce.
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag};

use super::{Placed, Target, place, safe_href, url_ranges};
use crate::model::Entity;

/// How a range of the draft looks. Ranges nest: a marker sits inside the strong text it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Style {
    /// Syntax the renderer hides: emphasis marks, backticks, fences, `>`, list markers,
    /// heading `#`s, link brackets and targets.
    Marker,
    Strong,
    Emphasis,
    Strike,
    /// Inline code, without its backticks.
    Code,
    /// A whole code block, fences included, without its trailing line break.
    CodeBlock,
    /// Link text, and bare URLs the renderer turns into links.
    Link,
    Mention,
    Channel,
}

struct Open {
    tag: Tag<'static>,
    range: Range<usize>,
    inner: Option<Range<usize>>,
}

/// The styled ranges of `source`, in source byte offsets, sorted by start. `entities` are the
/// mentions and channel links the draft carries; they are placed exactly as the renderer
/// places them, and nothing is styled inside code.
pub fn highlight(source: &str, entities: &[Entity]) -> Vec<(Range<usize>, Style)> {
    let placed = place(source, entities);
    let text = if placed.spans.is_empty() {
        source
    } else {
        placed.text.as_str()
    };
    let mut out: Vec<(Range<usize>, Style)> = vec![];
    let mut stack: Vec<Open> = vec![];
    // Code blocks and links: their text is never linkified or tinted.
    let mut in_code = 0usize;
    let mut in_link = 0usize;
    // Adjacent text events, scanned together for bare URLs like the renderer's linkify.
    let mut run: Option<Range<usize>> = None;
    let flush = |run: &mut Option<Range<usize>>, out: &mut Vec<(Range<usize>, Style)>| {
        if let Some(r) = run.take() {
            for u in url_ranges(&text[r.clone()]) {
                out.push((r.start + u.start..r.start + u.end, Style::Link));
            }
        }
    };

    for (event, range) in Parser::new_ext(text, super::options()).into_offset_iter() {
        if !matches!(event, Event::End(_))
            && let Some(top) = stack.last_mut()
        {
            let inner = top.inner.get_or_insert(range.start..range.start);
            inner.end = inner.end.max(range.end);
        }
        let is_text = matches!(
            event,
            Event::Text(_) | Event::Html(_) | Event::InlineHtml(_)
        );
        if !is_text {
            flush(&mut run, &mut out);
        }
        match event {
            Event::Start(tag) => {
                match &tag {
                    Tag::CodeBlock(_) => in_code += 1,
                    Tag::Link { .. } | Tag::Image { .. } => in_link += 1,
                    _ => {}
                }
                stack.push(Open {
                    tag: tag.into_static(),
                    range,
                    inner: None,
                });
            }
            Event::End(_) => {
                let Some(open) = stack.pop() else { continue };
                match &open.tag {
                    Tag::CodeBlock(_) => in_code = in_code.saturating_sub(1),
                    Tag::Link { .. } | Tag::Image { .. } => in_link = in_link.saturating_sub(1),
                    _ => {}
                }
                closed(text, open, &mut out);
            }
            Event::Text(_) | Event::Html(_) | Event::InlineHtml(_) => {
                if in_code > 0 {
                    continue;
                }
                for p in &placed.spans {
                    if p.text.start >= range.start && p.text.end <= range.end {
                        let style = match p.target {
                            Target::User(_) => Style::Mention,
                            Target::Channel(_) => Style::Channel,
                        };
                        out.push((p.text.clone(), style));
                    }
                }
                if in_link > 0 {
                    continue;
                }
                match run.as_mut() {
                    Some(r) if r.end == range.start => r.end = range.end,
                    _ => {
                        flush(&mut run, &mut out);
                        run = Some(range);
                    }
                }
            }
            Event::Code(_) => {
                let ticks = text[range.clone()]
                    .bytes()
                    .take_while(|b| *b == b'`')
                    .count();
                let inner = range.start + ticks..range.end.saturating_sub(ticks);
                if inner.start <= inner.end {
                    out.push((range.start..inner.start, Style::Marker));
                    out.push((inner.end..range.end, Style::Marker));
                    out.push((inner, Style::Code));
                }
            }
            // The renderer drops rules, and a hard break's backslash or spaces.
            Event::Rule | Event::HardBreak => out.push((range, Style::Marker)),
            _ => {}
        }
    }
    flush(&mut run, &mut out);

    let mut mapped: Vec<(Range<usize>, Style)> = out
        .into_iter()
        .map(|(r, s)| {
            (
                to_source(&placed, r.start, false)..to_source(&placed, r.end, true),
                s,
            )
        })
        .filter(|(r, _)| r.start < r.end)
        .collect();
    mapped.sort_by_key(|(r, _)| (r.start, r.end));
    mapped.dedup();
    mapped
}

/// Styles for a tag that just closed.
fn closed(text: &str, open: Open, out: &mut Vec<(Range<usize>, Style)>) {
    let Open { tag, range, inner } = open;
    let around = |out: &mut Vec<(Range<usize>, Style)>, style: Option<Style>| match &inner {
        Some(i) => {
            out.push((range.start..i.start, Style::Marker));
            out.push((i.end..range.end, Style::Marker));
            if let Some(s) = style {
                out.push((i.clone(), s));
            }
        }
        None => out.push((range.clone(), Style::Marker)),
    };
    match tag {
        Tag::Strong => around(out, Some(Style::Strong)),
        Tag::Emphasis => around(out, Some(Style::Emphasis)),
        Tag::Strikethrough => around(out, Some(Style::Strike)),
        // Unsafe targets render as their text alone; images flatten into their alt text.
        Tag::Link { dest_url, .. } => {
            around(out, safe_href(&dest_url).then_some(Style::Link));
        }
        Tag::Image { .. } => around(out, None),
        Tag::Heading { .. } => {
            if let Some(i) = &inner {
                out.push((range.start..i.start, Style::Marker));
                out.push((i.end..range.end, Style::Marker));
                out.push((i.clone(), Style::Strong));
            }
        }
        Tag::BlockQuote(_) => {
            let mut at = range.start;
            for line in text[range.clone()].split_inclusive('\n') {
                let prefix = line
                    .bytes()
                    .take_while(|b| matches!(b, b' ' | b'\t' | b'>'))
                    .count();
                for (i, b) in line.as_bytes()[..prefix].iter().enumerate() {
                    if *b == b'>' {
                        out.push((at + i..at + i + 1, Style::Marker));
                    }
                }
                at += line.len();
            }
        }
        Tag::Item => {
            let t = &text[range.clone()];
            let digits = t.bytes().take_while(u8::is_ascii_digit).count();
            let len = match t.as_bytes().get(digits) {
                Some(b'.' | b')') if digits > 0 => digits + 1,
                Some(b'-' | b'*' | b'+') if digits == 0 => 1,
                _ => 0,
            };
            if len > 0 {
                out.push((range.start..range.start + len, Style::Marker));
            }
        }
        Tag::CodeBlock(kind) => {
            let body = text[range.clone()].trim_end_matches(['\n', '\r']);
            let end = range.start + body.len();
            out.push((range.start..end, Style::CodeBlock));
            if let CodeBlockKind::Fenced(_) = kind {
                let first = body.find('\n').map_or(end, |i| range.start + i);
                out.push((range.start..first, Style::Marker));
                if let Some(nl) = body.rfind('\n') {
                    let line = &body[nl + 1..];
                    let lead = line
                        .bytes()
                        .take_while(|b| matches!(b, b' ' | b'>'))
                        .count();
                    let fence = line[lead..].trim_end();
                    let mark = body[..first - range.start]
                        .trim_start_matches([' ', '>'])
                        .bytes()
                        .next();
                    if fence.len() >= 3 && fence.bytes().all(|b| Some(b) == mark) {
                        out.push((range.start + nl + 1 + lead..end, Style::Marker));
                    }
                }
            }
        }
        _ => {}
    }
}

/// A placed-text offset in source bytes. An offset inside a placeholder moves to the start
/// (`end == false`) or end of its span.
fn to_source(placed: &Placed, off: usize, end: bool) -> usize {
    let i = placed.spans.partition_point(|p| p.text.start <= off);
    let Some(p) = i.checked_sub(1).map(|i| &placed.spans[i]) else {
        return off;
    };
    if off < p.text.end && off > p.text.start {
        if end { p.src.end } else { p.src.start }
    } else if off == p.text.start {
        p.src.start
    } else {
        off - p.text.end + p.src.end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Each range's text and style, for readable assertions.
    fn spans(src: &str, entities: &[Entity]) -> Vec<(String, Style)> {
        highlight(src, entities)
            .into_iter()
            .map(|(r, s)| (src[r].to_string(), s))
            .collect()
    }

    fn has(got: &[(String, Style)], text: &str, style: Style) -> bool {
        got.iter().any(|(t, s)| t == text && *s == style)
    }

    #[test]
    fn emphasis_markers_and_text() {
        let got = spans("a **b** _c_ ~~d~~", &[]);
        assert_eq!(
            got,
            vec![
                ("**".into(), Style::Marker),
                ("b".into(), Style::Strong),
                ("**".into(), Style::Marker),
                ("_".into(), Style::Marker),
                ("c".into(), Style::Emphasis),
                ("_".into(), Style::Marker),
                ("~~".into(), Style::Marker),
                ("d".into(), Style::Strike),
                ("~~".into(), Style::Marker),
            ]
        );
    }

    #[test]
    fn fences_are_one_block_with_marked_fences() {
        let src = "look:\n```rust\nlet x = **1**;\n```\nafter";
        let got = spans(src, &[]);
        assert!(has(&got, "```rust\nlet x = **1**;\n```", Style::CodeBlock));
        assert!(has(&got, "```rust", Style::Marker));
        assert!(has(&got, "```", Style::Marker));
        // Nothing inside the block is styled as Markdown.
        assert!(!got.iter().any(|(_, s)| *s == Style::Strong));
        // An open fence runs to the end, with no closing marker.
        let got = spans("```\nstill typing", &[]);
        assert_eq!(
            got,
            vec![
                ("```".into(), Style::Marker),
                ("```\nstill typing".into(), Style::CodeBlock),
            ]
        );
    }

    #[test]
    fn inline_code_hides_its_backticks_only() {
        let got = spans("run `x _y_` and ``a`b``", &[]);
        assert!(has(&got, "x _y_", Style::Code));
        assert!(has(&got, "a`b", Style::Code));
        assert!(has(&got, "``", Style::Marker));
        assert!(!got.iter().any(|(_, s)| *s == Style::Emphasis));
    }

    #[test]
    fn block_markers() {
        let got = spans("> quoted\n> more\n\n- one\n\n3. two\n\n# Title", &[]);
        assert_eq!(
            got.iter()
                .filter(|(t, s)| t == ">" && *s == Style::Marker)
                .count(),
            2
        );
        assert!(has(&got, "-", Style::Marker));
        assert!(has(&got, "3.", Style::Marker));
        assert!(has(&got, "# ", Style::Marker));
        assert!(has(&got, "Title", Style::Strong));
    }

    #[test]
    fn links_safe_unsafe_and_bare() {
        let got = spans(
            "[ok](https://a.org) [no](javascript:x) see https://b.org/x. `https://c.org`",
            &[],
        );
        assert!(has(&got, "ok", Style::Link));
        assert!(has(&got, "[", Style::Marker));
        assert!(has(&got, "](https://a.org)", Style::Marker));
        assert!(!has(&got, "no", Style::Link), "unsafe links render as text");
        assert!(has(&got, "](javascript:x)", Style::Marker));
        assert!(has(&got, "https://b.org/x", Style::Link));
        assert!(
            !has(&got, "https://c.org", Style::Link),
            "code is left alone"
        );
    }

    #[test]
    fn html_is_plain_text() {
        let got = spans("<b>hi</b>\n\n<div onclick=\"x\">", &[]);
        assert!(got.is_empty(), "{got:?}");
    }

    fn mention(start: u32, end: u32) -> Entity {
        Entity::Mention {
            user_id: Uuid::from_u128(1),
            start,
            end,
        }
    }

    #[test]
    fn mentions_and_channels_but_not_in_code() {
        let src = "hi @Ann_Lee_ in #dev, `@Ann_Lee_`";
        let entities = [
            mention(3, 12),
            Entity::ChannelLink {
                conversation_id: Uuid::from_u128(2),
                start: 16,
                end: 20,
            },
            mention(23, 32),
        ];
        let got = spans(src, &entities);
        assert_eq!(
            got,
            vec![
                ("@Ann_Lee_".into(), Style::Mention),
                ("#dev".into(), Style::Channel),
                ("`".into(), Style::Marker),
                ("@Ann_Lee_".into(), Style::Code),
                ("`".into(), Style::Marker),
            ],
            "the name's underscores are not emphasis"
        );
    }

    #[test]
    fn offsets_hold_with_multibyte_text() {
        let src = "日本語 **強い** 🎉 @Zoë _é_\n```\n絵文字 👩‍👩‍👧\n```";
        let at = src.find("@Zoë").unwrap() as u32;
        let got = spans(src, &[mention(at, at + "@Zoë".len() as u32)]);
        assert!(has(&got, "強い", Style::Strong));
        assert!(has(&got, "@Zoë", Style::Mention));
        assert!(has(&got, "é", Style::Emphasis));
        assert!(has(&got, "```\n絵文字 👩‍👩‍👧\n```", Style::CodeBlock));
        for (r, _) in highlight(src, &[]) {
            assert!(src.is_char_boundary(r.start) && src.is_char_boundary(r.end));
        }
    }
}
