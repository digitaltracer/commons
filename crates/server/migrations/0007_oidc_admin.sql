-- OIDC sign-in, operator account controls and member join dates (step 6).

-- Accounts made through OIDC have no password until one is set.
ALTER TABLE users ALTER COLUMN password_hash DROP NOT NULL;
-- A deactivated account can't sign in; its sessions are revoked when it is deactivated.
ALTER TABLE users ADD COLUMN deactivated_at timestamptz;

-- Rows from before this migration carry the migration's time.
ALTER TABLE workspace_members ADD COLUMN joined_at timestamptz NOT NULL DEFAULT now();

-- An identity at an OIDC provider, keyed by (issuer, subject), signs in one account.
CREATE TABLE oidc_identities (
    id             uuid PRIMARY KEY,
    user_id        uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    issuer         text NOT NULL,
    subject        text NOT NULL,
    email          text,
    email_verified boolean NOT NULL DEFAULT false,
    created_at     timestamptz NOT NULL DEFAULT now(),
    last_used_at   timestamptz,
    UNIQUE (issuer, subject)
);
CREATE INDEX oidc_identities_user ON oidc_identities(user_id);

-- A sign-in in progress at the provider, keyed by the state's hash. Single use, 10 minutes.
CREATE TABLE oidc_flows (
    state_hash    bytea PRIMARY KEY,
    nonce         text NOT NULL,
    pkce_verifier text NOT NULL,
    return_to     text NOT NULL,
    app_challenge text NOT NULL,
    invite        text,
    link_user_id  uuid REFERENCES users(id) ON DELETE CASCADE,
    expires_at    timestamptz NOT NULL
);

-- A finished flow waiting for the app to exchange its code. Single use, 2 minutes. A link
-- carries the identity, which is attached only at the exchange.
CREATE TABLE oidc_handoffs (
    code_hash      bytea PRIMARY KEY,
    user_id        uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind           text NOT NULL CHECK (kind IN ('session', 'linked')),
    app_challenge  text NOT NULL,
    expires_at     timestamptz NOT NULL,
    issuer         text,
    subject        text,
    email          text,
    email_verified boolean NOT NULL DEFAULT false
);
