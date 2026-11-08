# KANSO-1 — External task importer

## Status

Product decisions Q1–Q10 and the consolidated specification were approved on
KANSO-1. The human requested implementation in a dedicated OpenChamber session
in this worktree. The coordinating session verifies the result and manages
KANSO-1 and the pull request.

## Scope and card boundary

- KANSO-1 delivers shared importer infrastructure, human-only API operations,
  and a browser preview/filter/selection workflow.
- GitHub is the first planned provider. Its adapter, credentials, repository
  configuration, filters, and task categories belong to KANSO-2, not this card.
- KANSO-2 covers issues, PRs to review, and PRs to follow. Its project-level
  source configuration must allow multiple GitHub sources per Kanso project.
  The shared contract must not assume one source per destination project.
- KANSO-1 ships without an enabled production provider. Show an honest empty
  state; use a test-only provider to verify the complete workflow. Do not ship
  a demo importer that accepts arbitrary client-supplied tasks as provider data.
- No CLI import command, scheduled import, automatic background creation,
  refresh/synchronization, or write-back to cloud services in this milestone.

## Human workflow

1. Open a dedicated Imports page from a project.
2. Explicitly select an existing, non-archived destination project and an
   available provider/source. Do not automatically create projects.
3. Set the filters exposed by that provider and request a preview.
4. Review candidates, their source/type/link, and existing-import information.
   Select candidates explicitly; do not preselect tasks for automatic creation.
5. Confirm the selected import and display created/skipped/failed outcomes per
   item, with links to created or already-existing local tasks where available.

Changing the destination, provider/source, or filters invalidates the previous
selection. Search/preview never creates tasks. If no provider is registered,
explain that provider support is not yet available and disable import actions.

## Shared provider contract

- Isolate providers behind a small Rust interface/registry, using the current
  application architecture rather than adding a plugin runtime.
- Providers describe their non-secret source/filter controls and return
  normalized preview candidates: stable provider-defined identity, task kind,
  title, Markdown description, and original source URL.
- Keep cloud-specific querying, authentication, pagination, and category
  semantics in provider adapters. Do not hard-code GitHub fields into the
  shared import API or require credentials for KANSO-1 testing.
- Keep external I/O outside database locks and expose sanitized provider errors.
- The server validates selections against server-obtained preview data. The
  browser cannot invent provider identities or replace candidate content during
  confirmation. Bound previews/selections and reject stale or invalid previews
  clearly rather than importing an unrelated result set.

## Imported tasks and provenance

- Create ordinary active cards in **backlog**, with **normal** priority.
- Copy the title and Markdown description. Retain provider/type identification
  and a clickable source link, displayed in task detail.
- Do not import remote comments, labels, assignees, attachments, or due dates.
  Do not interpret source state as a local status transition or auto-completion.
- Imported cards remain editable and otherwise behave like normal local cards.
  Subsequent imports never overwrite local edits or synchronize source changes.
- Preserve the current human/agent meaning of `tasks.source`; store external
  provenance separately rather than treating a provider as an agent identity.
- Record creation activity and publish normal task-created events after commit.
- Include non-secret provenance in versioned exports and normal DB backups.

## Duplicate and failure semantics

- Scope duplicate detection to the destination project and stable logical
  provider identity. Provider adapters own the stable source/category identity
  rules; the generic layer must not deduplicate by title or mutable URL alone.
- Repeating an import into the same project skips the existing item and leaves
  its content/status unchanged, including when the local card is archived.
  The same source item may be imported into a different destination project.
- Enforce uniqueness transactionally so concurrent confirmations and retrying
  an uncertain response do not create duplicate cards.
- Import valid selected items even if other selected items fail validation.
  Report created/skipped/failed per item and allow retry without duplicating
  successful items. Persist each task and its provenance atomically.
- Enforce existing title/description limits; never silently truncate content.
  Invalid source URLs or malformed provider metadata cannot create unsafe links.
- Provider search/preview failure creates nothing. Distinguish provider-wide
  failure from individual invalid candidates; do not disguise either as an
  empty successful search.

## Authorization and safety

- Provider discovery/configuration metadata, preview, and confirmation are
  human-only; reject agent tokens. Reuse existing authentication and CSRF rules.
- Provider controls and responses contain only non-secret metadata. This card
  adds neither credential storage nor a reusable credential handoff mechanism.
  GitHub credential design belongs to KANSO-2.
- Treat external content as untrusted. Reuse safe DOM/Markdown rendering, allow
  only safe web source links, and do not render provider text as raw HTML.
- Never persist or return provider credentials in task content, activity,
  comments, SSE, exports, logs, or error responses.

## Acceptance checks

- Production starts with no providers and exposes an understandable empty UI.
- A test-only provider drives discovery, project/source/filter selection,
  preview, explicit selection, confirmation, and per-item outcomes.
- Test human authorization, agent rejection, unauthenticated access, and CSRF.
- Preview creates no cards; confirmation cannot forge candidates or import an
  unselected item. Test changed/expired preview and destination/source mismatch.
- Test invalid/archived projects, empty selections, field limits, unsafe URLs,
  malicious Markdown/provider text, and provider-wide failures.
- Test duplicate selection, repeat import, concurrent confirmation, preserved
  local edits, archived duplicates, and imports into different projects.
- Test mixed valid/invalid batches, retry behavior, atomic provenance writes,
  ordinary task activity/events, source-link display, and export provenance.
- Add an append-only migration and migration coverage, document the provider
  extension contract for KANSO-2, and update user/testing documentation.
- Run formatting, linting, regression tests, and focused browser checks. No
  live cloud credential is required to review KANSO-1.
