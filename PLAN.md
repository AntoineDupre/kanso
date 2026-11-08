# Kanso — implementation plan

Single-user kanban board, self-hosted, that can drive OpenCode/OpenChamber agent
sessions **optionally**. Public Apache-2.0 project; the board must be fully
usable without any agent integration.

Status: **proposed**, awaiting approval. Nothing is implemented yet.

---

## 1. Scope

### In scope (v1)

- Projects with their own board, plus one global cross-project view.
- Six fixed columns: `backlog`, `todo`, `in_progress`, `in_review`, `blocker`, `done`.
- Cards: Markdown description, priority, labels, due date, checklist,
  comments, dependencies, activity log, human-readable reference (`ALP-42`).
- `blocker` remembers the column the card came from and restores it in one click.
- Proposal inbox: agents propose cards, the human accepts or rejects them.
- Agent tokens with restricted scopes; every write is attributed.
- CLI (`kanso`) + skill file so an agent can pull, claim, comment and report.
- Optional push dispatch: one button creates an OpenChamber session bound to a
  card, with a git worktree, and follows its state.
- Live updates in the browser (SSE), notifications when an agent needs attention.
- Deployment: rootless Podman quadlet, nginx + TLS, ghcr.io image, auto-update.

### Out of scope (explicitly)

- Multi-user, sharing, real-time co-editing.
- Attachments, time tracking, estimates, recurring cards.
- Git operations inside Kanso (no diff, no merge, no branch management):
  OpenChamber and the human own git.
- Automatic move to `done`; automatic merge.
- Native mobile apps (responsive web only).
- Editing cards created by someone else's instance: single database, single admin.

---

## 2. Decisions taken

| # | Decision |
| --- | --- |
| Language | Rust backend, vanilla JS frontend, **no frontend build step, no npm** |
| Server | `axum` + `tokio`; `rusqlite` (`bundled`); `serde`; `tracing` |
| Store | SQLite file, WAL, migrations by `PRAGMA user_version` |
| Binaries | **one** `kanso` binary: `kanso serve` + CLI subcommands |
| Name | repo/image/binary `kanso` (Apache-2.0) |
| Auth | one UI password (`KANSO_PASSWORD`, 0600 env) + signed HttpOnly cookie; API tokens for CLI/agents |
| Tokens | per-agent, named, hashed (`sha256`), revocable, scopes `human` / `agent` |
| Agent limits | agents may not set `done`, delete, archive, accept proposals or manage tokens |
| Projects | created manually; optional "import from OpenChamber" pre-fills a form (no permanent coupling) |
| Agent flow | pull via CLI is the foundation; push via OpenChamber is an optional adapter |
| OpenChamber | enabled only when `KANSO_OPENCHAMBER_URL` + `KANSO_OPENCHAMBER_TOKEN` are set; per-project agent/model/worktree settings |
| Layout | **B — fixed right panel** (collapsible, overlay under 1100 px) |
| Drag & drop | pointer events, no library; "Move to…" menu as fallback |
| Labels | global label table with colors, reusable, autocompleted |
| Sub-tasks | checklist only; dependencies are card-to-card |
| Archive | archive instead of delete; hard delete is human-only |
| Markdown | vendored `marked.min.js` (pinned static file) |
| UI language | English strings in `web/i18n/en.js` from day one; i18n scaffolding ready for `fr` |
| Deployment | rootless quadlet, user `kanso`, `/var/lib/kanso`, `127.0.0.1:8087`, `kanso.dupre49.fr` |

Assumptions to veto if wrong: domain `kanso.dupre49.fr`, loopback port `8087`,
service user `kanso`, data dir `/var/lib/kanso` (small footprint, < 5 GB).

---

## 3. Architecture

```
browser ──https──> nginx (host, TLS) ──http──> kanso (container, 127.0.0.1:8087)
                                                   │
                                                   ├── SQLite  /var/lib/kanso/kanso.db
                                                   └── optional HTTP client ──> OpenChamber (192.168.1.24:3000)
agent CLI (mathieu, ~/.local/bin/kanso) ──https──> nginx ──> kanso
```

- The container is a dedicated, non-root service user (`kanso`); it never touches
  `/home/mathieu` or any repository. Repo paths are metadata only.
- Git worktrees are created **by OpenChamber**, which already runs as `mathieu`.
- nginx is the only TLS endpoint; SSE is proxied without buffering.
- No external runtime dependency: the image contains the binary, CA certs and the
  static web assets.

---

## 4. Data model (SQLite)

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- user_version = 1

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
```

Rules:

- **Rank**: `REAL`, midpoint insertion. Insert at top = `min - 1`, at bottom =
  `max + 1`, between `a` and `b` = `(a + b) / 2`. If the gap drops below `1e-6`,
  renumber the column `1..n` inside the same transaction.
- **Reference**: `KEY-NUMBER`, immutable once created (renaming a project key does
  not rewrite old references; they are resolved by number for that project).
- **Blocker**: entering `blocker` stores the previous status in `blocked_from`.
  Leaving it with `restore` returns to `blocked_from`; leaving it with an explicit
  target clears both fields.
- **Dependencies gate**: a card is claimable only if every `blocked_by` card is
  `done`; the API returns `409 dependency_unmet` otherwise.
- **Proposals**: `state = 'proposed'`, `status = 'backlog'`; they never appear on
  the board, only in the inbox. Accept → `state = 'active'`; reject → `archived`.
- **Deletion**: `state = 'archived'` by default. `DELETE /api/tasks/:id?hard=true`
  is human-only, requires a confirmation header.

---

## 5. HTTP API

JSON only; timestamps are Unix milliseconds (integers). Errors use
`{"error":{"code":"...","message":"..."}}` with proper status codes.

### Auth & meta

| Method | Path | Notes |
| --- | --- | --- |
| `POST` | `/api/auth/login` | `{password}` → sets `kanso_session` cookie |
| `POST` | `/api/auth/logout` | |
| `GET` | `/api/me` | `{kind:"ui"|"token", name, scope}` |
| `GET` | `/api/health` | `{status:"ok", version, db:"ok"}` |
| `GET` | `/api/integration` | `{openchamber:{enabled, url, reachable, version}}` |
| `GET` | `/api/events` | SSE stream, cookie auth only |

### Projects

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/projects` | active + archived (`?archived=1`) |
| `POST` | `/api/projects` | `{key,name,description,repo_path,color}` |
| `PATCH` | `/api/projects/:id` | includes agent/model/base_branch/worktree/prompt_template |
| `POST` | `/api/projects/:id/archive` | |
| `GET` | `/api/projects/:id/board` | convenience: project + columns, tasks grouped |

### Tasks

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/tasks` | filters: `project`, `status`, `state`, `label`, `q`, `claimed_by`, `attention` |
| `POST` | `/api/tasks` | `{project_id,title,description?,status?,priority?,due_date?,labels?}` |
| `GET` | `/api/tasks/:ref` | `:ref` = `ALP-42` or UUID; includes labels, checklist, comments, activity, dependencies |
| `PATCH` | `/api/tasks/:ref` | editable fields |
| `POST` | `/api/tasks/:ref/move` | `{status, before?, after?, restore?}` |
| `POST` | `/api/tasks/:ref/claim` | agent or human; sets `claimed_by`, status → `in_progress` |
| `POST` | `/api/tasks/:ref/release` | clears the claim, keeps the column |
| `POST` | `/api/tasks/:ref/comment` | `{body}` |
| `POST` | `/api/tasks/:ref/checklist` | `{text}` |
| `PATCH` | `/api/tasks/:ref/checklist/:item` | `{text?, done?, before?, after?}` |
| `DELETE` | `/api/tasks/:ref/checklist/:item` | |
| `POST` | `/api/tasks/:ref/dependencies` | `{blocked_by:[ref,…]}` |
| `POST` | `/api/tasks/:ref/archive` | agent allowed (own cards only) |
| `POST` | `/api/tasks/:ref/restore` | human only |
| `DELETE` | `/api/tasks/:ref?hard=true` | human only |
| `POST` | `/api/tasks/:ref/dispatch` | push: create an OpenChamber session |
| `POST` | `/api/tasks/:ref/session` | link an existing session (pull flow) |

### Agent helpers

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/agent/next` | `?project=&claim=1`; priority, then due date, then rank |
| `GET` | `/api/inbox` | proposals, newest first |
| `POST` | `/api/inbox/:ref/accept` | human only; `{status?}` default `backlog` |
| `POST` | `/api/inbox/:ref/reject` | human only |

### Labels, tokens, export

| Method | Path | Notes |
| --- | --- | --- |
| `GET/POST` | `/api/labels` | name + color, case-insensitive uniqueness |
| `PATCH/DELETE` | `/api/labels/:id` | deletion detaches from cards |
| `GET/POST` | `/api/tokens` | created token shown **once**; `{name, scope}` |
| `DELETE` | `/api/tokens/:id` | revoke |
| `GET` | `/api/export.json` | full dump (projects, tasks, comments) |
| `POST` | `/api/backup` | `VACUUM INTO` a timestamped file (human only) |

### Authorization matrix

| Action | `human` token / UI | `agent` token |
| --- | --- | --- |
| Read board, cards, comments | yes | yes |
| Create a card in Backlog/Todo | yes | **no** (proposal only) |
| Propose a card (inbox) | yes | yes |
| Claim / release / comment / checklist | yes | yes |
| Move card | any status | only between `todo`, `in_progress`, `in_review`, `blocker` |
| Set `done` | yes | no |
| Archive | any | only cards it created or claimed |
| Restore, hard delete | yes | no |
| Accept/reject a proposal | yes | no |
| Projects, labels, tokens, dispatch | yes | no |

---

## 6. CLI (`kanso`)

Config: `~/.config/kanso/config.toml` (0600) with `url` and `token`.
Output: aligned tables by default, `--json` for machines. Exit codes: `0` ok,
`1` usage, `2` API error, `3` conflict (already claimed, unmet dependency).

```
kanso serve [--port 8087] [--data-dir /data]
kanso config init --url https://kanso.example --token kanso_…
kanso project list | add | show | edit
kanso task list [--project ALP] [--status todo] [--label x] [--mine] [--json]
kanso task show ALP-42
kanso task add "Title" --project ALP [--description "…"] [--status todo] [--priority high] [--label NAME|ID]…
kanso task edit ALP-42 [--title …] [--description …] [--due 2026-11-12]
kanso next [--project ALP] [--claim]
kanso claim ALP-42
kanso release ALP-42
kanso move ALP-42 in_review | blocker --reason "…" | restore
kanso comment ALP-42 "text"            # or "-" to read stdin
kanso check ALP-42 --add "item" | --done 2 | --list
kanso depend ALP-42 on ALP-40
kanso propose "Idea" --project ALP [--description "…"] [--label NAME|ID]…
kanso inbox [--json]
kanso session link ALP-42 ses_… --directory /path
kanso token create --name build --scope agent   # prints the token once
kanso token list | revoke build
kanso skill [--print] [--install [--dir ~/.config/opencode/skills]]
kanso backup [--out DIR]
```

Agent-relevant commands (`next`, `claim`, `comment`, `move`, `propose`, `check`)
accept `--agent <name>` to override the token name in attributions.

---

## 7. Skill (agent instructions)

`skills/kanso/SKILL.md`, installed by `kanso skill install`. It teaches the loop:

1. `kanso next --project ALP` → read the card.
2. `kanso claim ALP-42` (refuse if `409`; pick the next one).
3. Do the work in the current worktree only.
4. Report with short comments at meaningful milestones, not a transcript.
5. Finish with `kanso move ALP-42 in_review` and a summary comment.
6. If blocked: `kanso move ALP-42 blocker --reason "…"`.
7. Never move to `done`, never delete, never change another card's priority.
8. Ideas discovered on the way: `kanso propose`, never create cards directly.

---

## 8. Web UI (no build)

```
web/
  index.html          # SPA shell, {{VERSION}} cache-buster injected by the server
  app.css
  app.js              # router, store, SSE, keyboard shortcuts
  api.js              # fetch wrapper + error toasts
  board.js            # columns, cards, pointer-event drag & drop
  panel.js            # right panel (layout B), tabs: details/activity/session
  forms.js            # create/edit, labels, checklist, dependencies
  i18n/en.js          # every string, t(key, vars)
  vendor/marked.min.js
```

- Routes (History API, server falls back to `index.html`):
  `/` (last project), `/p/:key`, `/p/:key/task/:n`, `/global`, `/inbox`,
  `/settings/projects`, `/settings/tokens`.
- Layout: top bar (project switcher, views, search, new card); board with six
  columns; right panel 420 px, collapsible, remembers its width in localStorage.
  Under 1100 px the panel overlays the board; under 700 px columns are 85 % wide
  with horizontal scroll.
- **Drag & drop**: pointer events only. Drag a card → placeholder follows, drop
  computes neighbours → `move` with `before`/`after`. Escape cancels. Touch works
  with a long press. Every card also has "Move to…".
- **Live**: SSE `/api/events` reconnects with backoff; a full refetch happens on
  reconnect. Optimistic updates reconcile on the server event.
- **Notifications**: if the user opted in, a browser `Notification` fires when a
  card gains `attention` or lands in `in_review` while the tab is hidden.
- Keyboard: `n` new card, `/` search, `j`/`k` next/previous card, `m` move menu,
  `Esc` close panel.
- Markdown rendered with vendored `marked`, sanitized by a small allow-list
  (no raw HTML: `marked` configured with `mangle:false`, `headerIds:false`, then
  a DOM sanitizer that drops `script`, `style`, `iframe`, `on*` attributes).

---

## 9. OpenChamber adapter (optional)

Enabled at runtime when both env vars exist; otherwise `/api/integration` reports
`enabled:false` and the UI never renders dispatch controls.

```
KANSO_OPENCHAMBER_URL=http://192.168.1.24:3000
KANSO_OPENCHAMBER_TOKEN=oc_client_…
```

**Dispatch** (`POST /api/tasks/:ref/dispatch`, human only):

1. Validate: integration enabled, project has `repo_path`, card not claimed.
2. `POST {url}/api/openchamber/control`

```json
{ "action": "session.create",
  "input": {
    "directory": "<project.repo_path>",
    "title": "ALP-42 — <title>",
    "prompt": "<rendered template>",
    "agent": "<project.agent ?? 'build'>",
    "model": "<project.model>",
    "worktree": "<worktree ? 'kanso/ALP-42-<slug>' : false>",
    "branch": "kanso/ALP-42-<slug>",
    "startRef": "<project.base_branch ?? 'main'>",
    "wait": false
  } }
```

3. Store `session_id`, `session_directory`, `claimed_by = 'openchamber'`,
   status → `in_progress`.
4. `PUT {url}/api/permission-auto-accept/sessions/{sessionId}`
   `{"mode":"auto","directory":…}` — never blocks on a permission with no UI.
5. Emit an activity entry and an SSE event.

**Status polling** (not SSE: fewer moving parts, a handful of sessions):
every 10 s for cards with a `session_id` not in a terminal state:

| OpenChamber state | Kanso |
| --- | --- |
| `busy` / `retry` | `session_status=running`, card stays `in_progress` |
| `idle` | `session_status=idle`, card stays `in_progress` and stays polled; the agent moves it to `in_review` explicitly when the work is ready |
| `idle` + failed | `session_status=failed`, `attention=1`, card stays |
| `waiting-permission` / `waiting-question` | `session_status=waiting`, `attention=1` |
| unreachable (3 failures) | `session_status=offline`, `attention=1` |

Idle is the end of an assistant turn, not the end of the work: it never moves a
card. Never automatic `done`. Session transitions are recorded in the activity
log.

**Prompt template** (default, overridable per project):

```text
You are working on card {{ref}}: {{title}}
Project: {{project}}

{{description}}

Rules:
- Work only inside the current worktree.
- Report progress with the CLI: `kanso comment {{ref}} "…"`.
- When finished, run `kanso move {{ref}} in_review` and comment a summary.
- If blocked, run `kanso move {{ref}} blocker --reason "…"`.
- Never set the card to done, never delete it, never touch other cards.
```

**Pull flow** (no OpenChamber involved): the agent runs `kanso next --claim`,
works in whatever directory it was started in, and links its session with
`kanso session link ALP-42 ses_…`. Kanso only stores the link.

---

## 10. Deployment

### Container

- `Containerfile`: builder `rust:1-alpine` (musl target) → runtime `alpine:3.21`
  with `ca-certificates`, non-root uid/gid 1000, `/data` volume, port 8087.
- Static musl binary also published as a GitHub release asset (`kanso-x86_64-unknown-linux-musl`).

### Quadlet (`deploy/kanso.container`, copied to `~kanso/.config/containers/systemd/`)

```ini
[Unit]
Description=kanso (rootless Podman)
After=network-online.target
Wants=network-online.target

[Container]
ContainerName=kanso
Image=ghcr.io/<owner>/kanso:latest
PublishPort=127.0.0.1:8087:8087
EnvironmentFile=%h/.config/containers/systemd/kanso.env
Volume=/var/lib/kanso:/data:Z
AutoUpdate=registry
HealthCmd=wget -qO- http://127.0.0.1:8087/api/health
HealthInterval=30s
HealthTimeout=5s
HealthRetries=3
HealthStartPeriod=20s

[Service]
Restart=always
TimeoutStartSec=120

[Install]
WantedBy=default.target
```

`kanso.env` (0600, `kanso:kanso`): `KANSO_PASSWORD`, `KANSO_DATA_DIR=/data`,
`KANSO_PUBLIC_URL`, `KANSO_OPENCHAMBER_URL`, `KANSO_OPENCHAMBER_TOKEN`,
`KANSO_LOG=info,kanso=debug`.

### Host steps (documented in `docs/DEPLOY.md`, applied with `tsudo`)

1. `useradd --system --home-dir /var/lib/kanso --shell /sbin/nologin kanso` + linger.
2. `/etc/nginx/conf.d/kanso.conf` (TLS, SSE unbuffered, `client_max_body_size 2m`).
3. Add `kanso.dupre49.fr` to `certbot_extend.sh`, `nginx -t`, run the script.
4. Enable `podman-auto-update.timer` in the `kanso` user (04:00, randomized).
5. Update `SERVICES.md` (user, port, URL, update, backup) and `AGENTS.md` if needed.
6. Duplicati job covering `/var/lib/kanso` (including `backups/`).

### Backup

- `kanso-backup.timer` (user `kanso`, daily 03:15): `podman exec kanso kanso backup`
  → `VACUUM INTO /data/backups/kanso-YYYYMMDD-HHMM.db`, keep 14 files.
- Duplicati backs up `/var/lib/kanso`; restore = stop unit, restore the newest
  dump as `kanso.db`, remove `-wal`/`-shm`, start unit, verify `/api/health`.

### CI (`.github/workflows/release.yml`)

- On tag `v*`: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`,
  musl build, multi-stage image build, push `ghcr.io/<owner>/kanso:<tag>` and
  `:latest`, attach the musl binary to the GitHub release.
- On PR: fmt, clippy, tests, `docker build` (no push).

---

## 11. Testing

**Unit tests** (`#[cfg(test)]`): rank insertion and renormalization, status
machine (including blocker restore), dependency gating, scope matrix, token
hashing, prompt rendering, slug generation, ref parsing.

**Integration tests** (`tests/api.rs`, in-process router + temp SQLite):
login/cookie/CSRF, token auth, project + task CRUD, move with before/after,
blocker round-trip, claim conflicts (`409`), proposals accept/reject, agent
denials (done/delete/accept), label uniqueness, checklist ordering, SSE emits on
mutation, export JSON shape, `VACUUM INTO` backup.

**CLI tests** (`tests/cli.rs`): run the binary against a live test server:
`config init`, `task add`, `next --claim`, `comment`, `move`, `--json` output,
exit codes for `409`/`403`.

**Manual UI checklist** (`docs/TESTING.md`): drag & drop mouse and touch, panel
collapse, SSE across two tabs, reconnect after server restart, keyboard-only
flow, mobile widths, markdown sanitization (a card containing `<script>` renders
as text).

**CI**: all of the above on every push/PR.

---

## 12. Milestones

| # | Deliverable | Done when |
| --- | --- | --- |
| M0 | Scaffold: cargo workspace, axum, config, SQLite migrations, `/api/health`, Dockerfile | `cargo test` green, container starts, health returns ok |
| M1 | Auth (password, cookie, tokens, scopes) + projects/tasks/labels/checklist/comments/activity API | integration tests for the whole API matrix pass |
| M2 | Board API + SSE + rank/move/blocker/dependencies/proposals | move and blocker round-trip tests pass |
| M3 | Web UI: board, panel B, drag & drop, create/edit, filters, global view, inbox | manual checklist passes on desktop + mobile widths |
| M4 | CLI + skill file | CLI test suite passes; an agent can complete a card end to end via CLI |
| M5 | OpenChamber adapter (dispatch + polling), settings screen | one real card dispatched into a worktree, moved to review by the agent |
| M6 | Deployment artefacts: quadlet, nginx, backup timer, GitHub Actions, docs | image published, unit running, TLS URL healthy, backup restores |
| M7 | Polish: notifications, export screen, keyboard help, README screenshots | reviewed and documented |

Each milestone is a separate commit series; the repository stays runnable at
every tag.

---

## 13. Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| OpenChamber `/api/openchamber/control` is an internal contract, not versioned | isolated `adapter/openchamber.rs`, startup version probe, `/api/integration` reports `reachable`/`version`, all failures degrade to "manual mode" |
| Container cannot reach `192.168.1.24:3000` from the rootless network | verified in M5 with a throwaway `podman run --rm … wget`; fallback documented (host gateway or a loopback publish for OpenChamber) |
| SSE through nginx buffered by default | `proxy_buffering off`, `proxy_read_timeout 1800`, verified with `curl -N` |
| Drag & drop on touch | pointer events + long press + "Move to…" fallback; manual test matrix |
| Markdown XSS | `marked` output sanitized by an allow-list before insertion; integration test with hostile input |
| SQLite corruption on abrupt stop | WAL + `synchronous=NORMAL`; nightly `VACUUM INTO`; health check verifies `PRAGMA quick_check` |
| nginx/container UID mismatch on `/var/lib/kanso` | `:Z` bind mount, container runs as uid 1000, `podman unshare chown` documented |

---

## 14. Immediate next steps after approval

1. Scaffold the Rust project, license, README, CI skeleton.
2. Implement M0–M1, then M2, then the UI, CLI, adapter, deployment.
3. At each milestone: commit, run the test suite, report the state here.
