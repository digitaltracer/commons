# As built: UX review, packages 2–6 (2026-11-19)

Built against the UX review proposal and the user's decisions of 2026-11-19. Package 1 (rendering) landed earlier in `8895834` and `18f0b07`. Package 7 (live Markdown in the composer, staged uploads, `:emoji:` and `#channel`) is not part of this work.

## Fixed

**Composer and drafts (package 2)**
- A4: Return inside an open ``` or ~~~ fence adds a line. ⌘Return always sends. The hint and the code-block button show when the caret is in a fence.
- A5: unsent text is kept per account, conversation and thread root, in the account's meta table (`composer_drafts`), saved on a 400 ms debounce and once more when the composer closes. It survives switching and restarting, and stays on this Mac. The sidebar marks a conversation with a pencil, and the core rebuilds the sidebar only when a conversation gains or loses its marker.
- A8: sending scrolls the list back to the newest message, moving the window there first if needed.
- A10: delete asks in a small popover with Cancel and Delete. There is no undo.
- A13: Escape closes the thread or members panel, unless a field or popover used the key first.
- B6: opening a conversation focuses its composer, unless the sidebar was used from the keyboard (a click with no pointer detail).
- B9: threads use the same composer. Narrow composers drop the block tools, then the inline marks, through container queries.
- Part of B5: ⌘B, ⌘I, ⌘E and ⌘⇧X wrap the selection. ↑ in an empty composer edits your last message.
- Editing happens in the message's row (composer board, state 6), and an open edit survives a switch to another conversation and back.
- `panels.rs` uses `ui::copy_text`.

**Scroll, unread and focus (package 3)**
- A6: the UI reports window focus, blur and visibility to the core (`set_window_active`). While inactive, the open conversation no longer suppresses notifications, and the list's read path (`mark_seen`) does nothing. Coming back marks what is on screen.
- A7: a ResizeObserver keeps a pinned list at the bottom as rows change height. Content growing under the list no longer unpins it: only scrolling up does.
- B3: the unread count when a conversation opens places a New line. With more than a screen unread, the list opens there. Scrolled away from the end, a pill offers Jump to latest, with the number of messages that arrived meanwhile.
- B7: each conversation and thread remembers its top row for the session.
- C6 (loading): skeleton rows until the first batch.
- Read marks have a trailing mark, so the last message of a burst isn't left unread.

**Layout (package 4)**
- B1: drag handles on the sidebar's and thread's edges (sidebar 208–420 px, default 256; thread 320–560 px, default 380), with arrow keys, Shift for bigger steps, Home and End, and double-click or Return to reset. They are separators with `aria-valuenow`. Widths are stored by the core in `ui_prefs.json`. The message column keeps at least 480 px: the sidebar narrows first, and below that an open thread or members panel floats over the messages with a scrim.
- C2: the rail is darker than the sidebar, with no border between them.
- B1 (window frame): the official `tauri-plugin-window-state`, on its 2.5 line (built on Tauri 2.12), restores the window's size, position, maximized and fullscreen state. It leaves decorations and visibility alone, so it can't undo the overlay title bar or reopen a hidden window. It runs from Rust only, so the capability file gains no permissions. macOS applies the 900×560 minimum only to drags, so any resize below it, from an old or edited state file or from code, is raised back to the minimum.

**Visual refresh (package 5)**
- B2: rows from one author within five minutes, with no divider between them, drop the avatar and name. The time shows in the gutter on hover or focus.
- B12: messages past about twelve lines collapse with Show more.
- C1: the selected sidebar row is a soft tint with bold text.
- C3: message text is 14.5 px at 1.5, with 6 px top padding on first rows and 1 px on grouped rows.
- C5: day labels are pills on the rule.
- C6: an intro block at the top of every conversation, with Add people for channels and group DMs.
- C7: thread summaries show up to three repliers and "Last reply … ago". The core's view carries `last_reply_at_ms` and `reply_user_ids`.
- C8: the header shows an avatar stack with the member count, and keeps the bell.
- C10: the typing line has a three-dot animation, which stops under reduced motion.
- A11: the list has one tab stop, and the arrow keys, Home and End move between rows. A row's actions show when it has focus. The author is a button. The emoji picker takes focus. Arrivals go to their own polite live region, and the list itself is no longer a live region.

**Jump palette and shortcuts (package 6)**
- B4: ⌘K opens a palette over every account's conversations, people without a DM yet, and public channels not joined (Return joins them). With no query it shows Recent and Unread. "Search messages for …" is the last row (⌘Return). Full search moves to ⌘F (the header field) and ⌘⇧F. The sidebar's Search row became a Jump to… button.
- The rest of B5: ⌥↑ and ⌥↓ walk the current workspace's conversations. ⌥⇧↑ and ⌥⇧↓ walk unread ones across workspaces. ⌘/ lists every shortcut.

## Deviations
- The ResizeObserver re-pin (A7) landed with package 2, because re-pinning after a send needed it.
- No `Cargo.toml` changes. web-sys has no ResizeObserver feature enabled here, so the UI binds it with a small `wasm_bindgen` extern in `ipc.rs`. Row navigation walks siblings instead of using `querySelectorAll`, for the same reason.
- No unread bar from the narrow board. The New line and the pill cover the same need.
- No dock button on the overlay thread. Widening the window docks it again.
- No separate 20-line cap for code blocks: the twelve-line message clamp covers them.
- The members panel shares the thread's width and overlay rule.
- Day labels, gutter times and "Last reply" still use the 12-hour clock (C9 was not in scope).

## Checks
- Core tests: drafts per conversation and thread with sidebar events only on marker changes, the sidebar flag, drafts across a restart, background window presentation and read rules, UI preferences clamped and across a restart, reply summaries. Proto test for width clamping. Desktop UI tests for the fence and Return rules, shortcuts, mention queries, pane width math and palette scoring.
- Every package was run in the browser harness (Chrome, Tauri IPC stubbed): drafts across switching and reload, fences, edit in place, delete confirm, Escape, re-pin after send, New line and pill, read marks with the window blurred and focused, scroll anchors, skeleton rows, pane drag and keyboard resize with reload, the thread overlay, grouping, Show more, the intro block, thread summaries, keyboard reactions, the live region, light and dark tokens, and the palette with 500 conversations (Return opened the channel in 37 ms).
- Window frame, in the real app (`cargo xtask app`, throwaway `HOME`). System Events had no accessibility access and the session couldn't capture the screen, so a temporary, uncommitted hook set the frame from code and then quit through the same exit path as ⌘Q. Results: the first launch opened at the default 1280×820; a frame of 1000×640 at (140, 80) came back exactly on relaunch; a state file edited to 700×400 opened at 900×560, and was saved that way; a 600×300 frame set from code was raised to 900×560. The traffic lights on a restored window were not looked at.
- The real app was built with `cargo xtask app` and launched once with a throwaway `HOME`: it reached the sign-in screen with no UI errors in the log. It wasn't driven further, because this session had no control of native windows.

## Left
- Not checked by hand in the real app: dragging and moving the window with the mouse before quitting, the traffic lights on a restored window, notifications from a second live client while the window is in the background, Escape and ⌥-arrows with a real keyboard, pointer drags on the pane handles, and VoiceOver reading the separators, rows and live region.
- Drafts are not cleared when a conversation is purged; they come back if access does.

# Package 7: composer v2 (2026-11-19)

Live Markdown in the composer, staged uploads (B8), `:emoji:` and a searchable picker (B10), and `#channel` (B11). Built to the proposal's package 7 and the user's decision that emphasis in the composer is colour only.

## Built

**Live Markdown (the highlight layer)**
- The textarea is still the only input. Behind it, a layer draws the same text with the same font, padding and wrapping, and the textarea's own text is transparent with a visible caret. The layer takes the textarea's client size, so a scrollbar can't change where lines wrap, and follows its scroll offset to the fraction of a pixel.
- `proto::markdown::highlight(source, entities)` returns byte ranges and styles from the renderer's own parser, options and entity placement: dimmed markers (`**`, `_`, backticks, `~~`, fences, `>`, list markers, heading `#`s, link brackets and targets), strong, emphasis and strike text, inline code, whole code blocks, links (bare URLs included, as the renderer linkifies them), and mention and channel-link tints. Nothing is styled inside code. It takes the draft's entities as well as its text, so a picked name's underscores are never read as emphasis, just as the renderer treats them.
- Colour and background only. Strong is the accent text colour, emphasis a muted blue, strike the tertiary grey, markers a dim grey (`--hlmark`, `--hlem`). A code block gets a full-width shaded box drawn under the text, outside its flow. Kerning and ligatures are turned off in both the textarea and the layer, so a style boundary can never move a glyph.
- The field's value is written only when it differs from the text, so typing and IME composition are never interrupted. Autocomplete picks and the formatting buttons type through `insertText`, so they join the field's undo stack.

**Staged uploads (B8)**
- Attach opens a picker for several files. Pasting files (a screenshot, or files copied in Finder) and dropping files on the window stage them too. Each file is a chip with its size, upload progress, Retry when it failed, and Remove.
- Send queues the text and every staged file as one message. Files alone send with an empty body. At most ten files per message, the server's limit.
- Outbox rule: uploads still go straight to the server as they are staged, not through the outbox. While a file is uploading or has failed, Send doesn't send: it says why ("Wait for the files to finish uploading", "Remove or retry the file that didn't upload") and keeps the text. The core refuses the same cases. Files that finished uploading send offline like any message, through the outbox. A file staged offline fails with "Couldn't reach the server" and can be retried.
- Staged files belong to the composer they were staged in (conversation and thread), and survive switching conversations and reloading the webview. They live in the core's memory, so a restart drops them, and the server keeps their unsent uploads, as it does for any upload that's never sent. Pasted bytes are copied to the account's `staged-uploads` folder so a failed upload can be retried, deleted when the file is sent or removed, and cleared at start.
- File paths never reach the webview. The picker stages files in the shell. Tauri handles native drops (the webview's own drop events carry no files), and the shell keeps the paths under a drop id that the UI claims for the composer under the drop point, or else the open conversation's composer. Pasted files cross the bridge as raw bytes, with their names percent-encoded in headers.

**`:emoji:` and the picker (B10)**
- A colon and two letters at the start of a word suggest emoji by shortcode, then by the words of their names (the data-only `emojis` crate has names and GitHub shortcodes, not CLDR keywords). Shorter shortcodes come first within a rank, so `:ta` offers 🌮 and 🎉 before 🍊. Not inside code, and not for times like 10:30. The arrow keys choose, Return or Tab inserts the emoji, Escape closes.
- The picker opens on a search field. The arrow keys move through an eight-column grid, Up from the top row goes back to the field, typing in the grid goes on in the field, and Return in the field picks the first result. It announces the number of results, and gives focus back to whatever opened it.
- Suggestion lists are tied to the field with `aria-controls` and `aria-activedescendant`, and close when the field loses focus.

**`#channel` (B11)**
- `#` at the start of a word suggests the workspace's channels the user is in: name prefix first, then a word of the name, then anywhere. Picking one inserts `#name` and sends a channel-link entity for it while the text stays. `# ` still starts a heading. The core's mention merge keeps picked channel links that cover a `#name`, and the server already validates them.

## Decisions and deviations
- `highlight` takes the entities as well as the source (`highlight(source, entities)`), so tints and emphasis match `render_with_entities`.
- Mentions are tinted when they were picked from autocomplete. A name typed out in full is still found by the core at send time and renders as a mention, but the composer doesn't tint it.
- Send waits rather than sending later on its own. The board's hint "Sends when the upload finishes" would need a held send that could outlive the composer, or fail after its text was cleared.
- No CLDR keywords: the `emojis` crate doesn't carry them, and name words cover most searches.
- The emoji grid's cells are buttons in labelled groups rather than ARIA grid cells, since there are no rows.
- The textarea still grows by logical lines (up to eight), so one long wrapped line scrolls inside a short field, as before.

## Checks
- Proto tests for `highlight`: emphasis markers and text, fences as one block with marked fences (open and closed), inline code with its backticks only, quote, list and heading markers, safe, unsafe and bare links, raw HTML left plain, mentions and channels tinted but not inside code, and offsets on character boundaries with CJK, emoji and accented text.
- Core tests: staging two files from disk and a pasted one, a failed upload (over the server's size limit) holding the send, retry and removal, one message carrying all three attachments in order, the pasted copy deleted, a thread's composer kept apart, files alone sending, and the ten-file limit. Picked channel links survive the mention merge and reach the server.
- Desktop UI tests: emoji and channel queries (times, words, code, headings), channel ranking, picks turning into entities only while their text is whole, layer painting, emoji search ranking, grid keys, the send rule, the paste rule, and chip text.
- Browser harness (Chrome, Tauri IPC stubbed, with staging added to the stub):
  - drift: with the textarea's own text made red and translucent, it sat exactly on the layer's text across bold, code, links, Japanese, Arabic, emoji with ZWJ sequences, a long URL breaking mid-word, and a wrapped paragraph, also scrolled to the bottom;
  - speed: 10,000 characters of mixed Markdown, inserting in the middle took a median of 5.7 ms and at most 7.9 ms per keystroke, layer render included, in a debug WASM build;
  - undo and redo stepped back through typing and an autocomplete pick (`@Priya Nair ` back to `@pri`); the spellcheck underline showed through the transparent text;
  - `:ta` with arrows and Return, the picker by keyboard only (search, arrows, Return, focus back to the composer and to the reaction button), `#eng` with Tab inserting `#engineering` and sending the channel-link entity at the right offsets, the light and dark tokens;
  - staging: two picked files, two pasted files (one name with a non-ASCII character), progress, Send refused with the text kept, Retry, one message with all four attachments; drops on the thread composer and on the message list landing in the right composers; chips kept across switching conversations. Escape was checked by dispatching the key, because the Chrome tool's Escape blurs the window.

- The real app was built with `cargo xtask app` and launched with a throwaway `HOME`. Its UI booted with no warnings or errors in the log. It wasn't driven further.

## Left
- Not checked in the real app (this session can't drive native windows): IME composition (Japanese, Pinyin), autocorrect, VoiceOver reading the field and suggestion lists, native drag and drop from Finder, pasting a Finder copy in WKWebView, and the picker dialog with several files. The IME path relies on the field's value never being rewritten while it matches, which the harness confirmed for typing but not for marked text.
- Staged files are lost on restart, and the server never reclaims unsent uploads.

# UI audit fixes (2026-11-19)

Eight problems from the UI audit, fixed before 0.1.0. The audit's ids are in brackets.

## Fixed
- **Dialogs keep focus (UI-01).** Every dialog built on the shared `Dialog` takes focus when it opens: its `autofocus` field, or else the dialog itself, never its first button. Tab and Shift-Tab wrap inside it, focus moved out by code is pulled back, Escape closes it wherever focus is unless a control inside used the key, and focus goes back to where it was when the dialog closes. Typing after ⌘/ no longer reaches the composer.
- **Removing an account asks first (UI-02).** Manage accounts now opens the same confirmation as the sign-in sheet, which counts unsent messages. Cancel has focus.
- **Editing a queued message keeps the draft (UI-03).** See the rule below.
- **The sidebar scrolls (UI-04).** The conversation list scrolls and the account footer stays at the bottom of the window. The sidebar column was missing `min-height: 0`.
- **Show more follows the width (UI-05).** A message body is measured again when its width changes. Only bodies at least three lines tall are watched, since a shorter one would need a column a quarter as wide to pass the twelve-line clamp. Each watch stops when its row goes.
- **The delete popover (UI-06, UI-21).** It closes when the window is resized, and focus goes back to the trash button. It opens with Cancel focused.
- **Starting a conversation says why it failed (UI-07).** The New message form shows the server's refusal. Once eight people are picked, the other checkboxes are disabled, and the footer keeps the limit in view.
- **A pasted invite offers the account choice (UI-09).** A pasted link follows the same rule as an opened one: no account on its server leads to sign-up, one account joins, and several accounts show the same Join as picker, with the first one chosen.

## The rule for queued edits
Edit on a queued message moves its text into the composer only when the composer is empty. If the composer holds a draft, nothing changes: the message stays queued, the draft stays, and a line under the composer says to send or clear the draft first. The composer discards the queued message itself, so the text is never removed before it has somewhere to go.

Editing in place, in the row, would need the core to change an outbox entry's text, so it was left out.

## Checks
- Desktop UI tests: Escape and Tab in a dialog, the queued-edit rule, the eight-person limit, and the routing of a pasted invite (two accounts on its server, one, none, and a bare token).
- Browser harness, with the audit's stub, before and after each fix: the shortcut sheet took focus and a typed Return sent nothing; Tab stayed inside Account settings for twelve presses and wrapped back; Escape closed the sheet after the composer was focused from code. Manage accounts removed nothing on the first click and Return on Cancel backed out. A queued edit with a draft kept both, and with an empty composer moved the text. With 74 conversations at 1000 × 700 the list scrolled and the footer stayed visible. Narrowing from 1440 to 900 px added Show more, and widening took it away. A resize closed the delete popover and returned focus to the trash button. Nine people couldn't be picked, and a refused start showed the reason. Pasting a link for a server with two accounts showed both, and Join used the one chosen.

## Left
- Not checked in WKWebView: dialog focus and Tab order, which follow Safari's rules for buttons (a click doesn't focus a button there, so focus may return to the page instead of the trash button or the opener).
- A queued thread reply's Edit still goes to the channel's composer, not the thread's.
- Delete on a queued message still skips confirmation (UI-13), and the narrow thread overlay still lets Tab reach the conversation behind it (UI-14). The switcher and the jump palette aren't built on `Dialog` and keep their own focus handling.
