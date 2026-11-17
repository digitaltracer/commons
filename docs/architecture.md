# Commons architecture plan

## 1. Outcome

Commons is a self-hosted, open-source team chat with a Rust desktop client, macOS first, and a Rust server.

**Client.** All client behaviour lives in a shared Rust core: sessions, sync, the offline cache, the send queue, unread state, and applying notifications. The UI is a Tauri 2 desktop shell. Its views are written in Leptos (Rust compiled to WebAssembly) and rendered in the system webview, and the same views later become the browser client.

**Server.** `commonsd` is a single Rust binary built on axum, tokio and sqlx over PostgreSQL. It speaks a documented JSON-over-HTTPS API plus one WebSocket per client, carrying ordered, resumable event streams. Bots and future browser and mobile clients use the same API.

**Search, files and sign-in.**
- Search ships on PostgreSQL full-text search behind a search interface. Chinese, Japanese and Korean (CJK) search is a known v1 limit, and the planned fix is an embedded Tantivy index.
- Files live on local disk or S3-compatible storage, and every download is checked by the server.
- Sign-in is email and password, or OIDC.

**Deployment.** `docker compose up -d` brings up PostgreSQL, the server and a TLS proxy on a 2 vCPU / 4 GB VPS. Scaling out means several `commonsd` processes sharing PostgreSQL and object storage. Huddles, bots and mobile are designed now and built after v1.

**Consequence.** The stack is mainstream, well-documented Rust plus web-style UI work, which keeps the contributor pool wide. The cost is a webview. Its memory, accessibility and native feel are tested in the first milestone, with a native-Rust toolkit as the fallback if the webview fails.

## 2. Current behaviour

The repository `/Users/adarsh/personal/commons` was inspected at `main` @ `09e136f`. It is greenfield:

- `README.md`, "Why Commons?": the project is open source and non-commercial, self-hosted on the operator's infrastructure, with a community roadmap.
- `README.md`, "Planned features": workspaces with public and private channels; threads; direct and group messages; mentions, reactions and notifications; file sharing; search across messages and files; voice and video huddles; integrations and bots via an open API; self-hosting with a single command.
- `README.md`, "License": undecided. An OSI-approved license is promised before the first release.
- `TAGLINE.md` holds the taglines.
- There is no code, build configuration, CI or license.

The user decided the following on 2026-11-17:

- All client logic lives in a shared Rust core. The UI toolkit is open.
- Platforms come in this order: Mac, then Linux and Windows desktop, then browser and mobile.
- There is no end-to-end encryption in v1.
- An open community builds the project.
- There is no federation.
- One server hosts one or more workspaces, sized for about 5,000 users with 500 online at once.
- v1 covers workspaces, public and private channels, DMs and group DMs, threads, mentions, reactions, files, search, desktop notifications, and email/password plus OIDC sign-in. Huddles, bots and mobile push are designed now and built later.

## 3. Decisions

| # | Decision | Why | Strongest rejected alternative |
|---|---|---|---|
| 1 | **Desktop UI: a Tauri 2 shell with Leptos views** (Rust compiled to WASM) in the system webview (WKWebView on macOS). The webview must pass a memory and accessibility spike in M0 before the rest of the UI is built on it. | The platform supplies screen readers, IME, rich text editing, selection and WebRTC. The same views become the browser client (criterion 4). Web-style UI work draws the most contributors (criterion 1). | **GPUI**: the most native and fastest option, but it has no browser target, its API changes often, its docs are thin and its accessibility is unproven. It is the fallback if the spike fails. **Iced and Slint** lag on rich text and accessibility. **A SwiftUI shell** means a second UI for every new platform. |
| 2 | **A headless client core** (`client-core`) owns all state and behaviour. The UI sends commands and renders view models. The crate graph forbids the UI crate from depending on storage or network crates. | This is the user's hard rule. It also keeps the UI replaceable, and lets the core run natively now and as WASM later. | Putting logic in UI components. |
| 3 | **The core runs natively in the Tauri process** and uses SQLite for its cache. Storage, transport, keychain and OS notifications sit behind traits, so a browser build can swap in IndexedDB or OPFS. **One core instance runs per signed-in server account**, each with its own SQLite file and connection, so the client can be signed in to several Commons servers at once. | Native SQLite and the OS keychain are mature on all three desktops. Per-account instances avoid global state, and many people belong to more than one community server. | Running the core as WASM inside the desktop webview, which gives weaker storage and no keychain. A single-server client, which would bake in a singleton that is hard to undo. |
| 4 | **The UI and core talk through windowed subscriptions over Tauri channels**, not one IPC event per message. The UI subscribes to a bounded window, such as a channel's visible message range. It first receives `{generation, revision, rows}`, then batched deltas `{generation, base_revision, revision, upserts, removals, order_changes}`. Bytes and unacknowledged batches are capped. A gap or overflow resets the window. Attachments travel as references, never as bytes. Images and files that the webview displays are loaded through a Tauri custom protocol (`commons-file://`) that the core serves. The core attaches the session token, so the webview never holds it. | Tauri recommends channels for streaming. Per-event JSON across IPC plus WASM deserialization is the main performance risk of decision 1, and windows bound it. | Pushing every store change to the UI, which is unbounded. |
| 5 | **Sync is server-authoritative, not a CRDT.** The client keeps a bounded cache and an outbox of pending sends, each with a client-generated UUIDv7 that the server deduplicates. Edits carry the revision they were based on, and the server rejects a stale edit, which comes back to the user as a draft. Deletes win over concurrent edits. Reactions are idempotent add and remove operations with no revision check. The outbox gives up after 7 days and surfaces the failure. Deduplication keys are kept for 14 days, longer than the outbox will retry. | Chat is append-heavy with no concurrent document editing. Idempotent IDs make retries safe, and revision checks never silently drop text. | **CRDTs**, which aren't needed. **Last-writer-wins edits**, which drop text when two devices edit at once. |
| 6 | **The wire protocol is custom.** It has versioned `/v1` JSON REST resources for commands and history, and one WebSocket per client for events. The OpenAPI spec is generated from code with utoipa, and event schemas are published. HTTPS replay is the source of truth, and the socket only delivers. The server advertises the minimum client version it supports. | There is no federation, so Matrix's event graph and state resolution would be pure cost. JSON plus OpenAPI is the easiest format for bot authors and contributors. | **Matrix**: built for federation, with conformance scope that doesn't remove Commons-specific authorization or UX work. **XMPP**: XML plus a patchwork of extensions. |
| 7 | **The server is a modular monolith**: one `commonsd` binary on tokio, axum and sqlx running HTTP, WebSocket and bounded background workers. Its modules are auth, directory (workspaces, users, roles), conversations, messages, sync, files, search and notify. Background work runs through a jobs table, so the same binary can run later as `--role api` or `--role worker`. | One process is the simplest to self-host and debug. Roles give a scale-out path without a rewrite. | **Microservices**: an operating cost a small VPS can't carry. |
| 8 | **PostgreSQL is the only server database.** | Concurrent writers, transactions, LISTEN/NOTIFY and replicas. sqlx checks queries at compile time against one dialect. | **SQLite on the server**: one writer at a time and no multi-node path. Supporting both doubles the query surface. |
| 9 | **Two durable event streams, both transactional.** (a) A **workspace stream**: each change in a workspace writes an `events` row in the same transaction. Its `seq` is allocated by `UPDATE workspaces SET seq = seq + 1 … RETURNING` as the last statement before commit, so seq order is commit order. (b) A **per-user stream**, built the same way with `users.seq`, carrying read cursors, `notify` events, preferences and workspace-membership changes. A client resumes with its user seq and one seq per workspace. The server prunes logs after 30 days. A resume from before the prune point gets a snapshot taken at a committed watermark H, followed by replay after H. The snapshot is read in one REPEATABLE READ transaction, so it matches H exactly. Every replay response returns `next_since` even when every event in the range was filtered out. **Presence and typing are ephemeral**: they go through the hub only and are never logged. Joining a workspace puts an event on the user stream that carries the workspace's starting seq. Removal from a workspace also goes on the user stream, and that event drives the client's purge, because a removed user can no longer resume the workspace stream. | A Postgres sequence can expose seq 11 before 10 commits, and a client resuming from 11 then skips 10 forever. A transactional log means a failed publish delays an event but never loses it. The workspace row lock lasts about one commit fsync, which caps one workspace at roughly 500 commits per second before overhead. Expected traffic at 500 online users is tens of sends per second, and moving user-scoped and ephemeral traffic off the row keeps it there. Sustained throughput and burst drain time are measured in step 3, not assumed. | **A global sequence with a reconciliation window**, which is harder to get right. **Redis streams**, an extra dependency. |
| 10 | **Every right to read a channel is a membership interval, and delivery needs both the interval and current access.** Membership rows record `joined_seq` and `left_seq`. Access a role implies, such as admins reading private channels, is written as intervals too, so a role change writes its interval changes in the same transaction and no access exists outside an interval. User u receives a content event at seq k only if two things hold: k falls inside one of u's intervals for that channel, and u can still read the channel at delivery time. Events about u's own membership are always delivered. That second check runs again for queued socket events and for notification content. **Joining:** when u joins at S, u receives `member_added` at S, and the core backfills with `GET /v1/channels/{id}/messages?until_seq=S`. Backfill pages are tagged with the interval they serve. The core discards pages from an interval that has since closed, and records the pending backfill in the same SQLite transaction that advances its stream cursor, so a crash resumes the backfill instead of skipping it. **Merging:** the cache merges message state monotonically by revision and keeps tombstones for deletes, so a late page can never roll back an edit or bring back a deleted message. **Leaving:** u receives `member_removed`, and the core purges the channel. **Rejoining** opens a new interval. New members see the channel's full history; a setting to hide earlier history is left for later. DMs and group DMs are workspace-scoped conversations under the same rule. | Filtering against current membership alone loses history for new members. Filtering by interval alone replays content to someone who reconnects after being removed, and misses access that came from a role. The two checks together, with monotonic merge, cover every sequence found, at the cost of per-channel backfill only. | **A per-user membership epoch** that forces a full, authorized workspace snapshot at a watermark on any change. It is more general and avoids these races by construction, but every join then costs a whole-workspace rebuild. |
| 11 | **Real-time fan-out runs through an in-process hub** with a bounded queue per connection. A slow consumer is disconnected and resumes from its seqs. With several nodes, Postgres NOTIFY carries only the stream and seq, each node reads the rows itself, and a periodic replay repairs missed wakeups. | No new dependency. Because the socket isn't the source of truth, a lost notify costs latency, not data. | **Redis or NATS from day one.** Deferred until measurements justify them. |
| 12 | **Search sits behind a `search` module interface. v1 ships PostgreSQL full-text search** using the `simple` configuration plus `unaccent`, with GIN indexes over messages, file names and extracted file text. Results sort by recency, with rank as a tiebreaker. Search covers the channels the user can read now: joined channels, plus public channels for anyone who isn't a guest, which matches Slack. The access filter runs in the same SQL query, before ranking and snippets. **CJK search is a known v1 limit.** The committed fix is an embedded Tantivy index with CJK tokenizers behind the same interface. It is triggered by real demand for CJK search, or by p95 search latency above 500 ms on the M4 benchmark corpus, and when either fires it ships next, ahead of the post-v1 queue. Extraction is a bounded job covering plain text, Markdown, source code and PDF. Other formats are searchable by name only. | No new service, the permission check fails closed in one query, and contributors can work on it without special knowledge. Postgres doesn't segment CJK text, and `pg_trgm` can't index queries under 3 characters, so it gives no real fix. | **Tantivy now**: better ranking and CJK support, but a second store to keep consistent and replicate, before anyone has shown demand. **Elasticsearch or Meilisearch**: an extra service. |
| 13 | **Files are stored through the `object_store` crate**, on local disk by default with S3-compatible storage optional. Uploads stream, are capped by quotas, and are finalized atomically. **Every download goes through `commonsd`, which checks access on each request.** There are no long-lived signed URLs. Revocation takes effect when the next request is authorized; a download already streaming finishes. The desktop webview fetches through the core's `commons-file://` protocol, which is decision 4. | Removing a member cuts off file access immediately (criterion 4). There is one interface for both backends, and S3 isn't required on a small VPS. | **Signed URLs**: they stay usable after access is revoked. They are deferred as a later bandwidth optimisation with a TTL under 60 seconds. **Blobs in Postgres**: they bloat backups. |
| 14 | **Accounts are server-level, with workspace memberships.** Each user has one account per server and a membership per workspace, which carries a per-workspace profile and a role (owner, admin, member or guest). Passwords are hashed with Argon2id, and login and reset endpoints are rate-limited. Sessions are opaque tokens stored hashed, so they can be revoked. Desktop tokens live in the OS keychain. OIDC uses authorization code with PKCE, plus state and nonce checks, through the system browser and a `commons://` deep link. Identity is keyed by issuer and subject, and linking an OIDC identity to an existing account always takes an explicit action. Email goes out over SMTP through lettre. | Revocable sessions and standard OIDC, without depending on an outside identity service. One account per server fits "one server, several workspaces". | **JWT sessions**, which are hard to revoke. **One account per workspace**, which means duplicate logins on the same server. |
| 15 | **Message bodies are restricted Markdown** with structured entities stored beside the text: mentions, channel links and emoji. The server checks that every mention targets a member of the conversation. Clients render text nodes only, with no raw HTML. The envelope has `content_type` and `encryption` fields, so encrypted DMs can be added later without changing the envelope. Search and server-side notification rules won't apply to encrypted content. | Structured mentions drive notifications without parsing the text again. Sanitized rendering closes off script injection. | **A full rich-text block tree**: heavier for bots and clients. |
| 16 | **Notification policy is decided on the server; presentation happens on the client.** The notify module decides who should be notified from mentions, DMs, keywords, mutes and preferences, and emits deduplicated `notify` events on the user stream. The core decides how to present them on this device: it suppresses notifications for the channel in focus, follows the OS do-not-disturb setting, and shows the OS notification. Unread and mention counts come from durable read cursors. Mobile push reuses the same decisions later through an APNs/FCM relay. | One policy for every platform, already shaped for push. | **Each client deciding for itself**, which means reimplementing the policy on every platform. |
| 17 | **Huddles arrive later, with media outside `commonsd`.** The server authorizes callers and issues room tokens for a separately deployed WebRTC SFU, with LiveKit the leading candidate, picked at that milestone. Huddle state travels as channel events. | Media scales differently, and keeping it out keeps the chat server small. | **An SFU built into `commonsd`.** |
| 18 | **Bots use the same REST and WebSocket API** with scoped bot tokens, plus incoming and outgoing webhooks and slash commands, in a later milestone. | One API to document, which is the README's "open API". | **A separate bot protocol.** |
| 19 | **Self-hosting is `docker compose up -d`**, running PostgreSQL, `commonsd` and Caddy for automatic TLS. Before first run you need a domain pointed at the host. A LAN mode uses Caddy's internal certificate authority. First-run setup asks for the domain, the first admin and SMTP. A backup and restore command covers the database and file store. Logs come from tracing and metrics are exported for Prometheus. | It meets "single command", and Caddy handles TLS with no configuration. | **A bare binary with an embedded database**, which has no path to scale out. |
| 20 | **One cargo workspace with few crates**: `proto` (wire and view-model types, safe for WASM), `server` (a library with modules, plus the `commonsd` binary), `client-core`, `apps/desktop` (the Tauri shell), `apps/desktop-ui` (Leptos views) and `xtask` (dev tasks). Modules come first, and a module becomes its own crate only when there's a reason. `docs/` holds contributor docs, ADRs, the API compatibility policy and security reporting. | Fewer crates make building and finding your way around easier for newcomers. The boundary that matters, UI against core, is enforced by the crate graph. | **One crate per concern**: clean, but heavier to start with. |

## 4. Scope and acceptance criteria

**v1 delivers:**
- workspaces, public and private channels, DMs and group DMs;
- threads, mentions, reactions, edit and delete, and unread state;
- file upload and download;
- search over messages, file names and extracted text (CJK excepted);
- desktop notifications;
- email/password and OIDC sign-in;
- several server accounts in one client;
- a signed and notarized Mac app, with Linux and Windows builds in CI;
- the compose deployment, with backup and restore.

**Excluded:** huddles, the bot and webhook API, browser and mobile clients, push notifications, CJK-aware search, E2EE, federation and visual design.

**Acceptance criteria:**
1. On a fresh VPS with a domain, `docker compose up -d` followed by first-run setup works, and the Mac client can then sign in with email or OIDC.
2. A message sent in a channel appears in order on every connected member's client. It is still there after both the client and the server restart.
3. A client that was offline converges to the server's state after reconnecting, through replay or a snapshot, including after the log was pruned. A send queued while offline is delivered exactly once.
4. A user never receives an event, search hit, snippet or file from a channel they can't currently read. That includes content replayed after a removal or a role downgrade. When the check can't be made, access is denied. A download already streaming when access is revoked is allowed to finish.
5. A user added to a private channel or DM sees its history. A removed user receives nothing further from it, and the client drops its cached copy.
6. A slow or dead socket cannot grow server memory without bound.
7. On 2 vCPU / 4 GB with representative history, 500 concurrent clients stay within the latency budget set at M2. The proposed budget is p95 send-to-receive under 300 ms.
8. The Mac app, counting its own process plus its webview processes, stays within the measured memory budget. The proposed budget is under 300 MB with three server accounts and 20 workspaces. The app passes a VoiceOver and keyboard-only pass over the core flows.
9. A stale edit never overwrites newer text, and the user gets their edit back as a draft.

## 5. Implementation plan

Nothing exists yet, so "Verified today" for each step is the same: the repo holds only `README.md` and `TAGLINE.md` @ `09e136f`. Every path below is proposed.

### Step 1. M0: skeleton, walking slice and the webview spike

- **Change.**
  - The workspace and crates from decision 20.
  - CI running fmt, clippy, tests and cargo-deny.
  - `cargo xtask dev`, which starts Postgres in Docker and runs the server and the app.
  - Migrations, and email/password accounts.
  - One workspace with one channel: the send API, the workspace event stream and the WebSocket.
  - The desktop app signs in, sends, receives live, and keeps its history across a restart, over the windowed subscription contract.
  - **Spike:** a 10,000-message virtualized list receiving bursts of live updates. Measure RSS for the app plus its WebContent processes, check scroll smoothness, and do a VoiceOver pass.
- **Implements.** Decisions 1–4 and 6–9. The spike tests decision 1 before anything else is built on it.
- **Invariant.**
  - A reader never sees seq k+1 before k has committed.
  - No event is published without its row.
  - Failing toward a late event is acceptable; failing toward a lost one is not.
- **Depends on.** Nothing.
- **Check.**
  - An integration test on testcontainers Postgres. Concurrent writers run with forced commit delays while a reader resumes again and again; assert it never observes k+1 without k.
  - A test that drops a delta batch and injects a stale-generation batch, then asserts the window resets, its queues stay bounded and its rows are correct.
  - The spike's numbers are recorded in an ADR. If the webview fails the budget or the VoiceOver pass, stop and re-decide decision 1 with GPUI as the candidate.

### Step 2. M1: conversations and sync

- **Change.**
  - Public and private channels, DMs and group DMs, membership with roles.
  - Membership intervals, including intervals derived from roles; `member_added` and `member_removed`; backfill tagged by interval, with monotonic revision merge and tombstones.
  - Workspace join and removal events on the user stream.
  - The per-user stream.
  - Cursor pagination, the SQLite cache with several accounts, resume, snapshot at a watermark, log pruning, and the outbox with deduplication.
- **Implements.** Decisions 3, 5, 9, 10, 11 and 14 (roles).
- **Invariant.**
  - Events are filtered as of the event's seq. The filter fails toward deny.
  - Resending with the same client ID does nothing.
  - An unfinished backfill always resumes.
- **Depends on.** Step 1.
- **Check.**
  - A property test that randomly drops and reconnects clients, randomly grants, revokes, downgrades and rejoins memberships, and forces the pruned-log path. Assert three things:
    - every payload a client receives was readable by that user at delivery, not just that the final cache matches;
    - each client's cache ends equal to the server's state projected to that client;
    - `next_since` always advances, so no client loops on a fully filtered range.
  - Tests for the reconnect-after-removal sequence: join at 10, disconnect at 12, a message at 15, removal at 20, resume from 12. No content from 15 may be delivered.
  - Tests for a delayed backfill page arriving after a live edit, after a delete, and after removal. None may roll back or resurrect anything.
  - A test that a client crashing mid-backfill, including just before the pending marker is written, completes the backfill on restart.
  - A slow-consumer test showing that a stalled socket is dropped and server memory stays flat.

### Step 3. M2: messaging features and notifications

- **Change.**
  - Threads, reactions, and mentions as validated entities.
  - Edits with a revision check, delete wins over edit, and Markdown sanitized at render.
  - Read cursors and unread counts.
  - The notify module, plus core presentation rules and OS notifications.
  - Presence and typing through the hub.
- **Implements.** Decisions 5, 15 and 16.
- **Invariant.**
  - A stale edit is rejected, never applied.
  - A notification is deduplicated per message and user.
  - Raw HTML never renders.
- **Depends on.** Step 2.
- **Check.**
  - Unit tests on the notify rules.
  - Edit-race and edit-versus-delete tests.
  - A test that a script tag in a message body renders as text.
  - A goose load test with 500 clients. Measure sustained commits per second on one workspace and the drain time after a 1,000-write burst. This sets the latency budget and checks the throughput assumption in decision 9.

### Step 4. M3: files

- **Change.**
  - Streaming upload with quotas.
  - `object_store` with local and S3 backends.
  - Downloads proxied through `commonsd` with a membership check on every request.
  - Thumbnails.
  - The extraction job, with retry and crash recovery.
- **Implements.** Decision 13.
- **Invariant.**
  - Downloads fail closed.
  - A failed extraction never blocks the download.
- **Depends on.** Step 2.
- **Check.**
  - A test that a user removed after fetching a file once is denied on the next request.
  - A test that a 1 GB upload keeps server RSS flat.
  - A test that a worker killed mid-job retries the job.

### Step 5. M4: search

- **Change.**
  - GIN indexes and the query API with the membership join.
  - Snippets and the search UI.
  - A benchmark corpus.
- **Implements.** Decision 12.
- **Invariant.** Hits and snippets come only from channels the user can read now. That means joined channels, plus public channels for anyone who isn't a guest. New members see full history, which matches decision 10. The filter fails toward deny.
- **Depends on.** Steps 3 and 4.
- **Check.**
  - A test that private-channel text never appears for a non-member, or for someone removed from the channel.
  - The p95 latency measured on the corpus and recorded against the Tantivy trigger.

### Step 6. M5: v1 hardening

- **Change.**
  - OIDC, multiple workspaces, invites, the admin UI and first-run setup.
  - Compose with Caddy, including LAN mode.
  - Backup and restore, and metrics.
  - A signed and notarized Mac build; Linux and Windows CI builds.
  - The accessibility and memory gates, rerun on the full app.
  - Contributor docs and ADRs.
- **Implements.** Decisions 14, 19 and 20.
- **Invariant.**
  - An OIDC identity links to an account only by explicit action.
  - A revoked session is refused on the next request and on the open socket.
- **Depends on.** Steps 1–5.
- **Check.**
  - OIDC tests: a bad state is rejected, a bad nonce is rejected, and an intercepted code fails without the PKCE verifier.
  - Session-revocation tests.
  - A restore drill: back up, wipe, restore, and the clients converge.
  - A test that a missed NOTIFY is repaired by periodic replay.
  - Acceptance criteria 1, 7 and 8 run on a real VPS and a real Mac.
  - A test that file and image loads in the webview go through `commons-file://` and are denied after removal.

### Step 7. After v1, in order

Bots and webhooks (decision 18), then huddles (decision 17), then the browser client (decisions 1, 3 and 4), then mobile and push (decision 16). Tantivy search (decision 12) jumps the queue whenever its trigger fires. Each gets its own short plan.

## 6. Testing strategy

Every check below is proposed, since no test or command exists yet.

| Criterion | Check |
|---|---|
| 1 | A scripted VPS smoke test, plus a manual sign-in on Mac |
| 2, 3 | Integration tests on testcontainers Postgres; the reconnect, membership and prune property test from step 2 |
| 4, 5 | Per-surface permission tests (events, search, files), including after removal; the backfill test |
| 6 | The slow-consumer test |
| 7 | The goose load test |
| 8 | An RSS script that counts the WebContent processes; a VoiceOver checklist |
| 9 | The edit-race tests |

Required services: Docker for testcontainers and compose. The load and acceptance runs need a 2 vCPU / 4 GB VM and a Mac.

## 7. Constraints and open questions

**Questions for the user (non-blocking):**
- **License.** Must be chosen before the first release, and cargo-deny needs it configured. Affects step 6.
- **CJK search in v1.** The default is no: it is a documented limit, and Tantivy comes later. Affects step 5.
- **Hiding history from new members.** The default is that new members see full history. An option to hide what came before they joined is deferred. It would add a joined-seq filter to search, backfill and downloads.
- **Whether the project runs a mobile push relay.** Affects step 7 only.

**Assumptions to measure (non-blocking):**
- **Budget figures.** The latency budget (p95 under 300 ms) and memory budget (under 300 MB) are proposed numbers, to be set by measurement in M0 and M2.
- **Commit time.** Commit fsync on the VPS SSD takes about 2 ms or less, roughly 500 commits per second per workspace. Decision 9 rests on this. Step 3 measures it, and if it doesn't hold, the per-workspace counter is revisited.

**Webview fallback.** If the M0 spike fails, decision 1 is reopened with GPUI as the fallback. Decisions 2–4 hold either way.

## 8. For the implementer

- Before starting, confirm the repo still matches `09e136f` plus any README changes.
- Work through steps 1 to 6 in order. Each step ends with its Check passing, then a commit.
- Record as-built notes in a dated "As built" section at the end of this document.
- **Stop and raise it if:** the step 1 spike fails; the seq-ordering test can't be made to pass; or a decision turns out to be infeasible with the named crate. Each of these is drift from the decisions.
- **Decide alone:** crate versions, module names, endpoint names within `/v1`, UI layout and copy.

## 9. Verification status

- **Repo.** Inspected at `main` @ `09e136f`. It is greenfield, so no decision rests on existing code. Every path and command is proposed.
- **External claims.** These come from the agents' knowledge and have not been checked against current releases or documentation in this session:
  - Tauri 2 channels and custom protocols;
  - Leptos;
  - utoipa;
  - `object_store`;
  - Tantivy CJK tokenizers;
  - that `pg_trgm` can't index queries under 3 characters;
  - that the PostgreSQL `simple` configuration doesn't segment CJK;
  - GPUI's accessibility and API stability.

  Step 1's spike and step 5's benchmark exist to test the claims that matter most.
- **Review.** Both agents checked the plan in two review rounds. The final round's repairs went in after that round and were not re-checked by the agents. They cover decision 10 (access from roles, the current-access check, the backfill merge rules), the `commons-file://` protocol, the throughput wording, the search scope and the related checks. Each repair is one an agent proposed, adopted as given.

## Decisions before implementation (2026-11-17)

- **Scope.** Build steps 1–5 now. Step 6 (v1 hardening) and step 7 are left for later, and the As built note lists them as not done.
- **Step 1 gate.** The webview spike runs and its numbers go into the ADR, but failing to meet the budget does not stop the build. The VoiceOver pass is left to the user and is not a condition for moving past step 1. If the memory or scrolling numbers miss the proposed budget, record that plainly in the ADR and in As built, then continue.
- **Toolchain.** The user has authorized the implementer to install what the build needs: `brew install rustup` (or rustup-init) with the `wasm32-unknown-unknown` target, and `cargo install` of trunk (or whatever Leptos build tool the step uses), tauri-cli and cargo-deny. Homebrew's `rust` formula is already installed; resolve any conflict between the two, and record the commands you ran under As built.
- **Where the plan lives.** The first commit copies this plan to `docs/architecture.md` (`docs: add architecture plan`). From then on, `docs/architecture.md` is the copy that gets committed, and the As built note goes there. `.agents/tasks/` is git-ignored on this machine and must never be committed.
- **License.** Still undecided (open question). cargo-deny checks the licenses of dependencies only. Do not add a LICENSE file, and do not set a `license` field beyond leaving it unset.

## As built (2026-11-17)

Scope changed during the build. The user stopped the work after step 3. Steps 4 and 5 go to a new implementer, and steps 6 and 7 are not done.

**Steps**
- **Step 1, done.**
  - Built: the workspace and crates; CI (fmt, clippy native and wasm32, `cargo xtask check-deps`, tests, cargo-deny); `cargo xtask dev`; migrations; email and password accounts; the transactional workspace stream; the WebSocket; the desktop app over windowed subscriptions.
  - Check: the seq-ordering test, `crates/server/tests/seq_order.rs`, passes, and so do the window tests (dropped batch, stale generation, unacknowledged cap, oversized delta).
  - Spike, recorded in `docs/adr/0001-webview-spike.md`. Memory is **370 MB RSS, which misses the 300 MB budget, and 186 MB physical footprint, which meets it**. Frame and paint timing was **not measured**, because the screen was locked. The VoiceOver checklist is left for the user.
- **Step 2, done.**
  - Built: channels, DMs and roles; intervals, including role-derived ones; the user stream; snapshots; pruning; backfill by interval with monotonic merge; the outbox; several accounts.
  - Checks pass: the property test (200 cases once, 24 by default); the reconnect-after-removal test (10/12/15/20 exactly); role downgrade; next_since advances; late backfill pages; the crash before and during backfill; the slow consumer (1 MB growth).
- **Step 3, done.**
  - Built: threads, reactions, validated mentions, edits with a revision check, delete wins, read cursors, the notify module, core presentation, OS notifications, presence and typing.
  - Checks pass: the notify-rule unit tests; edit race and edit versus delete; script tag rendered as text. The goose load test ran; see `docs/adr/0002-load-test.md`:
    - p95 send-to-receive is 727 ms, which misses the proposed 300 ms;
    - sustained throughput is 271 to 405 commits/s on tmpfs and 65 on virtiofs disk, below the ~500 assumed in decision 9;
    - the 1,000-write burst drains in 5.7 s.
- **Steps 4 (files) and 5 (search): not done; handed to a new implementer.**
- **Steps 6 (v1 hardening) and 7: not done.**

**Deviations, and why**
- **sqlx facade dropped.** The server uses `sqlx-core` and `sqlx-postgres` directly, through `crates/server/src/sqlx.rs`, with migrations embedded in `db.rs`.
  - Why: the `sqlx` facade lists `sqlx-sqlite` as an optional dependency, and cargo still checks its `links = "sqlite3"` key against rusqlite's.
  - That conflict pinned the client to libsqlite3-sys 0.30, which is affected by RUSTSEC-2026-0459.
  - The client now uses rusqlite 0.41 with libsqlite3-sys 0.39.1.
  - Queries are checked at runtime, not compile time; decision 8's rationale mentioned compile-time checks.
- **chrono replaced by time.** RUSTSEC-2026-0543 marks chrono unmaintained.
- **Unmaintained transitive crates are allowed.** `deny.toml` sets `unmaintained = "workspace"`. This covers paste (via leptos), proc-macro-error (via gtk) and astral-tokio-tar (via testcontainers).
- **Fan-out filters in memory.** Decision 11 is built as one feed per workspace per node (`feed.rs`), and each socket filters batches against its user's access set. The first version ran SQL per socket and failed the load test. `socket_filter.rs` proves the in-memory filter equals the SQL replay.
- **Workspace events commit one at a time.** The core commits each workspace event together with its own cursor advance, rather than one commit per page. A per-page commit applied a removal followed by a rejoin wrongly; the property test found it.
- **Mentions of non-members are dropped, not rejected.** They are stored as plain text and notify nobody, so a send never fails over a mention.
- **The UI's dependency rule is enforced by `cargo xtask check-deps`** (the wasm32 dependency graph), not by cargo-deny.

**Assumptions (routine choices)**
- Versions: Tauri 2.12 (3.x is alpha), Leptos 0.8, axum 0.8, sqlx-core 0.8.6, trunk as the Leptos build tool, testcontainers 0.27 with Postgres 16-alpine on tmpfs, goose 0.19.
- **Test Postgres uses tmpfs** because the Docker VM disk is full. I did not prune the user's 51 GB of unused volumes.
- The message list is a bounded window of 300 rows with variable row heights. It moves by 100 rows at either end and keeps the first visible row anchored.
- Decisions about the OS:
  - do-not-disturb is left to the OS, since macOS Focus filters notifications;
  - presence counts connections per node;
  - workspace members are added by email (invites are step 6);
  - the server ignores `--role` (decision 7 leaves it for later).
- Endpoints are under `/v1`; see `docs/api/openapi.json`, regenerated with `cargo xtask openapi`.

**Toolchain commands run**
- `~/.cargo/bin/rustup` was already installed. Homebrew's `rust` shadows it on PATH, so builds ran with `export PATH=$HOME/.cargo/bin:$PATH`.
- `rustup target add wasm32-unknown-unknown`
- `cargo install --locked trunk tauri-cli cargo-deny`, which installed trunk 0.22.1, tauri-cli 2.12.5 and cargo-deny 0.21.0.
- `cargo tauri icon` generated the placeholder icon.

**Loose ends from step 3 the next implementer must know**
- **Latency and throughput budgets are not met locally.** The VPS rerun and profiling are listed in ADR 0002: the pool cap of 32, one notify job per message, and per-socket framing.
- **Files are stubbed.** `messages::check_attachments` refuses attachments and `attach_files` is a no-op, so step 4 replaces both. The UI's Attach button and the `commons-file://` protocol are not wired.
- **No tests yet for presence or typing.** Typing is checked against access on send and on delivery.
- **Shared resources need care:**
  - the jobs worker loop is shared, so new job kinds go in `jobs::handle`;
  - tests that need determinism set `job_workers = 0` and call `jobs::run_one`.
- **Socket cursors.** A client hello with a cursor ahead of the head gets `resync` and a disconnect, by design.
- **Spike leftovers.** The spike's Tauri window stays on top in spike mode only. Its frame and paint numbers still need a run on an unlocked screen: `caffeinate -du cargo xtask spike`.
- **Open questions for the user:**
  - Which memory measure does acceptance criterion 8 use, RSS or physical footprint?
  - The license is still open.

## As built, steps 4–5 (2026-11-18)

A second implementer built steps 4 and 5 on `f2e255a`. Before starting, I confirmed the stubs As built describes: `messages::check_attachments` refused every attachment, `attach_files` did nothing, the Attach button invoked a command that did not exist, and no `commons-file` protocol was registered.

**Steps**
- **Step 4, done.** Uploads stream into `object_store` (local disk by default, S3 when `COMMONS_S3_BUCKET` is set), capped by the per-file limit and the uploader's quota. Downloads are proxied through `commonsd` with an access check on every request. Thumbnails and text extraction (plain text, Markdown, source code, PDF) run as jobs through `jobs::handle`. The desktop serves attachments to the webview through `commons-file://`, and Attach uploads a file and sends it.
  - Checks pass (`crates/server/tests/files.rs`, `upload_memory.rs`, `crates/client-core/tests/files.rs`):
    - a member removed from the channel, or from the workspace, after one successful fetch gets 404 on the next request;
    - a 1 GiB upload raised test-process RSS by 34 MB, which is the bounded multipart buffer (4 parts of 5 MB) plus the client, not the file size;
    - a job whose worker died holding the lease is reclaimed after the lease expires (attempt 2) and completes;
    - a failed PDF extraction leaves the download working.
- **Step 5, done.** PostgreSQL full-text search over messages, file names and extracted text, through the `commons_search` configuration (`simple` plus `unaccent`) with GIN indexes. One SQL statement applies the access filter, then ranks the hits and builds snippets for the returned page. The core turns hits into view models with marked snippets, and the desktop has a search box per workspace.
  - Checks pass (`crates/server/tests/search.rs`, `crates/client-core/tests/search.rs`). Private-channel text (messages, file names, file text) never reaches:
    - a member who isn't in the channel;
    - a guest;
    - someone removed from the channel or the workspace;
    - an admin after a downgrade.
    Guests see only channels they joined, and new members see earlier history.
  - Benchmark (`search_bench.rs`, ADR 0003): 1,000,000 messages and 20,000 files. With one searcher at a time, p95 is **257 ms**, so the 500 ms Tantivy trigger does not fire. With eight concurrent searchers, p95 is **528 ms**. Common words are the slow case.

**Deviations, and why**
- None from the decisions.
- `SearchHit` gained `conversation_name` and `file_name`. A hit can come from a public channel the user hasn't joined, and the client has no cached name for that channel.

**Assumptions (routine choices)**
- Versions: object_store 0.14 (aws feature), image 0.25 (png, jpeg, gif, webp), pdf-extract 0.12, mime_guess 2, tauri-plugin-dialog 2.8.
- Who can download a file:
  - the conversation must be readable now, which is `conversations::can_read_now`, the membership check step 4 names;
  - and the file must be on a message that isn't deleted, or be the caller's own unsent upload.
- Search scope is wider than the download check: it adds public channels for non-guests. A file hit in an unjoined public channel can't be downloaded until the user joins. The UI's "Join and open" does that.
- Upload API: `POST /v1/conversations/{id}/files?name=` with the raw bytes as the body. The server guesses the MIME type from the name.
- A message carries at most 10 files. Each file must be the sender's own unsent upload to the same conversation, and it goes out on one message only.
- Atomic finalize: the object is completed first, then its row is inserted. That insert re-checks access and the quota under a lock on the uploader. On any failure the object is deleted.
- Quota counts every file the user uploaded, including files on deleted messages.
- Limits:
  - thumbnails are 320 px PNGs, made from images up to 40 MB with decoder limits;
  - extraction reads files up to 50 MB, runs for at most 60 s, and keeps 256 KiB of text, because Postgres caps a tsvector at 1 MB.
- `file_updated` is emitted only when the thumbnail lands on a file that is already on a live message. A row lock orders this against a send that attaches the file.
- The desktop protocol renders only PNG, JPEG, GIF and WebP inline. Everything else goes out as `application/octet-stream` with `attachment`, `nosniff` and a sandbox CSP. The server sends the same headers.
- Attach picks one file with the native dialog, uploads it, then queues a message carrying it with an empty body. Uploads go straight to the server, not through the outbox, so attaching needs a connection.
- Search:
  - one endpoint per workspace, `GET /v1/workspaces/{id}/search?q=&limit=&offset=`;
  - `websearch_to_tsquery` syntax;
  - 20 hits per page by default, at most 50 per page and an offset of at most 1,000;
  - the module interface is the `search::search` function, which a Tantivy backend would replace.

**Not done, or not verified**
- **S3 is built but not tested.** There is no S3 or MinIO test, because the Docker VM disk is nearly full.
- **The desktop UI was not exercised in a running app.** That covers Attach, thumbnails over `commons-file://`, download links and the search panel. Clippy (native and wasm32) and `trunk build` pass, and the protocol's path parsing has a unit test. Step 6's webview check, that loads go through `commons-file://` and are denied after removal, is still to do.
- **Files are never reclaimed.** Objects for deleted messages, and abandoned unsent uploads, are not deleted.
- **Files load whole.** `commons-file://` buffers each file in memory, because Tauri's protocol responder takes a whole body. There are no HTTP range requests.
- **URL form on Linux and Windows is untested.** `views::file_url` uses the macOS form, `commons-file://localhost/…`, and Windows uses `http://commons-file.localhost/…`.
- **Search is measured locally only.** The VPS run for ADR 0003 is still to do. CJK remains the known v1 limit.
- **Open question for the user:** does the Tantivy trigger's p95 mean one searcher at a time, which reads 257 ms, or concurrent load, which reads 528 ms with eight searchers?
