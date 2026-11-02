CREATE TABLE projects (
  id                     TEXT PRIMARY KEY,
  key                    TEXT NOT NULL UNIQUE COLLATE NOCASE, -- 'ALP', shown as ALP-42
  name                   TEXT NOT NULL,
  description            TEXT NOT NULL DEFAULT '',
  repo_path              TEXT,                                -- metadata only
  color                  TEXT NOT NULL DEFAULT '#64748b',
  position               REAL NOT NULL,
  openchamber_project_id TEXT,                                -- 'path_…' when imported
  agent                  TEXT,                                -- 'build' | 'plan' | …
  model                  TEXT,                                -- 'provider/model'
  base_branch            TEXT,                                -- 'main'
  worktree               INTEGER NOT NULL DEFAULT 1,
  prompt_template        TEXT,                                -- NULL = built-in template
  archived_at            INTEGER,
  created_at             INTEGER NOT NULL,
  updated_at             INTEGER NOT NULL
);

CREATE TABLE tasks (
  id                TEXT PRIMARY KEY,
  project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  number            INTEGER NOT NULL,                          -- per project
  title             TEXT NOT NULL,
  description       TEXT NOT NULL DEFAULT '',
  status            TEXT NOT NULL CHECK (status IN
                      ('backlog','todo','in_progress','in_review','blocker','done')),
  state             TEXT NOT NULL DEFAULT 'active'
                      CHECK (state IN ('active','proposed','archived')),
  blocked_from      TEXT,                                      -- status before 'blocker'
  blocked_reason    TEXT,
  rank              REAL NOT NULL,                             -- order inside its column
  priority          TEXT NOT NULL DEFAULT 'normal'
                      CHECK (priority IN ('low','normal','high')),
  due_date          TEXT,                                      -- 'YYYY-MM-DD'
  source            TEXT NOT NULL DEFAULT 'human'
                      CHECK (source IN ('human','agent')),
  created_by        TEXT,                                      -- token name or 'ui'
  claimed_by        TEXT,                                      -- token name, NULL = free
  session_id        TEXT,
  session_directory TEXT,
  session_status    TEXT,                                      -- running|idle|waiting|failed|offline
  attention         INTEGER NOT NULL DEFAULT 0,                 -- needs human input
  done_at           INTEGER,
  archived_at       INTEGER,
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  UNIQUE (project_id, number)
);

CREATE TABLE labels (
  id    TEXT PRIMARY KEY,
  name  TEXT NOT NULL UNIQUE COLLATE NOCASE,
  color TEXT NOT NULL
);

CREATE TABLE task_labels (
  task_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, label_id)
);

CREATE TABLE checklist (
  id       TEXT PRIMARY KEY,
  task_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  text     TEXT NOT NULL,
  done     INTEGER NOT NULL DEFAULT 0,
  position REAL NOT NULL
);

CREATE TABLE comments (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  author     TEXT NOT NULL,
  body       TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE dependencies (
  task_id      TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  blocked_by   TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, blocked_by),
  CHECK (task_id <> blocked_by)
);

CREATE TABLE activity (
  id         TEXT PRIMARY KEY,
  task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  actor      TEXT NOT NULL,
  kind       TEXT NOT NULL,                                    -- created|moved|claimed|…
  data       TEXT NOT NULL DEFAULT '{}',                       -- JSON
  created_at INTEGER NOT NULL
);

CREATE TABLE tokens (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE COLLATE NOCASE,
  scope        TEXT NOT NULL CHECK (scope IN ('human','agent')),
  hash         TEXT NOT NULL,                                  -- sha256(token)
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
);

CREATE TABLE ui_sessions (
  id         TEXT PRIMARY KEY,                                 -- random 32 bytes
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL
);

CREATE INDEX tasks_board      ON tasks (project_id, state, status, rank);
CREATE INDEX tasks_session    ON tasks (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX comments_task    ON comments (task_id, created_at);
CREATE INDEX activity_task    ON activity (task_id, created_at DESC);
CREATE INDEX dependencies_rev ON dependencies (blocked_by);
