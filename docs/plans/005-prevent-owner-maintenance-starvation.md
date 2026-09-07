# Plan 005: Keep Owner maintenance deadlines live under notification traffic

> **Executor instructions:** Follow every step and verification gate. Stop rather than broaden scope. Update row 005 in `docs/plans/README.md` when done unless the dispatching reviewer maintains it.
>
> **Drift check first:** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs test_support/fake_lsp_server.rs tests/owner_lifecycle.rs`
> Review changes in these shared files against the excerpts below. Known changes from other selected plans may be reconciled after their tests pass; unexplained control-flow drift is a STOP condition.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

The Owner recreates a 25 ms sleep after every incoming event. A language server sending frequent notifications can prevent request timeout, caller-disconnect cancellation, and cancellation-grace cleanup indefinitely. A persistent deadline with priority over unrestricted traffic fixes this without changing serial Query dispatch.

## Current state

`src/session/owner_runtime.rs:277–280` starts the biased event selection:

```rust
tokio::select! {
    biased;
    pending = controls_rx.recv() => {
```

The stderr and frame branches occur before maintenance (`:437–505`). The maintenance branch at `:512–519` creates a fresh sleep every iteration:

```rust
_ = tokio_time::sleep(Duration::from_millis(25)) => {
    if lsp.maintain_active_queries(
        &bootstrap.owner_generation,
        &mut active_queries,
    ).await {
        should_stop = true;
    }
```

`maintain_active_queries` at `:1510–1541` is the deadline/cancellation seam:

```rust
let now = TokioInstant::now();
// ... for each ActiveQuery:
let caller_cancelled = *query.cancelled.borrow();
if caller_cancelled || now >= query.deadline {
```

It writes `$/cancelRequest`, then terminates the process tree once cancellation grace expires (`:1543–1569`). The same maintenance branch calls `process.try_wait`; move the whole maintenance work, not just request timeout checks. The existing post-dispatch timeout semantics and graceful-stop behavior are not being redesigned here.

Use `tests/owner_lifecycle.rs:414–469`, `force_stop_cancels_an_active_query_without_waiting_for_it`, as the subprocess/time-bound exemplar:

```rust
let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
while !marker.exists() && std::time::Instant::now() < marker_deadline {
    std::thread::sleep(std::time::Duration::from_millis(2));
}
assert!(marker.exists());
```

The test file's `Fixture` creates a temporary Workspace and platform-specific isolated configuration/state. The fake server in `test_support/fake_lsp_server.rs` already supports named scenarios, method event logs, independent framing, and deterministic marker files. Reuse these rather than testing against a real installed server. `[session].cancellation_grace` and `--request-timeout` already exist (`src/configuration.rs:906–907,970–972`). Tokio's installed features include time but not `test-util`; do not add features just to pause time.

**Domain/design:** `CONTEXT.md` defines an Owner as “A long-lived process responsible for one initialized language-server session and its Queries.” ADR `docs/adr/0001-use-per-session-background-owners.md:3` says “operations run serially” and “keeping session failures isolated.” ADR `docs/adr/0006-use-guarded-async-lsp-in-one-package.md:3` places “response correlation, and cancellation” in the session module. Keep those decisions; this is not a task-per-Query concurrency refactor.

## Commands you will need

Use the existing Rust toolchain, MSRV 1.89; do not install dependencies.

| Purpose | Command | Expected |
|---|---|---|
| Baseline/full tests | `cargo test --locked --all-targets --features fake-server` | Exit 0 |
| Regression tests | `cargo test --locked --features fake-server --test owner_lifecycle owner_maintenance_` | Two named new tests pass after the fix |
| Existing scheduling | `cargo test --locked --features fake-server --test owner_lifecycle owner_serializes_simultaneous_agent_operations_in_fifo_order -- --exact` | One test passes |
| Existing force stop | `cargo test --locked --features fake-server --test owner_lifecycle force_stop_cancels_an_active_query_without_waiting_for_it -- --exact` | One test passes |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Lint/typecheck | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |

## Scope

**Only implementation files allowed:**
- `src/session/owner_runtime.rs` — scheduling of existing maintenance.
- `test_support/fake_lsp_server.rs` — a bounded notification-traffic fixture scenario.
- `tests/owner_lifecycle.rs` — two regressions and narrowly required bounded subprocess/cleanup helpers.

**Metadata exception:** this plan and row 005 of `docs/plans/README.md`.

**Out of scope:** bounded language-server writes (plan 011), connection admission (plan 008), timer configuration/schema changes, `async-lsp` replacement, unrelated callback scheduling, changing serial dispatch or cancellation/error contracts, and new dependencies.

## Git workflow

Leave changes uncommitted unless separately authorized. If the operator requests a branch, use `advisor/005-prevent-owner-maintenance-starvation`; use plain imperative commit subjects if commits are requested (existing example: `Add Windows installer`). Do not push or open a PR.

## Steps

### 1. Establish a bounded, reproducible red test

Run the full baseline. Add a `notification-flood` fake-server scenario that initializes normally, accepts one fixture Query, creates a marker, and then emits a valid benign notification every 5 ms while withholding the Query response. Continue reading stdin and record receipt of `$/cancelRequest`, but deliberately do not settle that request. Bound the fixture's lifetime independently (for example, 10 seconds). A small scenario-specific stdin-reader thread plus `recv_timeout`/serialized stdout loop is sufficient; do not introduce a generalized threaded fake-server framework or write concurrently through unsynchronized stdout handles.

Add `owner_maintenance_times_out_under_notifications` and `owner_maintenance_cancels_disconnected_caller_under_notifications` in `tests/owner_lifecycle.rs`. Use a 100 ms request timeout for the first; use a long request timeout and close/kill only that CLI caller after the ready marker for the second. Configure a 250 ms cancellation grace in the isolated user configuration. Assert the fixture records cancellation while traffic continues and the Owner terminates that server after grace rather than remaining blocked.

Use subprocess `spawn`, `try_wait`, and a watchdog/cleanup guard with a 5-second outer deadline; do not call unbounded `Command::output()` on the very operation whose bounded completion is under test. On watchdog expiration clean up only the fixture processes and fail the assertion. A fixture marker/event-log gate, not sleep alone, establishes that the Query was dispatched before measuring. Ignored cancellation currently ends with the existing `protocol_failed` unsafe-delivery failure; do not require a new error code. The disconnect case must check actual cancellation and server cleanup, not merely the CLI process exiting.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle owner_maintenance_ -- --list` lists exactly the two new names. The focused regression command then fails on its explicit watchdog/bounded-cancellation assertion against the old scheduler, not on compilation or a malformed fixture frame. The command itself must terminate after bounded cleanup.

### 2. Keep one persistent maintenance timer with due-work priority

Create a `tokio_time::interval_at` outside the `while !should_stop` loop, first tick at now + 25 ms, period 25 ms. Set `MissedTickBehavior::Skip` so delayed work does not cause a burst of catch-up maintenance. Place its `tick()` branch first in the biased select, before control, stderr, server-frame, and ready callback branches. Move the existing maintenance body there unchanged except for necessities of scope/borrowing. A persistent resettable `Sleep` with equivalent absolute deadlines is also acceptable, but implement one mechanism, not both.

Do not recreate the timer in the loop, reset it on frame receipt, put it below an always-ready branch, or disable it while requests are active. Keep the separate absolute idle deadline. Preserve FIFO dispatch and control handling between ticks. No background task or custom scheduler is needed.

**Verify:** the two `owner_maintenance_` tests pass within their watchdogs. Both existing scheduling and force-stop commands from the table each run one passing test.

### 3. Verify all lifecycle behavior

Run the complete lifecycle integration target, then full tests and static gates. Check idle shutdown, initialization queuing, graceful drain, and process-exit detection remain covered; retain the existing behavior rather than relaxing assertions. A permanently blocked write still requires plan 011 and is not evidence that this timer fix failed.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle`, `cargo test --locked --all-targets --features fake-server`, `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features fake-server -- -D warnings`, and `git diff --check` all exit 0. `git status --short` shows only scoped changes relative to the baseline, plus the metadata exception.

## Test plan

The two new tests distinguish timeout cancellation from caller-disconnect cancellation. Each must observe cancellation under sustained traffic and process cleanup after ignored cancellation. Use generous outer scheduling tolerance, but never turn a failed bound into a longer unconstrained sleep. Existing FIFO and force-stop tests are required regressions. Test list output prevents zero-test false positives.

## Done criteria

- [x] Exactly the two named `owner_maintenance_` tests are listed and pass.
- [x] Each new test has bounded failure cleanup; no fixture Owner/server remains after the test.
- [x] FIFO, force-stop, and full lifecycle tests pass.
- [x] Full suite, formatting, Clippy, and `git diff --check` exit 0.
- [x] No out-of-scope file, protocol value, or dependency changed.
- [x] Row 005 in the index is updated with status and verification.

## Verification results

Implemented for #41 from `9876f67b9453ad5cacbe4226aefa0f25e733d4ab` in the operator's current checkout (already detached HEAD), with explicit commit authorization. The starting tree was clean. Drift from `5268c6a` in all three shared files was exactly merged plan 004 (#40/#52); its regressions passed in the baseline. The scheduling excerpts and maintenance body had not changed.

| Gate | Result |
| --- | --- |
| Baseline `cargo test --locked --all-targets --features fake-server` | 111 passed |
| `cargo test --locked --features fake-server --test owner_lifecycle owner_maintenance_ -- --list` | Exactly the two required names |
| Focused RED against the old scheduler | Both failed the explicit cancellation watchdog; command exited in 5.10s, with no malformed-frame or compile failure |
| Focused GREEN | Both passed in 0.44s |
| FIFO exact filter | 1 passed |
| Force-stop exact filter | 1 passed |
| Complete `owner_lifecycle` integration target | 19 passed |
| Final `cargo test --locked --all-targets --features fake-server` | 113 passed |
| `cargo fmt --all -- --check` | Exit 0 |
| `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0, during implementation and at the final gate |
| `git diff --check` | Exit 0 |
| Independent `/code-review` against the starting commit | Standards: 0 findings; Spec: 0 findings |
| Isolated idle-shutdown smoke check | 250ms idle timeout delivered `shutdown` and removed the Owner session within a 5s watchdog |

The production change adds one persistent interval and moves the entire existing maintenance branch first in the biased selection. The first tick is delayed 25ms and missed ticks are skipped. Serial dispatch, the separate absolute idle deadline, cancellation/error semantics, and maintenance work itself remain unchanged.

The fixture uses one stdout writer, a scenario-specific stdin reader, and an independent 10s process-exit watchdog. A lifetime file lock proves server cleanup; an event-log entry proves notification traffic continues after cancellation. Both tests share one 5s outer deadline, kill/reap the CLI on failure, reuse bounded isolated Owner force-stop cleanup, and check the Owner session disappears after server termination. Process inspection after RED found no remaining fixture servers or Owners. Existing lifecycle tests retain initialization queuing, graceful drain, and unexpected-exit coverage. The initial manual idle check incorrectly expected an `exit` event from the standard fixture, which exits when answering `shutdown`; correcting that smoke assertion passed without a source change.

Only the three allowed implementation files and the two metadata files changed. No branch switch, protocol/configuration/schema change, new dependency, or transport-write refactor was needed. Verification ran on Linux; native macOS/Windows verification remains with CI.

## STOP conditions

Stop on unexplained drift, an out-of-scope requirement, or two unsuccessful attempts at a verification gate. Stop if the regression is actually blocked in a server write rather than starved maintenance: record that distinction and leave the write fix to plan 011. Stop if the fake scenario cannot independently guarantee valid framed traffic and bounded teardown; do not accept a hanging red test. Do not add request concurrency or change public failure semantics as a shortcut.

## Maintenance notes

Review select branch order whenever new ready branches are introduced. A persistent timer below permanently ready traffic is still starvable. The guarantee here is scheduling progress between awaits; plan 011 must bound transport awaits to complete the Owner responsiveness guarantee. Execute this plan before plan 011, and serialize edits to the shared runtime and fixture files.
