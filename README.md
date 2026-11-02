# Kanso

A single-user kanban board for your projects, with **optional** OpenCode /
OpenChamber agent sessions.

The design and milestone plan live in [PLAN.md](PLAN.md). This branch implements
**M0 (scaffold)**: the server, SQLite schema, health endpoint, static shell and
container image exist; the board, API and CLI arrive in later milestones.
`PLAN.md` remains the design reference and its status line predates this
milestone.

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
| `KANSO_PUBLIC_URL` | no | — | Public base URL, reserved for links in later milestones |
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

Only `GET /api/health` exists in M0. Non-`/api` routes serve the static shell
from `web/`, with history fallback to `index.html` and `{{VERSION}}` replaced by
the binary version; a missing asset with a file extension returns 404.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
