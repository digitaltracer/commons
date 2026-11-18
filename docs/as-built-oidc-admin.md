# As built, OIDC and admin (2026-11-19)

Plan step 6, in part: OIDC sign-in (decision 14), the workspace admin screen, and the operator's account-recovery commands. This note sits beside `docs/architecture.md` rather than in it, so it can be committed alone.

## Phase A: server

**OIDC sign-in.**
- **Configuration.** `COMMONS_OIDC_ISSUER`, `COMMONS_OIDC_CLIENT_ID`, `COMMONS_OIDC_CLIENT_SECRET` and `COMMONS_OIDC_NAME`. OIDC is on only when all four are set. With some set, it stays off, and the startup log names the missing ones. `/v1/meta` carries `oidc: {name}` when it is on. The secret is left out of `Debug`.
- **Discovery.** The provider is found by discovery under the issuer. Its `issuer` must equal the configured one, and its endpoints must be https (http only to loopback, for tests). Discovery is cached for an hour. The JWKS is cached, and an unknown `kid` refetches it at most every 30 s.
- **Flow.** Server-side authorization code with PKCE (S256), `state` and `nonce`. `POST /v1/auth/oidc/start` stores the state hash, nonce, PKCE verifier, return target, the app's challenge, the invite and the link intent in `oidc_flows`, for 10 minutes. The callback consumes the row with `DELETE … RETURNING`, so a state works once. The redirect URI is `{COMMONS_PUBLIC_URL}/v1/auth/oidc/callback`.
- **id_token checks.** The signature is checked against the JWKS, with asymmetric algorithms only, and only those discovery lists. The `iss`, `aud`, `exp` (60 s leeway) and `sub` claims are required. The nonce must match. With several audiences, `azp` must be this client.
- **Accounts.** An identity is keyed by (issuer, subject) in `oidc_identities`, with `email` and `email_verified` stored.
  - A known identity signs its account in.
  - An unknown one makes a new account. Registration mode applies, through the same invite and first-account code that `register` uses, under the same workspace lock: `closed` refuses, and `invite` needs a valid invite.
  - The new account has no password (`users.password_hash` is now nullable).
  - An email that matches an existing account is refused with `email_taken`, telling the person to sign in with their password and link the provider in Settings. **Nothing is ever linked by email.**
- **Linking.** Linking happens only through an explicit intent: `link: true` at start, with the caller's session. Unlinking (`DELETE /v1/me/identities/{id}`) is refused with `last_sign_in_method` when the account has no password and no other identity. The user row is locked, so two concurrent unlinks can't both pass the check. `GET /v1/me/identities` lists the identities and `has_password`.
- **Handoff to the app.** The callback never puts a session in a URL. It redirects (303) to the app's loopback target with a one-time handoff code, valid for 2 minutes, or with `error` and `message`. `POST /v1/auth/oidc/exchange` trades the code, plus the app's PKCE verifier, for a session, which is created only then. A wrong verifier spends the code. Return targets must be `http://127.0.0.1:<port>/…` or `http://[::1]:<port>/…` (RFC 8252). `localhost`, https, other hosts and a missing port are refused. A `commons://` deep link is later work: unbundled dev builds can't register a URL scheme.

**Operator commands.** `commonsd admin` takes `list-users`, `reset-password <email>`, `revoke-sessions <email>`, `deactivate <email>`, `reactivate <email>` and `make-owner <email> <workspace>`.
- Each command prints its result and logs a `commons::admin` line on stderr. A failure exits non-zero.
- `reset-password` reuses the emailed reset: a `password_resets` row, valid here for 24 hours, and a link `{public_url}/reset#<token>`. It also revokes every session. The server now serves `/reset`, a small form that reads the token from the fragment, so the token stays out of access logs, and posts it to `/v1/auth/reset/confirm`. It also reads the `?token=` of emailed links.
- `deactivate` sets `users.deactivated_at` and revokes every session, whose NOTIFY closes the account's sockets on every node. Password login, OIDC sign-in, the OIDC exchange and every bearer request check the flag.
- `make-owner` takes a workspace id or exact name. It promotes a member, or joins a non-member as owner, through the same role-change code as the endpoint.
- The runbook (`docs/deploy.md`) gives the invocation for compose (`docker compose exec`) and for systemd (`systemd-run --pipe` with the unit's `EnvironmentFile`).

**Workspace admin endpoints.**
- `GET /v1/workspaces/{id}/admin/members` (owners and admins) returns role, join date and online state. Online is per node, as the presence endpoint is.
- `workspace_members.joined_at` is new. Rows from before the migration carry the migration's time.
- **Role rules.** Only an owner grants or removes owner, which was already so. New: a workspace keeps at least one owner. Demoting, removing or self-removing the last owner is refused with `last_owner`.
- Role changes moved into `directory::change_role`, which the endpoint and `make-owner` share.
- Remove, the invites list and revoke, and the guest or member toggle use the existing endpoints.

**Deviations**
- **`POST /v1/auth/oidc/start` instead of `GET`.** The invite secret would otherwise sit in a query string and land in access logs, and a link needs the caller's bearer token. The app starts the flow over HTTP and opens the returned `authorize_url`.
- **The app's half of the handoff is PKCE too.** This guards against another local program catching the loopback redirect, and against a login CSRF that hands the victim's app an attacker's code.
- **A link is made at the exchange, not at the callback.** The exchange needs both the app's verifier and the session that started the link. Otherwise someone could start a link on their own account, have a victim finish it at the provider, and get the victim's identity attached to their account. The callback only checks that the link can work, so the person hears of a conflict at once.
- **The test server's `public_url` is its real address** (`testing::spawn_with` binds first), so links and the OIDC redirect point at it.

**Assumptions (routine choices)**
- Unverified provider emails are accepted for new accounts, with `email_verified` stored. A provider that marks emails unverified could take an email first. This doesn't matter for Google.
- A known identity that arrives with an invite redeems it, unless the account is already a member, so a pasted invite isn't silently lost.
- The token request uses `client_secret_post` when discovery lists it, which Google does, and otherwise `client_secret_basic`, form-encoded.
- The authorize request sends `prompt=select_account`.
- `jsonwebtoken` 12 on the aws-lc-rs backend, already in the tree through rustls. The `rust_crypto` backend would bring in `rsa`, which carries an unfixed advisory. reqwest 0.13 with rustls is now a server dependency, and url 2.

**Checks** (`crates/server/tests/oidc.rs` and `admin.rs`, against an in-process fake provider with discovery, an RS256 JWKS and a token endpoint that enforces PKCE):
- the happy path, including the same identity signing in again;
- bad state, reused state and an expired flow;
- a bad nonce, the wrong audience and an expired token, with nothing stored;
- an intercepted code, redeemed directly without the verifier and injected into an attacker's flow;
- a reused handoff code, and a wrong app verifier;
- loopback-only return targets;
- invite required, a valid invite redeemed with its role, and closed mode;
- no auto-link by email;
- explicit link and unlink, identity in use, and the unlink guard;
- a link made only at the exchange, and only by the session that started it;
- a deactivated account refused;
- every operator command, through the real `commonsd` binary too;
- the role rules (owner-only, last owner), the guest toggle, and join dates and presence in the admin list.

**Not done, or not verified**
- Nothing ran against Google itself. The fake provider stands in, so the user's first real sign-in is the first live test.
- Session expiry is still to come, as the Review note says.
- `oidc_flows` and `oidc_handoffs` are pruned of expired rows on each start, not on a timer.

## Phase B: client

**Core (`client-core`).**
- `oidc::run` binds `127.0.0.1:0`, starts the flow with its own PKCE challenge, and has the shell open the provider in the system browser. It waits for `GET /oidc?…`, for at most 5 minutes or until cancelled. The browser gets a small "close this tab" page, or the refusal's message, and other paths get a 404. Then it exchanges the code with its verifier.
- `Core` adds `server_meta`, `add_account_oidc` (an invite passes through to the server), `sign_in_again_oidc` (keeps the cache and outbox, and refuses and logs out a session for a different user), `link_oidc` and `cancel_oidc`. Only one flow runs at a time; a new one cancels the old.
- Nothing is saved unless the flow finishes: the account, token and `accounts.json` are written only after a successful exchange, through the same `adopt` step as password sign-in.
- Errors reach the UI as the server's message, or as "took too long", "cancelled" or "couldn't open the browser".

**Shell.** New commands: `server_meta`, `oidc_sign_in`, `oidc_sign_in_again`, `oidc_link`, `oidc_cancel`, `identities`, `unlink_identity`, `admin_members`, `set_member_role`, `remove_workspace_member`, `list_invites` and `revoke_invite`. The browser opens with `open` on macOS, `xdg-open` on Linux, and `url.dll` on Windows.

**UI (`desktop-ui`).**
- **Sign-in and sign-up.** "Continue with ‹name›" appears once the typed server advertises a provider, checked 400 ms after the address stops changing. A pasted invite goes with it. While the browser is open, a note and a Cancel button replace the button.
- **Signed out.** The same button is on the signed-out card and in the "Sign in to …" sheet.
- **Settings** (new footer button, "Account settings"). Name and email; ways to sign in (password set or not, and each linked identity with **Unlink ‹name›**); **Link ‹name›**, or "Link another ‹name› account".
- **Manage workspace** (new header button, owners and admins).
  - Members, with avatar, presence, join date and a role picker (Owner only for owners, and owners locked for admins).
  - Remove, with an inline confirm.
  - Invites, with state (Active, Used, Revoked or Expired), creator and expiry, and Revoke; plus Invite people.
  - The guest or member toggle is the role picker.
  - Server refusals, such as `last_owner`, show in an alert, and the picker snaps back to the stored role.
- New styles use the existing `style.css` tokens. Every control is a labelled button or select.

**Also changed.** The fake provider moved from the server test into `server::fake_oidc` (testing feature) so the core tests share it. `commonsd admin` logs at `warn,commons::admin=info` by default, which hides sqlx's migration notices.

**Checks** (`crates/client-core/tests/oidc.rs`):
- a provider sign-in adds the account;
- a refused one stores nothing;
- an invite reaches the new account;
- a link, then signing in again after the operator's `revoke-sessions`;
- a different user's sign-in is refused, and its session ends;
- cancel and timeout.

**Run in the app** (`cargo tauri build --debug --no-bundle`, which is what `cargo xtask app` builds). The app ran from a throwaway `HOME` with file secrets, against a throwaway harness: the fake provider and a testcontainers server. Screen capture isn't permitted in this session. A temporary debug hook, never committed, ran JS in the webview, reported DOM text through `ui_log`, and replaced the system browser with `curl -L`. Every one of these worked:
- the provider button appears only when the server offers OIDC;
- Continue with Google creates the account and connects it;
- Settings lists the password as not set and the Google identity, and Unlink is refused as the only way in;
- Link shows the waiting note, and Cancel gives "The sign-in was cancelled.";
- Manage workspace lists members with join dates and presence;
- demoting the last owner is refused, and the picker snaps back;
- member to guest, revoking an invite, and removing with the confirm;
- `commonsd admin list-users` and `revoke-sessions` against the harness database: the app went to signed out at once, and Continue with Google on the card signed it back in;
- Google with an existing email is refused with the link hint, and nothing is stored;
- an invite link made in the app, pasted with Continue with Google, creates a new account in that workspace.

**Found and fixed by running it.**
- The role picker kept a refused value. It is now bound to the reloaded list.
- "Link Google" still showed when Google was already linked. It now reads "Link another Google account".
- A Google-only account's signed-out card offered only a password. It now offers the provider too.

**Not exercised, or not done**
- The real `open` of the system browser, and therefore a real browser's handling of the loopback page.
- VoiceOver over the new dialogs.
- Pixel screenshots.
- A `commons://` deep link, kept as later work.
- The admin screen lists invites, but making one still goes through the existing Invite people dialog.
