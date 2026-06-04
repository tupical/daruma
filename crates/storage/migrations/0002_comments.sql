-- Comment projection (rebuilt from CommentAdded/Edited/Deleted events).
CREATE TABLE IF NOT EXISTS comments (
    id          TEXT    PRIMARY KEY,
    task_id     TEXT    NOT NULL,
    parent_id   TEXT    NULL,
    author_json TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,
    edited_at   TEXT    NULL,
    deleted_at  TEXT    NULL
);

CREATE INDEX IF NOT EXISTS idx_comments_task_id    ON comments (task_id);
CREATE INDEX IF NOT EXISTS idx_comments_created_at ON comments (created_at);
