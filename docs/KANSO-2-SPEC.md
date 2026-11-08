# KANSO-2 — GitHub importer

## Status

Decisions Q1–Q14 were answered on KANSO-2. The human approved proceeding by
requesting a dedicated implementation session, verification, a pull request
with screenshots, and a review deployment at kanso.dupre49.fr.

## Scope

- Extend the existing manual external-task importer with GitHub support.
- GitHub sources are github.com repositories (`owner/repo`), not Projects boards.
- One Kanso project can link multiple repositories. Each repository independently
  enables issues, PRs to review, and PRs to follow.
- Human-only project settings manage links and enabled categories. Imports shows
  only repositories linked to the selected destination project.
- Each preview selects one repository and one enabled category. Assignee/author/
  reviewer filters are chosen per preview, not stored as repository rules.
- Preserve preview, explicit selection, confirmation, and per-item outcomes.
- No scheduled import, remote refresh, automatic card creation, notifications,
  task transitions, or GitHub writes. Following a PR means creating a card for
  its owner to work from, not automatically monitoring reviews or Actions.

## Authentication

**KANSO-22 supersession:** the operator-only/browser-connect restriction below
describes the original importer milestone. Global connection management now
also offers the separately approved, asynchronous browser device-login flow;
it does not initiate login from preview/import requests. The other importer
constraints in this document remain in force.

- Use installed GitHub CLI (`gh`) with its existing authentication; do not add
  a Kanso-managed token, OAuth app, or browser Connect GitHub flow.
- The operator logs gh into github.com using its browser login under the same
  OS user/environment as the Kanso server. Services/containers need access to
  that authentication; a login on another machine is insufficient.
- All Kanso humans share this GitHub identity. “Me” is the authenticated GitHub
  account, not the Kanso actor.
- Invoke gh with structured arguments, never a shell command built from input.
  Never extract its token or initiate interactive authentication in requests.
- Report missing CLI/authentication and provider failures safely; never return
  raw subprocess errors that could disclose credentials or private diagnostics.
- No Enterprise hosts, per-repository credentials, or multi-account selection.

## Categories and filters

### Issues

- Open issues only; exclude PRs.
- Default assignee: me. Allow an explicit username; an explicitly empty assignee
  means all assignees.

### PRs to review

- Open, non-draft PRs with a currently outstanding review request addressed
  directly to the selected user, defaulting to me.
- Filter by requested reviewer, never PR assignee.
- Always exclude PRs authored by the authenticated GitHub account, including
  when the reviewer filter specifies another user.
- Exclude team-only requests, even if the user belongs to that team.
- No historical-review matching. A new request may make a PR eligible again,
  but an existing local card still prevents a second import or reopening.

### PRs to follow

- Open PRs authored by the selected user, defaulting to me; include drafts.
- Allow an explicit author username. Exclude closed and merged PRs.

No arbitrary GitHub search syntax, labels, milestones, team filters, or imported
assignee/reviewer metadata controlling Kanso assignment in this version.

## Cards, identity, and lifecycle

- Copy the GitHub title/body and prepend compact context: repository, item
  number, category, author, and link. Preserve provenance and safe source links.
- Create ordinary active backlog tasks with normal priority, using the existing
  human import creation/activity/event behavior.
- Do not fetch/copy comment threads, review threads, attachments, or Actions logs.
- Use stable GitHub item identity, not title, URL, repository name, or category,
  for uniqueness within each destination Kanso project.
- A PR imported through review and follow still has only one card. Keep the
  first imported category. Repository rename must not create a duplicate.
- Re-import skips existing cards, including archived cards, preserving all local
  edits and status. The same GitHub item can be imported into another project.
- Preserve existing content limits: report invalid/oversized individual items,
  never silently truncate, and allow other valid selected items to import.

## Safety and implementation constraints

- Reuse existing human authorization, CSRF, immutable preview snapshots,
  transactional uniqueness, safe Markdown/DOM rendering, and export provenance.
- Validate project/repository/category membership server-side; do not rely on
  dropdown choices or trust client-supplied candidates.
- Keep subprocess/network I/O outside DB locks; bound execution time, output,
  pagination, and candidate counts. An incomplete/failed query must not appear
  as a complete successful empty result.
- Preserve generic importer separation; make project-scoped discovery possible
  without embedding GitHub querying into the shared import workflow.
- Add append-only migrations for repository/category configuration as needed.
- No credential values in API metadata, cards, exports, logs, or errors.

## Acceptance checks

- Project settings support multiple repositories and independent categories;
  disabled/unlinked sources cannot be imported through forged requests.
- Cover all three categories, default/explicit filters, issue-versus-PR
  distinction, outstanding direct review requests, own-author exclusions,
  drafts, and closed/merged exclusions.
- Exercise production provider registration and project-scoped discovery.
- Test gh missing/unauthenticated/failing, malformed responses, safe errors,
  pagination/limits/timeouts, and untrusted content without live credentials.
- Test same PR across categories, renamed repositories, repeat/concurrent
  imports, archived cards, local edit preservation, and separate projects.
- Preserve existing auth, snapshot, partial-import, provenance/export, and UI
  regressions. Document server-side gh setup and manual-only lifecycle.
- Run formatting, linting, automated tests, and focused browser/UI checks.
