//! Text rules for the composer, kept free of the DOM so they can be tested natively.

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
    fn mention_queries() {
        assert_eq!(mention_query("hi @an", 6), Some((3, "an".into())));
        assert_eq!(mention_query("mail a@b", 8), None);
        assert_eq!(mention_query("@", 1), Some((0, String::new())));
        assert!(matches_query("Ann Lee", "le"));
        assert!(!matches_query("Ann Lee", "nn"));
    }
}
