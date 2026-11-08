CREATE TABLE task_provenance (
  task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  provider TEXT NOT NULL,
  identity TEXT NOT NULL,
  kind TEXT NOT NULL,
  source_url TEXT NOT NULL,
  UNIQUE (project_id, provider, identity)
);
