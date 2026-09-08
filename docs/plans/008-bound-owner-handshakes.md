# Plan 008: Bound unauthenticated Owner connections

> **Executor instructions:** Follow the steps and verify their expected results. Stop rather than widen scope. Update row 008 in `docs/plans/README.md` when finished, unless the dispatching reviewer maintains the index.
>
> **Drift check first:** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs src/session/owner_protocol.rs`
> Compare modified source against the excerpts below; reconcile understood changes from other selected plans and rerun their tests. Stop on unexplained drift.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** security
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

The loopback listener spawns an unlimited number of tasks before authentication. Each task can allocate a 64 MiB frame body and wait indefinitely for missing bytes; the authenticated operation queue limit does not protect this boundary. Bound preauthentication admission and elapsed time without applying a handshake deadline to legitimate long-running Queries.

## Current state

`src/session/owner_runtime.rs:2257–2273`, in `spawn_owner_listener`:

```rust
let Ok((stream, _)) = listener.accept().await else {
    break;
};
// ... clone endpoint/channels ...
tokio::spawn(async move {
    let _ =
        handle_owner_connection(stream, endpoint, requests, controls, draining, status)
            .await;
    connection_closed.send_replace(Instant::now());
});
```

`handle_owner_connection` at `:2286–2310` reads the whole request before checking version, identities, and token:

```rust
let request: AuthenticatedOwnerRequest = read_owner_message(&mut stream).await?;
if request.owner_protocol_version != OWNER_PROTOCOL_VERSION
    || request.session_identity != endpoint.session_identity
    || request.owner_generation != endpoint.owner_generation
    || !constant_time_token_matches(&request.token, &endpoint.token)
{
    // ... existing owner_unavailable failure ...
    write_owner_message(&mut stream, &failure).await?;
    return Ok(());
}
```

`src/session/owner_protocol.rs:8–9,145–159` has a message bound but no time bound:

```rust
pub(crate) const OWNER_QUEUE_LIMIT: usize = 64;
const OWNER_MESSAGE_LIMIT_BYTES: usize = 64 * 1024 * 1024;
// ...
let length = input.read_u32().await? as usize;
if length > OWNER_MESSAGE_LIMIT_BYTES {
    return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "Owner protocol message exceeds its byte limit",
    ));
}
let mut bytes = vec![0; length];
input.read_exact(&mut bytes).await?;
```

The runtime already uses Tokio TCP/channels and `Arc`; Tokio `sync`, `time`, and `net` are installed. Use its semaphore and timeouts, not a new runtime or dependency. `src/session/owner_runtime.rs:2828–2883` has a private `#[cfg(test)] mod tests { use super::*; ... }` for testing private seams. `src/session/owner_protocol.rs:145–163` is generic over `AsyncRead`, so tests can use Tokio duplex streams as well as a loopback listener. The existing async-test pattern is `src/session/json_rpc_transport.rs:370–404`:

```rust
#[tokio::test]
async fn reads_partial_and_consecutive_frames() {
    // ...
    let (mut sender, receiver) = duplex(3);
    let sending = tokio::spawn(async move {
        sender.write_all(first).await.unwrap();
        sender.write_all(second).await.unwrap();
    });
```

**Domain/design:** `CONTEXT.md` distinguishes stable Session identity from Owner generation, “One concrete lifetime of an Owner for a Session identity.” Continue checking both, plus the constant-time token comparison. ADR `docs/adr/0001-use-per-session-background-owners.md:3` specifies “clients connect through authenticated loopback TCP, operations run serially”. ADR `docs/adr/0006-use-guarded-async-lsp-in-one-package.md:3` keeps “the protocol contract and safety limits under `lspctl`'s control.” Do not replace loopback IPC or introduce an operating-system service.

## Commands you will need

Run in the repository root with the installed Rust toolchain (MSRV 1.89); no installation step.

| Purpose | Command | Expected |
|---|---|---|
| Baseline/full tests | `cargo test --locked --all-targets --features fake-server` | Exit 0 |
| New private boundary tests | `cargo test --locked --bin lspctl owner_handshake_` | Four new tests pass after implementation |
| Existing real lifecycle | `cargo test --locked --features fake-server --test owner_lifecycle` | All pass |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Lint/typecheck | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Schema/state compatibility | `python scripts/release/check_schema.py` and `python scripts/release/check_stored_state.py` | Both exit 0 |

## Scope

**Only implementation files allowed:**
- `src/session/owner_runtime.rs` — admission, bounded authentication helper, and private tests.
- `src/session/owner_protocol.rs` — only narrowly required visibility/helper/test changes; preserve framing and limit.

**Metadata exception:** this plan and row 008 in `docs/plans/README.md`.

**Out of scope:** authenticated Query/queue deadlines, reducing the 64 MiB protocol limit, public configuration/schema changes, authentication format/version changes, replacement of constant-time comparison, global process rate limiting, OS-wide networking policy, and public security demonstration scripts. Do not print endpoint tokens or read actual user state in tests.

## Git workflow

Leave changes uncommitted unless the operator separately authorizes commits. Optional authorized branch: `advisor/008-bound-owner-handshakes`. Existing subjects use plain imperatives, for example `Add Windows installer`. Do not push or open a PR.

## Steps

### 1. Characterize admission and lifetime at the private boundary

Run baseline tests. In the runtime's private test module add four named tests:

- `owner_handshake_admission_is_bounded`: a private loopback listener, temporary in-memory endpoint identity, and stalled connections demonstrate that at most four authentication tasks are admitted. Additional sockets close without entering authentication or waiting on a spawned semaphore-acquisition task. Observe a test-local counter/channel at the actual handler boundary, not process-wide memory usage.
- `owner_handshake_deadline_releases_admission`: a partial header and a partial body fail within the handshake deadline and release their permits; after stalled sockets expire, one well-formed authenticated Status request succeeds. Use small payloads; tests must not intentionally allocate hundreds of MiB.
- `owner_handshake_failure_response_is_bounded`: use a bounded duplex writer or small private generic authentication seam to stall an authentication-failure response; the same absolute deadline ends it and releases the permit. Cover invalid protocol/version, Session identity, Owner generation, and token without printing values.
- `owner_handshake_does_not_deadline_authenticated_query`: authenticate, then deliberately hold a queued request response past the handshake interval. It must remain pending, retain its existing queue/Query policy, and succeed when released. All authentication permits must already be available while it waits.

Use Tokio `timeout` for each outer test watchdog and deterministic channels to establish admission/completion. Abort/join only the listeners/tasks created by each test. If a private helper takes a `Duration`/absolute deadline, tests may use 50–100 ms while production uses the fixed policy in step 2; this is not a public configuration setting. Do not add Tokio `test-util` or rely on an unbounded sleep. Tests at new seams may require first exposing an equivalent, behavior-preserving helper.

**Verify:** `cargo test --locked --bin lspctl owner_handshake_ -- --list` lists exactly the four full test names. The focused test command must fail at a bounded admission/deadline assertion before the policy is implemented; compilation failure or zero tests is not a red gate.

### 2. Bound work before spawning and end authentication at one deadline

Use a shared `Arc<tokio::sync::Semaphore>` with a private constant limit of **4 concurrent unauthenticated connections**. Immediately after `accept`, call `try_acquire_owned` before spawning a handler. On exhaustion, drop the accepted stream without spawning a task or trying to write an overload response. Move the permit into the admitted task.

Define a private **5-second absolute handshake deadline** beginning at admission. It covers reading the full framed request, validating all existing authentication fields, and best-effort writing an authentication-failure response. Do not restart the five seconds between header/body/failure write. A timeout or malformed input closes the connection and drops the permit; no public error-code addition is necessary. `read_owner_message`'s existing length bound still applies.

Split only the authentication portion from the existing handler if needed. Once validation succeeds, drop the authentication permit and call the existing authenticated handler with the parsed request. Do not retain the permit during Status output, queue waiting, a running Query, response flushing, or the caller-disconnect watcher. Do not wrap `handle_owner_connection` as a whole in the handshake timeout. Preserve the existing authentication-failure JSON when it can be delivered within budget.

The deliberate ceiling is four frame allocations of at most 64 MiB each, plus bounded task/JSON decoding overhead. Document this as a `ponytail:` ceiling comment near admission; incremental body allocation can be a future improvement if this bounded memory ceiling is too high. Do not claim the cap bounds authenticated work or provides fairness under continuous hostile arrivals.

**Verify:** the four new private tests pass. `cargo test --locked --features fake-server --test owner_lifecycle` passes, including initialization/status and long active Query cases. An authenticated Query is not cancelled at five seconds merely because its connection was once unauthenticated.

### 3. Check cleanup and compatibility

Review every authentication exit for permit release, including parse errors, timeout, invalid identity, failed failure-response writes, and listener shutdown. The permit's ownership should make release automatic; do not manually increment/decrement a second production counter. Preserve existing `connection_closed` notification behavior and avoid retaining per-socket entries in an unbounded map.

**Verify:** full tests, formatting, Clippy, schema and stored-state commands above all exit 0. `git diff --check` exits 0. `git status --short` shows no out-of-scope changes relative to the initial working tree. Rerun the four-test filter and require exactly four tests passed, not merely exit 0.

## Test plan

The four named tests cover cap, incomplete header/body timeouts, failure-output timeout, permit recovery, unchanged authentication checks, and legitimate long Query lifetime. Use private in-memory or loopback fixtures; do not attach to an existing Owner or expose real tokens. Test post-deadline recovery after the initial stalled cohort is gone, not an unproven availability guarantee during an endless arrival flood.

## Done criteria

- [x] Four `owner_handshake_` tests are listed and pass with bounded cleanup.
- [x] No task is spawned merely to wait for preauthentication admission.
- [x] The permit is released before valid authenticated request processing.
- [x] One absolute deadline covers the read and authentication-failure write.
- [x] Lifecycle/full tests, formatting, Clippy, schema/state checks, and `git diff --check` exit 0.
- [x] No new dependency, public setting, protocol field, or out-of-scope modification.
- [x] Row 008 in the index records completion/verification or a blocking reason.

## Completion evidence

- Initial worktree was clean at `c8b6413`. Drift from `5268c6a` was confined to completed plans 004/005 (partial-result chunks and Owner maintenance); authentication and framing were unchanged. Baseline full suite: 120 tests passed, including their regressions.
- Extracted private listener/authentication seams before enabling the policy. Exact four-test discovery passed; the red run failed the admission, incomplete-frame deadline, and blocked-failure-write assertions. The authenticated Query lifetime control passed.
- Admission now uses `try_acquire_owned` before spawning, with four permits and a five-second absolute deadline. Authentication owns the permit, so every return/cancellation releases it before authenticated processing. Existing framing, identity/token checks, queue policy, and connection-close notifications remain intact.
- Independent review found that Tokio can poll a ready request past an expired deadline. A prebuffered expired-request regression reproduced this; explicit checks before reading and after decoding/validation now deny it.
- Final focused discovery lists exactly four tests; all four pass. Coverage includes excess sockets, partial headers/bodies, malformed/oversized frames, listener shutdown, failure-response backpressure and all authentication fields, post-timeout Status recovery, and queued/active Query responses beyond the handshake interval with all permits available.
- `cargo test --locked --features fake-server --test owner_lifecycle`: 21 passed. Final `cargo test --locked --all-targets --features fake-server`: 124 passed.
- Clippy with `-D warnings`, formatting, schema check, stored-state check, and `git diff --check`: all exited 0.
- Implementation changes are confined to `src/session/owner_runtime.rs`; only this plan and index row 008 accompany them. No dependency or public contract changes. Verified on Linux; native macOS/Windows verification remains for CI.

## STOP conditions

Stop on unexplained drift, two failed attempts at a gate, or an out-of-scope requirement. Stop if valid requests cannot be handed off without changing their lifetime/queue semantics or public framing. If a required deployment needs a lower memory ceiling than four 64 MiB messages, report the policy decision rather than silently lowering the existing message limit. Do not claim this solves authenticated denial of service or prevents continuously arriving peers from monopolizing admission.

## Maintenance notes

Review the product of message-size bound and unauthenticated concurrency whenever either constant changes. Recheck that new preauthentication error paths share the absolute deadline and cannot wait while holding a permit indefinitely. Keep admission private and simple; introduce configurable or weighted limits only if a demonstrated deployment requirement justifies their public contract cost.
