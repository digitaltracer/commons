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
