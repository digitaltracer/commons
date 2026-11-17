-- The per-user stream (decision 9b): read cursors, notify events, preferences and
-- workspace-membership changes. Same allocation rule as the workspace stream.
ALTER TABLE users
    ADD COLUMN seq         bigint NOT NULL DEFAULT 0,
    ADD COLUMN log_floor   bigint NOT NULL DEFAULT 0,
    ADD COLUMN preferences jsonb NOT NULL DEFAULT '{}';

CREATE TABLE user_events (
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    seq        bigint NOT NULL,
    kind       text NOT NULL,
    payload    jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, seq)
);
CREATE INDEX user_events_created ON user_events(created_at);

CREATE INDEX events_workspace_created ON events(workspace_id, created_at);
