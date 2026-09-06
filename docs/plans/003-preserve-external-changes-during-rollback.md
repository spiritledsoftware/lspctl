# Plan 003: Preserve external changes when Application rollback lacks proof

> **Executor instructions:** Follow the steps and verification gates in order. Do not treat an existing file, matching inode alone, or an intended final digest as proof that this Application changed it. Leave changes uncommitted unless separately authorized. Update plan 003 in `docs/plans/README.md` on completion unless a reviewer owns the index.
>
> **Drift check, run first:** `git diff --stat 5268c6a..HEAD -- src/mutation/application.rs src/mutation/planner.rs src/mutation/state.rs docs/plans/003-preserve-external-changes-during-rollback.md docs/plans/README.md`. Plan 002 is expected to change manifest inspection and directory-digest handling. Read that diff, map the excerpts below to the strengthened implementation, and keep its tests passing. STOP on unrelated/unexplained semantic drift; do not undo the prerequisite.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** L
- **Risk:** HIGH
- **Depends on:** `docs/plans/002-bind-recursive-directory-membership.md`
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05
- **Audit finding:** 3

## Why this matters

After a commit failure, rollback currently restores every changed original file whose identity still matches and removes occupied paths that were originally missing. Those differences can belong to an external editor, even when Application rejected the relevant operation before writing anything. The fix must undo only provable Application effects, otherwise leave the bytes intact and expose Recovery instead of falsely reporting `rolled_back`.

## Current state

- `src/mutation/application.rs` owns staging, ordered commit, rollback, durable terminal Receipts, and explicit Recovery.
- `src/mutation/planner.rs` owns canonical operations, resource identities/digests, and capability-root inspection.
- `src/mutation/state.rs` owns version-1 journals and compatibility tests.

`src/mutation/application.rs:869–870` rejects an externally changed text resource before writing it:

```rust
if digest_raw_bytes(&bytes) != before_digest {
    return Err("Text resource changed during commit.".to_owned());
}
```

Nevertheless `rollback_transaction` restores changed files merely because their identities still match (`src/mutation/application.rs:1404–1417`):

```rust
if expected == actual {
    continue;
}
if expected.resource_kind == ResourceKind::File
    && actual.resource_kind == ResourceKind::File
    && expected.identity_digest == actual.identity_digest
{
    let backup_path = backup_path_for(&transaction.backups, &expected.path)
        .ok_or_else(|| "A required rollback backup is missing.".to_owned())?;
    restore_in_place.push((backup_path, expected.path.clone()));
}
```

`rollback_resource_operations` deletes any occupied originally absent create target (`src/mutation/application.rs:1464–1473`):

```rust
CanonicalOperation::Create { path, .. } => {
    let expected = manifest_for_path(&transaction.before_manifest, path)?;
    let actual = inspect_manifest_path(planner, path)?;
    if !expected.exists && actual.exists {
        let relative = planner.relative_path(path)?;
        remove_capability_resource(planner.capability_root(), relative)
            .map_err(|error| error.to_string())?;
        flush_parent(path).map_err(|error| error.to_string())?;
    }
}
```

`commit_operations` (`:764–842`) records only a failing operation index, not which effects completed. `TransactionRecord` (`src/mutation/state.rs:130–148`) contains ordered operations and before/intended/observed manifests but no completed-effect provenance. Explicit `recover_rollback` and automatic rollback both reach the same destructive helper (`src/mutation/application.rs:650–666`).

### Constraints and test exemplar

`CONTEXT.md:49–55` defines Recovery as “Resolution of a failed Application whose filesystem state could not be restored automatically,” and a Receipt as “A durable record of the terminal outcome of one Application or Recovery.” A preserved external change cannot truthfully be described by the existing `rolled_back`/`filesystem_state: unchanged` outcome if the complete pre-Application manifest is not restored.

`docs/adr/0004-apply-exact-previews-with-recoverable-transactions.md:3` requires at-most-once completion and “either reaches the intended manifest, rolls back, or exposes Recovery with the exact observed state.” It advertises LSP `undo`, not `transactional`; conservative Recovery is allowed. `docs/adr/0007-use-capability-based-workspace-filesystem-access.md:3` keeps no-follow access and journal ownership inside Mutation. Do not substitute unrestricted path-based restore/delete calls.

Use the existing inline test pattern in `src/mutation/application.rs:2525–2607`:

```rust
stage_transaction(&transaction, &transaction.operations, &mutation_limits).unwrap();
commit_operations(&planner, &artifact_directory, &transaction.operations).unwrap();
let restored = rollback_transaction(&transaction, &planner).unwrap();
assert!(manifest_mismatches(&transaction.before_manifest, &restored).is_empty());
assert_eq!(fs::read_to_string(source).unwrap(), "source");
```

The same module's `abandoned_commit_is_sealed_and_rolled_back_without_replaying_writes` (`:2610–2690`) supplies an isolated `MutationStateStore::open_at`, persists a transaction, reconciles it, then calls explicit Recovery. Update its setup to use the new real commit-progress path; do not weaken its original-byte and terminal-cleanup assertions.

## Commands you will need

Run from the root with existing Rust 1.89+ tooling and dependencies.

| Purpose | Command | Expected result |
|---|---|---|
| Discover regressions | `cargo test --locked --bin lspctl rollback_provenance -- --list` | All required names listed |
| Focused tests | `cargo test --locked --bin lspctl rollback_provenance` | After implementation, all pass, nonzero count |
| Mutation suite | `cargo test --locked --bin lspctl mutation::` | All pass, including plan 002 |
| Full suite | `cargo test --locked --all-targets --features fake-server` | All pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Format | `cargo fmt --all -- --check` | Exit 0 |
| Schemas | `python scripts/release/check_schema.py` | Exit 0 |
| Immutable fixtures | `python scripts/release/check_stored_state.py` | Exit 0 |

## Scope

**Implementation files allowed:** `src/mutation/application.rs`; `src/mutation/planner.rs` only for reusable capability-safe identity inspection needed by commit proof; `src/mutation/state.rs` only for compatibility tests or narrowly sharing existing record-write primitives.

**Metadata exception:** this plan and its index row.

**Out of scope:** public schema changes, changes to `TransactionRecord`'s version-1 wire shape, immutable `tests/fixtures/stored-state/v1/*`, blanket rollback refusal for every new Application, alternate transaction engines, watchers, user-facing force/rebase options, new dependencies, unrelated filesystem hardening. Runtime transaction artifacts described below are temporary protected state, not new repository files.

## Git workflow

Use the operator's branch; `advisor/003-safe-rollback` is a suggested name only if branch creation is requested. Leave changes uncommitted. If a commit is separately requested, use imperative wording such as `Preserve external changes during rollback`. Do not push or open a PR.

## Steps

### Step 1: Reproduce destructive rollback without timing races

Add these tests in the application module using `TempDir`, existing planner setup, and direct private staging/commit seams:

- `rollback_provenance_preserves_unwritten_text`: stage two text changes, externally alter the second file, then commit the ordered batch. The first write can succeed; the second fails its before-digest guard. Calling the rollback path must not restore the second file from backup. Require Recovery when the complete original manifest cannot be restored.
- `rollback_provenance_preserves_uncreated_target`: stage a create, externally create its target before commit, observe the create failure, then ensure rollback leaves its exact bytes and identity intact.
- `rollback_provenance_preserves_postcommit_editor_change`: successfully perform an Application text/create effect using the private seam, externally edit/replace that resource before rollback, and require preservation rather than overwrite/deletion.

Do not use background threads or sleeps. The explicit boundaries between staging, commit, and rollback are sufficient.

**Verify:** `cargo test --locked --bin lspctl rollback_provenance -- --list` lists all three names. `cargo test --locked --bin lspctl rollback_provenance` fails on preservation/outcome assertions in the current implementation, not setup or compilation.

### Step 2: Record bounded effect evidence without changing stored v1 journals

Use one small, versioned **private progress sidecar inside the existing owned transaction artifact directory**, plus its in-memory representation. This avoids changing the public `TransactionRecord` shape or first-release fixtures. Bind it to the transaction ID, canonical operations digest, and before-manifest digest. Validate the artifact owner marker, no-follow file access, schema/version, operation indices, and a size bound derived from the existing entry/operation limits before trusting it. Missing/invalid progress is absence of proof, never permission to overwrite.

For the serial `commit_operations` loop:

1. Persist an operation's `pending` intent before its first filesystem effect.
2. Capture completed-effect proof from resources actually opened/moved by that operation: operation index, affected resource identity, canonical expected after-content (or exact resource transfer), and protected undo-resource identity where applicable. A new create needs the identity of the file this Application actually created, not whichever file later occupies that path.
3. Persist `completed` proof before advancing to another operation. A failure proven to occur before any mutation may be marked `no_effect`; a partial write, failed flush, partial overwrite-rename, or failed evidence persistence remains uncertain.
4. Treat a persisted `pending` entry after restart as uncertain unless no change can be established without destructive action. Never interpret a missing completion record as “the operation did not happen.”

Use the installed atomic-write facility and existing private permissions, flush, and ownership patterns. The sidecar must not reference arbitrary external backup destinations. Do not add per-request environment-controlled failure injection; a private `#[cfg(test)]` hook or small test writer seam is permitted for precise failure boundaries.

A post-write observation is not automatically proof: validate against canonical content and the identity obtained from the mutated handle. In text commit, retain/revalidate the actual no-follow handle before truncation so reopening a replaced path does not produce a false identity claim. If any necessary identity cannot be established, stop committing and expose Recovery.

**Verify:** add `rollback_provenance_progress_round_trips_and_rejects_invalid_evidence` and `rollback_provenance_uncertain_commit_requires_recovery`. The latter covers failure before a filesystem effect, after an effect but before the progress commit, and after progress durability. The list command must include both; run each by its full unique test-name filter and require at least one executed test and exit 0. Existing private-staging tests also pass.

### Step 3: Preflight rollback from proven effects before changing anything

Replace whole-manifest “restore every difference/delete every created-looking path” logic with reverse-order handling of completed effects. Preflight the complete rollback against current observations before performing destructive rollback actions:

- Never restore a file solely because the original identity still matches.
- For completed text and overwrite-create effects, require that current identity and contents still match the last proven Application-produced state. Restore only the matching original backup.
- For a new create, remove only the exact created resource with matching proven state; an external replacement or subsequent edit is a conflict.
- For rename/delete, require the recorded moved resource/undo identity and the certified directory membership from plan 002. An occupied restored destination or externally changed subtree is a conflict.
- Walk repeated text edits and create/rename/delete sequences in reverse **operation order**. Do not compare an intermediate operation directly against the final path without accounting for later proved operations.
- Leave `no_effect` operations untouched. Pending/ambiguous effects must not trigger guessed restoration. If rollback cannot be proved safe, return the existing Recovery path without first overwriting the conflict.

The same proof policy applies to explicit `recover_rollback`: supplying the observed digest authorizes an attempt, not inference that every observed change came from the Application. `recover_accept_current` remains the explicit non-replay alternative. Preserve accurate observed manifests and the Workspace write stop.

Keep `rolled_back`/`restored` only when the **entire** recorded before-manifest is actually restored; otherwise produce existing `recovery_required`/`recovery_failed` results. Revalidate before destructive operations, keep artifacts on failures, and do not claim protection from all theoretically indistinguishable concurrent edits.

**Verify:** all Step 1 tests now pass. Add and pass `rollback_provenance_restores_completed_ordered_effects`, with successful ordinary text, repeated text, new create, overwrite-create, rename-overwrite, and delete cases. Run `cargo test --locked --bin lspctl mutation::application::tests` with a nonzero test count and exit 0.

### Step 4: Preserve inspectability and crash-safe behavior for old records

A missing sidecar on a legacy version-1 transaction is expected. Continue to deserialize it and expose Recovery/status/`accept-current`; do not rewrite it into a falsely certified new transaction. If its filesystem already matches the before-manifest, non-destructive completion is allowed. Otherwise refuse rollback operations that require unavailable provenance, preserving artifacts and bytes with structured Recovery failure.

Add `rollback_provenance_legacy_journal_is_readable_but_not_authority` using a synthetic legacy journal assembled in `TempDir`, not a modified immutable fixture. Cover a matching-before no-op case, a changed file that must survive, and explicit accept-current cleanup. Extend the abandonment test to verify that completed durable evidence permits safe rollback after simulated restart, while pending evidence does not.

**Verify:** `cargo test --locked --bin lspctl rollback_provenance` lists and passes the seven required named tests. Run `cargo test --locked --bin lspctl mutation::state::tests::first_release_stored_state_fixtures_remain_readable` and `python scripts/release/check_stored_state.py`; both exit 0.

### Step 5: Run complete verification and report the compatibility restriction

Run full tests, Clippy, format, schema and fixture checks from the table. Record that legacy ambiguous rollback may now refuse safely instead of restoring guessed state; this is intentional, not a reason to refresh first-release fixtures. Record actual native platforms tested and update the index only after all gates pass.

**Verify:** every gate exits 0; `git diff --check` exits 0; `git status --short` contains only allowed changes relative to the initial worktree.

## Test plan

The seven required `rollback_provenance_*` tests above cover untouched external writes/creates, edits after an Application effect, invalid evidence, crash/evidence gaps, successful ordered undo, and legacy records. Use byte equality plus identity checks where supported; also assert terminal outcomes, retained artifacts, and write-stop behavior. Keep the existing at-most-once, resource-identity, staging-ownership, and abandoned-commit tests green. Add no live races or real-user-state fixtures.

## Done criteria

- [x] All seven named regression tests are listed and pass; no zero-test filtered runs.
- [x] An unwritten external text change and an uncreated external file both survive failed Application and explicit Recovery attempts.
- [x] Completed ordinary Application effects still roll back successfully when unchanged externally.
- [x] Simulated crash gaps never become permission for destructive guessed restoration.
- [x] New progress data is bounded and validated; legacy v1 records/immutable fixtures remain readable and unchanged.
- [x] Full tests, Clippy, format, schema, fixture, and diff checks exit 0.
- [x] Only allowed paths changed and index row 003 is updated.

## STOP conditions

STOP if plan 002 is not complete, resource identity cannot be tied to the mutated handle, safe sidecar validation requires a public format change, a supported ordered sequence cannot be rolled back without guessing, or the proposed solution relies solely on a numeric completed-operation count. STOP rather than implementing a second general transaction framework. Also STOP on unexplained drift, two failed verification attempts, or required out-of-scope edits. Do not resolve a conflict by deleting user bytes or refreshing immutable fixtures.

## Completion evidence — 2026-09-06

Implemented for #39 from `230891f`, retaining plan 002's directory certificates and existing regression assertions. The operator explicitly authorized continuing after the verification-failure STOP and requested a commit on the current branch. No other safety STOP was waived.

A private, bounded progress sidecar binds ordered before/after effect evidence to the transaction, canonical operations, and original manifest. Pending or invalid evidence does not authorize destructive undo. Rollback preflights the full reverse chain and revalidates each destructive step, including both legs of overwrite-rename undo. Workspace write stops and artifacts remain when external changes prevent exact restoration.

**Compatibility restriction:** legacy v1 journals remain readable and support status and `accept-current`. Without provenance, rollback succeeds only when the complete before-manifest already matches; otherwise it refuses safely instead of restoring guessed state. No public schema or immutable stored-state fixture changed. Snapshot evidence has a documented quadratic operation-count storage ceiling; future changed-resource-only proofs are deferred.

Native verification: **Linux x86_64 only**. Windows/macOS runtime validation remains for CI.

| Gate | Result |
| --- | --- |
| `cargo check --locked --all-targets --features fake-server` | Passed |
| `cargo test --locked --bin lspctl rollback_provenance -- --list` | All seven required names plus the review regression listed (8 total) |
| `cargo test --locked --bin lspctl rollback_provenance` | 8 passed |
| Each unique progress round-trip and uncertain-commit test filter | 1 passed per filter |
| `cargo test --locked --bin lspctl mutation::application::tests` | 20 passed |
| `cargo test --locked --bin lspctl mutation::` | 33 passed |
| `cargo test --locked --bin lspctl mutation::state::tests::first_release_stored_state_fixtures_remain_readable` | 1 passed |
| `cargo test --locked --all-targets --features fake-server` | 108 passed across all targets |
| `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `python scripts/release/check_schema.py` | Passed |
| `python scripts/release/check_stored_state.py` | Passed |
| `git diff --check` | Passed |

The three initial preservation regressions failed on destructive rollback before the fix. Two-axis review: Standards found no violations; Spec found one missing revalidation between overwrite-rename rollback legs. The additional deterministic `rollback_provenance_rechecks_each_rename_leg` regression failed before its fix and now passes for an externally occupied destination and externally edited undo resource. Follow-up review confirmed the finding resolved, with no outstanding findings.

## Maintenance notes

Review the journal-to-filesystem crash windows and every destructive branch before approving this high-risk change. A completed-operation counter alone is insufficient: external edits after that completion must still be detected. Future resource operations must provide their own proof or fall back to Recovery. Plan 006 shares initial transaction ownership and plan 012 shares staging; serialize those diffs and preserve their boundaries. More conservative Recovery is preferable to a false `rolled_back` Receipt.
