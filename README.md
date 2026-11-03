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
| `KANSO_OPENCHAMBER_URL` | no | — | OpenChamber base URL; enables the optional adapter only together with the token |
| `KANSO_OPENCHAMBER_TOKEN` | no | — | `oc_client_…` bearer token for OpenChamber; never returned by the API |

Logs go to stdout without ANSI colours, which is journald-friendly. SIGINT and
SIGTERM trigger a graceful shutdown with a 10-second drain deadline.

## CLI

`kanso` is both the server and the client. Write a client config once:

```sh
kanso config init --url https://kanso.example --token kanso_…
```

`config init` validates the token with `GET /api/me`, prints the identity and
writes `~/.config/kanso/config.toml` atomically with mode 0600. It refuses to
replace an existing file without `--force`. `KANSO_URL` and `KANSO_TOKEN`
override the file per value; `KANSO_CONFIG` overrides its path.

```sh
kanso project list | add --key ALP --name Alpha | show ALP | edit ALP --name …
kanso task list [--project ALP] [--status todo] [--label bug] [--mine]
kanso task show ALP-42
kanso task add "Title" --project ALP [--description …] [--status todo] [--priority high]
kanso task edit ALP-42 [--title …] [--description …] [--due 2026-12-01]
kanso next [--project ALP] [--claim]
kanso claim ALP-42
kanso release ALP-42
kanso move ALP-42 in_review | blocker --reason "…" | restore
kanso comment ALP-42 "text"             # "-" reads stdin
kanso check ALP-42 --add "item" | --done 2 | --list
kanso depend ALP-42 on ALP-40
kanso propose "Idea" --project ALP [--description …]
kanso inbox
kanso session link ALP-42 ses_… --directory /path/to/worktree
kanso token create --name build --scope agent   # prints the token once
kanso token list | revoke build
kanso skill [--print] [--install [--dir ~/.config/opencode/skills]]
kanso backup [--out DIR]
```

Output is aligned plain text with no colour or ANSI escapes. For commands that
talk to the API, `--json` prints the API's own JSON unchanged. Runtime errors go
to stderr as `kanso: <code>: <message>`; clap prints its own `error: …` usage
text. Exit codes are `0` success, `1` usage or start-up, `2` API/network/local
I/O error and `3` conflict (already claimed, dependency unmet, not active, not
claimable, not claimed, rate limited).

`next`, `claim`, `comment`, `move`, `propose` and `check` accept
`--agent <name>`. It sends `X-Kanso-Agent`, which overrides the token name in
comments, activity and `created_by` (not `claimed_by` or claim ownership). The header is honoured only for
agent-scope tokens; a human token ignores it, and an empty, over-long, reserved
(`ui`) or control-character name is a validation error.

`kanso skill --print` writes the embedded `skills/kanso/SKILL.md`; `kanso skill
--install` writes `<dir>/kanso/SKILL.md` (default
`~/.config/opencode/skills`), creating parents and reporting whether it
installed, updated or found the file already up to date.

`kanso backup` asks the server to run `VACUUM INTO` under
`<data_dir>/backups/` and prints the server-side path. `--out DIR` is resolved
by the server under the data directory; paths that escape it are rejected. There
is no download endpoint in this milestone, so a host directory cannot be
targeted directly.

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

All API responses are JSON except the SSE stream; timestamps are Unix milliseconds. Failures use
`{"error":{"code":"...","message":"..."}}` with codes `validation`,
`unauthorized`, `forbidden`, `not_found`, `conflict`, `csrf`, `rate_limited`
and `internal` (500), plus the specific conflict codes `already_claimed`,
`not_claimable`, `dependency_unmet`, `not_claimed`, `not_active`,
`not_proposed` and `already_dispatched`. The optional OpenChamber adapter adds
`integration_disabled`/`integration_unreachable` (503) and
`openchamber_error` (502). `GET /api/health` and `POST /api/auth/login` are public; every
other `/api` route needs a session cookie or `Authorization: Bearer kanso_...`.

### Authentication and meta

| Method | Path | Notes |
| --- | --- | --- |
| `POST` | `/api/auth/login` | `{password}` → `204` + HttpOnly `kanso_session` cookie (30 days) |
| `POST` | `/api/auth/logout` | Deletes the session and clears the cookie |
| `GET` | `/api/me` | `{kind:"ui"\|"token", name, scope}` |
| `GET` | `/api/health` | `{status, version, db}` |
| `GET` | `/api/events` | Server-Sent Events; cookie or bearer auth |
| `GET` | `/api/integration` | `{openchamber:{enabled,url,reachable,version}}`; disabled is `{enabled:false}` |

Failed logins are limited to five per minute per client address (`429`,
`rate_limited`). Cookie-authenticated `POST`/`PATCH`/`DELETE` requests must carry
an `Origin` (or `Referer`) whose host matches the request `Host` or
`KANSO_PUBLIC_URL`; bearer-token requests are exempt from CSRF checks. A cookie
request to `GET /api/agent/next?claim=1` mutates state, so it needs the same
origin check.

### Projects

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/projects` | Active projects; `?archived=1` includes archived ones |
| `POST` | `/api/projects` | `{key,name,description?,repo_path?,color?}` |
| `GET` | `/api/projects/:id_or_key` | Case-insensitive key |
| `PATCH` | `/api/projects/:id` | Name, description, key, colour, repo and OpenChamber fields |
| `POST` | `/api/projects/:id/archive` | Idempotent |
| `GET` | `/api/projects/:id_or_key/board` | `{project, columns}` for one project |
| `GET` | `/api/board` | `{projects:[{project,columns}]}`; `?project=` limits it to one |

Project keys are 2–6 characters of `A-Z`/`0-9`, stored uppercase and unique
case-insensitively.

### Tasks

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/tasks` | `project`, `status`, `state`, `label`, `q`, `claimed_by`, `attention`, `limit` (100, max 500), `offset` |
| `POST` | `/api/tasks` | `{project_id\|project_key,title,description?,status?,priority?,due_date?,labels?}`; an agent token creates a proposal instead |
| `GET` | `/api/tasks/:ref` | `ALP-42` (case-insensitive) or UUID; includes labels, checklist, comments, activity and dependencies |
| `PATCH` | `/api/tasks/:ref` | `title`, `description`, `priority`, `due_date`, `labels`; other fields are rejected |
| `POST` | `/api/tasks/:ref/move` | `{status?,before?,after?,restore?,reason?}`; `reason` (max 500 characters) is stored in `blocked_reason` when entering `blocker` |
| `POST` | `/api/tasks/:ref/claim` | Human or agent; sets `claimed_by`, status → `in_progress` |
| `POST` | `/api/tasks/:ref/release` | Clears the claim, keeps the column |
| `POST` | `/api/tasks/:ref/session` | Human only; `{session_id,directory,status?}` with status `running` by default (also `idle`, `waiting`, `failed`, `offline`) |
| `POST` | `/api/tasks/:ref/dispatch` | Human only; creates an OpenChamber session/worktree for the card when the adapter is enabled |
| `POST` | `/api/tasks/:ref/dependencies` | `{blocked_by:[ref,…]}` replaces the set; `[]` clears it (human only) |
| `POST` | `/api/tasks/:ref/archive` | Human only in M2 |
| `POST` | `/api/tasks/:ref/restore` | Human only in M2 |
| `DELETE` | `/api/tasks/:ref?hard=true` | Human only; cascades to children |

`GET /api/tasks` defaults to `state=active`; use `?state=archived` or
`?state=proposed` for the archive or the proposal inbox. Task numbers are
allocated per project inside the create transaction. New cards append to their
column with the fractional rank helper described in `PLAN.md`.

`move` inserts at the top (`before`), bottom (neither neighbour) or between two
neighbouring cards (`after` is the preceding card and `before` the following
one; both must be adjacent among the visible active cards). When the midpoint
gap falls below `1e-6`, the target column is renumbered `1..n` in the same
transaction. Entering `blocker` stores `blocked_from`; `restore:true` returns to
that column, an explicit `status` wins over `restore`, and a move with neither
clears `blocked_from`/`blocked_reason` while keeping the card in `blocker` —
unless it sends `reason`, which updates `blocked_reason` in place and keeps
`blocked_from`. Entering `done` sets `done_at`; leaving it clears the timestamp.

An agent-scope bearer token may send `X-Kanso-Agent: <name>` to override its
token name in comments, activity and `created_by`. Claim ownership (`claimed_by`,
release checks) stays the token principal. The header is ignored
for human tokens and rejected with `400 validation` when the name is empty,
longer than 64 characters, contains a control character or is `ui`.

`claim` requires an active card in `backlog`/`todo`, no existing claim, and every
`blocked_by` card in `done`; failures are `409 already_claimed`,
`dependency_unmet` or `not_claimable`. `release` returns `409 not_claimed` when
there is nothing to release. `/api/agent/next` considers only unclaimed active
cards in `backlog`/`todo`, in non-archived projects, whose `blocked_by` cards are
all `done`; it returns the best one (`high` priority first, then earliest due
date with NULLs last, then rank and number) and `204` when none is available.
`claim=1` claims it atomically in the same transaction. A proposal (`state=proposed`) never appears in `/api/tasks`
default listings, the board or `next`; `GET /api/tasks?state=proposed` still
returns it.

Dependencies accept references from any project. A self-dependency or a cycle
is `400 validation`; an unknown reference is `404 not_found`.

### Inbox

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/inbox` | Proposed cards, newest first |
| `POST` | `/api/inbox/:ref/accept` | Human only; `{status?}`, default `backlog`; appends to the column |
| `POST` | `/api/inbox/:ref/reject` | Human only; archives the proposal |

Accepting or rejecting a card that is not proposed returns `409 not_proposed`.

### Board shape

`GET /api/projects/:id_or_key/board` returns `{project, columns}` and
`GET /api/board` returns `{projects:[{project,columns}]}`. `columns` always has
`backlog`, `todo`, `in_progress`, `in_review`, `blocker` and `done`; each array
contains active cards ordered by `rank` then `number`. A card carries `id`,
`reference`, `title`, `status`, `priority`, `due_date`, `labels`, `claimed_by`,
`attention`, `session_status`, `blocked_from`, `checklist_done`,
`checklist_total` and `blocked` (an unfinished `blocked_by` card) — not the full
detail. The checklist counters and `blocked` flag are additions needed by the
web cards, which show `2/3` progress and a lock without fetching every detail.
The global view lists active projects in `position` order; `?project=` resolves
one project (archived or not).

### Events

`GET /api/events` is a `text/event-stream`. Each frame has `event: <type>`,
`id: <n>` and a `data:` JSON object `{"id":n,"type":"<type>","data":{…}}`.
The server sends a `: heartbeat` comment every 25 s. IDs are monotonic inside
one process and restart at 1 after a restart, so clients refetch on reconnect.
A lagged or stalled subscriber is disconnected instead of blocking publishers.

Event types include `task.created`, `task.updated`, `task.moved`, `task.claimed`,
`task.released`, `task.archived`, `task.restored`, `task.deleted`,
`dependencies.changed`, `proposal.created`, `proposal.accepted`,
`proposal.rejected`, `project.created`, `project.updated`, `project.archived`,
`comment.created`, `checklist.created`, `checklist.updated`,
`checklist.deleted`, `label.created`, `label.updated` and `label.deleted`.
Task events carry the card summary under `data.task`; `task.moved` also carries
`from`, `to`, the activity kind (`moved`, `blocked` or `unblocked`) and the
ordered target `column`; `task.archived`/`task.restored` also carry `column`;
`dependencies.changed` carries `task` and the full `dependencies` object;
checklist creation/update events carry `task_id` plus `item` (updates also carry
the full reordered `checklist`), and `checklist.deleted` carries `task_id` and
`item_id`. Token and session events are deliberately not broadcast.

### Labels, checklist, comments and tokens

| Method | Path | Notes |
| --- | --- | --- |
| `GET`/`POST` | `/api/labels` | `{name,color}`; names unique case-insensitively, colour `#rrggbb` |
| `PATCH`/`DELETE` | `/api/labels/:id` | Deleting detaches the label from cards |
| `POST` | `/api/tasks/:ref/comment` | `{body}`; author is `ui`, the token name, or `X-Kanso-Agent` for an agent token |
| `POST` | `/api/tasks/:ref/checklist` | `{text}` |
| `PATCH` | `/api/tasks/:ref/checklist/:item` | `{text?,done?,before?,after?}`; `after` names the preceding item and `before` the following one |
| `DELETE` | `/api/tasks/:ref/checklist/:item` | Human only in M2 |
| `GET`/`POST` | `/api/tokens` | `{name,scope}`; the `kanso_` token is returned once only; `ui` is reserved |
| `DELETE` | `/api/tokens/:id` | Revoke; repeating it is idempotent |
| `POST` | `/api/backup` | Human only; `VACUUM INTO` under `<data_dir>/backups/`; body `{dir?}` resolves under the data directory |

Agent-scoped tokens may read projects, tasks, labels, boards, the inbox and
`/api/agent/next`; claim and release their own claims; move cards between
`todo`, `in_progress`, `in_review` and `blocker`; comment; add, update and
reorder checklist items; and create proposals. They may not move a card to `backlog` or `done` (nor move one out of
`backlog`), release another identity's claim, accept or reject proposals, create
active cards, edit cards, change dependencies, archive, restore or delete cards,
delete checklist items, or manage projects/labels/tokens.

Every task mutation writes an `activity` row (`created`, `proposed`, `updated`,
`moved`, `blocked`, `unblocked`, `claimed`, `released`, `accepted`, `rejected`,
`dependencies_changed`, `commented`, `checklist_added`, `checklist_updated`,
`checklist_removed`, `label_added`, `label_removed`, `session_linked`,
`dispatched`, `auto_moved`, `session_idle`, `session_failed`,
`session_offline`, `session_running`, `permission_auto_accept_failed`,
`dispatch_abandoned`, `archived`, `restored`)
with a small JSON `data` payload.

The data directory also holds `session.key`, a random 0600 HMAC key used to sign
session cookies; deleting it revokes every UI session.

Non-`/api` routes serve the static shell from `web/`, with history fallback to
`index.html` and `{{VERSION}}` replaced by the binary version; a missing asset
with a file extension returns 404.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
