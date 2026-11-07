# KANSO-11 — One-time secret handoff

## Status and scope

Design confirmed by the human through grilling Q1–Q15 on KANSO-11.
Implementation is explicitly requested in a separate OpenChamber session in
the same worktree. The coordinating session verifies the result and opens a PR.

This is a one-time task/session-bound handoff, not a credential vault. Kanso
owns the human input UI and the agent CLI. Ordinary blocker replies remain
unchanged and must never carry secret values.

## Trust and confidentiality

- Never persist submitted secrets in SQLite, files, backups, saved prompts,
  comments, activity, SSE, exports, logs, tracing or debug/error output.
- Hold submitted values only transiently in server memory. This is an
  application-level no-persistence guarantee, not protection against swap,
  dumps, compromised processes or privileged users. Erasure is best effort.
- Send only non-authorizing request IDs and non-secret metadata into agent
  conversations. Never send the value or retrieval capability to the model.
- Authenticate retrieval with the original agent token and a separate random
  capability retained only by the requesting CLI. Persist only its verifier,
  never the raw capability. Task/session binding adds consistency checks; a
  session ID is public metadata, not authentication.
- Sessions sharing an agent token and OS user are one trust domain. Do not
  claim isolation against a process able to inspect that user's files or an
  authorized child process. A child can leak its environment or output.
- Use the existing authenticated transport and CSRF protections. Request and
  response bodies containing values/capabilities must not be logged; sanitize
  upstream errors rather than retaining arbitrary response bodies.

## Request lifecycle

- Initial creation requires an active card in `in_progress`, a linked session,
  an enabled integration and an agent token. Bind the original token identity,
  card and session. Atomically create the request and move the card to blocker.
- Allow only one live request per card. A request holds one value. Request
  multiple credentials sequentially.
- The same requesting token can re-request while still in blocker only after
  its previous request is terminal, the linked session is unchanged and no
  newer ordinary blocker question exists. Never overwrite a live request or
  an unrelated question. Replacements have a new ID and capability; old
  capabilities remain unusable. A changed session must resume normally first.
- States must distinguish waiting, ready, consumed, declined, cancelled,
  expired and invalidated. Terminal transitions cannot restore a value.
- An unanswered request expires 24 hours after creation. A submitted value
  expires 15 minutes after submission or at the request deadline, whichever
  comes first. Notification retries do not extend deadlines.
- Server restart invalidates every outstanding handoff and verifier. Persisted
  metadata must clearly show it is unavailable and a new request is needed;
  never restore submitted values or pretend they remain ready.
- Session change, leaving blocker, archiving or revoking the original token
  invalidates the handoff. Deadline and authorization checks apply to every
  retrieval even if background cleanup has not run.
- Expiry removes values at their deadline. Cap global live requests at 100
  and total in-memory submitted bytes at 1 MiB. At capacity return a clear
  retry-later error without consuming a request or overwriting accepted data.
- Persist non-secret metadata/history, including safe audit outcomes. Do not
  expose secret length/hash fingerprints. Request metadata can appear in
  comments, exports and SSE; secrets and capabilities cannot.

## CLI and API contract

Exact endpoint/flag names should follow existing conventions; the required
operations and semantics are:

1. **Request:** agent CLI supplies card reference, label (at most 120
   characters) and purpose (at most 2,000 characters). Return/display only the
   public request ID. Save the private capability outside the repo in a
   per-user private state/config directory (0700), in a 0600 file. Provision the
   ID/capability locally before creation, so an uncertain server response still
   leaves a known ID and usable private capability for status/cancellation. Never print
   it, put it in command arguments, or put it in wake-up messages. Guard
   against symlinks and accidental overwrites. Fail safely if local storage
   cannot be established; do not leave a misleading usable request.
2. **Inspect:** authenticated users can read non-secret status, deadlines,
   label/purpose and safe delivery outcomes. No endpoint reveals submitted
   values to the UI or returns capabilities through task detail/history.
3. **Submit:** human-only, authenticated and CSRF-protected. Accept exactly one
   non-empty UTF-8 value of at most 16 KiB; preserve whitespace and newlines
   exactly. Reject NUL, because environments cannot represent it. Reject
   duplicate submission; no edit/replace of an accepted value.
4. **Decline:** human can decline with an optional non-secret explanation.
5. **Cancel:** human or original requesting agent can cancel waiting/ready
   requests; discard the value and invalidate retrieval.
6. **Consume/execute:** shape `kanso secret exec <request> --env API_TOKEN --
   <command>`. Read the capability privately from local state, authenticate
   with the original token and validate card/session/state/deadlines. Consume
   the value atomically before launching the child; exactly one concurrent
   retrieval can succeed. Never automatically retry retrieval. A lost HTTP
   response, CLI crash, launch failure or failed child requires a new request.
   Inject into the named environment variable, directly execute without an
   implicit shell, inherit normal streams and preserve child exit status.
   Validate the environment name. No raw-value output, plaintext secret file
   or credential-file mode. An explicit shell chosen by the caller is allowed.
7. **Capability cleanup:** delete after consumption/cancellation or known
   expiry. If the CLI never runs again, an unusable capability file may remain
   until a later invocation cleans it up. Never store the submitted value in
   this file.

This is single-process Kanso. No multi-instance coordination, vault, binary
uploads, structured bundles, format validators or reusable credentials in v1.

## Human UI and notification behavior

- Dedicated section in the blocker dialog shows label, purpose, state,
  deadlines and clear security guidance. Public label/purpose must themselves
  contain no credentials.
- Provide a masked value input with explicit reveal, supporting multiline
  text. Do not save the value as a draft or in browser storage. Clear it on
  successful submission/cancellation. Never echo it in a response or status.
- Separate Submit, Decline and Cancel from the ordinary comment/reply box;
  warn never to put credentials in that box. Ordinary questions still work.
- Submission and decline notify the originally linked session through a
  reference-only message. Record/show sent, failed or unknown delivery.
  Retry only that metadata notification while the handoff remains valid;
  never replay a value or extend expiry. Preserve duplicate-safe identity.
- The card stays in blocker after submission/decline/notification. The agent
  explicitly resumes work. Do not poll for a human response.
- Expiry/cancellation is visible in request status; no automatic wake-up for
  these outcomes in v1. Restart status must explain that re-entry is needed.

## Acceptance checklist

- Test ordinary blocker/reply behavior remains unchanged.
- Test create eligibility, binding, atomic blocker transition, one-live-request
  conflict and the narrowly scoped terminal-request replacement rule.
- Test human-only submission/decline, CSRF, exact whitespace/newline handling,
  empty/NUL/size validation, duplicate rejection and no secret draft storage.
- Test original token plus capability authorization; wrong token/capability,
  revoked token, changed session, leaving blocker and archive invalidate use.
- Test expiry and restart invalidation, timely value cleanup, request/byte
  caps, and error paths that preserve previously accepted state.
- Test concurrent at-most-once consume, lost-response behavior, launch failure
  and command failure without retrieval replay or implicit shell execution.
- Test private directory/file permissions, symlink/overwrite rejection,
  capability cleanup, environment injection and child exit-status propagation.
- Test metadata-only session notifications, safe failed/unknown statuses,
  duplicate-safe retries, deadlines unchanged and no automatic column move.
- Demonstrate submitted values and raw capabilities are absent from SQLite
  (including retry prompts), comments, activity, task detail, SSE, exports,
  backups, logs/errors and model-visible messages. The authorized consume
  exchange is the only server response containing the value; private initial
  CLI provisioning is the only server response containing the raw capability;
  the private consume request also supplies it for verification.
- Document transport, server-memory, same-user/shared-token and child-output
  limitations accurately. Run formatting, linting and the full regression
  suite, plus focused UI/CLI security tests.
