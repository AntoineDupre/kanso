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
