-- Only public metadata and a verifier are stored here, never a submitted value.
CREATE TABLE secret_requests (
  id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  token_id TEXT NOT NULL REFERENCES tokens(id),
  session_id TEXT NOT NULL,
  session_directory TEXT,
  label TEXT NOT NULL,
  purpose TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('waiting','ready','consumed','declined','cancelled','expired','invalidated')),
  verifier TEXT,
  created_at INTEGER NOT NULL,
  deadline INTEGER NOT NULL,
  value_deadline INTEGER,
  explanation TEXT,
  invalidation_reason TEXT,
  delivery_status TEXT CHECK (delivery_status IN ('sent','failed','unknown')),
  delivery_error TEXT,
  delivery_prompt TEXT
);
CREATE UNIQUE INDEX secret_one_live_per_task ON secret_requests(task_id)
  WHERE state IN ('waiting','ready');
CREATE INDEX secret_requests_task ON secret_requests(task_id, created_at DESC);

CREATE TRIGGER secret_task_invalidate AFTER UPDATE OF status, state, session_id, session_directory, blocked_reason ON tasks
WHEN OLD.status IS NOT NEW.status OR OLD.state IS NOT NEW.state OR OLD.session_id IS NOT NEW.session_id OR OLD.session_directory IS NOT NEW.session_directory OR OLD.blocked_reason IS NOT NEW.blocked_reason
BEGIN
  UPDATE secret_requests SET state = 'invalidated', verifier = NULL,
    invalidation_reason = CASE WHEN OLD.session_id IS NOT NEW.session_id OR OLD.session_directory IS NOT NEW.session_directory THEN 'session_changed' ELSE 'card_changed' END
  WHERE task_id = NEW.id AND state IN ('waiting','ready')
    AND (NEW.status <> 'blocker' OR NEW.state <> 'active' OR NEW.session_id IS NOT session_id OR NEW.session_directory IS NOT session_directory OR OLD.blocked_reason IS NOT NEW.blocked_reason);
  UPDATE secret_requests SET invalidation_reason = 'session_changed'
  WHERE task_id = NEW.id AND (OLD.session_id IS NOT NEW.session_id OR OLD.session_directory IS NOT NEW.session_directory);
END;

CREATE TRIGGER secret_token_invalidate AFTER UPDATE OF revoked_at ON tokens
WHEN NEW.revoked_at IS NOT NULL
BEGIN
  UPDATE secret_requests SET state = 'invalidated', verifier = NULL, invalidation_reason = 'token_revoked'
  WHERE token_id = NEW.id AND state IN ('waiting','ready');
END;
