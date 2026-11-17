-- Uploaded files (decision 13). The bytes live in the object store under `files/{id}`;
-- a row exists only once its object is complete, so a row never points at a partial upload.
CREATE TABLE files (
    id              uuid PRIMARY KEY,
    workspace_id    uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    uploader_id     uuid NOT NULL REFERENCES users(id),
    -- Set when a message attaches the file; until then only the uploader may read it.
    message_id      uuid REFERENCES messages(id) ON DELETE SET NULL,
    name            text NOT NULL,
    mime            text NOT NULL,
    size            bigint NOT NULL,
    has_thumbnail   boolean NOT NULL DEFAULT false,
    -- pending, done, none (format not extracted) or failed. Never gates the download.
    text_state      text NOT NULL DEFAULT 'pending'
                    CHECK (text_state IN ('pending', 'done', 'none', 'failed')),
    extracted_text  text,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX files_conversation ON files(conversation_id);
CREATE INDEX files_message ON files(message_id) WHERE message_id IS NOT NULL;
CREATE INDEX files_uploader ON files(uploader_id);
