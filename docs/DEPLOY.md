# Deploying Kanso

This is the operator runbook for the production host. It assumes the hosting
rules in the external host service repository's `RULES.md`: a dedicated user,
rootless Podman through Quadlet, loopback-only publishing, host nginx as the
only TLS endpoint, secrets in a `0600` environment file and Duplicati for
backups.

The repository contains the artefacts; the actual host deployment, the git tag
and the image push are operator actions. Nothing here should be run as a shell
script without reading it first.

```
browser ──https──> nginx (host, TLS) ──http──> kanso (rootless container, 127.0.0.1:${KANSO_PORT})
                                                    └── SQLite ${KANSO_HOME}/data/kanso.db
```

| Item | Value |
| --- | --- |
| Public URL | `https://${KANSO_DOMAIN}` |
| Container image | `${KANSO_IMAGE}` |
| Service user / home | `${KANSO_USER}` / `${KANSO_HOME}` |
| Loopback port | `127.0.0.1:${KANSO_PORT}` |
| Data | `${KANSO_HOME}/data` → `/data`, SQLite, `backups/`, and optional GitHub CLI credentials |
| Deployment values | `deploy/deploy.env` (gitignored; `deploy/deploy.env.example` committed) |
| Quadlet source | `deploy/kanso.container.in` (rendered to `deploy/out/kanso.container`) |
| Network unit | `deploy/kanso.network` (static) |
| nginx source | `deploy/nginx.conf.in` (rendered to `deploy/out/nginx.conf`) |
| Backup units | `deploy/kanso-backup.service`, `deploy/kanso-backup.timer` |
| Release workflow | `.github/workflows/release.yml` |

## 1. Prerequisites

- Fedora Server with rootless Podman 5.x, `systemd`, `crun`, `uidmap`/subuid
  ranges, nginx with the shared `/etc/nginx/ssl/ssl.conf`.
- `certbot_extend.sh` from the host service repository and the shared
  certificate `${KANSO_CERT_NAME}`.
- DNS: `${KANSO_DOMAIN}` points at the host, and only `80/tcp` and `443/tcp`
  are reachable from the internet. Do **not** open `${KANSO_PORT}` in firewalld.
- A release that published `${KANSO_IMAGE}` (pushing a `v*`
  tag runs `.github/workflows/release.yml`). If the package is private, give
  the service user a **persistent** registry auth file (for example
  `${KANSO_HOME}/.config/containers/auth.json` with `REGISTRY_AUTH_FILE` set),
  because Podman's default rootless auth file under `/run/user/…` is lost on
  reboot. Put `REGISTRY_AUTH_FILE=${KANSO_HOME}/.config/containers/auth.json`
  in `${KANSO_HOME}/.config/environment.d/10-kanso-registry.conf` for the user
  manager, and pass the same variable to manual `podman pull` commands. Never
  put a registry token in this repo.
- Duplicati is already running on the host. Add `${KANSO_HOME}/data` to a job
  after the first backup exists (section 6). If GitHub CLI is authenticated,
  this directory can contain plaintext credentials; protect and encrypt backups
  accordingly, restrict restore access, and rotate credentials if exposed. A
  database-only restore does not restore the gh login; authenticate again if
  needed. Never commit or print its credential files.
- Root actions go through the operator's normal `sudo`/`tsudo` path. The
  commands below use `sudo` for readability; use the approved wrapper if your
  host requires it.

The service user must not own the public TLS material and does not need sudo.

## 2. Render the artefacts, create the service user and directories

Start from the repository root. The host values live only in the ignored
`deploy/deploy.env`; the committed `deploy/deploy.env.example` shows the shape.

```sh
[ -f deploy/deploy.env ] || cp deploy/deploy.env.example deploy/deploy.env
${EDITOR:-vi} deploy/deploy.env

# Choose TLS in deploy.env before rendering. Either a host-managed snippet:
#   KANSO_TLS_INCLUDE=/etc/nginx/ssl/ssl.conf
# or a self-contained TLSRef 6.0 modern snippet:
#   cp deploy/tls.conf.example deploy/tls.conf
#   ${EDITOR:-vi} deploy/tls.conf    # set certificate/key/trust paths and resolver
#   KANSO_TLS_SNIPPET=deploy/tls.conf
# Leave both empty only to add TLS by hand later; the renderer warns.

set -a; . deploy/deploy.env; set +a
./deploy/render.sh
```

The committed nginx template contains no `include`, certificate path or host TLS
path. The render step inserts either `include <path>;` or the snippet contents;
with neither it warns and leaves a comment to add TLS manually. Relative
`KANSO_TLS_SNIPPET` paths resolve from the repository root. The script
refuses missing required values or `example.com` placeholders and writes:

```text
deploy/out/kanso.container
deploy/out/nginx.conf
deploy/out/kanso.env.example
```

`deploy/deploy.env` and `deploy/out/` are gitignored. `render.sh --env FILE
--out DIR` overrides both paths. Keep `KANSO_PASSWORD` and the OpenChamber token
in the separate app `kanso.env`, never in `deploy.env`.

Then create the service account and directories:

```sh
# -F gives a system account the subuid/subgid range rootless Podman needs;
# plain --system leaves /etc/subuid and /etc/subgid empty for the user.
sudo useradd --system -F --home-dir "${KANSO_HOME}" --create-home \
  --shell /sbin/nologin "${KANSO_USER}"
sudo loginctl enable-linger "${KANSO_USER}"
loginctl show-user "${KANSO_USER}" -p Linger          # Linger=yes
grep -E "^${KANSO_USER}:" /etc/subuid /etc/subgid   # one range in each file

# Everything the host user needs to own: the Quadlet/env files, the systemd
# user units, Podman's own storage and the parent of the data directory.
sudo install -d -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0755 ${KANSO_HOME}
sudo install -d -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0755 \
  ${KANSO_HOME}/.config \
  ${KANSO_HOME}/.config/containers \
  ${KANSO_HOME}/.config/containers/systemd \
  ${KANSO_HOME}/.config/environment.d \
  ${KANSO_HOME}/.config/systemd \
  ${KANSO_HOME}/.config/systemd/user \
  ${KANSO_HOME}/.local \
  ${KANSO_HOME}/.local/share \
  ${KANSO_HOME}/.local/share/containers \
  ${KANSO_HOME}/.cache \
  ${KANSO_HOME}/data

# Only the dedicated data subdirectory is the bind source. The container runs
# as uid 1000 (a subuid on the host), so it must own /data to create kanso.db
# and backups/ there. The rest of the home stays owned by the host service user
# and is NOT relabeled by :Z.
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman unshare chown 1000:1000 ${KANSO_HOME}/data
```

Why the ownership split: `${KANSO_HOME}/data` is the only path Podman
bind-mounts and relabels; `.config`, `.local` and `.cache` stay host-owned so
the Quadlet, env file, user units and Podman storage remain readable and
unrelabeled. Do not `chown -R` the whole home to `1000`, and do not put the
bind source back at `${KANSO_HOME}`. Files under `data/` are container-owned
on the host; use `sudo` or the container to inspect them. The Quadlet sets
`PodmanArgs=--umask=0077`, so `kanso.db`, its `-wal`/`-shm` sidecars and the
backup dumps are mode `0600` rather than world-readable.

## 3. Install the Quadlet and environment file

Install the rendered Quadlet and app env example into the service home. The
rendered files are in `deploy/out/`; the service's Quadlet reads
`%h/.config/containers/systemd/kanso.env`, where `%h` is `${KANSO_HOME}` for
this user.

```sh
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0644 deploy/kanso.network \
  ${KANSO_HOME}/.config/containers/systemd/kanso.network
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0644 deploy/out/kanso.container \
  ${KANSO_HOME}/.config/containers/systemd/kanso.container
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0600 deploy/out/kanso.env.example \
  ${KANSO_HOME}/.config/containers/systemd/kanso.env
```

Kanso uses the named bridge network `kanso.network` instead of Podman's default
pasta netns. Under pasta, the host's own LAN address is shadowed inside the
container and requests to it loop back to the container; on the bridge the
OpenChamber adapter must use `http://host.containers.internal:3000` (Podman's
host alias; on this host it resolves to `169.254.1.2`), not the LAN IP. This
also means OpenChamber must listen on the address that alias maps to (the host's
LAN/default-interface address), not only on `127.0.0.1`. Quadlet generates
`kanso-network.service`; `kanso.service` gains a dependency on it.

Fill in the secrets in the installed file. The render step has already substituted
`KANSO_PUBLIC_URL`; replace the OpenChamber example URL only if you use the
adapter:

```sh
sudoedit ${KANSO_HOME}/.config/containers/systemd/kanso.env
sudo chown ${KANSO_USER}:${KANSO_USER} ${KANSO_HOME}/.config/containers/systemd/kanso.env
sudo chmod 0600 ${KANSO_HOME}/.config/containers/systemd/kanso.env
```

The rendered file already contains `KANSO_DATA_DIR=/data`, the public URL
`https://${KANSO_DOMAIN}` and `KANSO_LOG`. Set the UI password; uncomment and
fill the OpenChamber pair only if you use the adapter:

```ini
KANSO_PASSWORD=<long random UI password>
# KANSO_OPENCHAMBER_URL=http://host.containers.internal:3000
# KANSO_OPENCHAMBER_TOKEN=<OpenChamber bearer token>
```

The OpenChamber pair is optional; the adapter stays disabled unless both values
are non-empty, and the rendered example ships both lines commented out. The
real file lives outside the repository at
`${KANSO_HOME}/.config/containers/systemd/kanso.env`; keep it out of chat,
tickets and logs. The repo only contains the templates
`deploy/kanso.env.example.in` and `deploy/deploy.env.example` (a local
`deploy/kanso.env` is ignored as a safety net).

Verify the file:

```sh
sudo stat -c '%a %U:%G %n' ${KANSO_HOME}/.config/containers/systemd/kanso.env
# 600 ${KANSO_USER}:${KANSO_USER} ...
```

## 4. Pull the image and start the service

Run Podman as the service user. `XDG_RUNTIME_DIR` is needed outside a login
session; with lingering enabled it is `/run/user/$(id -u "${KANSO_USER}")`.

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman pull "${KANSO_IMAGE}"
# Private package: add
#   REGISTRY_AUTH_FILE=${KANSO_HOME}/.config/containers/auth.json
# to the env list above. The user manager reads environment.d, but `sudo -u`
# does not, so manual commands need the variable explicitly.

sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user daemon-reload
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user start kanso.service
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user status kanso.service --no-pager
```

The generated unit is `kanso.service` (Quadlet turns `kanso.container` into
that name). Do **not** run `systemctl --user enable` on it: Quadlet units are
generated/transient, and the generator applies `[Install] WantedBy=default.target`
itself during `daemon-reload`. `is-enabled` normally reports `generated`; the
next boot is the real test.

Check the container and the loopback endpoint:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" podman ps
curl -fsS http://127.0.0.1:${KANSO_PORT}/api/health
# {"status":"ok","version":"0.6.0","db":"ok"}
```

The Quadlet has `HealthCmd` on `/api/health`, so `podman ps` should eventually
show `(healthy)`:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman inspect --format '{{.State.Health.Status}}' kanso
```

## 5. nginx, TLS and the public URL

Install the rendered server block and verify the TLS source you selected in
step 2 exists before reloading. Do not create or edit certificate files by
hand.

```sh
sudo install -o root -g root -m 0644 deploy/out/nginx.conf /etc/nginx/conf.d/kanso.conf
sudo nginx -t
```

For the host-managed `KANSO_TLS_INCLUDE` path with the shared certbot lineage,
edit the operator's `certbot_extend.sh` (outside this repository) and add
`-d ${KANSO_DOMAIN}` to the `certbot certonly --cert-name ${KANSO_CERT_NAME}
--expand …` call. Then run it as root; it expands the certificate, runs
`restorecon` and reloads nginx:

```sh
sudo ./certbot_extend.sh
sudo nginx -t
sudo systemctl reload nginx
```

For the self-contained `KANSO_TLS_SNIPPET` path, set the certificate, key,
trust-chain and resolver values in `deploy/tls.conf`, rerun `./deploy/render.sh`, install the
rendered `deploy/out/nginx.conf`, then `nginx -t` and reload. If the snippet
points at the host's certbot files, keep using `certbot_extend.sh` for renewal
and certificate expansion.

With `KANSO_TLS_INCLUDE`, the rendered config includes that host-managed
snippet (for this host, `/etc/nginx/ssl/ssl.conf`); with `KANSO_TLS_SNIPPET`, it
embeds the self-contained TLSRef 6.0 modern snippet. The committed template
contains neither. The rest of the file uses the four standard proxy headers,
`client_max_body_size 2m`, HTTP/1.1 to the backend and
`proxy_buffering off; proxy_cache off;`. `/api/events` is an exact-match
location with a 1800 s read timeout and `X-Accel-Buffering: no`, because SSE
must not be buffered. `/api/backup` also has its own exact-match location with
660 s read/send timeouts, above the CLI's 600 s deadline for `VACUUM INTO`.
Card dispatch (`/api/tasks/<ref>/dispatch`) gets a 240 s location because the
adapter waits up to 150 s for a worktree plus 30 s for permission auto-accept.
The HTTP→HTTPS redirect and the ACME challenge stay in the host's existing
`/etc/nginx/conf.d/http.conf`, as RULES §6 requires; this file does not
duplicate them.

Verify:

```sh
curl -fsS https://${KANSO_DOMAIN}/api/health
curl -sSI https://${KANSO_DOMAIN}/ | head
```

Open the URL, log in, and confirm the board loads. In the browser Network panel,
`/api/events` must stay open in `text/event-stream` and show `: heartbeat`
comments; if it closes immediately, re-check the `location = /api/events`
block and reload nginx after `nginx -t`.

For the GitHub importer, authenticate the container's shared GitHub
identity as the service user (not the operator's own Podman account):

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman exec -it kanso gh auth login --hostname github.com --web --git-protocol https --skip-ssh-key
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman exec kanso gh auth status --hostname github.com
```

See the container section of the README for shared-account and credential
storage precautions. `/data/.config/gh` remains there even if `KANSO_DATA_DIR`
is overridden; it needs its own persistent, protected mount in that case.

## 6. Backups

The timer runs daily at 03:15 with a 10-minute randomized delay and
`Persistent=true` (a missed run catches up after boot). The service waits for
the container health check (`podman wait --condition=healthy`) before calling
the server-side `VACUUM INTO` through the CLI, so a boot-time catch-up cannot
race the HTTP listener:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman exec kanso kanso backup
# creates /data/backups/kanso-YYYYMMDD-HHMMSS-<uuid>.db inside the container
```

`kanso backup` is a CLI command, not an in-process server action, so the
container needs a CLI config with a URL and a **human-scope API token**. Create
one token once in the web UI (**Settings → Tokens**, scope `human`, name
`backup`), then write it without putting it in shell history:

```sh
read -rsp 'Human backup token: ' KANSO_BACKUP_TOKEN; echo
# The CLI runs inside the container: the internal port is always 8087, not
# KANSO_PORT (which only controls the host publish).
printf 'url = "http://127.0.0.1:8087"\ntoken = "%s"\n' "$KANSO_BACKUP_TOKEN" \
  | sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
      podman exec -i kanso sh -c \
      'umask 077; mkdir -p /data/.config/kanso; cat > /data/.config/kanso/config.toml'
unset KANSO_BACKUP_TOKEN

sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman exec kanso sh -c \
  'ls -l /data/.config/kanso/config.toml'   # -rw------- kanso kanso
```

The token is stored only inside the data directory (`/data/.config/kanso/`),
which Duplicati covers with encryption. Revoke and replace it if it leaks.

Install the timer units (the service and timer are user units; systemd runs
them in the `${KANSO_USER}` user manager at boot because lingering is enabled):

```sh
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0644 deploy/kanso-backup.service \
  ${KANSO_HOME}/.config/systemd/user/kanso-backup.service
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0644 deploy/kanso-backup.timer \
  ${KANSO_HOME}/.config/systemd/user/kanso-backup.timer
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user daemon-reload
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user enable --now kanso-backup.timer
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user list-timers kanso-backup.timer
```

Run it once now and inspect the result:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user start kanso-backup.service
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  journalctl --user-unit kanso-backup.service -n 50 --no-pager

sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman exec kanso sh -c 'ls -lh /data/backups'
sudo ls -lh ${KANSO_HOME}/data/backups
```

The service keeps the newest 14 `kanso-*.db` files. Pruning **must** run inside
the container (`podman exec kanso sh -c '…'`). Rootless Podman maps container
uid 1000 to a subuid on the host, so the host service user cannot delete the
files the container wrote; the container user can. This is why the delivered
service has an `ExecStartPost` that runs the `ls -1t | tail -n +15 | rm` loop
inside the container rather than on the host.

A backup file on the host is owned by a subuid (for example `166535`). That is
normal. Use `sudo` or the container to inspect it; never `chown -R` the data
directory to the host user to "fix" it.

Add `${KANSO_HOME}/data` to Duplicati now. The env file and Quadlet live
outside that tree in `${KANSO_HOME}/.config`; back them up separately (they are
small and contain secrets, so keep the Duplicati encryption on). Test a restore
following section 8 and record the result in the host backup log.

## 7. Updates

The Quadlet has `AutoUpdate=registry`; enable the user timer and move its daily
run to 04:00 with a randomized delay, matching the host rules and leaving the
03:15 backup window ahead of it:

```sh
sudo install -d -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0755 \
  ${KANSO_HOME}/.config/systemd/user/podman-auto-update.timer.d
sudo tee ${KANSO_HOME}/.config/systemd/user/podman-auto-update.timer.d/override.conf \
  >/dev/null <<'EOF'
[Timer]
OnCalendar=
OnCalendar=*-*-* 04:00:00
RandomizedDelaySec=30m
EOF
sudo chown -R ${KANSO_USER}:${KANSO_USER} ${KANSO_HOME}/.config/systemd/user/podman-auto-update.timer.d
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user daemon-reload
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user enable --now podman-auto-update.timer
```

Before the first update, and periodically afterwards:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman auto-update --dry-run
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user list-timers podman-auto-update.timer
```

The update timer follows the image reference in `Image=`; with the delivered
`:latest` tag it follows the newest published build. A major upgrade that
changes database behaviour must be treated as a manual operation: take a backup
first, read the release note, stop the unit, update, start, verify
`/api/health` and the board, and keep the previous image tag for rollback. If
you pin `Image=` to a version tag during a rollback, the timer checks that tag
instead; pinning is therefore both the rollback mechanism and the way to stop
`:latest` from moving back.

## 8. Restore

Stop the board and the backup timer so nothing writes while the database is
replaced. `kanso.db-wal` and `kanso.db-shm` must be removed: a dump is a
standalone SQLite file and stale WAL/SHM files can replay changes that are not
in the dump.

```sh
KANSO_UID="$(id -u "${KANSO_USER}")"
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" \
  systemctl --user stop kanso-backup.timer
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" \
  systemctl --user stop kanso.service
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" podman ps --all
```

Choose the dump and copy it into place. Use `sudo install` so the intermediate
owner is the service user, then map it to container uid 1000, which is the user
the server runs as:

```sh
# List candidates newest first, then choose explicitly: the newest dump for a
# normal restore, or the pre-upgrade dump when rolling an image back.
sudo sh -c "ls -1t \"${KANSO_HOME}/data/backups\"/kanso-*.db | head -n 10"
BACKUP="${KANSO_HOME}/data/backups/<chosen-kanso-YYYYMMDD-HHMMSS-uuid.db>"
echo "$BACKUP"
sudo install -o "${KANSO_USER}" -g "${KANSO_USER}" -m 0600 "$BACKUP" ${KANSO_HOME}/data/kanso.db
sudo rm -f ${KANSO_HOME}/data/kanso.db-wal ${KANSO_HOME}/data/kanso.db-shm
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" \
  podman unshare chown 1000:1000 ${KANSO_HOME}/data/kanso.db
```

Optional: remove `${KANSO_HOME}/data/session.key` to revoke every UI session.
If you leave it, existing browser sessions keep working after the restore.

Start again and verify:

```sh
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" \
  systemctl --user start kanso.service
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$KANSO_UID" \
  systemctl --user start kanso-backup.timer
curl -fsS http://127.0.0.1:${KANSO_PORT}/api/health
curl -fsS https://${KANSO_DOMAIN}/api/health
```

Log in and open a known card. If the health check fails, inspect
`journalctl --user-unit kanso.service` and `podman logs kanso` before touching
the dump again. The database must be writable by container uid 1000; an
`attempt to write a readonly database` error almost always means the restore
skipped the `podman unshare chown` step.

## 9. Rollback

Two things can be rolled back: the image and the database. They go together if
the newer image ran a migration.

1. Take a backup before the upgrade (the timer does this nightly; run it
   manually for an immediate one).
2. Change `Image=` in `${KANSO_HOME}/.config/containers/systemd/kanso.container`
   to the previous immutable tag or digest of the same image (for example a
   previously published `vX.Y.Z` tag).
3. Reload and restart as the service user:

   ```sh
   sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
     systemctl --user daemon-reload
   sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
     systemctl --user restart kanso.service
   ```

4. If the old image cannot read the migrated schema, restore the **pre-upgrade**
   dump explicitly (select it by timestamp or name rather than taking the
   newest file); section 8 shows the mechanics.
5. Leave `Image=` pinned to the old tag until the fix is ready. Reverting it to
   `:latest` before then lets the 04:00 auto-update move the service forward
   again; set it back and repeat step 3 after the fix.

Never delete the previous dump until the new version has passed a reboot and a
restore test.

## 10. Verification checklist

Run these after installation and after every update:

```sh
# Loopback
curl -fsS http://127.0.0.1:${KANSO_PORT}/api/health

# Public TLS and redirect
curl -fsS https://${KANSO_DOMAIN}/api/health
curl -sSI http://${KANSO_DOMAIN}/ | head

# Container health and logs
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman inspect --format '{{.State.Health.Status}}' kanso
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  journalctl --user-unit kanso.service -n 50 --no-pager

# SSE: open the board's Network tab and confirm /api/events stays open.
# From a shell, without putting the session cookie in history or chat:
#   read -rsp 'Session cookie: ' KANSO_COOKIE; echo
#   printf 'cookie = "kanso_session=%s"\n' "$KANSO_COOKIE" \
#     | curl -N -K - https://${KANSO_DOMAIN}/api/events
#   unset KANSO_COOKIE
# Expect event frames and a ': heartbeat' roughly every 25 seconds.

# Updates and backup timers
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  podman auto-update --dry-run
sudo -u "${KANSO_USER}" -H env XDG_RUNTIME_DIR="/run/user/$(id -u "${KANSO_USER}")" \
  systemctl --user list-timers

# nginx config
sudo nginx -t
```

Also confirm after a reboot: `loginctl show-user "${KANSO_USER}" -p Linger` is `yes`, the
container is healthy without a login, and the public URL answers.

## 11. Troubleshooting

| Symptom | Check / fix |
| --- | --- |
| `kanso.service` not found after `daemon-reload` | The file must be at `${KANSO_HOME}/.config/containers/systemd/kanso.container`; run `systemctl --user daemon-reload` as `${KANSO_USER}` with `XDG_RUNTIME_DIR`, then `systemctl --user cat kanso.service`. If it is still missing, check the syntax with `/usr/libexec/podman/quadlet -dryrun -user`. |
| Container exits at startup | `journalctl --user-unit kanso.service`, `podman logs kanso`; usually a missing/empty `KANSO_PASSWORD`, a bad env file path, or a data directory not writable by uid 1000. |
| `attempt to write a readonly database` | Re-run `podman unshare chown 1000:1000 ${KANSO_HOME}/data` (or the file restored in section 8); do not chown the home to the host user. |
| Port `${KANSO_PORT}` already in use | `ss -ltnp "( sport = :${KANSO_PORT} )"`; another unit or a manual `podman run` is holding it. Stop the duplicate. |
| nginx 502 | `curl http://127.0.0.1:${KANSO_PORT}/api/health`; check the container is running and `proxy_pass http://127.0.0.1:${KANSO_PORT}`. `nginx -t` and `journalctl -u nginx`. |
| Public URL works but SSE drops | The `/api/events` exact location must not buffer; check `proxy_buffering off`, `proxy_cache off`, `proxy_read_timeout 1800s`, and reload nginx. Browser devtools shows whether the stream is `text/event-stream`. |
| Login succeeds but POSTs fail CSRF | `KANSO_PUBLIC_URL` must be `https://${KANSO_DOMAIN}`; nginx must send `Host`, `X-Real-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`. |
| `/api/integration` says `reachable:false` while OpenChamber runs | Confirm the container is on `kanso.network`, not the default pasta netns: `systemctl --user cat kanso.service` should show `Network=kanso.network`, and `podman inspect kanso --format '{{.NetworkSettings.Networks}}'` should list `kanso`. Check `podman exec kanso wget -qO- http://host.containers.internal:3000/api/version`; it must resolve to the expected host alias (`169.254.1.2` on this host), not the host's LAN IP. |
| Backup service fails | `podman exec kanso kanso backup` by hand; verify `/data/.config/kanso/config.toml`, the human-scope token is not revoked, and the server is healthy. |
| Backup files are root/subuid-owned | Expected under rootless Podman. Prune and inspect inside the container; use `sudo` for host copies. |
| Timer never fires | `loginctl show-user "${KANSO_USER}" -p Linger`, `systemctl --user list-timers` as `${KANSO_USER}`, and `journalctl --user-unit kanso-backup.timer`. `Persistent=true` catches up after boot. |
| Auto-update does nothing | `podman auto-update --dry-run` as `${KANSO_USER}`; `AutoUpdate=registry` must be on the container and `Image=` must be a tag such as `:latest` or `:vX.Y.Z` (a digest pin never auto-updates). Check the timer override and `podman-auto-update.timer`. |
| SELinux denial on the mount | Keep `:Z`; inspect `ls -Z ${KANSO_HOME}/data` and `ausearch -m avc -ts recent`. Do not disable SELinux; fix the label with `restorecon`/`podman unshare` as documented. |
| Certificate renewal misses the name | Re-check `certbot_extend.sh`, run it, then `nginx -t` and reload. Never hand-edit `/etc/letsencrypt`. |

## 12. What lives where

```
deploy/deploy.env.example       -> deploy/deploy.env (copy; gitignored)
deploy/tls.conf.example         -> deploy/tls.conf (copy; gitignored, optional)
deploy/kanso.network            -> ${KANSO_HOME}/.config/containers/systemd/kanso.network
deploy/kanso.container.in       --render--> deploy/out/kanso.container
                                -> ${KANSO_HOME}/.config/containers/systemd/kanso.container
deploy/kanso.env.example.in     --render--> deploy/out/kanso.env.example
                                -> ${KANSO_HOME}/.config/containers/systemd/kanso.env (0600)
deploy/nginx.conf.in            --render--> deploy/out/nginx.conf
                                -> /etc/nginx/conf.d/kanso.conf
deploy/kanso-backup.service     -> ${KANSO_HOME}/.config/systemd/user/kanso-backup.service
deploy/kanso-backup.timer       -> ${KANSO_HOME}/.config/systemd/user/kanso-backup.timer
```

Do not commit `deploy/deploy.env`, `deploy/out/`, a real `kanso.env`, a token, a
password or a host path that contains one.
