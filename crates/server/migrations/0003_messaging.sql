-- Durable read cursors; unread counts derive from them (decision 16).
CREATE TABLE read_cursors (
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    seq             bigint NOT NULL,
    PRIMARY KEY (user_id, conversation_id)
);

-- One notification per message and user, however often the notify job runs.
CREATE TABLE notify_sent (
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    message_id uuid NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, message_id)
);
