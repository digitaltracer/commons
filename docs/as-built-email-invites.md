# As built: invites bound to an email address (2026-11-19)

An invite is made for one email address, and only an account with that address can use it. Before this, anyone holding the link could.

**Server.**
- `POST /v1/workspaces/{id}/invites` takes a required `email`. It is stored normalised, with the same trim and lower-case as accounts. A missing field gets 422 and an unusable one gets 400. Inviting an address that is already an active member of the workspace gets 409 `already_member`.
- Several open invites to one address in one workspace are allowed. That was the simpler choice: nothing has to be revoked behind the admin's back, and each invite still works once.
- Migration 8 adds a nullable `invites.email`. Rows from before it have none, and every check treats them as `invite_invalid`. New rows always have one, which the handler enforces. A `NOT VALID` check constraint was not used, because it would also refuse revoking an old row.
- The check runs in the transaction that holds the workspace's membership lock and the invite row:
  - **Password sign-up**: the typed email must match. The check runs after the token checks and before the password hash, so a mismatch costs no hash.
  - **OIDC sign-up** (a new account): the provider's email must match and be `email_verified`. Otherwise the refusal is the same, and no user or identity is stored.
  - **An existing account** (`POST /v1/invites/accept`, or an existing OIDC identity signing in with an invite): the account's stored email must match.
- Every mismatch is 403 `invite_email_mismatch`, "This invite was made for a different email address." It never names the invited address.
- `Invite` in listings carries `email`, null for old rows.

**Desktop app.**
- Invite people has a required email field above role and expiry. It is checked in the dialog, then by the server. After a link is made, the dialog shows "Invite for x@… · expires …".
- The invites list in Manage workspace shows each invite's email, role, creator, expiry and state. Old invites read "No email address" and **Not valid**, with no Revoke.
- Sign-up or join with someone else's link shows the server's mismatch sentence. The link carries no email, so the form can't fill it in.

**Checks.**
- `crates/server/tests/invites.rs`: create without an email and with invalid ones; register with the matching email, a different one, and a different case with spaces; an existing user accepts with the matching and a different email; an old invite is refused on register and on accept; a member invited again; a mismatch costs no hash.
- `crates/server/tests/oidc.rs`: a different address, the matching address unverified, and the matching address verified in another case; an existing identity with someone else's invite.
- `crates/client-core/tests/desktop_ui.rs`: a sign-up with someone else's link fails without naming the address and leaves no account.
- `desktop-ui`: a unit test for the dialog's email check.

**Weakness.** There is no SMTP, so a password sign-up proves only that the person knows the invited address and holds the link, not that they own the mailbox. Someone who has the link and guesses the address gets in. A link holder can also try addresses against the mismatch refusal, limited only by the per-IP register limit. Google sign-up does prove ownership, through `email_verified`.

## Invite links open the app (2026-11-19)
An invite link now opens a page with **Open in Commons**, which opens the installed app on a filled-in screen. Before this, the page told people to copy its address into the app.

**Server.**
- `/invite` (and the older `/invite/{token}`) is a server-rendered page with a small inline script, in light and dark. The script reads the token from the fragment (or the old path), checks its shape, and builds `commons://invite?server=<page origin>#<token>`. **Open in Commons** is a button, not a redirect, because Safari shows an error dialog when no app takes the scheme. **Download for Mac** goes to `COMMONS_APP_DOWNLOAD_URL`, by default `https://github.com/digitaltracer/commons/releases/latest`. The paste-it-yourself instructions stay as a fallback line.
- The page sends `no-store`, `no-referrer` and its own `Content-Security-Policy`: `default-src 'none'` and a `script-src` with the script's sha256. nginx sets no CSP, so the page needs no nginx change.
- `POST /v1/invites/preview` takes `{token}` in the body, so the token stays out of request lines and logs. It returns the workspace name, the inviter's display name, the role, the expiry, the email masked as `ad•••@gmail.com`, and the server's OIDC provider name. It is unauthenticated and limited per client IP like register.
- The preview runs the same checks as an accept: secret, email bound, the creator still an owner or admin, not revoked, used or expired. It takes neither the workspace's advisory lock nor the invite's row lock, so it never waits for an accept or blocks one. Every refusal, including a malformed or over-long token, is one 403 `invite_invalid`: "This invite link isn't valid. It may have expired, been used or been revoked. Ask for a new one."

**App.**
- `tauri-plugin-deep-link` registers the `commons` scheme. The shell keeps the latest link and tells the UI, and the UI takes it with `take_invite_link`. The same pull runs when the UI starts, which covers the link that launched the app. On macOS a link reaches the running instance, so no single-instance plugin is needed.
- The core parses the link strictly: only `commons://invite`; exactly one `server` parameter; the server an `https` origin with no user info, path, query or fragment; `http` only for `localhost` and `127.0.0.1` in debug builds; a token of the invite shape; at most 2048 characters. Refusals are short sentences that never echo the link. The token is never logged; the shell logs only "commons:// link received".
- A valid link costs one request, the preview. The screen it opens then depends on this device's accounts on that origin:
  - none: the sign-up form, with the server and invite filled in and hidden. It shows "Join <host>", who invited which masked address to which workspace as what role, the expiry, and a **New server** notice. **Sign in with <provider>** (when the server has one) or a password sign-up finishes it. "Create a new account" can be unticked to sign in to an existing account on that server instead.
  - one: the join panel, filled in, with "Joining as <email>".
  - several: the join panel with a choice of account.
  - The join panel also has **Use another account**, which switches to the filled-in form, for an invite made for an address none of the accounts has.
- A link never submits anything, joins anything or changes the open workspace; the person clicks Create account, Sign in with Google or Join. The provider name comes from the preview, so an opened invite sends no separate `/v1/meta`.
- Pasted links work as before, in both the add-account form and the join panel.

**Security rules, as built.** Links are untrusted: any page can fire one, and macOS doesn't say which. So a link only fills in a form, the server's host is the screen's heading, a server with no account in this app gets the New server notice, and nothing beyond the read-only preview is sent until a click.

**Checks.**
- `crates/server/tests/invites.rs`: the page and its CSP hash on both routes, with an escaped download URL; a preview's fields, that previewing doesn't use the invite, and that it is refused once used; expired, used, revoked, junk, empty, wrong-secret, unknown-id and over-long tokens all give the same body; a preview returns while an accept holds both locks. A unit test covers the email masking.
- `crates/client-core/src/deeplink.rs`: accept and reject cases for the parser, and the same-origin check.
- `crates/client-core/tests/oidc.rs`: a link previews, lists no accounts, and signs up through the fake OIDC provider into the workspace; a second link on the same server lists that account; a used link is refused; password sign-up with an opened link; bad links fail before any request.
- `desktop-ui`: the choice between sign-up, join and picking an account.
- By hand, on a debug bundle with its own bundle id, against a local commonsd: the page in light and dark, and without a token; a cold start from a link onto the sign-up form with the New server notice; expired, revoked, malformed, `http://evil.example` and non-invite links while running (refused, with only the preview sent for the first two); a password sign-up from the form; a second invite opening the join panel with no notice; the account picker; pasted links. The server log showed only `POST /v1/invites/preview` for each link.

**Not verified.**
- The browser's own hand-off: Chrome's "Open Commons?" prompt and Safari's behaviour with and without the app installed. The page's script was run on its own to get the link it builds, and that link was opened with `open`.
- Google sign-up from an opened link in the real app. It was checked only against the fake OIDC provider in the tests.
- A release (signed, notarized) build, and links on the deployed server.
- The scheme registers only in a bundled build (`cargo tauri build`), when macOS sees the app's Info.plist. `cargo xtask dev` and `cargo xtask app` run the bare binary, so `commons://` links don't reach them.
