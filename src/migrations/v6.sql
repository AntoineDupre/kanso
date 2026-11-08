CREATE TABLE github_repositories (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repository TEXT NOT NULL COLLATE NOCASE,
    issues INTEGER NOT NULL CHECK (issues IN (0, 1)),
    review INTEGER NOT NULL CHECK (review IN (0, 1)),
    follow INTEGER NOT NULL CHECK (follow IN (0, 1)),
    PRIMARY KEY (project_id, repository)
);
