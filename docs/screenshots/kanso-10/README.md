# KANSO-10 screenshot

Captured from a local build of the KANSO-10 branch at 1440×900. The board has
five cards across the columns; `ALP-42` is in Blocker with the agent's question,
a linked idle session and one sent reply. The right panel shows the new
**Agent question** section: the question, the session status, the last reply
and the reply box.

| File | View |
| --- | --- |
| `dialog.png` | Blocker card and the Agent question section |

Reproduce:

```sh
rm -rf /tmp/kanso-10
KANSO_PASSWORD=dev KANSO_DATA_DIR=/tmp/kanso-10 KANSO_BIND=127.0.0.1:8099 \
KANSO_OPENCHAMBER_URL=http://127.0.0.1:9 KANSO_OPENCHAMBER_TOKEN=dummy \
cargo run -- serve
```

Seed a project and a blocker card with a reason, a linked session and a sent
reply, then open `http://127.0.0.1:8099/p/ALP/task/42`. The manual checklist is
in [`../../TESTING.md`](../../TESTING.md).
