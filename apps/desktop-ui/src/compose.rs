//! Text rules for the composer, kept free of the DOM so they can be tested natively.
use std::ops::Range;

use proto::markdown::Style;
use proto::model::Entity;
use proto::view::{StagedFile, StagedState};
use uuid::Uuid;

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

/// Whether the caret, after `before`, sits in code: an open fence, or inline code opened on
/// its line.
pub fn in_code(before: &str) -> bool {
    let line = before.rsplit('\n').next().unwrap_or("");
    in_open_fence(before) || line.matches('`').count() % 2 == 1
}

/// The sigil's position and the word typed after it, when `sigil` starts a word.
fn sigil_query(text: &str, caret: usize, sigil: char) -> Option<(usize, &str)> {
    let before = text.get(..caret)?;
    let at = before.rfind(sigil)?;
    if at > 0 && !before[..at].ends_with(|c: char| c.is_whitespace() || "([{".contains(c)) {
        return None;
    }
    if in_code(before) {
        return None;
    }
    Some((at, &before[at + sigil.len_utf8()..]))
}

/// A `:name` being typed: where the `:` is and the name so far, lower-cased. Two letters
/// first, so times and ratios don't open it.
pub fn emoji_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let (at, q) = sigil_query(text, caret, ':')?;
    let ok = (2..=32).contains(&q.len())
        && q.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c));
    ok.then(|| (at, q.to_ascii_lowercase()))
}

/// A `#name` being typed: where the `#` is and the name so far. `# ` starts a heading instead.
pub fn channel_query(text: &str, caret: usize) -> Option<(usize, String)> {
    let (at, q) = sigil_query(text, caret, '#')?;
    let ok = q.len() <= 80 && q.chars().all(|c| c.is_alphanumeric() || "-_.".contains(c));
    ok.then(|| (at, q.to_lowercase()))
}

/// Channels for `q`: name prefix first, then a word of the name, then anywhere.
pub fn channel_matches<'a>(
    channels: &'a [(Uuid, String)],
    q: &str,
    limit: usize,
) -> Vec<&'a (Uuid, String)> {
    let q = q.to_lowercase();
    let rank = |name: &str| {
        let n = name.to_lowercase();
        if n.starts_with(&q) {
            Some(0)
        } else if n.split(['-', '_', '.', ' ']).any(|w| w.starts_with(&q)) {
            Some(1)
        } else if n.contains(&q) {
            Some(2)
        } else {
            None
        }
    };
    let mut hits: Vec<(u8, &(Uuid, String))> = channels
        .iter()
        .filter_map(|c| rank(&c.1).map(|r| (r, c)))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.1.cmp(&b.1.1)));
    hits.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// What an autocomplete pick inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    User(Uuid),
    Channel(Uuid),
}

/// Entities for the picks whose text (`@Name`, `#name`) is still in the body, each matched to
/// a separate, whole occurrence.
pub fn picked_entities(body: &str, picked: &[(Pick, String)]) -> Vec<Entity> {
    let mut taken: Vec<(usize, usize)> = vec![];
    let mut out = vec![];
    for (pick, needle) in picked {
        let hit = body
            .match_indices(needle.as_str())
            .map(|(s, _)| (s, s + needle.len()))
            .find(|(s, e)| {
                body[*e..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && !"-_".contains(c))
                    && taken.iter().all(|(ts, te)| e <= ts || s >= te)
            });
        if let Some((s, e)) = hit {
            taken.push((s, e));
            let (start, end) = (s as u32, e as u32);
            out.push(match *pick {
                Pick::User(user_id) => Entity::Mention {
                    user_id,
                    start,
                    end,
                },
                Pick::Channel(conversation_id) => Entity::ChannelLink {
                    conversation_id,
                    start,
                    end,
                },
            });
        }
    }
    out.sort_by_key(|e| match e {
        Entity::Mention { start, .. }
        | Entity::ChannelLink { start, .. }
        | Entity::Emoji { start, .. } => *start,
    });
    out
}

/// Why Send must wait, if it must: a staged file is still uploading or didn't upload. The
/// core refuses the same cases.
pub fn send_blocked(files: &[StagedFile]) -> Option<&'static str> {
    if files
        .iter()
        .any(|f| matches!(f.state, StagedState::Uploading))
    {
        Some("Wait for the files to finish uploading")
    } else if files
        .iter()
        .any(|f| matches!(f.state, StagedState::Failed { .. }))
    {
        Some("Remove or retry the file that didn't upload")
    } else {
        None
    }
}

/// Why Edit on a queued message can't move its text into the composer: the composer already
/// holds a draft, which it would replace. The queued message stays queued.
pub const REFILL_REFUSED: &str =
    "Your draft has text. Send or clear it, then choose Edit again to change the waiting message.";

/// Whether a queued message's text may replace the composer's: only when the composer's own
/// draft has loaded and is empty, so no draft is ever lost.
pub fn can_refill(draft: &str, loaded: bool) -> bool {
    loaded && draft.trim().is_empty()
}

/// Whether a paste carrying files should stage them rather than paste its text. Finder puts
/// the files' names in the text; anything else (a rich copy with a rendered image) pastes as
/// text.
pub fn paste_stages_files(text: &str, names: &[String]) -> bool {
    !names.is_empty()
        && text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .all(|l| names.iter().any(|n| n == l))
}

/// The second line of a file chip.
pub fn chip_status(f: &StagedFile, size: impl Fn(u64) -> String) -> String {
    match &f.state {
        StagedState::Uploading => {
            let pct = (f.sent * 100).checked_div(f.size).unwrap_or(0).min(99);
            format!("Uploading · {pct}%")
        }
        StagedState::Ready { .. } => size(f.size),
        StagedState::Failed { error } => error.clone(),
    }
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
    fn a_queued_edit_never_replaces_a_draft() {
        assert!(can_refill("", true));
        assert!(can_refill(" \n", true));
        assert!(!can_refill("Separate unsent draft", true));
        // The saved draft may still be loading: it would be overwritten once it arrives.
        assert!(!can_refill("", false));
    }

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
    fn emoji_queries() {
        assert_eq!(emoji_query("Shipped :ta", 11), Some((8, "ta".into())));
        assert_eq!(emoji_query(":Tada", 5), Some((0, "tada".into())));
        assert_eq!(emoji_query("(:+1", 4), Some((1, "+1".into())));
        // One letter, times, words and code don't open it.
        assert_eq!(emoji_query("hi :t", 5), None);
        assert_eq!(emoji_query("at 10:30", 8), None);
        assert_eq!(emoji_query("key:value", 9), None);
        assert_eq!(emoji_query("a :ta b", 7), None);
        assert_eq!(emoji_query("`x :ta", 6), None);
        assert_eq!(emoji_query("```\n:ta", 7), None);
        assert_eq!(emoji_query("`x` :ta", 7), Some((4, "ta".into())));
    }

    #[test]
    fn channel_queries() {
        assert_eq!(channel_query("see #", 5), Some((4, String::new())));
        assert_eq!(channel_query("see #Rel", 8), Some((4, "rel".into())));
        assert_eq!(channel_query("#dev-ops", 8), Some((0, "dev-ops".into())));
        assert_eq!(channel_query("# Title", 7), None, "a heading");
        assert_eq!(channel_query("issue#4", 7), None);
        assert_eq!(channel_query("`#dev", 5), None);
        let chans = vec![
            (Uuid::from_u128(1), "release-planning".to_string()),
            (Uuid::from_u128(2), "dev".to_string()),
            (Uuid::from_u128(3), "devrel".to_string()),
            (Uuid::from_u128(4), "web-dev".to_string()),
            (Uuid::from_u128(5), "random".to_string()),
        ];
        let names = |q: &str| {
            channel_matches(&chans, q, 6)
                .into_iter()
                .map(|c| c.1.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("dev"), vec!["dev", "devrel", "web-dev"]);
        assert_eq!(names("plan"), vec!["release-planning"]);
        assert_eq!(names("and"), vec!["random"]);
        assert_eq!(channel_matches(&chans, "", 2).len(), 2);
    }

    #[test]
    fn picks_become_entities_while_their_text_stays() {
        let ann = Pick::User(Uuid::from_u128(1));
        let dev = Pick::Channel(Uuid::from_u128(2));
        let picked = vec![(dev, "#dev".to_string()), (ann, "@Ann".to_string())];
        let got = picked_entities("@Ann see #dev-ops and #dev", &picked);
        assert_eq!(
            got,
            vec![
                Entity::Mention {
                    user_id: Uuid::from_u128(1),
                    start: 0,
                    end: 4
                },
                Entity::ChannelLink {
                    conversation_id: Uuid::from_u128(2),
                    start: 22,
                    end: 26
                },
            ],
            "#dev-ops is another channel's name"
        );
        assert!(picked_entities("Ann and dev", &picked).is_empty());
    }

    fn file(state: StagedState, sent: u64) -> StagedFile {
        StagedFile {
            id: Uuid::nil(),
            name: "a.png".into(),
            size: 200,
            sent,
            state,
        }
    }

    #[test]
    fn staged_files_and_sending() {
        let ready = file(
            StagedState::Ready {
                file_id: Uuid::nil(),
            },
            200,
        );
        let up = file(StagedState::Uploading, 124);
        let bad = file(
            StagedState::Failed {
                error: "too big".into(),
            },
            0,
        );
        assert_eq!(send_blocked(&[]), None);
        assert_eq!(send_blocked(std::slice::from_ref(&ready)), None);
        assert!(
            send_blocked(&[ready.clone(), up.clone()])
                .unwrap()
                .contains("uploading")
        );
        assert!(
            send_blocked(&[ready.clone(), bad.clone()])
                .unwrap()
                .contains("retry")
        );
        let size = |n: u64| format!("{n} B");
        assert_eq!(chip_status(&up, size), "Uploading · 62%");
        assert_eq!(chip_status(&ready, size), "200 B");
        assert_eq!(chip_status(&bad, size), "too big");
    }

    #[test]
    fn pastes_with_files() {
        let names = vec!["a.png".to_string(), "b.pdf".to_string()];
        assert!(paste_stages_files("", &names), "a screenshot has no text");
        assert!(
            paste_stages_files("a.png\nb.pdf", &names),
            "Finder names the files"
        );
        assert!(
            !paste_stages_files("Quarterly numbers", &names),
            "a rich copy pastes its text"
        );
        assert!(!paste_stages_files("", &[]));
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
