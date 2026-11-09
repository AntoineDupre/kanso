# KANSO-1 — GitLab repositories and manual imports

The current KANSO-1 card adds GitLab alongside the existing GitHub importer.
It supersedes this file's historical specification for the original generic
importer. The card remains the acceptance authority; this document records the
implementation and review boundary.

## Delivered behavior

Projects can link multiple GitLab repositories across GitLab.com and
self-managed instances while retaining GitHub configuration. Each link stores
an HTTPS origin, a project path with nested groups, and three independent
category flags. Human-only GET/PUT configuration uses existing authentication
and CSRF checks. Additive migration v7 creates `gitlab_repositories`; previous
GitHub configuration and imported project data remain intact.

The GitLab adapter uses the server's `KANSO_GITLAB_TOKENS_FILE`, a bounded JSON
map of instance origins to classic `read_api` personal access tokens. The UI,
database, cards, activity and exports never store the credentials. Errors are
sanitized, redirects are refused, and reflected credential content in remote
JSON is rejected, including escaped representations. No `glab` installation or
browser login is involved. [Setup](IMPORTS.md#gitlab) and
[Podman secret mounts](DEPLOY.md#gitlab-authentication) are documented.

Issues use REST API v4, open state and the selected assignee (defaulting to the
authenticated account); explicitly including all assignees removes that filter.
Followed merge requests use the selected author and open state, including
drafts. Review requests use GraphQL's selected reviewer plus that reviewer's
`mergeRequestInteraction.reviewState`: only `UNREVIEWED` and `REVIEW_STARTED`
qualify. Completed/approved/changes-requested/unapproved reviews do not qualify
until re-requested. Drafts and the authenticated account's own MRs are excluded
from review candidates. An assignee filter never substitutes for review state.

Provider/source/category selection, preview, explicit item selection,
confirmation and per-item outcomes reuse the existing shared import workflow.
Snapshots are server-owned; preview creates nothing. Identity consists of the
canonical instance, immutable remote project ID, entity type and item IID.
Repository rename and switching MR categories therefore preserve identity.
Repeat imports skip existing cards, including archived cards, and preserve
local edits. Provenance and safe source links remain visible and exported.

## Acceptance and verification map

| Criterion | Verification |
| --- | --- |
| Repository configuration | `tests/gitlab.rs`: CRUD, multiple repositories/instances, nested paths, independent flags, canonicalization and GitHub coexistence; Chromium settings workflow |
| Authentication | Adapter fixture token-file bounds and HTTP401/403/404; redirect/credential-reflection tests; documented host/Podman setup |
| Issues | Adapter fixtures: default/explicit/all assignees, exact REST query and local filtering |
| MRs to review | GraphQL fixtures: selected reviewer, actual pending state, re-request, own-author/draft exclusion, explicit other reviewer |
| MRs to follow | REST fixtures: selected author, open state, drafts included, closed/merged excluded |
| Import workflow | Shared and GitLab API tests: preview creates nothing, snapshot/selection validation, per-item outcomes; Chromium explicit selection and invalidation |
| Identity/provenance | API fixtures: concurrent duplicates, category/rename stability, instance/project separation, archived cards, local edits, source details and exports |
| Authorization/failures | Human/agent/anonymous/CSRF tests; malformed/oversized/partial/failed queries; removed-source confirmation; bounded transport and secret reflection |
| Compatibility | Additive migration tests retain GitHub data; existing Rust and GitHub UI regressions |

The independent reviewer re-ran 13 adapter tests, six GitLab API integration
tests and the generic/GitHub/GitLab Node workflow, and found no blocking code
defect. A mobile layout issue found during supervisor screenshot inspection was
fixed in `4253d99` and rechecked in Chromium, with an explicit minimum field
width assertion at a 360px viewport. Final execution results
and commit references are recorded in the KANSO-1 handoff comment.

Final supervisor checks (2026-11-09):

| Check | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo clippy --locked --all-targets -- -D warnings` | Passed |
| `cargo test --locked` | 329 passed, zero failed/ignored; authorized local sockets for isolated HTTP fixtures |
| `node tests/imports_ui.mjs` | Passed, generic/GitHub/GitLab workflows |
| `cargo build --locked --bin kanso` + `node scripts/test-imports-ui.mjs` | Passed in Chromium, actual local configuration CRUD plus intercepted external previews; desktop/mobile and safe rendering |
| `docker build -f Containerfile -t kanso:kanso-1-review .` | Final image built after mobile fix (`6191b3f590a3`) |
| Disposable rootless Podman 4.9.3 smoke | Passed: uid 1000, readable 0600 read-only token mount, health, GitLab configuration, sanitized negative preview, zero created cards, no synthetic token in errors/export/logs |
| `./scripts/sbom.sh /tmp/kanso-1-sbom` | Passed using cargo-cyclonedx 0.5.10, backend musl and frontend CycloneDX 1.5 documents |
| Independent review | Approved all nine criteria, then rechecked final mobile fix and documentation; no blocking findings |

The Podman smoke used the same backend as the final image, with a synthetic
invalid token only. It verifies deployment plumbing and safe failure, not
successful authentication to GitLab. Test artifacts/logs and generated SBOMs
are under `/tmp/kanso-1-*`; UI evidence is retained in
[`docs/screenshots/kanso-1`](screenshots/kanso-1/).

Implementation commits: `384bfd2` (configuration, API, migration, UI and
integration tests), `b3229bf` (provider and transport/filter fixtures),
`4253d99` (mobile layout and browser width assertion). Commits are local and
unsigned: the configured SSH signing key was locked and no usable askpass was
available. No repository signing configuration was changed. No PR was opened
or service deployed by this manual session.

## Supported GitLab versions and practical limits

The supported target is **GitLab 17.11+**, including GitLab.com, with REST API v4
and GraphQL review-state support. The baseline is based on the official
[17.11 interaction type](https://gitlab.com/gitlab-org/gitlab/-/blob/v17.11.0-ee/app/graphql/types/user_merge_request_interaction_type.rb)
and the current [GraphQL reference](https://docs.gitlab.com/api/graphql/reference/),
[REST MR API](https://docs.gitlab.com/api/merge_requests/) and
[review-state documentation](https://docs.gitlab.com/user/project/merge_requests/reviews/).
Fixtures validate that contract. No valid credential or live instance of each
GitLab release was used, so this is not a release-by-release interoperability
certification. Unsupported/missing review-state fields fail explicitly.

- HTTPS origins with optional ports; no HTTP, URL path prefixes, userinfo,
  query strings or fragments. Standard webPKI trust roots; private CA trust
  configuration is outside this adapter.
- ASCII project paths and usernames. Maximum 50 repository links per project.
  Instance aliases are distinct identities; normalize the origin consistently.
- At most 30 seconds per preview, three pages of 100 records, 4 MiB per response,
  200 candidates and 100 reviewers per MR. An incomplete nested reviewer list,
  unsupported GraphQL response or exceeded bound fails instead of returning
  partial success. The credential map is at most 1 MiB and 100 instances.
- Classic personal access tokens with `read_api`; fine-grained tokens and
  browser authentication are outside this implementation.
- Manual read-only imports only. No remote writes, cloning, background sync,
  automatic monitoring or card completion.

Valid remote token interoperability, private enterprise networking and a live
production rollout remain unverified. All local API/browser/transport checks
use isolated data and synthetic fixtures; the existing service is untouched.
The human owns review and completion; agents never move this card to `done`.
