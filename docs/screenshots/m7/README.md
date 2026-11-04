# M7 screenshots

Captured from a local instance of the M7 build with two projects, eight cards
across the six columns, labels, a checklist, a comment, a dependency, an agent
claim, one linked session and one proposal. 1440×900 unless noted. The
integration view talks to a real OpenChamber 2.6.0.

| File | View |
| --- | --- |
| `board-light.png` | Board, light theme, six columns |
| `board-dark.png` | Board, dark theme |
| `card-panel.png` | Card panel with description, checklist, dependencies and activity |
| `global-view.png` | Global view grouped by project |
| `inbox.png` | Proposal inbox |
| `integration-settings.png` | Settings → Integration |
| `export-settings.png` | Settings → Export and the notification controls |
| `keyboard-help.png` | Keyboard shortcut dialog (`?`) |
| `mobile.png` | 430×900, horizontally scrolling columns |

Reproduce:

```sh
rm -rf /tmp/kanso-m7
KANSO_PASSWORD=dev KANSO_DATA_DIR=/tmp/kanso-m7 cargo run -- serve
```

The manual checklist is in [`../../TESTING.md`](../../TESTING.md).
