# Manual UI testing (M3)

The board is a plain ES-module SPA served by `kanso serve`; there is no build
step. Run the automated checks first:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

`tests/ui.rs` covers the shell, every module, `i18n/en.js`,
`vendor/marked.min.js`, content types and the deep-link fallback.

## Start a clean local instance

```sh
rm -rf /tmp/kanso-ui
KANSO_PASSWORD=dev KANSO_DATA_DIR=/tmp/kanso-ui cargo run -- serve
```

Open <http://127.0.0.1:8087>. Use a current Chromium or Firefox. Keep the
browser DevTools console open and watch for errors during every step.

To restart the server, stop it with Ctrl+C and run the same command again; the
database in `/tmp/kanso-ui` survives, which is useful for the reconnection test.

## 1. Login and session

1. Load the page in a private window. Only the login card appears; no board
   layout flashes before the auth check resolves.
2. Enter a wrong password: an inline error appears.
3. Enter `dev`: the board opens (empty until a project exists).
4. Use the top-bar sign-out control: the login card returns, the event stream
   closes and client state is gone.
5. In DevTools, clear the `kanso_session` cookie and press reload: the login
   card appears instead of a broken board.

## 2. Projects and cards

1. On the empty board choose **Create a project** and fill in key `ALP`, name
   `Alpha`, a description, a colour, and (optionally) the agent fields. Save.
2. Open **Settings → Projects** and confirm the project is listed; edit its
   name and save. Keep `ALP` active for the rest of this run; archiving is
   exercised on a throwaway second project in section 8 (archived keys stay
   taken, so `ALP` cannot be re-created without a fresh data directory).
3. Create three cards with **New card**: one in Backlog, one in To do, one in
   In progress. Give them different priorities and a due date in the past and
   one in the future. Labels are created in the panel flow of section 6.
4. Use a column's **Add a card** footer and press Enter: the card is created in
   that column without opening the full dialog.
5. Copy the URL of an open card (`/p/ALP/task/1`) and hard-reload it. The board
   and the 420 px panel return on the deep link.
6. Enter a made-up project key (`/p/NOPE`) and a made-up card
   (`/p/ALP/task/9999`): both show a friendly not-found page with a link back to
   the board, not a blank page or console crash.

## 3. Drag & drop

1. With a mouse, drag a card within its column: a ghost follows the pointer, a
   dashed placeholder moves between cards, and the drop reorders immediately.
   Reload and confirm the order survived.
2. Drag a card across columns. The source count drops, the target count rises
   immediately (optimistic update), and the panel/column order matches after the
   request completes. If the server rejects the move, a toast appears and the
   board refetches.
3. Start a drag and press Escape: the card returns to its place and no request is
   sent.
4. Drag a card to the far left/right edge of the board: the board auto-scrolls.
5. In DevTools device emulation (touch), a short swipe scrolls the board without
   starting a drag; press and hold a card for about 250 ms, then drag. A plain
   tap opens the card.
6. On every card, open the **⋮ / Move to…** control and move it with the menu.
   This must work without a pointer drag.

## 4. Keyboard-only flow

Focus the page (not an input) and try:

- `n` opens the new-card dialog.
- `/` focuses the search field; `Esc` clears and leaves it.
- `j` / `k` selects the next / previous visible card, scrolls it into view and
  opens its panel.
- `m` opens the **Move to…** menu for the selected card.
- `Esc` closes the panel.

Shortcuts must do nothing while an input, textarea or select has focus. Tab
through the top bar, columns and panel controls; every action is reachable and
has a visible focus ring.

## 5. Two tabs and live updates

1. Open the board in two tabs.
2. Create a card, move it, comment on it, and toggle a checklist item in tab A.
   Tab B updates without a reload (checklist progress/comment included when its
   panel is open).
3. In tab A, open a card panel. In tab B, archive or restore that card; tab A
   reflects the state change.
4. Stop the server with Ctrl+C. Both tabs show a connection-lost toast; tab B
   keeps working in a degraded read-only-ish state.
5. Start the server again. Within the backoff window both tabs reconnect, show a
   connection-restored toast, and fully refetch the board and any open card.

## 6. Card panel

On one card, verify in order:

1. Description: Markdown renders; **Edit** → change → **Save**; **Cancel**
   restores the old text.
2. Status, priority and due date editors persist after reload. Clearing the due
   date removes the chip.
3. Labels: on a clean database use **New label** first to create `urgent` and
   add it in the same step; remove it, then add it again from the label picker.
4. Checklist: add two items; toggle one (progress becomes `1/2` and the board
   card shows `1/2`); edit an item's text; move it up/down; remove an item.
5. Comments: add a comment; it appears in the panel and, when the other tab has
   the same card panel open, there too. The board itself does not show comments.
6. Dependencies: add `ALP-2` to `ALP-1`'s **Blocked by** list. `ALP-2` now shows
   **Blocks** on its own panel. While `ALP-2` is not done, `ALP-1`'s card shows a
   lock and claiming it returns a `dependency_unmet` toast. Remove it from
   `ALP-1`'s **Blocked by** list, then add it again and remove it from
   `ALP-2`'s **Blocks** list; both directions have separate code paths.
7. Claim / Release: claim an unclaimed backlog/todo card; it moves to In progress
   and shows the claim badge. Release it.
8. Archive the card: it leaves the board but the panel stays usable and offers
   **Restore**. Restore it. Delete a throwaway card and confirm the confirmation
   dialog; it disappears everywhere.

## 7. Inbox

1. Create an agent token in **Settings → Tokens** (scope Agent) and copy it.
2. Push a proposal through the API:

   ```sh
   curl -s -X POST http://127.0.0.1:8087/api/tasks \
     -H "Authorization: Bearer kanso_REPLACE_ME" \
     -H "Content-Type: application/json" \
     -d '{"project_key":"ALP","title":"Proposal from an agent","description":"Details"}'
   ```

3. The Inbox tab badge increments in every open tab. The proposal is newest
   first and does not appear on the board.
4. Accept it into a chosen column (default Backlog): the toast confirms, the
   proposal leaves the inbox and the card appears on the board.
5. Reject another proposal: confirm the dialog, it disappears and stays off the
   board.
6. With an empty inbox, the empty state is shown.

## 8. Global view and settings

1. Create a second project and a few cards. Open **Global**: cards are grouped
   by project, then by the six fixed columns. By default only In progress, In
   review and Blocker are shown; tick **Show all columns** to see everything.
2. Search filters cards by reference, title, label and claim name. The search
   works in both project and global views.
3. **Settings → Projects**: create, edit (`PATCH` persists all agent fields),
   and archive a project. Archived projects are listed separately.
4. **Settings → Tokens**: create a Human and an Agent token; the plaintext is
   shown once and the Copy button works. The list shows scope, creation and
   last-used time. Revoke one and confirm it is marked revoked.
5. Agent identities in the UI would not see project/token/inbox admin controls;
   the API still enforces this, but the browser login is always the human UI
   session. Check with a bearer token only through the API if needed.

## 9. Markdown safety

Put this in a card description (and in a comment), save, then inspect the DOM:

```markdown
<script>window.__xss = 1</script>
<img src=x onerror="window.__xss = 2">
![tracking pixel](https://example.com/pixel.png)
<iframe src="https://example.com"></iframe>
[bad](javascript:alert(1)) [data](data:text/html,<b>x</b>)
**safe** and `code`
[good](https://example.com)
```

Expected: no script runs, `window.__xss` is undefined, the raw HTML tags and
`javascript:` links render as inert text (or lose their link), the Markdown
image becomes its alt text, and the good link opens in a new tab with
`rel="noopener noreferrer"`. No external image or iframe request appears in the
Network panel.

## 10. Responsiveness and themes

1. At 1100 px wide: the panel overlays the board instead of shrinking the
   columns; drag its left edge to resize (320–640 px) and reload to confirm the
   width and collapsed state persist in `localStorage`.
2. At 700 px wide: columns are about 85 vw and the board scrolls horizontally;
   the panel becomes full-width when opened.
3. Toggle the operating system between light and dark themes; both remain
   readable (cards, panel, labels, modals, toasts, due-date colours).
4. Scroll the board horizontally and vertically; the top bar and page stay
   usable, and the board has no double scrollbar surprises.

## 11. Notifications (optional)

1. Click the bell control (never on page load) and grant permission.
2. Hide the tab (switch windows) and, from another tab or the API, move a card to
   In review. A single browser notification fires. (The `attention` flag is
   written by the agent adapter in M5; for M3 the In review branch is the
   reachable notification path.)
3. Click the bell once to turn notifications off, deny permission in browser
   settings, then click the bell again: a warning toast explains that
   notifications are blocked and no permission prompt loops.
