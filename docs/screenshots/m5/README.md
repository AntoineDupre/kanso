# M5 screenshots

Captured against a local Kanso instance connected to a real OpenChamber 2.6.0
(`KANSO_OPENCHAMBER_URL` / `KANSO_OPENCHAMBER_TOKEN`), 1440×900 unless noted.

The card `E2E-1` was dispatched from the card panel; OpenChamber created the
worktree, the agent wrote `E2E.md`, and the poller moved the card to **In review**
when the session went idle.

| File | View |
| --- | --- |
| `integration-settings.png` | Settings → Integration, enabled, reachable, version |
| `card-session.png` | Card panel session block and automatic activity entries |
| `dispatch-button.png` | Dispatch control on an unclaimed card, plus the link form |
| `board.png` | Board after the automatic move to In review |
| `integration-dark.png` | Settings → Integration in the dark theme |

Reproduce the setup:

```sh
KANSO_PASSWORD=dev KANSO_DATA_DIR=/tmp/kanso-m5 \
KANSO_OPENCHAMBER_URL=http://<openchamber>:3000 \
KANSO_OPENCHAMBER_TOKEN=oc_client_… cargo run -- serve
```

The manual checklist is in [`../../TESTING.md`](../../TESTING.md).
