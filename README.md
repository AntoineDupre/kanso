# Kanso

A single-user kanban board for your projects, with **optional** OpenCode /
OpenChamber agent sessions.

The design, scope and milestone plan live in [PLAN.md](PLAN.md). It is the
reference for the architecture, data model, HTTP API, CLI and roadmap; the code
and its tests show what is implemented today. This README covers building,
running, testing and configuring the project.

## Requirements

- Rust 1.89 or newer (`rust-version` in `Cargo.toml`)
- SQLite is compiled in through `rusqlite`'s `bundled` feature, so no system
  SQLite is needed.

## Build and run

```sh
cargo build
KANSO_PASSWORD=dev KANSO_DATA_DIR=./data cargo run -- serve
curl http://127.0.0.1:8087/api/health
# {"status":"ok","version":"0.1.0","db":"ok"}
```

`cargo run -- serve --port 9000 --data-dir /tmp/kanso` replaces the port in
`KANSO_BIND` and overrides `KANSO_DATA_DIR`. The database is created at
`<data_dir>/kanso.db` with WAL enabled and migrations applied at startup.

## Configuration

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `KANSO_PASSWORD` | yes | — | UI password; startup fails when missing or empty |
| `KANSO_DATA_DIR` | no | `/data` | Directory holding `kanso.db` |
| `KANSO_PUBLIC_URL` | no | — | Public base URL; `https://` makes the session cookie `Secure` and its host is accepted as a CSRF origin |
| `KANSO_BIND` | no | `0.0.0.0:8087` | Listen socket address (`IP:port`) |
| `KANSO_LOG` | no | `info` | `tracing` filter, e.g. `info,kanso=debug` |

Logs go to stdout without ANSI colours, which is journald-friendly. SIGINT and
SIGTERM trigger a graceful shutdown with a 10-second drain deadline.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the same commands plus a container build.

## Container

```sh
podman build -f Containerfile -t kanso .
podman run --rm -p 127.0.0.1:8087:8087 \
  -e KANSO_PASSWORD=dev -v kanso-data:/data kanso
```

The image is a multi-stage musl build on Alpine, runs as uid/gid 1000, keeps
data in the `/data` volume, exposes port 8087 and probes `/api/health`.

## HTTP surface

All API responses are JSON; timestamps are Unix milliseconds. Failures use
`{"error":{"code":"...","message":"..."}}` with codes `validation`,
`unauthorized`, `forbidden`, `not_found`, `conflict`, `csrf`, `rate_limited`
and `internal` (500). `GET /api/health` and `POST /api/auth/login` are public; every
other `/api` route needs a session cookie or `Authorization: Bearer kanso_...`.

### Authentication and meta

| Method | Path | Notes |
| --- | --- | --- |
| `POST` | `/api/auth/login` | `{password}` → `204` + HttpOnly `kanso_session` cookie (30 days) |
| `POST` | `/api/auth/logout` | Deletes the session and clears the cookie |
| `GET` | `/api/me` | `{kind:"ui"\|"token", name, scope}` |
| `GET` | `/api/health` | `{status, version, db}` |

Failed logins are limited to five per minute per client address (`429`,
`rate_limited`). Cookie-authenticated `POST`/`PATCH`/`DELETE` requests must carry
an `Origin` (or `Referer`) whose host matches the request `Host` or
`KANSO_PUBLIC_URL`; bearer-token requests are exempt from CSRF checks.

### Projects

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/projects` | Active projects; `?archived=1` includes archived ones |
| `POST` | `/api/projects` | `{key,name,description?,repo_path?,color?}` |
| `GET` | `/api/projects/:id_or_key` | Case-insensitive key |
| `PATCH` | `/api/projects/:id` | Name, description, key, colour, repo and OpenChamber fields |
| `POST` | `/api/projects/:id/archive` | Idempotent |

Project keys are 2–6 characters of `A-Z`/`0-9`, stored uppercase and unique
case-insensitively.

### Tasks

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/tasks` | `project`, `status`, `state`, `label`, `q`, `claimed_by`, `attention`, `limit` (100, max 500), `offset` |
| `POST` | `/api/tasks` | `{project_id\|project_key,title,description?,status?,priority?,due_date?,labels?}` |
| `GET` | `/api/tasks/:ref` | `ALP-42` (case-insensitive) or UUID; includes labels, checklist, comments, activity and dependencies |
| `PATCH` | `/api/tasks/:ref` | `title`, `description`, `priority`, `due_date`, `labels`; other fields are rejected |
| `POST` | `/api/tasks/:ref/archive` | Human only in M1 |
| `POST` | `/api/tasks/:ref/restore` | Human only in M1 |
| `DELETE` | `/api/tasks/:ref?hard=true` | Human only; cascades to children |

Task numbers are allocated per project inside the create transaction. New cards
append to their column with the fractional rank helper described in `PLAN.md`.

### Labels, checklist, comments and tokens

| Method | Path | Notes |
| --- | --- | --- |
| `GET`/`POST` | `/api/labels` | `{name,color}`; names unique case-insensitively, colour `#rrggbb` |
| `PATCH`/`DELETE` | `/api/labels/:id` | Deleting detaches the label from cards |
| `POST` | `/api/tasks/:ref/comment` | `{body}`; author is `ui` or the token name |
| `POST` | `/api/tasks/:ref/checklist` | `{text}` |
| `PATCH` | `/api/tasks/:ref/checklist/:item` | `{text?,done?,before?,after?}`; `after` names the preceding item and `before` the following one |
| `DELETE` | `/api/tasks/:ref/checklist/:item` | Human only in M1 |
| `GET`/`POST` | `/api/tokens` | `{name,scope}`; the `kanso_` token is returned once only |
| `DELETE` | `/api/tokens/:id` | Revoke; repeating it is idempotent |

Agent-scoped tokens may read projects, tasks and labels, comment, and add or
update checklist items (including reordering them with `before`/`after`). They
may not create, patch, archive, restore or delete cards, delete checklist items,
manage projects/labels/tokens, or read the token list. M2 adds move/claim,
dependencies and proposals.

Every task mutation writes an `activity` row (`created`, `updated`, `commented`,
`checklist_added`, `checklist_updated`, `checklist_removed`, `label_added`,
`label_removed`, `archived`, `restored`) with a small JSON `data` payload.

The data directory also holds `session.key`, a random 0600 HMAC key used to sign
session cookies; deleting it revokes every UI session.

Non-`/api` routes serve the static shell from `web/`, with history fallback to
`index.html` and `{{VERSION}}` replaced by the binary version; a missing asset
with a file extension returns 404.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
