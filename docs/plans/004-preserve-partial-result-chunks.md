# Plan 004: Preserve partial-result chunks across the Owner boundary

> **Executor instructions:** Read this plan completely, follow the steps, and run every verification gate. Stop under the conditions below instead of broadening scope. When finished, update row 004 in `docs/plans/README.md`, unless the dispatching reviewer owns the index.
>
> **Drift check first:** `git diff --stat 5268c6a..HEAD -- src/session/owner_runtime.rs src/query.rs test_support/fake_lsp_server.rs tests/owner_lifecycle.rs`
> Compare changed files with the excerpts below. Other selected plans share the runtime and fixture; proceed only if those changes are understood, their tests pass, and the partial-result invariant described here still applies. Stop on unrelated or unexplained drift.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

An Owner currently flattens array-valued LSP progress chunks before returning them to a short-lived CLI process. The named Query merger expects complete chunks, so otherwise successful references, workspace-symbol, and code-action results fail validation. Preserve wire chunks internally without changing the public flattened partial results attached to failures.

## Current state

- `src/session/owner_runtime.rs` owns active requests, progress collection, Owner responses, and failure evidence.
- `src/query.rs` validates and merges named Query results.
- `test_support/fake_lsp_server.rs` is an independently framed server compiled only with `fake-server`.
- `tests/owner_lifecycle.rs` exercises separate CLI processes against a real background Owner with isolated configuration/state.

At `src/session/owner_runtime.rs:1023–1026`:

```rust
match value {
    Value::Array(items) => query.partial_items.extend(items),
    value => query.partial_items.push(value),
}
```

The success envelope at `:967` emits `"partialResults": query.partial_items`. However, `src/query.rs:1339–1348` requires each partial to be an array:

```rust
for (index, partial) in partials.into_iter().enumerate() {
    let Value::Array(mut partial) = partial else {
        return Err(invalid_result(
            command.method().unwrap(),
            "an array partial result",
            &format!("$partial[{index}]"),
        ));
    };
    items.append(&mut partial);
}
```

`attach_dispatch_evidence` at `src/session/owner_runtime.rs:2637–2641` exports a different, public representation:

```rust
if !query.partial_items.is_empty() {
    failure["partialResult"] = json!({"items": query.partial_items.clone(), "complete": false});
}
```

`partial_result_too_large` also uses the flattened array and its length (`:1031–1042`). Preserve its item-count meaning rather than reporting chunk count. The existing failure integration test is the contract exemplar, `tests/owner_lifecycle.rs:560–583`:

```rust
assert_eq!(failure["error"]["code"], "server_error");
assert_eq!(failure["partialResult"]["complete"], false);
assert_eq!(
    failure["partialResult"]["items"][0]["name"],
    "partial-symbol"
);
```

Match the fixture's `TempDir` and per-platform environment isolation, explicit `fixture.stop(workspace)`, `serde_json::Value` assertions, and no-output-on-stderr checks. Do not use an actual user's configuration or server. `src/configuration.rs:1030–1033` already exposes `[protocol].max_partial_result_bytes`; no new option is needed for a small-limit test.

**Domain/design constraints:** `CONTEXT.md:11–13` defines Query as “An agent's request for semantic code intelligence from a language server.” Its Owner is “A long-lived process responsible for one initialized language-server session and its Queries.” ADR `docs/adr/0001-use-per-session-background-owners.md:3` says “clients connect through authenticated loopback TCP, operations run serially”. ADR `docs/adr/0006-use-guarded-async-lsp-in-one-package.md:3` assigns the session module “bounded JSON-RPC framing, serialization, request identifiers, response correlation, and cancellation”. Keep that ownership and the one-package/custom-transport design.

## Commands you will need

Run from the repository root, with the existing Rust toolchain (MSRV 1.89). Do not install dependencies or change manifests.

| Purpose | Command | Expected |
|---|---|---|
| Baseline/full tests | `cargo test --locked --all-targets --features fake-server` | Exit 0; all tests pass |
| Focused tests | `cargo test --locked --features fake-server --test owner_lifecycle owner_partial_results_` | Exactly the three new tests run and pass after implementation |
| Existing failure contract | `cargo test --locked --features fake-server --test owner_lifecycle query_failure_preserves_server_error_partial_results_context_and_trace -- --exact` | One test passes |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Lint/typecheck | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Schema assets | `python scripts/release/check_schema.py` | Exit 0 |

## Scope

**Only implementation files allowed:**
- `src/session/owner_runtime.rs` — chunk storage and failure conversion.
- `src/query.rs` — focused merger regression tests; keep valid-result validation strict.
- `test_support/fake_lsp_server.rs` — deterministic partial-result fixture responses.
- `tests/owner_lifecycle.rs` — boundary tests and narrowly necessary fixture configuration helper.

**Metadata exception:** this plan and row 004 in `docs/plans/README.md` may be updated with status/results.

**Out of scope:** diagnostic-cache persistence (plan 013), Owner protocol/schema version changes, request scheduling, public envelope shapes, new dependencies, raw-request normalization, arbitrary server installation, and unrelated fixture refactoring.

## Git workflow

Keep source changes uncommitted unless the operator separately authorizes commits. If a branch is authorized, use `advisor/004-preserve-partial-result-chunks`; existing commit subjects use plain imperatives (for example, `Add Windows installer`). Do not push or open a PR.

## Steps

### 1. Verify baseline and add a failing boundary regression

Run the baseline first. Add three tests in `tests/owner_lifecycle.rs`:

1. `owner_partial_results_chunks_merge_success`: fake `workspace/symbol` returns two nonempty array chunks then a final array. Use valid `SymbolInformation` objects with names, kind, and file URI/range locations inside the temporary Workspace. The named `workspace-symbols` CLI must return all items in wire-chunk-then-final order, once each, with exit 0.
2. `owner_partial_results_chunk_failure_remains_flat`: two chunks then a server error; assert every public `partialResult.items` member is a symbol object, not a nested chunk, and `complete` remains false. Retain method/context/trace checks.
3. `owner_partial_results_limit_keeps_flat_count`: use a small existing `[protocol].max_partial_result_bytes` fixture setting and a multi-item chunk that exceeds it. Make the fixture process cancellation and settle the request. Assert `partial_result_too_large`, flattened retained items, and item count equal to the number of flattened items rather than chunk count. Account bytes from serialized chunk payloads, not a guessed character count.

Use distinct fixture query strings in the existing `workspace/symbol` branch; do not change the standard scenario globally. Stop the fixture Owner on success and assertion failure using a bounded test-local cleanup guard.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle owner_partial_results_ -- --list` must list exactly those three names. Then run the focused command from the table: the success test must fail because valid array progress became individual objects; the failure-preservation tests may already pass. Compilation errors or zero matched tests are not an acceptable red gate.

### 2. Store chunks once and flatten only at the failure boundary

In `ActiveQuery`, rename the collection to `partial_chunks` and push the whole progress `Value`; success `partialResults` uses this collection unchanged. Retain the existing cumulative serialized-byte accounting, token correlation, chunk ordering, and cancellation policy. Do not keep both a fully cloned flattened collection and a chunk collection.

Use one small private conversion at the failure boundary: flatten an array chunk one level, preserve a non-array chunk as one item, and preserve order. Reuse it in `attach_dispatch_evidence` and the immediate partial-limit failure. Calculate `partialItemCount` from the flattened item count. Do not recursively flatten arrays within result objects, flatten diagnostic objects' `items`, or relax `merge_partial_results` to accept arbitrary scalar result shapes.

**Verify:** run the focused three-test command and the existing failure-contract command: three new tests and the existing test all pass. Run `cargo test --locked --bin lspctl query::tests` → all existing Query tests pass, including partial-merger tests.

### 3. Verify all consumers and close the plan

Search every `partial_chunks`, `partialResults`, and `partialResult` use in the scoped runtime/query files. Confirm successful Owner responses contain chunks while all public failure paths (server error, timeout/cancel, post-response stale checks, limit breach, and Owner failure) use the single conversion. Existing object-shaped document/workspace diagnostic chunks must remain objects. Add a focused unit assertion if this is not already covered by merger tests; do not implement plan 013 here.

**Verify:** `cargo test --locked --all-targets --features fake-server`, `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --features fake-server -- -D warnings`, and `python scripts/release/check_schema.py` all exit 0. `git diff --check` exits 0; `git status --short` lists only the scope and metadata exception relative to the recorded initial working tree.

## Test plan

The three named integration tests above are required. Use the existing failure test rather than a mock dispatcher as the structural pattern: this defect occurs specifically across fake server → Owner → short-lived CLI. Existing array-merger and object-diagnostic tests must continue passing. A test-list check is mandatory before accepting any filtered test command as evidence.

## Done criteria

- [x] All three named boundary tests are present in `--list` output and pass.
- [x] Existing public failure shape/context/trace test passes.
- [x] Full tests, formatting, Clippy, schema check, and `git diff --check` exit 0.
- [x] No public schema or Owner protocol version changed.
- [x] `git status --short` contains no new out-of-scope modification.
- [x] Row 004 in the index records DONE and verification evidence, or BLOCKED with its cause.

## Verification results

Implemented for #40 from `a35fd8051cdaa462e3d3d4429829570edb2ff149` in the operator's current checkout (already detached HEAD); the operator explicitly authorized a commit. No branch was switched or created. The working tree was initially clean. The required drift check against `5268c6a` showed no changes in the four scoped implementation files.

| Gate | Result |
| --- | --- |
| Baseline `cargo test --locked --all-targets --features fake-server` | 108 passed |
| `cargo test --locked --features fake-server --test owner_lifecycle owner_partial_results_ -- --list` | Exactly the three required test names |
| Focused red gate | Success regression failed with `invalid_server_result`, expected `an array partial result` at `$partial[0]`; both failure regressions passed |
| Focused green gate | All three passed after preserving chunks |
| Existing failure contract, exact filter | 1 passed |
| `cargo test --locked --bin lspctl query::tests` | 16 passed, including array and object-shaped diagnostic mergers |
| Final `cargo test --locked --all-targets --features fake-server` | 111 passed |
| `cargo fmt --all -- --check` | Exit 0 |
| `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| `python scripts/release/check_schema.py` | Exit 0 |
| `git diff --check` | Exit 0 |
| Independent `/code-review` against the starting commit | Standards: 0 findings; Spec: 0 findings. Corrected the metadata description of the already-detached checkout. |

The consumer audit confirmed success `partialResults` retains complete chunks. Server-error, settled cancellation/timeout, post-response stale-check, and Owner-failure paths use `attach_dispatch_evidence`; it and the immediate limit failure reuse the same one-level conversion. Empty flattened results remain omitted, and non-array chunks remain whole objects. Existing Query merger tests already cover document/workspace diagnostic objects, so `src/query.rs` needed no change. The limit regression checks two retained symbol objects, serialized chunk bytes, and the fixture's cancellation response. New boundary tests stop the isolated Owner explicitly on success and use bounded panic cleanup on assertion failure.

Only the three scoped implementation files and the two permitted metadata files changed. No schema, protocol, manifest, dependency, scheduling, or diagnostic-persistence changes were needed. Verification ran on Linux; native macOS/Windows verification remains with CI.

## STOP conditions

Stop and report if the excerpt mismatch is unexplained, a verification fails twice after a reasonable fix attempt, or a necessary file is outside scope. Stop if the public failure schema requires chunk arrays rather than the existing flat-item contract, if preserving chunks requires an Owner protocol migration, or if a server fixture cannot produce standards-valid symbols without changing unrelated capability semantics. Do not suppress validation errors to turn the red test green.

## Maintenance notes

Review representation changes at both success and failure boundaries, particularly item-count versus chunk-count accounting. Future Query wrappers must receive original progress payloads and implement their method-specific merger; they must not make the transport infer result shapes. Plan 013 consumes these preserved chunks for diagnostic persistence and should execute after this plan.
