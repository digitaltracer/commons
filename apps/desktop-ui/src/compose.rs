//! Text rules for the composer, kept free of the DOM so they can be tested natively.
use std::ops::Range;

use proto::markdown::Style;

/// Whether the caret, after `before`, sits inside a ``` or ~~~ fence that isn't closed yet.
pub fn in_open_fence(before: &str) -> bool {
    let mut open: Option<(char, usize)> = None;
    for line in before.split('\n') {
        let t = line.trim_start_matches(' ');
        if line.len() - t.len() > 3 {
            continue;
        }
        let Some(ch) = t.chars().next().filter(|c| *c == '`' || *c == '~') else {
            continue;
        };
        let run = t.chars().take_while(|c| *c == ch).count();
        if run < 3 {
            continue;
        }
        match open {
            None if ch == '~' || !t[run..].contains('`') => open = Some((ch, run)),
            Some((o, n)) if o == ch && run >= n && t[run..].trim().is_empty() => open = None,
            _ => {}
        }
    }
    open.is_some()
}

/// What Return does in a composer.
#[derive(Debug, PartialEq, Eq)]
pub enum Return {
    Send,
    /// Leave the key alone: the field inserts a line break.
    Newline,
}

/// ⌘Return always sends; Shift-Return and Return inside an open fence add a line.
pub fn on_return(meta: bool, shift: bool, before_caret: &str) -> Return {
    if meta {
        Return::Send
    } else if shift || in_open_fence(before_caret) {
        Return::Newline
    } else {
        Return::Send
    }
}

/// The markers a formatting shortcut wraps the selection in: ⌘B, ⌘I, ⌘E and ⌘⇧X.
pub fn shortcut_markers(key: &str, shift: bool) -> Option<&'static str> {
    match (key.to_ascii_lowercase().as_str(), shift) {
        ("b", false) => Some("**"),
        ("i", false) => Some("_"),
        ("e", false) => Some("`"),
        ("x", true) => Some("~~"),
        _ => None,
    }
}

/// The text to insert for the code-block button, and how far back from its end the caret goes.
/// `at_line_start` says whether the selection starts a line.
pub fn code_block(selected: &str, at_line_start: bool) -> (String, usize) {
    let lead = if at_line_start { "" } else { "\n" };
    let close = "\n```";
    (format!("{lead}```\n{selected}{close}"), close.len())
}

/// An `@` name being typed: where the `@` is (byte offset) and the text after it.
pub fn mention_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let before = &text[..caret];
    let at = before.rfind('@')?;
    if at > 0 && !before[..at].ends_with(|c: char| c.is_whitespace() || "([{".contains(c)) {
        return None;
    }
    let q = &before[at + 1..];
    if q.len() > 32 || q.contains('\n') || q.starts_with(' ') {
        return None;
    }
    Some((at, q.to_string()))
}

pub fn matches_query(name: &str, q: &str) -> bool {
    let q = q.to_lowercase();
    let n = name.to_lowercase();
    n.starts_with(&q) || n.split_whitespace().any(|w| w.starts_with(&q))
}

/// One stretch of the highlight layer: a byte range and the styles on it, as bits.
pub type Run = (Range<usize>, u16);

/// The highlight layer's content: plain runs, and code blocks that group their runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece {
    Runs(Vec<Run>),
    Block(Vec<Run>),
}

pub fn bit(style: Style) -> u16 {
    1 << match style {
        Style::Marker => 0,
        Style::Strong => 1,
        Style::Emphasis => 2,
        Style::Strike => 3,
        Style::Code => 4,
        Style::CodeBlock => 5,
        Style::Link => 6,
        Style::Mention => 7,
        Style::Channel => 8,
    }
}

/// The classes for a run's styles. Code blocks are drawn by their group, not per run.
pub fn classes(bits: u16) -> String {
    const NAMES: [(Style, &str); 8] = [
        (Style::Marker, "hm"),
        (Style::Strong, "hs"),
        (Style::Emphasis, "he"),
        (Style::Strike, "hx"),
        (Style::Code, "hc"),
        (Style::Link, "hk"),
        (Style::Mention, "hu"),
        (Style::Channel, "hh"),
    ];
    NAMES
        .iter()
        .filter(|(s, _)| bits & bit(*s) != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cuts `0..len` into runs where the set of styles is constant, and groups code-block runs.
pub fn paint(len: usize, ranges: &[(Range<usize>, Style)]) -> Vec<Piece> {
    let mut edges: Vec<(usize, i8, usize)> = Vec::with_capacity(ranges.len() * 2);
    for (r, s) in ranges {
        let i = bit(*s).trailing_zeros() as usize;
        let (a, b) = (r.start.min(len), r.end.min(len));
        if a < b {
            edges.push((a, 1, i));
            edges.push((b, -1, i));
        }
    }
    edges.sort_unstable();
    let mut counts = [0i32; 9];
    let mask = |counts: &[i32; 9]| {
        counts
            .iter()
            .enumerate()
            .filter(|(_, n)| **n > 0)
            .fold(0u16, |m, (i, _)| m | 1 << i)
    };
    let mut runs: Vec<Run> = vec![];
    let mut at = 0;
    for (pos, delta, i) in edges {
        if pos > at {
            runs.push((at..pos, mask(&counts)));
            at = pos;
        }
        counts[i] += i32::from(delta);
    }
    if at < len {
        runs.push((at..len, mask(&counts)));
    }
    let block = bit(Style::CodeBlock);
    let mut pieces: Vec<Piece> = vec![];
    for run in runs {
        let in_block = run.1 & block != 0;
        match pieces.last_mut() {
            Some(Piece::Block(v)) if in_block => v.push(run),
            Some(Piece::Runs(v)) if !in_block => v.push(run),
            _ if in_block => pieces.push(Piece::Block(vec![run])),
            _ => pieces.push(Piece::Runs(vec![run])),
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_open_and_close() {
        assert!(!in_open_fence("plain text"));
        assert!(in_open_fence("```"));
        assert!(in_open_fence("look:\n```rust\nfn main() {"));
        assert!(!in_open_fence("```\ncode\n```"));
        assert!(in_open_fence("```\ncode\n```\nmore\n```"));
        // A closing fence needs at least as many marks, of the same kind, and nothing after.
        assert!(in_open_fence("````\ncode\n```"));
        assert!(in_open_fence("~~~\ncode\n```"));
        assert!(in_open_fence("```\ncode\n``` x"));
        assert!(!in_open_fence("~~~\ncode\n~~~~"));
        // Inline code and indented code are not fences.
        assert!(!in_open_fence("use ``` for blocks"));
        assert!(!in_open_fence("    ```"));
        assert!(!in_open_fence("```js```"));
    }

    #[test]
    fn return_sends_unless_a_fence_is_open() {
        assert_eq!(on_return(false, false, "hi"), Return::Send);
        assert_eq!(on_return(false, true, "hi"), Return::Newline);
        assert_eq!(on_return(false, false, "```"), Return::Newline);
        assert_eq!(on_return(true, false, "```\nx"), Return::Send);
        assert_eq!(on_return(false, false, "```\nx\n```"), Return::Send);
    }

    #[test]
    fn shortcuts_and_code_blocks() {
        assert_eq!(shortcut_markers("b", false), Some("**"));
        assert_eq!(shortcut_markers("X", true), Some("~~"));
        assert_eq!(shortcut_markers("x", false), None);
        assert_eq!(shortcut_markers("b", true), None);
        assert_eq!(code_block("", true), ("```\n\n```".to_string(), 4));
        assert_eq!(code_block("a", false), ("\n```\na\n```".to_string(), 4));
    }

    #[test]
    fn paint_cuts_runs_and_groups_blocks() {
        let src = "a **b**\n```\nx\n```";
        let pieces = paint(src.len(), &proto::markdown::highlight(src, &[]));
        let m = bit(Style::Marker);
        let s = bit(Style::Strong);
        let cb = bit(Style::CodeBlock);
        assert_eq!(
            pieces,
            vec![
                Piece::Runs(vec![(0..2, 0), (2..4, m), (4..5, s), (5..7, m), (7..8, 0)]),
                Piece::Block(vec![(8..11, cb | m), (11..14, cb), (14..17, cb | m)]),
            ]
        );
        assert_eq!(classes(m | s), "hm hs");
        // Every byte is covered once, in order.
        let mut at = 0;
        for p in &pieces {
            let (Piece::Runs(v) | Piece::Block(v)) = p;
            for (r, _) in v {
                assert_eq!(r.start, at);
                at = r.end;
            }
        }
        assert_eq!(at, src.len());
        assert_eq!(paint(0, &[]), vec![]);
    }

    #[test]
    fn mention_queries() {
        assert_eq!(mention_query("hi @an", 6), Some((3, "an".into())));
        assert_eq!(mention_query("mail a@b", 8), None);
        assert_eq!(mention_query("@", 1), Some((0, String::new())));
        assert!(matches_query("Ann Lee", "le"));
        assert!(!matches_query("Ann Lee", "nn"));
    }
}
