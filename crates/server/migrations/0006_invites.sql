-- Single-use invite links to a workspace. Only the token's hash is stored.
CREATE TABLE invites (
    id           uuid PRIMARY KEY,
    token_hash   bytea NOT NULL,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    role         text NOT NULL CHECK (role IN ('member', 'guest')),
    created_by   uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz,
    used_by      uuid REFERENCES users(id) ON DELETE SET NULL,
    revoked_at   timestamptz
);
CREATE INDEX invites_workspace ON invites(workspace_id, created_at DESC);
