# Plan 011: Bound language-server writes and retire unusable transports

> **Executor instructions:** Read completely; follow every step and verification gate. Stop instead of inventing a new timeout setting, public error contract, or transport architecture. Update row 011 in `docs/plans/README.md` unless the dispatching reviewer maintains it.
>
> **Drift check first:** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs src/session/json_rpc_transport.rs test_support/fake_lsp_server.rs tests/owner_lifecycle.rs`
> Plan 005 deliberately changes the shared event loop and fixture after this baseline. Read its diff and rerun its two tests before reconciling excerpts here. Other understood selected-plan changes are acceptable only after checking their tests and preserving their invariants; stop on unexplained drift.

## Status

- **Status:** DONE — bounded/fatal writes verified; 136 tests and all compatibility gates pass
- **Priority:** P1
- **Effort:** M
- **Risk:** MED
- **Depends on:** `docs/plans/005-prevent-owner-maintenance-starvation.md`
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

A language server that stops reading stdin can block an Owner in `write_all` or `flush`. Because the Owner loop awaits that write, response deadlines, cancellation, and force-stop processing cannot rescue it. Bound writes at the shared seam, treat possibly partial writes as uncertain delivery, and ensure any failed or cancelled frame transmission retires the stream before another Query can dispatch.

This plan also establishes the transport-failure invariant needed by plan 010: after a synchronization event is partially delivered, that Owner generation cannot be reused to query inconsistent server state. Plan 010 repairs DocumentStore event ordering separately; do not implement it here.

## Current state

- `src/session/json_rpc_transport.rs` owns frame serialization, byte bounds, and writing.
- `src/session/owner_runtime.rs` owns operation deadlines, tracing, process cleanup, and dispatch.
- `test_support/fake_lsp_server.rs` provides independently framed deterministic server scenarios.
- `tests/owner_lifecycle.rs` tests real CLI → Owner → server lifetimes with temporary state.

`src/session/json_rpc_transport.rs:189–201`:

```rust
self.output
    .write_all(&header)
    .await
    .map_err(JsonRpcTransportError::WriteFrame)?;
self.output
    .write_all(&body)
    .await
    .map_err(JsonRpcTransportError::WriteFrame)?;
self.output
    .flush()
    .await
    .map_err(JsonRpcTransportError::WriteFrame)?;
```

Both runtime writers (`src/session/owner_runtime.rs:2063–2084`) await this same method without a deadline. `start_dispatch` synchronizes Documents and writes the request before installing `ActiveQuery.deadline` (`:853–901`):

```rust
if let Err(error) = self
    .write_lsp_message_traced(&request, trace.as_mut())
    .await
{
    let _ = response.send(OwnerResponse::failure(
        owner_generation,
        transport_failure(error),
    ));
    return;
}
// ... ActiveQuery:
deadline: TokioInstant::now() + timeout,
```

Initialization likewise writes before its response deadline (`:736–760`). Graceful shutdown sends close notifications and `shutdown` before setting a response deadline (`:1978–2005`), and later gives process waiting a fresh timeout (`:2055–2060`). The runtime already stores `cancellation_grace` and `shutdown_timeout`; launch settings supply initialization duration, and `start_dispatch` receives the selected request duration.

Preserve the conservative existing failure shape at `:2764–2770`:

```rust
fn transport_failure(error: io::Error) -> Value {
    json!({
        "category": "query", "code": "transport_failed", "message": "The language-server transport failed.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe",
        "data": {"reason": error.to_string(), "osCode": error.raw_os_error()}
    })
}
```

`send_synchronization_events` already reports uncertain/unsafe failures (`:1757–1765`). However, returning an error alone does not ensure subsequent dispatch stops: `start_dispatch` can return to the event loop, and best-effort refresh callers may consume errors. A fatal runtime state must outlive the helper result.

**Existing exemplars:** `src/session/json_rpc_transport.rs:370–404` uses `#[tokio::test]`, `duplex(3)`, and spawned bounded stream producers. Its `outbound_limit_writes_nothing` test (`:506–525`) verifies validation before any output:

```rust
assert!(matches!(
    writer.write_json_rpc_frame(&json!({"too": "large"})).await,
    Err(JsonRpcTransportError::OutboundBodyTooLarge { .. })
));
```

Keep local pre-write serialization/size rejection distinct from a poisoned stream. The lifecycle exemplar `tests/owner_lifecycle.rs:414–469` starts a marked active request and asserts force-stop duration and structured failure. Reuse its isolated `Fixture`, but add an actual bounded child watchdog for tests of hangs rather than blocking on `Command::output()`.

Use existing `SupervisedServerProcess::terminate_process_tree(Duration::ZERO)` (`src/session/process_supervision.rs:63–74`) for process-tree cleanup. Do not implement platform-specific process termination again.

**Domain/design:** `CONTEXT.md` defines an Owner generation as “One concrete lifetime of an Owner for a Session identity”; retire that generation after unsafe transport failure. ADR `docs/adr/0001-use-per-session-background-owners.md:3` says “operations run serially” and “keeping session failures isolated”. ADR `docs/adr/0006-use-guarded-async-lsp-in-one-package.md:3` assigns “bounded JSON-RPC framing, serialization, request identifiers, response correlation, and cancellation” to the session module and explicitly excludes the dependency's transport main loop. Preserve those choices.

## Commands you will need

Use the existing toolchain, MSRV 1.89; no dependencies or toolchain installs.

| Purpose | Command | Expected |
|---|---|---|
| Prerequisite regression | `cargo test --locked --features fake-server --test owner_lifecycle owner_maintenance_` | The two plan-005 tests pass |
| Low-level regressions | `cargo test --locked --bin lspctl frame_write_` | The three new named tests pass after implementation |
| Owner regressions | `cargo test --locked --features fake-server --test owner_lifecycle owner_write_` | The four new named tests pass after implementation |
| Complete tests | `cargo test --locked --all-targets --features fake-server` | Exit 0 |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Lint/typecheck | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Schema compatibility | `python scripts/release/check_schema.py` | Exit 0 |
| Stored-state compatibility | `python scripts/release/check_stored_state.py` | Exit 0 |

## Scope

**Only implementation files allowed:**
- `src/session/owner_runtime.rs` — common bounded-write seam, absolute operation deadlines, fatal transport state, cleanup, and private tests if needed.
- `src/session/json_rpc_transport.rs` — classify pre-write versus write failures; ensure failed/cancelled writes cannot be reused; low-level tests.
- `test_support/fake_lsp_server.rs` — stalled-reader and broken-writer scenarios with explicit readiness gates.
- `tests/owner_lifecycle.rs` — integration regressions and narrow bounded process/fixture helpers.

**Metadata exception:** this plan and row 011 in `docs/plans/README.md`.

**Out of scope:** DocumentStore event ordering (plan 010), new config/schema/error codes, Owner IPC handshake timeouts, replacing process supervision, writer worker/queue architecture, new crates or Tokio features, filesystem transaction timeouts, and changes to successful Query result shapes.

## Git workflow

Keep changes uncommitted unless the operator separately authorizes commits. Optional authorized branch: `advisor/011-bound-language-server-writes`. If commits are requested, use plain imperative subjects, matching `Add Windows installer`. Do not push or open a PR.

## Steps

### 1. Establish the prerequisite and load-bearing red regressions

Run the prerequisite and full test commands. Read the plan-005 timer change; its persistent timer cannot interrupt a write awaited inside a selected branch, which is why this plan is still needed.

Add the following low-level tests in `src/session/json_rpc_transport.rs` (all names prefixed `frame_write_`):

1. `frame_write_timeout_poisons_partial_frame`: write a payload larger than a small duplex capacity while its reader remains alive but idle. A bounded write deadline fails; a second attempted frame fails immediately without emitting a new header.
2. `frame_write_io_error_poisons_partial_frame`: a minimal test-local `AsyncWrite` accepts a prefix then returns an I/O error; a second frame is rejected. Include a flush-error case so successful `write_all` is not mistaken for completed delivery.
3. `frame_write_validation_error_preserves_stream`: reject an oversized or non-object message before any bytes, then send a valid frame successfully. Retain `outbound_limit_writes_nothing` unchanged as a sibling regression.

Add four integration tests in `tests/owner_lifecycle.rs`:

- `owner_write_request_stall_is_bounded`: the fake server initializes, acknowledges a fixture gate, then keeps stdin open without reading it. Send an 8 MiB valid raw request body (below configured message limit, through a temporary params file rather than command-line length limits), with 200 ms request timeout. Assert bounded structured failure with `delivery: uncertain` and `retry: unsafe`, server cleanup, and no later dispatch on the old generation.
- `owner_write_synchronization_stall_retires_generation`: stall reading after initialization and send a large within-limit Document requiring `didOpen`; verify bounded uncertain/unsafe synchronization failure and that the next Query cannot dispatch on the retired generation. This tests transport teardown only, not digest mismatch event ordering.
- `owner_write_shutdown_uses_one_budget`: initialize a fixture then stop it reading stdin. Arrange enough close notifications to exceed pipe capacity (or use a bounded injected writer at the same runtime seam) and assert shutdown completes within one configured shutdown budget plus cleanup/watchdog tolerance, not one full budget per close/request/exit/wait phase.
- `owner_write_io_failure_retires_generation`: the fixture closes its input/breaks the pipe while staying alive long enough to distinguish a write failure from an ordinary server exit. Assert the unusable transport is retired even when no timeout occurs, and queued work cannot dispatch on that generation.

Each lifecycle test must have a readiness marker, an outer 5-second watchdog for its short configured budget, bounded cleanup on assertion failure, and separate evidence that the fake server remains alive at the moment writes stall. Reuse/create only test-local process helpers. If an 8 MiB payload cannot reliably exceed a platform's pipe capacity, the small-duplex test is the deterministic root assertion; do not blindly increase payloads beyond protocol/Document bounds. Confirm actual within-limit configuration before sending.

**Verify:** run each focused command with `-- --list` first: exactly three `frame_write_` and four `owner_write_` new names must appear. The focused tests fail against the current implementation at explicit bounded/poison assertions and terminate via their watchdogs; compiler errors and zero tests are not red evidence. Add minimal test seams first if necessary without fixing behavior.

### 2. Introduce one bounded write seam and explicit unusable state

Keep serialization and outbound-size validation before output. Add an absolute-deadline argument at the low-level/common runtime seam and bound header, body, and flush as one write, not three fresh timeouts. Both traced and untraced runtime writers must call that seam; preserve trace bytes for complete frames without pretending a partial frame was complete.

Once output begins, mark the writer non-reusable until the entire write and flush succeeds. A timeout/cancelled future or `WriteFrame` I/O error must leave it non-reusable; subsequent calls return immediately. One simple state bit set before awaiting I/O and cleared on success is preferable to a new writer task or queue. Local message validation errors before output must not poison the stream.

At the runtime seam, any non-reusable writer failure records a fatal transport state and terminates the supervised process tree using the existing helper. The event loop checks fatal state before the next operation can dispatch, fails active/queued work using existing failure helpers, and leaves the Owner generation. Cover callers that intentionally handle synchronization errors as best effort: catching the `Result` must not clear fatal state or permit the current request to proceed. Do not attempt `shutdown` or `exit` on an already poisoned writer.

**Verify:** the three low-level tests pass; run `cargo test --locked --bin lspctl session::json_rpc_transport::tests` → all transport tests pass, including unchanged raw-null and outbound-limit behavior. `owner_write_io_failure_retires_generation` runs exactly once and passes.

### 3. Propagate existing operation budgets to every sibling write

Use absolute `TokioInstant` deadlines, calculated once per logical phase; do not introduce a new public write-timeout field. Apply this policy explicitly:

| Context | Deadline/budget |
|---|---|
| Initialization request, callbacks, and `initialized` | One deadline from the existing initialization timeout, established before the first initialization write |
| Query pre-dispatch synchronization and request send | One deadline established when `start_dispatch` begins, using its selected request timeout |
| Awaiting Query response and callbacks for that active Query | The remaining same request deadline, never a fresh timeout after sending |
| Cancellation notification/settlement | Existing cancellation-grace deadline; an expired request deadline must not prevent its cancellation frame from being attempted |
| Shutdown closes, `shutdown`, response wait/callbacks, `exit`, and child wait | One existing shutdown deadline established before the first close event; force cleanup when remaining budget is exhausted |
| Standalone refresh/post-commit/file-operation response without a Query deadline | Existing shutdown-duration budget for the whole outbound batch; do not allocate a fresh budget for every file |

Thread deadlines through the existing helper calls (or one tightly scoped runtime operation deadline with guaranteed restoration); avoid an implicit stale deadline that leaks into the next independent Query. Cancellation/active callback work uses the relevant phase deadline, not an already expired earlier phase. List every caller of `write_lsp_message`, `write_lsp_message_traced`, and `write_json_rpc_frame_with_bytes`; ensure synchronization, file operations, server responses including apply-edit callbacks, initialization, cancellation, shutdown, and ordinary requests all route through the common bound.

Retain existing error shapes. A request whose bytes may have left the process is uncertain/unsafe; do not report `not_sent` merely because its complete frame was not flushed. A locally rejected outbound value before any transmission can preserve its existing safe local-validation semantics. Where `ActiveQuery` has not yet been installed, send that pending caller its failure before tearing down the generation. Preserve Mutation/Receipt results already committed before post-commit notification failure; never turn a transport error into an implicit filesystem rollback.

**Verify:** all four `owner_write_` tests and both prerequisite `owner_maintenance_` tests pass. `cargo test --locked --features fake-server --test owner_lifecycle` passes, including graceful close-before-shutdown ordering, FIFO dispatch, callbacks, trace, and force stop. `rg -n 'write_lsp_message|write_json_rpc_frame_with_bytes' src/session` enumerates only the audited call sites; inspect each against the table and record the list in completion notes.

### 4. Verify compatibility and leave an auditable result

Run every full/static/compatibility command in the table. Verify a healthy subsequent invocation may create a fresh Owner generation, but no queued operation on the retired generation reached the server. Keep tests precise about those different outcomes; do not assert that the entire Session identity is permanently unavailable.

**Verify:** full tests, formatting, Clippy, schema/state checks, and `git diff --check` all exit 0. `git status --short` lists only in-scope files/metadata relative to the initial working tree. Rerun both regression filters with their required nonzero counts before recording DONE.

## Test plan

Required new tests are the three low-level and four lifecycle names listed in step 1. The low-level tests establish deterministic partial-header/body/flush behavior, including immediate I/O error and safe pre-write rejection. The lifecycle tests establish deadline policy and fatal generation retirement across request, Document synchronization, and shutdown paths. Plan-005 flood/cancellation tests and all existing lifecycle tests remain mandatory. Exercise initialization and callback call sites through existing tests plus the explicit deadline call-site review; add a focused private test if an uncovered branch does not use the common seam.

## Done criteria

- [x] Three named `frame_write_` and four named `owner_write_` tests are listed and pass.
- [x] Plan-005 maintenance tests and all lifecycle tests pass.
- [x] A failed/abandoned output attempt cannot be followed by any new frame or Query dispatch on that generation.
- [x] Both traced and untraced writes use one bounded seam; every write caller has the policy-table deadline.
- [x] Shutdown uses remaining time from one deadline, not fresh per-stage budgets.
- [x] Possibly transmitted requests remain uncertain/unsafe; pre-write rejection remains distinguishable.
- [x] Full/static/schema/state checks and `git diff --check` exit 0; no out-of-scope modification.
- [x] Row 011 records status and verification; plan 010 may then rely on fatal write teardown.

## Completion evidence

Work for #46 began on `advisor/011-bound-language-server-writes` from clean `ebe38fe` (merged #57). Prerequisite #41 is closed. Shared-file drift from `5268c6a` was reviewed: plans 004/005/007/008/009 account for the changes; plan 005's persistent, first-priority maintenance interval is retained. Baseline full tests passed **129**, and the two `owner_maintenance_` regressions passed.

The uncommitted implementation threads absolute deadlines through initialization, dispatch/synchronization, response callbacks, cancellation, shutdown, and standalone refresh batches. Header/body/flush use one low-level timeout and a cancellation-safe non-reusable bit. All runtime writers route through `write_lsp_frame`, which records fatal transport failure and uses existing process-tree cleanup. The event loop retires that generation and fails queued work; pending dispatch failures are flushed before Owner teardown. Pre-write validation does not poison the stream. Immediate I/O errors retain their source/OS code, and committed Application/Receipt outcomes are not rolled back on notification failure.

### Regression and verification evidence

- Exactly three `frame_write_` names listed. Initial runnable RED failed at `frame write ignored its deadline` and `failed writer accepted another frame`; the validation-preservation sibling already passed.
- All three new low-level tests and all **nine** transport tests passed after implementation, including raw-null and unchanged outbound-limit behavior. Timeout coverage includes partial header/body and abandoned-future reuse; I/O coverage includes flush failure.
- Exactly four `owner_write_` names listed. Initial request/synchronization fixture ordering was not reliable: queued CLI Capabilities preflight could be mistaken for queued Dispatch, allowing a small follower to overtake the large request. A retained temporary-workspace probe isolated this fixture defect.
- Final request/synchronization tests retain real CLI 8 MiB requests/Documents and the 200 ms budget. A first fixture gate queues the CLI Capabilities preflight ahead of a second, distinctly named gate. The second gate holds the Owner until the actual CLI Dispatch is queued, then a generation-bound authenticated IPC follower is admitted behind it. Status checks require the correct active gate method, rejecting stale first-gate snapshots. I/O tests queue two actual Dispatch requests before closing stdin. All tests retain the five-second outer watchdog, live-process lock evidence, and bounded panic cleanup.
- Revised runnable RED temporarily restored original unbounded, reusable header/body/flush behavior at the existing seam (deadline signatures retained). All four tests failed their explicit write watchdog / server-retirement assertions in **5.56s**. Bounded behavior was restored immediately afterward.
- Work initially stopped after two unsuccessful GREEN attempts; the operator explicitly authorized renewed diagnosis. A targeted probe showed the CLI already returning structured `transport_failed` before any frame prefix: debug-build serialization can exhaust the 200 ms budget before output begins. Waiting for a prefix was therefore invalid. The final two-gate fixture does not assume that budget remains after serialization; deterministic partial-header/body/flush behavior is covered at the small-duplex seam.
- The final two-gate fixture was checked against temporarily restored unbounded/reusable writes: all four tests failed explicit CLI-watchdog/server-retirement assertions in **5.57s**, not readiness assertions. The bounded implementation was restored afterward.
- Independent review found and resolved two test races: stale Status gate identity and unbounded prefix reads after scheduler delays. The in-memory transport test uses already-enabled paused Tokio time and bounded prefix reads; no dependency or feature changes were needed. No additional production correctness findings remained.
- Final lists contain exactly **3** low-level and **4** lifecycle names. All **9** transport tests, **2** maintenance tests, **30** lifecycle tests, and **136** full tests pass. Ten consecutive focused lifecycle repetitions passed (**40/40** individual tests).
- Formatting, Clippy `-D warnings`, schema compatibility, stored-state compatibility, and `git diff --check` pass. Only the four allowed implementation files and this plan/index are modified. Verified locally on Linux Rust **1.98.1**; native Windows/macOS and MSRV 1.89 were not run locally.

Local diagnostic logs: `/tmp/46-frame-red.log`, `/tmp/46-owner-revised-red.log`, `/tmp/46-owner-green1.log`, `/tmp/46-owner-green2.log`, `/tmp/46-owner-final-red.log`, `/tmp/46-lifecycle-final.log`, `/tmp/46-full-final.log`. Retained manual-probe workspaces contain only isolated fixture state, including test authentication material; do not publish their contents.

### Outbound caller audit

| Runtime caller | Supplied absolute budget |
| --- | --- |
| `initialize`: initialize request, initialization callback responses, initialized notification | One initialization deadline established before the first write |
| `start_dispatch`: best-effort refresh, explicit Document synchronization, traced request | One selected request deadline, retained by `ActiveQuery` |
| `handle_concurrent_frame`: post-response Document validation | Remaining request deadline |
| Partial-result cancellation and `maintain_active_queries` | Newly established cancellation-grace deadline, not expired request deadline |
| `write_server_response`: invalid/busy/cancelled server requests and routed/apply-edit responses | Active Query/cancellation phase, or enclosing standalone/shutdown batch deadline; rechecks phase after draining frames |
| Apply-edit post-commit synchronization | Enclosing callback's active Query deadline; committed ledger/Receipt retained on failure |
| Owner Diagnostics / RefreshDocuments | One shutdown-duration deadline per standalone batch, including file operations and all synchronization events |
| `send_synchronization_events` / `send_file_operation_notifications` | Explicit enclosing batch deadline forwarded to every notification |
| `graceful_shutdown`: closes, shutdown, callbacks/response wait, exit, child wait | One deadline established before closes; child wait uses `timeout_at`, poisoned writer is not reused |
| `write_lsp_message`, `write_lsp_message_traced`, `write_server_response` | All call `write_lsp_frame`; it is the only production caller of `write_json_rpc_frame_with_bytes` |

Changes remain uncommitted for operator review; no push or PR was made. Plan 010 may rely on fatal write teardown, but its Document synchronization ordering work is not implemented here.

## STOP conditions

Stop on unexplained prerequisite drift, two failed attempts at a gate, or a needed out-of-scope file. Stop if a protocol/schema change or new timeout setting appears necessary. Stop if the selected request timeout is documented to exclude synchronization in a binding contract that conflicts with the policy table; report the conflict rather than silently changing semantics. Stop if process cleanup itself fails on a platform: preserve the evidence and do not invent a second platform supervisor. Do not claim hard real-time guarantees over synchronous filesystem work or an OS process stuck in uninterruptible kernel I/O. If a fixture simply exits instead of staying alive with an unread pipe, it has not reproduced this defect.

## Maintenance notes

Review error paths for both immediate I/O errors and timed-out futures; either can leave a partial frame and poisoned server Document state. New outbound notifications must inherit an absolute operation budget and the same fatal-state guard. Never retry a partial frame on the same stream. Plan 010 depends on this generation-retirement invariant, so later best-effort synchronization changes must not weaken it. Configurable write budgets or a writer task remain deferred until an actual requirement exceeds this shared, bounded seam.
