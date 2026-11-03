# M3 screenshots

Captured from a local instance running M3, 1440×900 unless noted, with two
projects, nine cards across the six columns, labels, a checklist, a comment, a
dependency, an agent claim and one proposal.

| File | View |
| --- | --- |
| `login.png` | Sign-in screen |
| `board-light.png` | Board, light theme, six columns |
| `card-panel.png` | Card panel with checklist and claim |
| `global-view.png` | Global view grouped by project |
| `inbox.png` | Proposal inbox |
| `tokens.png` | Settings → Tokens |
| `mobile.png` | 430×900, horizontally scrolling columns |
| `board-dark.png` | Dark theme with the card panel open |

Reproduce:

```sh
rm -rf /tmp/kanso-ui
KANSO_PASSWORD=dev KANSO_DATA_DIR=/tmp/kanso-ui cargo run -- serve
```

The manual checklist for these views is in [`../../TESTING.md`](../../TESTING.md).
