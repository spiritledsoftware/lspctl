# Plan 006: Release Preview reservations when pre-journal preparation fails

> **Executor instructions:** Execute the steps and verification commands in order. This is a reservation-lifetime fix, not permission to clear arbitrary `reserved` flags. Leave changes uncommitted unless separately authorized, and update plan 006 in `docs/plans/README.md` when complete unless the reviewer owns the index.
>
> **Drift check, run first:** `git diff --stat 5268c6a..HEAD -- src/mutation/application.rs src/mutation/state.rs docs/plans/006-release-prejournal-preview-reservations.md docs/plans/README.md`. Plans 002/003 may already have changed Application preflight or commit evidence. Compare live code with the excerpts, retain those changes, and map this plan to the same reservation-to-journal handoff. STOP on unexplained semantic drift.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05
- **Audit finding:** 6

## Why this matters

`apply_preview` durably reserves a Preview, then several fallible preparation calls propagate errors without releasing it. A Receipt-capacity error or inspection failure can therefore make the same Preview permanently return `application_busy`, despite there being no recoverable transaction. Explicitly bound reservation ownership lets the same immutable Preview be retried after the actual prerequisite is fixed.

## Current state

`src/mutation/application.rs:89–93` reserves before preparation and manually releases only selected error paths:

```rust
let mut stored = context.store.reserve_preview(preview_id)?;
if deadline_expired(context.caller_deadline) {
    let _ = context.store.release_preview(&mut stored);
    return Err(application_cancelled(preview_id));
}
```

`src/mutation/application.rs:132–140` uses `?` while that reservation is live:

```rust
context
    .store
    .ensure_receipt_capacity(context.receipt_limits)?;
let planner = WorkspaceEditPlanner::open(
    &stored.workspace_path,
    parse_position_encoding(&stored.preview.position_encoding),
    context.preview_limits,
    context.mutation_limits,
)
```

Other unguarded failures are `list_transactions` (`:107`), manifest inspection (`:142–154`), ID generation (`:175`), and the first `write_transaction` (`:197`). Once that initial journal exists, later failures belong to the staged/committing/Recovery lifecycle rather than pre-journal cleanup.

`src/mutation/state.rs:279–306` owns the durable flag:

```rust
let mut preview = self.read_preview(preview_id)?;
if preview.preview.reserved {
    // returns application_busy
}
preview.preview.reserved = true;
self.write_preview(&preview)?;
// release_preview performs the inverse write:
preview.preview.reserved = false;
self.write_preview(preview)
```

Reserved records are excluded from expiration pruning (`src/mutation/state.rs:685`). Clearing them at random or during normal pruning could invalidate a live transaction.

A load-bearing detail: `write_record` commits the atomic record **before** applying file permissions (`src/mutation/state.rs:946–958`):

```rust
file.write_all(bytes)
    .and_then(|()| file.commit())
    .map_err(|error| {
        state_failure(record_type, path,
            "A state record cannot be committed.", error.raw_os_error())
    })?;
restrict_file(path)
```

Thus `write_transaction` can return `Err` even though the journal is already present. An unconditional release-on-`Err` wrapper is wrong at this handoff.

### Conventions and exemplar

`CONTEXT.md:33–39` defines Preview as an “identified, immutable representation of one Mutation” and Application as the Agent-authorized attempt to commit it. Retry must use the **same ID**, not create a replacement Preview to hide the leak.

`docs/adr/0004-apply-exact-previews-with-recoverable-transactions.md:3` requires complete journaling, at-most-once Receipts, and explicit Recovery for incompletely restored state. `docs/adr/0007-use-capability-based-workspace-filesystem-access.md:3` permits `atomic-write-file` for single-record state replacement while Mutation owns the multi-resource lifecycle. Preserve those boundaries.

Use the inline test pattern from `src/mutation/application.rs:2282–2366`:

```rust
let workspace = TempDir::new().unwrap();
let state = TempDir::new().unwrap();
let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
// Build a Preview with create_preview_record and local settings.
let first = apply_preview(&mut context, &id).unwrap();
let second = apply_preview(&mut context, &id).unwrap();
assert_eq!(first["outcome"], "applied");
assert_eq!(second["outcome"], "already_applied");
```

`src/mutation/state.rs:1184–1216` shows capacity tests using local `ReceiptSettings`, `write_receipt`, and the `state_capacity_exceeded` code. Keep tests filesystem-local, not dependent on a live Owner or real user state.

## Commands you will need

Run from the root with the existing Rust 1.89+ toolchain and locked dependencies; no installation is needed.

| Purpose | Command | Expected result |
|---|---|---|
| List focused tests | `cargo test --locked --bin lspctl preview_reservation -- --list` | All required names listed |
| Focused tests | `cargo test --locked --bin lspctl preview_reservation` | After fix, nonzero test count and all pass |
| Mutation tests | `cargo test --locked --bin lspctl mutation::` | All pass |
| Full tests | `cargo test --locked --all-targets --features fake-server` | All pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Format | `cargo fmt --all -- --check` | Exit 0 |
| Stored compatibility | `python scripts/release/check_stored_state.py` | Exit 0 |

## Scope

**Implementation files allowed:** `src/mutation/application.rs`; `src/mutation/state.rs` only if a small existing-state readback/error primitive or inline test is needed.

**Metadata exception:** this plan's notes/status and its index row.

**Out of scope:** stored-state schema/version changes, force-unreserve commands, clearing historical crash-orphan reservations, changing pruning policy, Receipt retention changes, post-commit rollback/provenance changes, real process-crash reconciliation, unrelated planner changes, new dependencies, and immutable fixtures.

## Git workflow

Use the existing operator-selected branch. Only if separately requested, create `advisor/006-preview-reservations`. Leave changes uncommitted by default; an authorized commit can use `Release Preview reservations after preparation failures`. Do not push or open a PR.

## Steps

### Step 1: Reproduce a capacity failure followed by retry of the same Preview

Add `preview_reservation_released_after_capacity_failure` in the application test module. Fill a local store's Receipt capacity with a legitimate terminal test Receipt, then create a different valid text Preview. Call `apply_preview` with the full capacity and assert `state_capacity_exceeded`, the stored Preview's `reserved == false`, unchanged Workspace bytes, and no transaction for the failed attempt. Increase the local test Receipt capacity to make one slot available; retry the **same Preview ID** and require `applied`, then `already_applied` with the same Receipt. Do not discard/recreate the Preview or delete a live Receipt to make the test pass.

**Verify:** `cargo test --locked --bin lspctl preview_reservation_released_after_capacity_failure -- --list` lists exactly that test. Run it without `--list`: it must fail on the reservation or retry assertion in the current implementation, not fixture setup.

### Step 2: Make the preparation-to-journal ownership handoff explicit

Keep the Workspace Application lock held throughout. Collect pre-journal preparation into one fallible path, with one caller-visible error cleanup path instead of another collection of hand-written releases. A closure/helper returning prepared values is sufficient; do not introduce a generic transaction framework or a `Drop` guard that silently loses release errors.

The boundary is:

- **Before a journal can exist:** on every returned preparation error, call `release_preview`, return the original failure when release succeeds, and propagate a structured state-write failure if release itself fails. Do not report safe successful cleanup after a failed persistence write.
- **First journal write succeeds:** transfer reservation ownership to the existing transaction lifecycle; subsequent errors must not automatically release it.
- **First journal write returns an error:** read back that exact generated transaction ID under the same lock. If a matching journal exists, retain the reservation and let Recovery handle it. If the state store positively reports `recovery_not_found`, release it. If readback is corrupt, inaccessible, or uncertain, preserve ownership and return a structured state/Recovery failure; never assume absence from an I/O error.

Cover deadline expiry, reauthorization errors/staleness, transaction-list errors or existing Recovery, capacity errors, planner/inspection errors, stale manifests, and ID generation. Do not allow release code to delete a transaction. Preserve existing `stage_transaction` cleanup rules after a durable journal exists.

Also inspect failure of `reserve_preview` itself: its state write can fail after commit for the same permission reason. If this case cannot be disambiguated safely under the lock without widening the change, STOP and report it rather than claiming the plan covers every persistence failure.

**Verify:** the Step 1 test passes. Add `preview_reservation_released_after_preflight_failure`, table-driving deterministic reauthorization/stale/inspection failures through the real preparation boundary and requiring no reservation/transaction after each. Run `cargo test --locked --bin lspctl preview_reservation`; all listed tests pass.

### Step 3: Prove the journal boundary and cleanup-failure behavior

Add `preview_reservation_retained_after_durable_journal` and `preview_reservation_release_failure_is_reported`. For the first, test both normal journal creation and a simulated write error with successful readback of an already committed matching journal; neither may unreserve the Preview. A private test-only persistence seam is acceptable to create that exact ordering without relying on OS permissions or sleeps. Include the contrasting definitely-not-found journal case, which must release.

For release-write failure, force the local state write to fail deterministically and require a structured failure instead of silently returning a result implying successful cleanup. Do not set global environment variables or production failpoints. If a small cleanup helper is extracted, test its error/readback inputs directly in addition to the end-to-end capacity regression.

**Verify:** the list command shows all four required `preview_reservation_*` names. `cargo test --locked --bin lspctl preview_reservation` and `cargo test --locked --bin lspctl mutation::` both exit 0 with nonzero counts.

### Step 4: Run full gates and update the index

Run full tests, lint, format, and fixture integrity checks from the table. Retain the original error codes/stages on successfully cleaned-up errors and retain at-most-once Application tests. Record exact tests run, then update the index status.

**Verify:** every gate and `git diff --check` exit 0; `git status --short` shows only allowed changes relative to the starting worktree.

## Test plan

Required names: `preview_reservation_released_after_capacity_failure`, `preview_reservation_released_after_preflight_failure`, `preview_reservation_retained_after_durable_journal`, and `preview_reservation_release_failure_is_reported`. Cover the same-ID retry, no filesystem changes/no new transaction before journaling, structured failure on cleanup failure, and retained ownership after journal creation. Existing lock-cancellation, at-most-once, state reservation/discard, and staging-ownership tests must remain green.

## Done criteria

- [x] All four required tests are listed and pass; the capacity case fails before the implementation change.
- [x] A pre-journal capacity/inspection error no longer wedges its Preview ID.
- [x] Journal-present and journal-uncertain errors never release transaction-owned reservations.
- [x] Failed reservation cleanup is reported, not ignored.
- [x] Full tests, Clippy, format, fixture integrity, and diff checks exit 0.
- [x] Only scoped files changed and the index status is updated.

## STOP conditions

STOP if the lock does not cover the handoff, an error cannot distinguish absent from uncertain journal ownership, or implementing the fix requires changing persistent formats or clearing live reservations. Also STOP on unexplained drift, out-of-scope edits, or a verification gate that fails twice after a reasonable correction. Do not use a finally/Drop cleanup that unreserves after a durable transaction exists.

## Verification results

Implemented for #42 from clean checkout `c65ef87c0cd6f5bca3a4112cad51c4b9642ccab5`, already detached HEAD, with explicit commit authorization. Drift from `5268c6a` is explained by merged plans 002 (`230891f`) and 003 (`a35fd80`); their directory-membership and rollback-provenance behavior is retained. `state.rs` had no intervening changes.

| Gate | Result |
| --- | --- |
| Baseline `cargo test --locked --bin lspctl mutation::` | 33 passed |
| Capacity test list, then RED before implementation | Exactly one listed; failed the stored reservation assertion after the expected capacity error |
| Journal-boundary RED | Definitely absent journal incorrectly retained its reservation |
| Reservation-write RED | Commit-then-error incorrectly retained its reservation |
| `cargo test --locked --bin lspctl preview_reservation -- --list` | All four required names plus the reservation-write ownership test |
| `cargo test --locked --bin lspctl preview_reservation` | 5 passed |
| `cargo test --locked --bin lspctl mutation::` | 38 passed |
| Final `cargo test --locked --all-targets --features fake-server` | 118 passed |
| `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0; also run during implementation |
| `cargo fmt --all -- --check` | Exit 0 |
| `python scripts/release/check_stored_state.py` | Exit 0 |
| `git diff --check` | Exit 0 |
| Independent `/code-review` against the starting commit | Standards: 0 findings; Spec: 0 findings |

One fallible preparation closure now covers the reservation-to-journal boundary under the existing Workspace lock. Cleanup persistence failures replace the preparation error; successful cleanup preserves the original failure. Initial journal-write errors read back the exact generated ID: absent journals release, matching journals retain ownership, and mismatched/corrupt/inaccessible evidence returns a structured failure without releasing or deleting anything. Existing post-journal staging cleanup remains unchanged.

`reserve_preview` itself compensates a failed write only after this invocation read an unreserved record under the lock. That safely handles errors both before and after atomic commit, propagates compensation failures, and never clears an existing reservation. Private persistence seams exercise these orderings without OS-permission assumptions, global failpoints, or sleeps. The capacity test fills a slot through a real successful Application and retries the same second Preview after increasing local capacity, retaining both Receipts.

Only the two allowed source files and this plan/index metadata changed. No persistent format, pruning policy, dependency, or immutable fixture changed. Verification ran on Linux; native macOS/Windows verification remains with CI.

## Maintenance notes

Keep future fallible preparation calls inside this lifetime boundary. Review the atomic-commit-then-permissions error case specifically; checking only a returned `Result` is insufficient. Historical process-crash orphan reservations remain a separate recovery-policy question, deliberately not solved by a blanket flag reset. Serialize changes with plans 002/003/012 because they share `application.rs`.
