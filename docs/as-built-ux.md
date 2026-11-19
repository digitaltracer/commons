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
