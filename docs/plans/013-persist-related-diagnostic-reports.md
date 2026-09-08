# Plan 013: Preserve related diagnostic reports across CLI invocations

> **Executor instructions:** This file is a complete handoff for audit finding #13. Read it fully, execute the numbered steps, and retain verification evidence. Do not implement other audit findings. Leave source changes uncommitted. Update this plan and its row in `docs/plans/README.md` only after verification.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs src/workspace/diagnostics.rs src/query.rs src/session.rs tests/owner_lifecycle.rs test_support/fake_lsp_server.rs`
> Record `git status --short` as well. Plan 004 changes partial-result representation intentionally: inspect that landed diff and adapt these references to its complete-chunk representation. Stop on unrelated behavioral drift or overlapping uncommitted edits, not merely shifted line numbers. Never restore pre-004 flattening.

## Status

- **Status:** DONE — seven regressions and all local acceptance gates pass
- **Finding:** Audit #13
- **Priority:** P2
- **Effort:** M
- **Risk:** MED — bounded-cache accounting, effective/full versus raw response separation
- **Depends on:** `docs/plans/004-preserve-partial-result-chunks.md`
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

Each CLI invocation creates a fresh DiagnosticCache and imports the persistent Owner's cache. The CLI knows how to cache related Documents, but the Owner stores only the main report. Consequently, a first Query can succeed and a subsequent valid `unchanged` related report can fail with `invalid_server_result`. Related reports carried only in partial-result chunks are also missing from the Owner cache.

Persist each accepted main and related report in the existing bounded Owner cache. Cache effective full content with the latest result IDs; retain the exact server response separately for output evidence. No disk persistence or new cache implementation is needed.

## Current state

- `src/session.rs:162–195`: a new CLI DiagnosticCache imports `OwnerRequest::Diagnostics` state before composing each diagnostic Query.
- `src/session/owner_runtime.rs:951–968`: successful responses currently cache only the final result, before post-response Document validation.
- `src/session/owner_runtime.rs:1827–1842`: `record_pull_diagnostics` calls the single-document helper.
- `src/workspace/diagnostics.rs:64–126`: export/import transfers independent cached records.
- `src/workspace/diagnostics.rs:204–308`: `apply_pull_report` and related-aware `apply_document_pull_report` already implement most reconstruction behavior.
- `src/query.rs:774–793,1322–1417,1437–1529,1762–1825`: shared partial merger, validation, and cache preconditions.
- `tests/owner_lifecycle.rs:19–129`: native temporary configuration and reusable fake-server fixture.
- `test_support/fake_lsp_server.rs:153–179,230–246`: fixed diagnostic capability and full/empty response branches.

`src/session/owner_runtime.rs:1827–1835` currently contains:

```rust
fn record_pull_diagnostics(&mut self, method: &str, params: Option<&Value>, result: &Value) {
    match method {
        "textDocument/diagnostic" => {
            if let Some(uri) = params
                .and_then(|params| params.pointer("/textDocument/uri"))
                .and_then(Value::as_str)
            {
                self.diagnostics.apply_pull_report(uri, result.clone());
```

The CLI already uses the related-aware function, after checking every related URI in `src/query.rs:1773–1784`:

```rust
if let Some(related) = result.get("relatedDocuments").and_then(Value::as_object) {
    for (related_uri, related_report) in related {
        require_cached_unchanged_report(
            related_uri,
            related_report,
            diagnostics,
            &format!("$.relatedDocuments[{related_uri:?}]"),
        )?;
    }
}
let diagnostic = diagnostics.apply_document_pull_report(uri, result);
```

An additional round-trip detail matters: `apply_pull_report` updates `snapshot.result_id` for an unchanged report, but `export_state` exports `rawReport` rather than `result_id`. Import reconstructs the ID from that report. Store effective full content carrying the latest ID so a third invocation does not revert to an older result ID. Preserve bounded `serialized_bytes` accounting through the existing `store` path when cached payloads change.

### Conventions and exemplar

Use the existing `DiagnosticCache`, `DiagnosticResult`, `json!`, and `Result<_, ContractFailure>` patterns. `src/workspace/diagnostics.rs:512–547`, `document_pull_reconstructs_related_unchanged_reports`, is the unit-test exemplar:

```rust
let result = cache.apply_document_pull_report(
    "file:///main",
    json!({
        "kind":"unchanged",
        "resultId":"main-two",
        "relatedDocuments": {
            "file:///related": {"kind":"unchanged", "resultId":"related-two"}
        }
    }),
);
assert_eq!(
    result.effective_report["relatedDocuments"]["file:///related"]["items"],
    json!([{"message":"one"}])
);
```

That existing test uses one cache instance; new tests must cross both export/import and real CLI process boundaries. Keep `DiagnosticResult.raw_report` as evidence from the current server response, even if cached pull payloads become effective full reports.

### Domain constraints

`CONTEXT.md` defines **Owner** as “A long-lived process responsible for one initialized language-server session and its Queries” and **Document** as “A filesystem-backed text file whose current snapshot has been presented to a language server.” ADR 0001 mandates serial Queries and warm per-Session Owners. ADR 0003 says the filesystem is authoritative and open Documents are bounded. Keep diagnostic snapshot/byte limits, eviction, invalidation, and Session isolation; no global or on-disk diagnostics database.

## Commands you will need

Run from the repository root. Rust 2024, MSRV 1.89, one Cargo package. Use existing dependencies only.

| Purpose | Command | Expected |
| --- | --- | --- |
| List cache regression tests | `cargo test --locked --bin lspctl related_diagnostics -- --list` | The two unit tests below are present |
| Cache tests | `cargo test --locked --bin lspctl related_diagnostics` | Both pass after implementation |
| List integration regressions | `cargo test --locked --features fake-server --test owner_lifecycle related_diagnostics -- --list` | The five integration tests below are present |
| Integration regressions | `cargo test --locked --features fake-server --test owner_lifecycle related_diagnostics` | All five pass |
| Full tests/build | `cargo test --locked --all-targets --features fake-server` | Exit 0 |
| Type/lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0, no warnings |
| Format | `cargo fmt --all -- --check` | Exit 0 |
| Public contract | `python scripts/release/check_schema.py` | Exit 0 |
| Stored-state compatibility | `python scripts/release/check_stored_state.py` | Exit 0 |
| Patch check | `git diff --check` | Exit 0 |

Full gates passed at audit time (84 Rust tests then); do not require the count to stay constant. A filtered command running zero tests is a failure of this plan's verification.

## Scope

**The only implementation files allowed:**

- `src/session/owner_runtime.rs`: persist complete, validated successful diagnostic results and their related records.
- `src/workspace/diagnostics.rs`: preserve full effective reports/current IDs through bounded export/import; unit tests.
- `src/query.rs`: expose/reuse the existing pure partial merger and named-result validator at crate scope, or one small shared wrapper; do not create a second merger.
- `test_support/fake_lsp_server.rs`: opt-in deterministic related-diagnostic fixture responses.
- `tests/owner_lifecycle.rs`: process-boundary regression tests using `Fixture`.

**Metadata exception:** this plan's evidence/status and its index row in `docs/plans/README.md`.

**Out of scope:** `src/session.rs` is read-only call-flow context; no public schemas, CLI flags, persistent stored-state format, settings, Cargo dependencies, general diagnostic freshness redesign, or Document synchronization change. Plan 010 owns synchronization; plan 004 owns transport chunk preservation. Do not change raw Query results to normalized or reconstructed results.

## Git workflow

Use the assigned branch/worktree. If separately authorized to create a branch, use `advisor/013-related-diagnostics`. Save starting HEAD/status. Do not commit, push, or create a PR without permission. A later authorized commit should use an imperative subject such as `Preserve related diagnostic reports across CLI calls`, matching existing history.

## Steps

### Step 1: Verify the complete-chunk prerequisite

Read the Owner response path, the diagnostics import in `dispatch_owner_query`, and the shared Query merger/validator. Confirm plan 004 retains complete progress values through the internal Owner response and preserves the existing flattened failure envelope. Inspect existing pull-cache eviction and result-ID invalidation before modifying either.

**Verify:** `cargo test --locked --all-targets --features fake-server` → all tests pass, including plan 004's partial-result regressions. If the prerequisite still flattens array chunks internally, STOP; do not add a second interpretation of `partialResults` here.

### Step 2: Add process-boundary and cache-round-trip regressions

Add these named tests:

- `related_diagnostics_survive_cache_round_trip` in the inline diagnostics tests: store a full main/related pair; export/import into a new cache; apply unchanged main/related reports with new IDs; export/import again. Assert original diagnostic items, latest IDs for both URIs, and full effective reports. Assert the immediate result's raw report still says unchanged. This catches loss of IDs as well as loss of content.
- `related_diagnostics_respect_cache_limits` in that module: verify snapshot and byte caps with related reports, invalidation removes pull IDs, and an evicted related report is not resurrected by importing another record's embedded `relatedDocuments`. Include result-ID growth that crosses the byte limit: reconstruct the current response from the pre-update cache before storing any changed entry. It may be a complete response even if retention evicts an entry needed by a future request. Also cover full main + uncached unchanged related: reject that cache update without storing an unresolved main or partially changing prior records. Assertions can access private cache fields from the inline test module. Missing prerequisite content must never become fabricated empty diagnostics.
- `related_diagnostics_survive_cli_invocations` in `tests/owner_lifecycle.rs`: three separate CLI invocations against the same Owner. First returns full main/related reports, second returns unchanged reports with newer IDs, third verifies the new main previous-result ID was actually sent. Assert reconstructed related items, `diagnostics.rawReport` evidence, and unchanged Owner generation.
- `related_diagnostics_from_partial_chunks_survive_cli_invocations`: same sequence, but the related full report arrives only in a matching `$/progress` chunk and the final main full report has no related field. Subsequent unchanged reports must reconstruct successfully.
- `related_diagnostics_workspace_partials_persist`: a Workspace report supplies one full Document only in a partial chunk, then an independent CLI call receives it as unchanged. Assert retained items, latest ID, and the valid reconstructed Workspace envelope.
- `related_diagnostics_rejected_queries_preserve_cache`: seed a full main/related pair, then exercise malformed final data, full main + uncached unchanged related, partials followed by server error, and cancelled partial delivery. Make a later valid fixture response assert the previous IDs/content still match the accepted seed, not rejected data. Use deterministic bounded cancellation gates from plan 004's fixture rather than sleep races.
- `related_diagnostics_raw_queries_keep_exact_results`: issue a raw diagnostic method with explicit params and tracing; assert its final `result` equals the fixture's exact JSON rather than reconstructed full content. Where explicit progress is supplied, assert the existing raw trace/failure-evidence semantics, not a newly merged public result. Malformed diagnostic JSON values are still exact raw results and must not poison the named-query cache.

Extend the fake server with an opt-in flag such as `--related-diagnostics=final` or `--related-diagnostics=partial` parsed independently of the existing Scenario enum, preserving Standard capabilities. Derive the related URI from a real temporary sibling Document; advertise `interFileDependencies: true` for these fixtures. Return valid diagnostics (including range/message), stable URI identities, and predictable ID advancement. Honor the actual `partialResultToken`. In the third response, fail the fixture explicitly if the request supplies an old previous-result ID. Default fake-server behavior must not change.

Use `Fixture::with_server_arguments`; launch each Query with a separate `fixture.command` call rather than reusing the CLI's in-memory cache. Always stop only this test's Owner, including failure cleanup.

**Verify:** both discovery commands list their specified tests. Run both targeted commands: the CLI repeated-unchanged cases must fail before the production fix with invalid/missing cached related reports, not fixture initialization failures. The cache test may expose the independent latest-ID round-trip failure; characterization subcases already green should stay asserted.

### Step 3: Cache complete, valid diagnostic responses in the Owner

Change `record_pull_diagnostics` to receive the successful final result and the complete partial chunks of that Query. Map only `textDocument/diagnostic` and `workspace/diagnostic` to their existing `QueryCommand` variants. Reuse `merge_partial_results` and `normalize_named_result` on cloned values; widening these pure helpers to `pub(crate)` is sufficient if no shared wrapper already exists after plan 004. Never mutate the exact result or partial evidence returned through the Owner protocol.

For a valid merged Document report, call `apply_document_pull_report`, not just `apply_pull_report`. For a valid merged Workspace report, retain `apply_workspace_pull_report` so reports present only in workspace partial chunks are cached too. Shape validation alone is insufficient: before modifying the cache, require retained full content for every unchanged main, related, or Workspace report. An unresolved unchanged child rejects the complete cache update, including a full main that otherwise looks valid. Implement that prerequisite check/reconstruction in the existing cache methods as specified in step 4; do not duplicate it in the Owner. Do not cache malformed payloads or partials from failed/cancelled Queries. Place this work on the successful post-response validation path, not before a named Query has failed its Document validation. The normal Query layer remains responsible for exposing `invalid_server_result`; an optional cache update must not convert a raw Query into a named/normalized failure.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle related_diagnostics` → full and partial process-boundary paths pass once step 4's ID persistence is also present; until then, only the third-invocation latest-ID assertion may remain red. `cargo test --locked --bin lspctl named_cardinality_and_invalid_results_are_normalized` → passes. Retain the green partial-success/failure tests added by plan 004.

### Step 4: Preserve effective full pull state and current IDs under the existing bounds

Keep export/import record-oriented. In the existing Document and Workspace cache methods, first read the relevant pre-update snapshots and reconstruct the entire incoming report into local effective reports without mutating or evicting cache entries. If any unchanged report cannot be resolved, return the existing incomplete/null-effective outcome without changing cache state; the named Query layer retains its structured invalid-result behavior. Only after all prerequisites resolve should the method store accepted records under existing limits. This is local validate-then-store ordering, not a new cache transaction framework or clone of the entire cache. After accepting an unchanged pull report, the cached full report must carry its latest result ID while retaining the original items. After applying related reports, retain an effective full main report with resolved related content, and retain each related URI as its own bounded cache entry. The immediate `DiagnosticResult.raw_report` must still be the exact current response; do not replace the public raw evidence with synthesized full content.

Route changed cached payloads through existing byte accounting/eviction (`store`) rather than mutating a JSON value without updating `serialized_bytes`. Finish reconstruction before those stores so eviction cannot invalidate another unchanged report mid-response. Current-response completeness means all data was supplied or successfully reconstructed; it does not promise every record remains cached for the next invocation. If enlarged result IDs trigger eviction, retain the complete current result while honoring both limits for retained state. On import, restore only the independently exported records; do not recursively reinsert related records from a main report and accidentally revive an evicted entry or change eviction priority arbitrarily. No new export fields are needed if each exported pull `rawReport` now represents the full effective cached report with current IDs. Published push records keep their existing raw semantics.

Add malformed/missing-cache assertions to the named diagnostic tests if necessary: a valid unchanged report without retained full data remains an invalid-server-result/incomplete-cache condition, never invented empty diagnostics. Preserve the limits even when one related set is larger than cache capacity.

**Verify:** `cargo test --locked --bin lspctl related_diagnostics` and `cargo test --locked --features fake-server --test owner_lifecycle related_diagnostics` → all seven named tests pass (two unit, five integration). `cargo test --locked --bin lspctl workspace::diagnostics::tests` → existing freshness, unchanged, and oversized-cache tests all pass.

### Step 5: Verify the complete integration and update status

Run every table command, retaining test counts and exits. Compare changed and untracked files to the recorded starting state; allow only implementation scope and metadata exceptions. Record the initial red failures and final green evidence here. Mark DONE in this file and the index only when all required gates pass.

**Verify:** full test/build, Clippy, format, schema, stored-state, and diff checks all exit 0. Both test discovery commands list the specified names. `git diff --name-only` and `git ls-files --others --exclude-standard` show no unrelated changes introduced by this work.

## Test plan

The two cache tests protect storage semantics and bounds; the five integration tests protect the actual fake-server → Owner → fresh CLI path, including Workspace partials, rejected updates, and exact raw output. Both layers are necessary: the existing single-cache related-report test already passes while production loses records. Verify latest result IDs on a third invocation, final-only and partial-only related reports, actual synthetic diagnostic content, raw unchanged evidence, cache invalidation, and eviction. Preserve existing raw, published, workspace, and partial-failure behavior through the full suite.

## Done criteria

- [x] All seven named regression tests are discovered and pass (two unit, five integration).
- [x] Uncached unchanged children reject the entire cache update; result-ID growth cannot cause mid-response reconstruction failure.
- [x] Workspace-only partials persist; malformed, errored, and cancelled results leave accepted cached state intact.
- [x] Three transient CLI calls reuse the same Owner and reconstruct full related reports, including partial-only input.
- [x] Third-call fixture assertions prove previous-result IDs survive export/import updates.
- [x] Cache-limit tests assert both snapshot count and serialized byte bounds; no evicted entry is revived by import.
- [x] Raw Query output and diagnostic `rawReport` evidence retain their original semantics.
- [x] Full test/build, lint, format, schema, stored-state, and whitespace gates exit 0.
- [x] Only scoped implementation/metadata files changed; temporary Owners cleaned up.
- [x] Plan and index status/evidence updated.

## Verification evidence (2026-09-08)

- Starting HEAD: `e83786f` (merged PR #59), detached at `origin/main`, clean worktree. #40 is closed. Drift review covered the landed complete-chunk change plus explicit-server selection, bounded handshakes/writes, and synchronization delivery; those invariants remain intact. `ActiveQuery.partial_chunks` retains whole progress values, while `partial_result_items` alone supplies the existing flattened failure evidence. Baseline full suite passed **139** tests (`/tmp/49-baseline.log`).
- Cache RED: both required unit tests failed on the original implementation: export/import reverted `main-two` to `main-one`, and an unresolved related report yielded non-null effective data instead of rejecting the update (`/tmp/49-cache-red.log`). An initial Rust sum type-inference typo was fixed before recording this intended RED.
- CLI RED: final-only and partial-only related reports both passed the first full response and failed the second independent CLI invocation with `invalid_server_result` at the related URI; Workspace follow-up lacked the partial-only record's previous ID (`/tmp/49-cli-red.log`). A relative test `--file` argument was corrected before this evidence. Malformed coverage uses an invalid report shape (`items: 17`), not deeper Diagnostic validation outside this plan.
- The Owner now merges complete chunks and validates cloned diagnostic values through the existing Query helpers only after successful post-response Document validation. It never rewrites final results or raw partial evidence. Cache reconstruction reads all prerequisites before any bounded store, so unresolved children reject the whole update and ID-growth eviction cannot interrupt current-response reconstruction. Pull cache payloads retain effective full content/latest IDs; import remains independent-record-only and published records are unchanged.
- Caller audit: `dispatch_owner_query` imports independent Owner state before composing diagnostic requests (read-only `session.rs`); document/workspace envelopes retain their existing invalid-result checks and exact current raw metadata. Owner successful responses call the shared merger/validator for only the two diagnostic methods, and errored/cancelled/failed-validation paths do not update the pull cache. `apply_pull_report` remains the independent import path; document/workspace methods reconstruct all accepted reports before storing. `store`, result-ID invalidation, and byte accounting remain the existing bounded mechanism.
- Discovery lists exactly **two** unit and **five** integration `related_diagnostics` tests. All seven pass, all six diagnostics unit tests pass, and `named_cardinality_and_invalid_results_are_normalized` passes. The five integration tests also passed three repeated runs (**15/15**). Fixtures assert actual latest previous IDs on the third process invocation, stable Owner generations, final/partial/Workspace reconstruction, malformed/unresolved/error/cancel rejection, and exact raw results/progress traces (`/tmp/49-cli-green.log`). Cancellation uses the existing partial-byte-limit/acknowledgement gate, not a sleep race.
- Full suite passed **152** tests with concurrent plan 012 changes present (104 unit + 4 CLI + 3 documentation + 38 lifecycle + 3 installation; `/tmp/49-full-green.log`). Clippy with `-D warnings`, formatting, schema, stored-state, and `git diff --check` all exit **0**. Tests ran locally on Linux; native Windows/macOS and MSRV were not run locally.
- Issue #49 changes are limited to its five allowed implementation files and this plan. Other worktree changes belong to the separately authorized plans 012/014; the root coordinator owns their shared index update and final combined review. No untracked files, dependency/schema changes, commit, push, or PR introduced by this task. Temporary fixtures stop only their own Owners, including panic cleanup.

- Final coordinator verification after all three plans: **155** tests pass (107 unit + 4 CLI + 3 documentation + 38 lifecycle + 3 installation), exact regression discovery counts **6/2/5/3**, and Clippy, formatting, schema, stored-state, and whitespace gates pass. Logs: `/tmp/48-50-root-full-green.log`, `/tmp/48-50-root-clippy.log`. Independent coordinator review found no blocking findings; the shared index now marks plans 012–014 DONE. Changes remain uncommitted; native Windows/macOS/MSRV CI was not run.

## STOP conditions

Stop if plan 004's chunk representation is absent/ambiguous; handling related reports would require a new persistent format or public output field; the fake server cannot demonstrate three independent CLI calls sharing an Owner; cache bounds cannot be preserved with the existing record model; or verification fails twice after reasonable correction. If prior work already fixes part of this bug, keep the regression and implement only the remaining delta.

## Maintenance notes

Review `apply_pull_report`, export/import, and result-ID invalidation together whenever diagnostics change. Effective cache payloads and exact current raw responses are different concepts even where legacy field names say `raw_report`; document that internal distinction locally rather than renaming the whole subsystem. Do not promote this into an unbounded related-report graph or persistent database. Tests for partial-related diagnostics depend on retaining complete chunks at the Owner boundary.
