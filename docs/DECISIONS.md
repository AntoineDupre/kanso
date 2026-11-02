# Decisions

Small choices the plan leaves open, recorded as one line each.

- CLI usage errors exit 1 (clap defaults to 2) to match PLAN §6; API/conflict exit codes 2/3 arrive with the CLI in M4.
- `/api/health` returns HTTP 503 with `{"status":"error","version":"…","db":"error"}` when `PRAGMA quick_check` fails; the plan only defines the healthy shape.
- The health `quick_check` result is cached for 5 s and refreshed single-flight, so unauthenticated probes cannot saturate the blocking pool.
- Static assets are read from `web/` next to the executable, falling back to `./web`; startup fails fast when `index.html` is absent.
- `/api` paths never fall back to `index.html`; missing paths with a file extension return 404, extensionless paths serve the shell.
- All static responses use `Cache-Control: no-cache` until asset URLs are fingerprinted.
- `PRAGMA synchronous=NORMAL` is set alongside WAL, following the corruption mitigation in PLAN §13.
- `serve --port` replaces the port in `KANSO_BIND` and `--data-dir` overrides `KANSO_DATA_DIR`; there is still no config file.
- The crate is split into `src/lib.rs` plus a thin `src/main.rs` so the M1 in-process API tests can import the router.
- The runtime image uses `alpine:3.24` instead of PLAN §10's `3.21` because 3.21 reached end of support on 2026-11-01; the builder is pinned to `rust:1.99-alpine`.
- The container healthcheck derives its port from `KANSO_BIND` and allows 10 s, while the default check interval remains 30 s.
- No `anyhow` or `tempfile` dependency: a small `Error` type and std temporary directories cover the scaffold.
- Session cookies are `session_id.hmac_sha256_base64url`; the HMAC key is a random 32-byte `session.key` in the data directory, created atomically with mode 0600 and independent of `KANSO_PASSWORD`, so sessions survive restarts and deleting the file revokes them all.
- UI sessions expire 30 days after login; `last_seen` slides at most once per hour, but does not extend `expires_at` (the milestone says `expires_at` 30 days and `last_seen` sliding).
- The login limiter allows five attempts per client per 60 s and resets on success. With the loopback nginx proxy, the key is `X-Real-IP` (then the last `X-Forwarded-For` hop); direct non-loopback connections use the socket address and ignore forwarded headers.
- `GET /api/projects?archived=1` returns active and archived projects; the default excludes archived ones.
- Task list ordering uses the board order (`backlog, todo, in_progress, in_review, blocker, done`) rather than SQLite's lexical status order, then rank and number.
- `PATCH` payloads use `deny_unknown_fields`, so `status`, `rank`, `project_id` and other non-editable fields are rejected with `validation` (400) rather than silently ignored; `due_date: null` clears the date.
- Checklist `before`/`after` name neighbouring item ids: `after` is the item that precedes the moved item and `before` the item that follows it. When both are present they must be in that order; a gap below `2 * MIN_GAP` renumbers the checklist inside the transaction.
- Comments and activity rows use `ui` for the password session and the token name for bearer tokens; task `created_by` follows the same rule.
- Token names are trimmed and limited to 64 characters (the plan sets no limit); token `last_used_at` is refreshed at most once per minute, session `last_seen` at most once per hour.
- API token hashes are stored as lowercase SHA-256 hex and are never returned; only `POST /api/tokens` returns the plaintext token, once.
- Internal failures return `500` with code `internal` and a generic message; details are logged server-side. Serde data errors (Axum's 422) are mapped to `400 validation`, while 413/415 keep their status and use the `validation` code.
- M1 follows the milestone brief for hard deletion: `DELETE /api/tasks/:ref?hard=true` requires the flag and a human identity. The extra confirmation header described in PLAN §4 is deferred to the API/UI work that can send it.
- Task numbers follow the brief's `max(number) + 1` inside the create transaction. Hard-deleting the highest-numbered card therefore allows that number to be reused; a permanent per-project counter is not part of M1.
- Agent scope in M1 is deliberately the milestone subset: read projects/tasks/labels, comment, and add or update checklist items (reordering included). Checklist deletion and all task/project/label/token writes are human-only until M2's move/claim work; `GET /api/tokens` is also human-only.
- `getrandom`, `sha2` and `base64` are the only new dependencies: OS randomness, SHA-256/HMAC and URL-safe token encoding are security primitives not worth reimplementing.
