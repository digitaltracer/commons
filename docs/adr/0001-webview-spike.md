# ADR 0001: M0 webview spike for the Tauri + Leptos desktop UI

- Status: **recorded; budget not yet confirmed**. Building continues on decision 1, as the 2026-11-17 implementation decisions allow.
- Date: 2026-11-17
- Decision under test: decision 1 in `docs/architecture.md` (Tauri 2 shell with Leptos views in WKWebView).

## What was measured

Run with `cargo xtask spike`. It builds the release app (`cargo tauri build --no-bundle`), starts it with `COMMONS_SPIKE=1`, samples memory every 500 ms, and collects the UI's own timing report. Results go to `target/spike/results.json`.

The spike data set has no server behind it:

- 3 local accounts, 20 workspaces between them, and 6 channels in each workspace (120 conversations).
- One channel holds 10,000 messages; every other channel holds 200.
- Live bursts once a second for 20 s. Each burst is 50 new messages, each committed separately, plus 20 edits of recent messages.
- The message list is a bounded window of up to 300 rows over the 10,000 (decision 4). Scrolling past either end moves the window by 100 rows and keeps the first visible row anchored in place.

Machine: Apple silicon Mac, 18 cores, 48 GB RAM, macOS 27.2.1, WKWebView system framework.

## Results

### Memory

| Measure | App process | WebKit processes | Total | Proposed budget |
|---|---|---|---|---|
| RSS (`ps`), steady-state median | 154 MB | 216 MB | **370 MB** | 300 MB |
| RSS, peak | 177 MB | WebContent 149, GPU 28, Networking 8 | **394 MB** | 300 MB |
| Physical footprint (`footprint`), at 40 s | 83 MB | WebContent 85, GPU 13, Networking 5 | **186 MB** | 300 MB |

The two measures disagree about the budget:

- **By RSS, the budget is missed**: about 370 MB steady and 394 MB at peak.
- **By physical footprint, the budget is met**: 186 MB. This is the figure Activity Monitor shows as "Memory".

RSS double-counts pages that the app and the three WebKit processes share, and it counts clean pages that are mapped from system frameworks. Acceptance criterion 8 doesn't say which measure the budget uses. That choice is open; see below.

### Burst handling and scrolling

**Not measured.** During the measured runs the Mac's screen was locked. WebKit suspends `requestAnimationFrame` when nothing is visible, and both batch-to-paint latency and scroll frame times are taken from animation frames. The UI therefore reported 0 samples, so there are no numbers to record. Scroll smoothness is unknown until the spike runs on an unlocked screen.

The core side of the window contract was exercised during the run. Logged batches showed in-order deltas (revisions 1 to 11 in the sample) applied without a reset, while the window held at 300 rows with the total growing from 10,000 to 10,300. The window protocol itself is covered by unit tests: a dropped delta, a stale generation, the unacknowledged-batch cap, and an oversized delta.

To get the missing numbers, run this with the screen unlocked and the Commons window left visible (it stays on top for about a minute):

```
caffeinate -du cargo xtask spike
```

The `ui` section of `target/spike/results.json` then holds:

- `live.batch_to_paint_ms`: time from a batch arriving to the next painted frame;
- `scroll.scroll_frames`: frame intervals while scrolling;
- `scroll_long_frames_over_50ms`;
- `scroll_window_move_paint_ms`.

### Defects the spike found and fixed

- The CSP's `connect-src` left out `'self'`, so the webview could not fetch its own `.wasm` file.
- The row version was a 64-bit hash. It lost precision as a JavaScript number, so every batch failed to decode. It is now 32 bits.
- Every sidebar update remounted the message list, dropping its subscription and scroll position. The view now reads the sidebar only where it needs it. The core also coalesces sidebar updates to at most one per 100 ms.

## Outcome

- **Memory:** inside the budget by physical footprint, over it by RSS. Recorded as a miss against the RSS reading.
- **Scrolling and bursts:** not measured.
- **VoiceOver:** not done. This pass belongs to the user; the checklist is below.

The implementation decisions of 2026-11-17 say a failed or unmeasured spike does not stop the build, so the work continues on Tauri + Leptos. Decision 1 stays provisional, with GPUI as the fallback (plan §7), until three things are known:

1. which measure the memory budget uses;
2. the frame and paint numbers;
3. the VoiceOver result.

Step 6 reruns these gates on the full app.

## VoiceOver and keyboard checklist (for the user)

Start the app with `cargo xtask dev`, or run the spike binary (`COMMONS_SPIKE=1 target/release/commons-desktop`) to test with 10,000 messages. Turn on VoiceOver with Cmd+F5. Record pass or fail for each item:

- [ ] Sign-in form: each field announces its label. The checkbox announces "Create a new account" and its state. An error is announced when it appears.
- [ ] The sidebar is announced as the "Conversations" navigation. Each channel button reads its title and unread count, and the current one is announced as current.
- [ ] Tab and Shift+Tab reach every sidebar button, the message list, the composer, Attach and Send, in a sensible order, with a visible focus ring.
- [ ] The message list is announced as "Messages" (a log). New messages are read out without stealing focus.
- [ ] Inside the list, the VoiceOver cursor reads author, time, body, "(edited)" and the reaction buttons with their counts.
- [ ] Scrolling to the top with VO+arrow keys loads older messages, and the cursor stays on the same message.
- [ ] Composer: labelled "Message #channel". Enter sends and Shift+Enter adds a new line. "sending…" and "Not sent" are announced; Retry and Discard can be reached.
- [ ] Thread pane: "Reply in thread" opens it, it is announced as "Thread", and Close returns focus sensibly.
- [ ] The draft banner after a rejected edit is announced as an alert, and its text can be read.
- [ ] The typing indicator is announced politely.
- [ ] Every action above works with the keyboard only, with VoiceOver off.

## Open question for the user

Should the memory budget in acceptance criterion 8 be measured as physical footprint, which is what Activity Monitor shows, or as summed RSS? The plan's "RSS script" wording suggests RSS, and on that measure this build is about 70 MB over the budget.
