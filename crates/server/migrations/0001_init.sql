-- Accounts are server-level; memberships are per workspace (decision 14).
CREATE TABLE users (
    id            uuid PRIMARY KEY,
    email         text NOT NULL UNIQUE,
    password_hash text NOT NULL,
    display_name  text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    token_hash bytea PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz
);
CREATE INDEX sessions_user ON sessions(user_id);

CREATE TABLE password_resets (
    token_hash bytea PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL,
    used_at    timestamptz
);

-- seq is the workspace stream head, allocated by UPDATE ... RETURNING just before commit.
-- log_floor: events with seq <= log_floor have been pruned.
CREATE TABLE workspaces (
    id         uuid PRIMARY KEY,
    name       text NOT NULL,
    seq        bigint NOT NULL DEFAULT 0,
    log_floor  bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workspace_members (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role         text NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'guest')),
    display_name text NOT NULL,
    joined_seq   bigint NOT NULL,
    left_seq     bigint,
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user ON workspace_members(user_id);

CREATE TABLE conversations (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind         text NOT NULL CHECK (kind IN ('public', 'private', 'dm', 'group_dm')),
    name         text,
    dm_key       text,
    is_default   boolean NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, dm_key)
);
CREATE INDEX conversations_workspace ON conversations(workspace_id);

-- Every right to read a conversation is an interval [joined_seq, left_seq) (decision 10).
-- joined_seq/left_seq are stamped by the statement that allocates the event's seq.
CREATE TABLE access_intervals (
    id              bigserial PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    via             text NOT NULL CHECK (via IN ('membership', 'role')),
    joined_seq      bigint,
    left_seq        bigint
);
CREATE INDEX access_intervals_lookup ON access_intervals(conversation_id, user_id);
CREATE INDEX access_intervals_user ON access_intervals(user_id) WHERE left_seq IS NULL;
CREATE UNIQUE INDEX access_intervals_one_open
    ON access_intervals(conversation_id, user_id, via) WHERE left_seq IS NULL;

CREATE TABLE messages (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    author_id       uuid NOT NULL REFERENCES users(id),
    client_id       uuid NOT NULL,
    thread_root_id  uuid REFERENCES messages(id),
    body            text NOT NULL,
    entities        jsonb NOT NULL DEFAULT '[]',
    content_type    text NOT NULL DEFAULT 'text/markdown',
    encryption      text,
    attachments     uuid[] NOT NULL DEFAULT '{}',
    revision        bigint NOT NULL DEFAULT 0,
    deleted         boolean NOT NULL DEFAULT false,
    created_seq     bigint,
    updated_seq     bigint,
    created_at      timestamptz NOT NULL DEFAULT now(),
    edited_at       timestamptz
);
CREATE INDEX messages_history ON messages(conversation_id, created_seq DESC);
CREATE INDEX messages_thread ON messages(thread_root_id) WHERE thread_root_id IS NOT NULL;

CREATE TABLE reactions (
    message_id uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    emoji      text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, user_id, emoji)
);

-- The workspace stream. audience: 0 workspace-wide, 1 conversation (interval-filtered),
-- 2 subject only. The subject of a membership event always receives it.
CREATE TABLE events (
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    seq             bigint NOT NULL,
    kind            text NOT NULL,
    audience        smallint NOT NULL,
    conversation_id uuid,
    message_id      uuid,
    subject_user_id uuid,
    interval_id     bigint,
    payload         jsonb NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, seq)
);
CREATE INDEX events_created ON events(created_at);

-- Client-generated keys that make retried commands no-ops (decision 5). Kept 14 days.
CREATE TABLE idempotency_keys (
    user_id    uuid NOT NULL,
    key        uuid NOT NULL,
    response   jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, key)
);
CREATE INDEX idempotency_keys_created ON idempotency_keys(created_at);

-- Background work (decision 7). A claimed job holds a lease; an expired lease is reclaimed.
CREATE TABLE jobs (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL,
    payload      jsonb NOT NULL,
    run_at       timestamptz NOT NULL DEFAULT now(),
    attempts     int NOT NULL DEFAULT 0,
    max_attempts int NOT NULL DEFAULT 5,
    locked_until timestamptz,
    last_error   text,
    done_at      timestamptz,
    failed_at    timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX jobs_ready ON jobs(run_at) WHERE done_at IS NULL AND failed_at IS NULL;
