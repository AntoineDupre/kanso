# External task imports

Open **Imports** from the board, choose an active destination project, provider,
source and any provider filters, then preview. Nothing is created until you
explicitly select candidates and choose **Import selected**. The results show
created, skipped and failed items; retrying skips existing imports, even when
the local card is archived. Local edits are never overwritten. Imports create
ordinary active backlog cards with normal priority; the task detail links to
the original. Provenance is included in JSON exports and database backups.

No external provider is enabled yet. GitHub support and configuration are
planned separately in KANSO-2. No credentials are needed for these tests.

## Provider extension contract

Implement `importer::Provider` and register it in the server's `AppState` via
`with_import_providers`. The production registry is currently empty. `info()`
describes public, non-secret provider metadata, source choices and text filter
controls; `preview()` validates filter semantics and performs remote I/O on a
blocking worker, without a database lock. The adapter owns authentication,
pagination, source configuration and any category-specific semantics. It must
not put credentials in controls, results, URLs, task content, logs or errors.
`info()` must be fast and nonblocking. The shared layer does not time out or
cancel `preview()`; each adapter must set connection, read and overall request
deadlines and bound pagination and response sizes itself. The shared preview
limits are enforced after the adapter returns, not while it fetches data.
All returned errors are sanitized. A source must have a stable ID; a provider
may expose multiple sources. Each candidate's `identity` must identify the
logical item across those sources, including any relevant category distinctions;
do not use mutable titles or URLs as identity. The provider ID is part of the
deduplication key, scoped to the destination project.

Previews are held only in memory, bound to the authenticated session or token,
project, provider and source, expire after ten minutes, and are limited to 200
candidates and 2 MiB. Starting a newer, well-formed preview request invalidates
the caller's prior one, even if validation or the provider call fails.
Confirmation accepts only distinct indexes into that server snapshot, not
candidate content. A restart or expired snapshot requires a new preview.
Candidate identities and kinds must be nonempty, bounded and free of control
characters; source URLs must be safe HTTP(S) URLs without userinfo. Existing
title and description limits apply. Invalid items are reported individually,
without truncation. Each successful item and its provenance is committed
atomically before its normal creation event is published.

The API endpoints are `GET /api/imports/providers`, `POST /api/imports/preview`,
and `POST /api/imports/confirm`; all require human authorization and use normal
cookie CSRF protection. Providers are trusted server-side code, not plugins or
client-supplied candidates. `tests/api.rs` contains a test-only adapter and
exercises the end-to-end flow without cloud access.

## API workflow

1. `GET /api/imports/providers` returns provider IDs/names, available source
   IDs/names, and filter IDs/labels. An empty array means no provider is
   registered. Filter controls are plain text; adapters validate their semantics.
2. `POST /api/imports/preview` accepts the following shape (IDs below are
   illustrative, not a bundled provider):

   ```json
   {
     "project_id": "existing-project-uuid",
     "provider": "provider-id",
     "source": "source-id",
     "filters": {"query": "assigned to me"}
   }
   ```

   It returns `preview_id` and a `candidates` array. Each item has an `index`,
   normalized `candidate`, `valid` flag, and an `existing` local task reference
   or `null`. The normalized candidate contains `identity`, `kind`, `title`,
   `description`, and `source_url`. Previewing never creates cards.
3. `POST /api/imports/confirm` accepts only snapshot context and selected indexes:

   ```json
   {
     "preview_id": "server-returned-preview-id",
     "project_id": "existing-project-uuid",
     "provider": "provider-id",
     "source": "source-id",
     "selected": [0, 2]
   }
   ```

   It returns an array of `{index, status, reference, message}` outcomes.
   `status` is `created`, `skipped`, or `failed`; `reference` is a local task
   reference when available, and `message` is a safe error or `null`. Selection
   must be nonempty and indexes distinct. Task content cannot be supplied or
   edited by the client at confirmation. Retry the same snapshot/selection after
   an uncertain result; successful prior imports are skipped. If the snapshot
   expired or was invalidated, obtain a new preview and select again.

All calls must use the same authenticated human session or token. The cache
holds at most 50 previews globally, evicting the oldest at capacity; each human
identity has at most one current preview. Missing/expired/context-mismatched
snapshots return `409`; invalid controls/selections return `400`; sanitized
provider failures or oversized previews return `503`. Provider-wide failure
must not be treated as a successful empty result. Invalid individual items
remain visible in preview and fail individually if selected through the API.
