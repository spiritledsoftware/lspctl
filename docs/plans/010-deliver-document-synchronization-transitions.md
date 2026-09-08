# Plan 010: Deliver every committed Document synchronization transition

> **Executor instructions:** Read this entire plan before editing. Run each step's verification and retain its result. Implement only the scoped change, not the rest of the audit. Leave changes uncommitted for review. Update only this plan's status and its row in `docs/plans/README.md` after verification.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs src/workspace.rs src/session.rs src/session/owner_protocol.rs tests/owner_lifecycle.rs test_support/fake_lsp_server.rs`
> Also run `git status --short`. Plan 011 intentionally changes transport handling in this scope: read its landed diff and map the excerpts below to the updated functions. Unexplained behavioral drift or somebody else's uncommitted edits to these functions is a STOP condition; never revert prerequisite fixes to match an old excerpt.

## Status

- **Status:** DONE — three regressions, 139 tests, and all acceptance gates pass
- **Finding:** Audit #10
- **Priority:** P1
- **Effort:** M
- **Risk:** MED — synchronization ordering and failure delivery semantics
- **Depends on:** `docs/plans/011-bound-language-server-writes.md`; its transport failure handling must prevent reuse after incomplete frame delivery
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

A transient CLI snapshots a Document before dispatch. If the file changes before the Owner handles that request, the Owner currently updates its DocumentStore, discovers the expected digest mismatch, and returns without sending the resulting close/open events. A later retry sees the updated digest as unchanged and never sends those lost events. The language server can then answer Queries against stale text, despite the Owner believing it is synchronized.

The invariant is simple: a DocumentStore transition may not survive into another Query unless all of its synchronization events were delivered. Deliver successful refresh events before rejecting the stale Query. If delivery fails, use the fatal transport handling from plan 011, not a retry on an uncertain stream.

## Current state

Read these implementation and test seams:

- `src/workspace.rs:132–198`: `DocumentStore::refresh` mutates the cache and returns `RefreshOutcome { snapshot, events }`.
- `src/workspace.rs:249–272`: failed reads may remove a Document and queue `DidClose` in `pending_events`.
- `src/session/owner_runtime.rs:1572–1617`: explicit synchronization and best-effort refresh.
- `src/session/owner_runtime.rs:1685–1768`: post-response validation and event delivery.
- `src/session/owner_runtime.rs:366–389`: `OwnerRequest::Diagnostics` also calls `synchronize_documents`.
- `src/session.rs:1066–1105`, `src/session/owner_protocol.rs:30–72,145–160`: private endpoint discovery and framed protocol, for an isolated regression fixture only.
- `tests/owner_lifecycle.rs:19–129,234–274`: native-platform temporary configuration, CLI invocation, and synchronization regression pattern.
- `test_support/fake_lsp_server.rs:91–119,514–531,615–667`: fake server's open-Document tracking and callback readers.

The ordering bug in `src/session/owner_runtime.rs:1577–1598` is:

```rust
let outcome = self.documents.refresh(
    &document.path,
    &document.language_id,
    self.negotiated.text_synchronization,
)?;
if outcome.snapshot.digest != document.expected_digest {
    return Err(ContractFailure {
        // document_changed_while_reading; synchronize; not_sent; safe
```

Only after that return branch does the existing function execute:

```rust
self.send_synchronization_events(outcome.events).await?;
```

`DocumentStore::refresh`, `src/workspace.rs:190–198`, has already committed:

```rust
self.documents.insert(
    uri.clone(),
    OpenDocument {
        snapshot: snapshot.clone(),
        last_used: self.use_clock,
    },
);
```

Use existing `Result<_, ContractFailure>` error handling. Keep `document_changed_while_reading` with `delivery: "not_sent"` and `retry: "safe"` when only synchronization notifications were sent: the requested semantic Query was not sent. A synchronization transport failure instead retains `owner_unavailable`, uncertain delivery, and unsafe retry; do not mask it with a safe digest error.

Test conventions from `tests/owner_lifecycle.rs:259–274`:

```rust
assert_eq!(raw["result"], json!({"fixture": true}));
assert_eq!(
    raw["context"]["synchronization"]["postResponseChanged"][0]["uri"],
    url::Url::from_file_path(dunce::canonicalize(&synchronized).unwrap())
        .unwrap()
        .to_string()
);

fixture.stop(workspace);
```

Use `Fixture` and `TempDir`; do not touch real user configuration. Ensure the test's Owner is stopped even when an assertion fails, using a test-local cleanup guard if the fixture still lacks one. Do not add a production testing command.

### Domain constraints

`CONTEXT.md` calls the persistent process an **Owner**, its file snapshots **Documents**, and semantic requests **Queries**. ADR 0003 states: “hash them before use, and replace changed snapshots with `didClose` followed by `didOpen`”; it explicitly excludes a filesystem watcher. ADR 0001 says operations run serially. Preserve those decisions: no watcher, incremental-edit implementation, second cache, or parallel dispatch is needed.

## Commands you will need

Run from the repository root. This is one Rust 2024 Cargo package, MSRV 1.89; no dependency installation is required in a provisioned checkout.

| Purpose | Command | Expected |
| --- | --- | --- |
| Discover regression tests | `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry -- --list` | The three names specified below, not zero tests |
| Targeted tests | `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry` | All three pass after the fix |
| Complete tests/build | `cargo test --locked --all-targets --features fake-server` | Exit 0; all tests pass |
| Type/lint gate | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0, no warnings |
| Format check | `cargo fmt --all -- --check` | Exit 0 |
| Contract check | `python scripts/release/check_schema.py` | Exit 0 |
| Stored-state check | `python scripts/release/check_stored_state.py` | Exit 0 |
| Whitespace | `git diff --check` | Exit 0 |

At the audit baseline, 84 Rust tests and all listed checks passed. Counts may grow as prerequisite plans land; do not freeze that count into a test.

## Scope

**Implementation files allowed:**

- `src/session/owner_runtime.rs`: event delivery ordering and pending-event drainage at existing refresh call sites.
- `tests/owner_lifecycle.rs`: isolated Owner regression tests and the minimum private-protocol fixture helper.
- `test_support/fake_lsp_server.rs`: observe the text actually delivered in `didOpen`, not text read from disk.

**Metadata exception:** this plan's status/evidence and its row in `docs/plans/README.md`.

**Read-only context, not modification scope:** `src/workspace.rs`, `src/session.rs`, `src/session/owner_protocol.rs`, ADRs, and the contract assets. The DocumentStore already returns the needed events; do not add a new prepare/commit abstraction. Transport policy belongs to landed plan 011. No timeout configuration, schemas, lockfile, raw-query API, or unrelated diagnostic-cache change.

## Git workflow

Work on the operator-provided branch or disposable worktree. If authorized to create a branch, use `advisor/010-document-synchronization`. Record the starting HEAD and working-tree status. Do not commit, push, or open a PR without separate permission. If later asked to commit, match the repository's imperative subjects, e.g. `Preserve Document synchronization after a stale request`.

## Steps

### Step 1: Confirm the prerequisite and baseline

Read the landed transport handling from plan 011. Verify a failed notification write marks the transport unusable and prevents another Query from dispatching. Trace all `self.documents.refresh`, `refresh_open_documents`, `drain_pending_events`, and `send_synchronization_events` calls in `owner_runtime.rs`, including explicit Diagnostics, pre-dispatch, post-response, best-effort refresh, post-commit refresh, and shutdown.

**Verify:** `cargo test --locked --all-targets --features fake-server` → exit 0. `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs` → any changed handling is explained by reviewed prerequisites or other indexed plans. If transport reuse remains possible after a partial write, STOP rather than adding a second fatal-state implementation here.

### Step 2: Reproduce stale synchronization without a timing race

Add three integration tests, using only the fixture's temporary Workspace and Owner:

1. `synchronization_retry_after_digest_mismatch_uses_current_text`: cover both an initially unopened file and a previously opened file. Initialize the Owner using existing CLI calls. Obtain its endpoint only from the temporary fixture tree's `owners/endpoints` directory, matching the returned Session identity and Owner generation. A small test-only helper may exchange a length-prefixed JSON Owner request using `std::net::TcpStream` with read/write timeouts. Read the authentication data from that generated endpoint into memory; never print it or inspect real user state. Follow `AuthenticatedOwnerRequest` and `OwnerRequest::Diagnostics` exactly—do not invent a public command. Send a Document with a deliberately outdated expected digest while the actual file has new content. Assert the structured mismatch, then retry through normal CLI `raw --sync-file` synchronization with current bytes. Assert the fake server reports those new bytes, with the same Owner generation. Repeat with a private `OwnerRequest::Dispatch` carrying the stale expected digest and a distinct fixture semantic method: assert its method never appears in the fake server's flushed event log before a later barrier Query completes. `Diagnostics` alone cannot prove that a stale semantic Query was withheld. Follow the full Dispatch definition, including its existing timeout and raw/synchronization flags, and do not modify the wire format.
2. `synchronization_retry_after_failed_read_closes_old_document`: open a file, delete it, then send a synchronization request to the same Owner. Assert the read failure. Query the fake server's open-Document set without synchronizing any file and assert the missing file was closed. Recreate and synchronize it, then assert the new text was delivered.
3. `synchronization_retry_after_postresponse_read_failure_closes_document`: start a Query against an open file and pause its fake-server response at a fixture-owned marker/release gate. Once the request is observed, delete the file and release the response. Assert the post-response read failure, then use `test/open-documents` without synchronization to prove the pending close was delivered. Recreate/synchronize and verify current server text. Extend the existing `test/await-file-change` method with an optional bounded release-marker mode rather than relying on its fixed sleep; leave its current callers unchanged.

For text evidence, extend the existing fake server open-Document store to retain `didOpen` text as well as URI membership. Update `update_open_documents` and its callback-reader callers consistently. Preserve the existing `test/open-documents` response (`count`, `uris` array), and add only a fixture method such as `test/document-text` returning the remembered text for a URI. Never implement this method by reading the filesystem: that would hide the bug. Reuse any equivalent test helper introduced by earlier plans.

**Verify:** the discovery command lists exactly the three `synchronization_retry` tests. `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry` → compilation succeeds and at least the digest-mismatch case fails on stale/missing server text before changing production ordering. A handshake failure, timeout, or fixture crash is not the intended red result.

### Step 3: Deliver transitions before reporting a stale request

In `synchronize_documents`, retain the existing refresh outcome, deliver its events immediately, then compare its snapshot digest with the expected digest and return the existing mismatch error. Sending current Document state is not permission to send the stale semantic Query: `start_dispatch` must still return before constructing/sending that Query.

Replace the refresh `?` with a bounded error branch where needed so pending close events from failed reads are drained and sent before returning the original read error. If sending fails, return the synchronization transport error instead. Apply the same invariant to `validate_documents_after_query`: successful refresh events are handled regardless of whether the digest changed, and pending close events are not lost on failure. Preserve named-query post-response errors and raw-query `postResponseChanged` metadata.

Check best-effort and post-commit callers too. They already collect multiple mutated outcomes; after a send failure the Owner must use plan 011's fatal path, never keep only a partially synchronized cache alive. Do not retry a partially written notification batch on the same transport.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry` → all three tests pass, including first-open, replace-open, stale Dispatch suppression, and post-response failed-read subcases. `cargo test --locked --features fake-server --test owner_lifecycle owner_serializes_simultaneous_agent_operations_in_fifo_order` → passes, preserving post-response raw staleness behavior.

### Step 4: Run all gates and record the evidence

Run every command in the commands table. Inspect `git diff --name-only` and `git ls-files --others --exclude-standard`: every change must be in the implementation scope or the metadata exception, relative to the recorded starting state. Record the red failure and green command results in this plan, then mark this plan and its index row DONE only after all gates pass.

**Verify:** all commands exit 0; discovery still lists all three tests; `git diff --check` has no output; no production command/schema or out-of-scope file changed.

## Test plan

Use `Fixture` in `tests/owner_lifecycle.rs`, not a mocked DocumentStore alone. Assert remembered server text, same Owner generation, exact stale error metadata, missing-file closure, and successful retry. The existing `graceful_stop_closes_open_documents_before_shutdown` test must remain green. All socket reads and fixture waits need finite bounds, and cleanup must target only the fixture's processes. Keep existing no-synchronization behavior for servers without `openClose` support; do not synthesize events when negotiation says `None`.

## Done criteria

- [x] All three explicitly named tests are discovered and pass; their assertions cover all subcases above.
- [x] Complete test/build, Clippy, format, schema, stored-state, and whitespace commands exit 0.
- [x] A digest mismatch still prevents the semantic Query from being sent.
- [x] Regression evidence proves the next Query sees delivered new text rather than merely updated cache state.
- [x] Existing FIFO, raw staleness, and graceful close-before-shutdown tests pass.
- [x] No changes outside implementation scope and metadata exception; no leftover fixture Owner.
- [x] Status/evidence recorded here and in `docs/plans/README.md`.

## Completion evidence

Issue #47 began on `advisor/010-deliver-document-synchronization-transitions` at clean `814cb18` (merged #58). Issue #46 was confirmed closed. The required drift review from `5268c6a` found no changes to `workspace.rs` or `owner_protocol.rs`; plans 004/005/007/008/009/011 explain the runtime, routing, and fixture changes. The landed 011 transport diff was read, preserving its absolute deadlines, cancellation-safe writer poisoning, maintenance priority, and fatal generation cleanup. The endpoint layout and authenticated request definitions still match the specified private fixture seam. Baseline full tests passed **136** (`/tmp/47-baseline.log`).

### Regression evidence

- Discovery lists exactly the three specified `synchronization_retry` tests. Initial runnable RED (`/tmp/47-synchronization-red.log`) reached the intended assertions: a retry observed `null` instead of `"new text\\n"`, and both explicit and post-response failed reads left fake-server open-Document count **1** instead of **0**. Authentication, digest mismatch, and read-failure assertions passed first. One earlier fixture argument typo (`--params` rather than `--params-json`) was corrected before this intended RED; it was not counted as regression evidence.
- The digest test covers all four Diagnostics/Dispatch × unopened/reopened combinations, remembered `didOpen` text, same Owner generation, exact safe mismatch metadata, and stale semantic-method suppression before a flushed barrier Query. The two failed-read tests verify close delivery without another synchronization, then recreate/synchronize and observe current text on the same generation. Post-response deletion uses an optional bounded marker/release mode; existing fixed-delay callers are unchanged.
- The audit also exposed reachable local frame-size rejection: Document limits are independent of frame limits. Rejecting `didOpen` before output leaves a reusable byte stream but an inconsistent committed DocumentStore. Within the existing digest test, first-open and reopen subcases use a 16 KiB frame limit and 32 KiB Document. RED (`/tmp/47-validation-red.log`) explicitly reported `rejected synchronization left its Owner reusable`. The final implementation retires that session through the existing fatal state/process cleanup, without changing low-level pre-write validation semantics. The subcases assert uncertain/unsafe synchronization error precedence over the digest error, retirement, and delivered text on a fresh healthy generation.
- All three regressions pass, including **five** consecutive focused repetitions (**15/15** tests). The original low-level `frame_write_validation_error_preserves_stream` test remains green. Independent root review found no blocking correctness or scope issue.

### Refresh/event caller audit

| Caller | Event/error handling verified |
| --- | --- |
| `synchronize_documents` (Diagnostics and pre-dispatch) | Send every successful refresh outcome before comparing its digest; failed refresh drains pending closes before returning its original error. Notification failure takes precedence. A stale semantic Query still returns before construction/send. |
| `validate_documents_after_query` | Send successful events regardless of digest comparison; drain failed-read closes before returning. Preserve named-query validation errors and raw-query `postResponseChanged` metadata. |
| `refresh_open_documents_best_effort` | Send collected outcomes and pending closes; a failed send retains fatal state and `start_dispatch` cannot continue. No retry of an uncertain batch. |
| Owner `RefreshDocuments` | Existing outcome and pending-event batches are delivered under the single standalone deadline; caught failures cannot preserve the generation. |
| `synchronize_documents_after_commit` | Evaluate pending-close delivery before combining success flags: a read failure must not short-circuit away already-drained closes. Existing committed Mutation/Receipt results remain intact. |
| `graceful_shutdown` | Existing pending closes plus `close_all` remain ordered before shutdown under one budget; an already-fatal session is terminated without further frames. |
| `send_synchronization_events` and shared writer | Any undelivered committed transition records the existing fatal state and terminates the server. The common frame writer rejects subsequent writes; end-loop retirement and response-flush guards cover both fatal cache divergence and the original non-reusable-writer condition. |

No DocumentStore abstraction, watcher, schema, configuration setting, dependency, or protocol command was added. All refresh sites were inspected; the production delta is confined to `owner_runtime.rs`.

### Final gates

- `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry -- --list`: exactly **3** names.
- Targeted regressions, the FIFO/raw-staleness test, and graceful close-before-shutdown test: pass.
- `cargo test --locked --all-targets --features fake-server`: **139** passed (96 unit + 4 CLI + 3 documentation + 33 lifecycle + 3 installation), including all landed 011/005 regressions; `/tmp/47-full-green.log`.
- Clippy `--all-targets --features fake-server -- -D warnings`, format check, schema check, stored-state check, and `git diff --check`: all exit **0**.
- Scope inspection lists only the three allowed implementation files and this plan/index metadata; no untracked files or observed leftover fixture Owner/server process. Verified locally on Linux Rust **1.98.1**; native Windows/macOS and MSRV 1.89 were not run locally.

The implementation was committed as `6237fbe` and opened as PR #59 after operator approval.

### Native CI follow-up

- PR #59's initial macOS/Windows stable and MSRV jobs all failed the same explicit failed-read regression: the fake server retained one open Document. Linux and the post-response deletion test passed. Evidence: Actions runs `34252804986` and `34252809434`.
- The private-protocol fixture sent its raw temporary path after deletion, unlike every production `OwnerDocumentInput` constructor, which sends the canonical `DocumentSnapshot.path`. macOS temporary-directory aliases and Windows path representations therefore produced a different URI from the stored Document; a failed read could not identify that Document for closure. No production caller uses the fixture's noncanonical input pattern.
- Added a Unix symlink alias to the existing failed-read test, reproducing the native assertion deterministically on Linux: `cargo test --locked --features fake-server --test owner_lifecycle synchronization_retry_after_failed_read_closes_old_document` failed with open count **1**, expected **0** (`/tmp/47-ci-alias-red.log`). The shared fixture input helper now canonicalizes its path, and the deletion test prepares that input before removing the file, matching the real CLI snapshot/dispatch sequence. Assertions and production code are unchanged.
- With the fixture corrected, all three focused regressions and all **139** tests pass (`/tmp/47-ci-full-green.log`); discovery still lists exactly three names. Clippy, formatting, schema, stored-state, and whitespace checks pass. Native CI rerun remains pending; this follow-up is uncommitted for review.

## STOP conditions

Stop and report if the private protocol or endpoint layout no longer matches the referenced definitions; the fixture cannot be isolated from real user state; plan 011 does not make partial delivery fatal; a public schema change appears necessary; or a verification fails twice after a reasonable correction. If a dependency already fixed the ordering, demonstrate the regression is green and retain the test-only delta rather than duplicating its implementation.

## Maintenance notes

Every future refresh caller must account for both returned events and pending events from failed reads. Review early `?` and digest-error returns whenever synchronization changes. Keep these tests at the Owner/server boundary: a unit test that only inspects `DocumentStore::get` cannot prove this invariant. Deferred: watchers, incremental edits, and general synchronization transaction abstractions.
