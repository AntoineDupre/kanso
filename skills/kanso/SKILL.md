---
name: kanso
description: Pull a card from the Kanso board, work on it in the current worktree, and report progress with the kanso CLI. Use when asked to work a Kanso card or when the task is tracked on the Kanso board.
---

# Working a Kanso card

You are an agent working a card from a Kanso board. Kanso tracks the work; the
human owns priorities, review and completion. Work only in the worktree you were
given: never create another worktree, change directory, or touch another card.

## The loop

1. **Pick a card.** Run `kanso next --project ALP` (omit `--project` for any
   project). Read the card with `kanso task show ALP-42`.
2. **Claim it.** Run `kanso claim ALP-42`. If it exits with code `3`, the card is
   already claimed or blocked; pick the next one instead of retrying.
3. **Do the work** in the current worktree only.
4. **Report milestones**, not a transcript. A short `kanso comment ALP-42 "…"`
   when a meaningful piece lands, when you hit a decision, and when you stop.
5. **Hand off for human review.** When the work is ready for a human to review,
   usually after opening a PR/MR, run `kanso move ALP-42 in_review` to move the
   card to **IN REVIEW**. Leave a summary comment with what changed, what you
   tested, the PR/MR link if applicable, and anything the human reviewer needs
   to know. Do not move the card to IN REVIEW for an AI-to-AI review; keep it
   in progress until it is ready for human review.
6. **If blocked**, run
   `kanso move ALP-42 blocker --reason "what you need"`. The reason is shown on
   the board; keep it concrete and short (500 characters at most). The human's
   answer arrives as a new message in the same session; continue the work and
   move the card out of blocker when you are unblocked. Do not poll for the
   answer.

## Rules

- IN REVIEW is reserved for human review, not review between AI agents.
- Never move a card to `done`. Review and completion are the human's decision.
- Never delete or archive a card, and never change another card's priority.
- Ideas discovered along the way go through `kanso propose "Idea" --project ALP
  --description "…"`. Do not create cards directly.
- Do not accept or reject proposals, edit projects, or manage tokens.
- If a command fails, read the exit code and the message on stderr; do not retry
  a conflict blindly.

## Command shapes

```sh
kanso next [--project ALP] [--claim] [--agent NAME]
kanso task show ALP-42
kanso claim ALP-42 [--agent NAME]
kanso release ALP-42
kanso comment ALP-42 "short milestone"     # "-" reads stdin
kanso check ALP-42 --add "item"
kanso check ALP-42 --done 2
kanso check ALP-42 --list
kanso move ALP-42 in_review [--agent NAME]
kanso move ALP-42 blocker --reason "…" [--agent NAME]
kanso move ALP-42 restore
kanso propose "Idea" --project ALP [--description "…"] [--agent NAME]
kanso inbox
# human only, not part of the agent loop:
kanso session link ALP-42 ses_… --directory /path/to/worktree
```

`--agent NAME` replaces the token name in comments, activity and `created_by`
for that command. Use it when the token is shared between agent roles.

Add `--json` to any command to print the API's own JSON for scripts.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | success |
| `1` | usage error (bad arguments, missing config) |
| `2` | API error (400, 401, 403, 404, 5xx), network failure or local I/O error |
| `3` | conflict: already claimed, dependency unmet, not active, not claimable or rate limited |

Errors are printed to stderr as `kanso: <code>: <message>`.

## Configuration

`kanso config init --url https://kanso.example --token kanso_…` validates the
token with `GET /api/me` and writes `~/.config/kanso/config.toml` with mode
0600. `KANSO_URL` and `KANSO_TOKEN` override the file; `KANSO_CONFIG` overrides
its path. The usual agent setup is a token with scope `agent`.
