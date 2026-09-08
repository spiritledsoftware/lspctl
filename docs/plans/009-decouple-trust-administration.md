# Plan 009: Make Trust revocation and named grants independent of unrelated executable availability

> **Executor instructions:** This plan explicitly chooses the schema-compatible safety fix described in Step 1; fully independent named-status success remains deferred. Do not invent a replacement aggregate digest or silently expand the contract. Follow every verification command. Update this plan's status and its row in `docs/plans/README.md`; record the residual limitation even when this bounded plan is DONE.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/configuration.rs src/configuration/trust.rs tests/owner_lifecycle.rs`
> Also run `git status --short`. Compare changes with the excerpts, including read-only contract assets. Reviewed unrelated changes may be accommodated; changes to Trust identity, aggregate semantics, or state locking require a refreshed plan.

## Status

- **Status:** DONE — bounded safety fix; independent named-status success remains deferred
- **Audit finding:** 9
- **Priority:** P1
- **Effort:** M
- **Risk:** MED
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

Trust commands resolve every current project-controlled executable before choosing a server. An unrelated unavailable executable therefore blocks a named grant and even revocation; the selected executable's removal also prevents revocation. An incomplete declaration reaches an `unwrap` and can panic outside the JSON contract. Revocation should remove authorization and signal matching Owners independently of executable resolution, while named grants should inspect only their selected declaration.

The audit also requested independently usable named status. The current status success schema requires a non-null aggregate digest over **all** current project declarations. That requirement conflicts with producing a named-status success when another declaration cannot be resolved. This plan exposes that decision rather than disguising a partial digest as a complete one.

## Current state

- `src/configuration/trust.rs` owns Trust state, declaration/aggregate digests, command handlers, and Owner invalidation.
- `src/configuration.rs` owns configuration merging, server selection, and executable resolution.
- `tests/owner_lifecycle.rs` provides CLI subprocesses with isolated platform-specific user configuration/state and a known fake executable.
- `assets/contract/catalog.json:4575–4603` defines the output-field requirements; read it and `assets/contract/schemas.json`, but do not modify either without a revised, approved scope.

`grant_trust`, `revoke_trust`, and `trust_status` resolve all declarations first (`src/configuration/trust.rs:156–159,313–316,350–353`):

```rust
let configuration = command_configuration(invocation)?;
let declarations = current_declarations(&configuration)?;
```

`current_declarations` maps every server with nonempty `project_fields` through `current_declaration`, which begins with `resolve_server_executable(server)?` (`:432–454`). Resolution currently assumes a completed server (`src/configuration.rs:451`):

```rust
let executable = server.executable.as_ref().unwrap();
```

That assumption is not guaranteed for Trust administration: project validation allows partial declarations (`src/configuration.rs:1121–1132`) to support merging. A suitable existing structured error already appears in `select_named_server`/`select_configured_server` (`:362–396`):

```rust
code: "server_declaration_incomplete",
stage: "select_server",
delivery: "not_sent",
retry: "after_change",
data: json!({"server": name, "missingFields": ["executable"]}),
```

Reuse that error contract rather than inventing a new code or globally rejecting legitimate partial configuration.

The aggregate is a digest of the complete sorted server→declaration-digest map (`src/configuration/trust.rs:510–522`):

```rust
digest_canonical_value(
    "lspctl-trust-aggregate-v1",
    &Value::Object(
        declarations.iter().map(|(name, declaration)| {
            (name.clone(), Value::String(declaration.digest.clone()))
        }).collect(),
    ),
)
```

The published catalog distinguishes optional change-envelope aggregate from mandatory status aggregate:

```text
assets/contract/catalog.json:4585  result.aggregateDigest: sha256-digest?
assets/contract/catalog.json:4601  result.aggregateDigest: sha256-digest
```

`trust_change_envelope` already accepts `Option<String>` and removes null members (`src/configuration/trust.rs:594–619`). Named grant/revoke can omit unavailable aggregate metadata without a schema change. `trust_status` cannot simply emit null or omit the field. Never hash only resolvable declarations and present that as the all-declarations authorization digest.

ADR `docs/adr/0002-bind-project-trust-to-server-declarations.md:3` says the grant “binds to the declaration digest and resolved executable path” and “revoke or Denial stops matching owners.” Executable bytes are deliberately not hashed. `CONTEXT.md:66–72` distinguishes a **Trust grant** from a **Denial**: revocation removes the record; it does not create a Denial. Preserve those rules and existing lock ordering.

Test conventions:

```rust
// tests/owner_lifecycle.rs:84–96
let output = self.output_with_environment(arguments, environment);
assert!(output.status.success(), "command failed: {}",
    String::from_utf8_lossy(&output.stdout));
assert!(output.stderr.is_empty());
serde_json::from_slice(&output.stdout).unwrap()
```

`Fixture::with_server_arguments` isolates HOME/XDG or APPDATA/LOCALAPPDATA and configures `env!("CARGO_BIN_EXE_lspctl-fake-server")` in user configuration. Reuse subprocess isolation; do not mutate the test process's environment. The inline Trust tests at `src/configuration/trust.rs:847–874` also verify the first-release stored-state fixture and changed-field reporting.

## Scope

**Schema-compatible implementation files allowed:**

- `src/configuration.rs` — defensive incomplete-declaration error at the shared resolver seam, reusing existing validation shape.
- `src/configuration/trust.rs` — selected declaration lookup, revocation ordering/metadata, and inline tests.
- `tests/owner_lifecycle.rs` — isolated CLI Trust regression tests using existing fixtures.

**Metadata exception:** this plan and its row in `docs/plans/README.md`, including the Step 1 decision.

**Out of scope:** schema/catalog changes unless this plan is explicitly revised and approved; persisted Trust format changes, declaration/aggregate digest algorithms, `--all` authorization semantics, executable-byte hashing, process environment overrides, Owner authentication/runtime implementation, unrelated Mutation lock changes, new dependencies. A named-status success-schema change is **not** implicitly authorized by this plan.

## Commands you will need

Run from repository root with existing Rust 1.89+ and locked dependencies. No installation step.

| Purpose | Command | Expected result |
|---|---|---|
| Trust unit tests | `cargo test --locked --bin lspctl configuration::trust::tests::` | All tests pass |
| Discover integration regressions | `cargo test --locked --features fake-server --test owner_lifecycle trust_administration_ -- --list` | Five named tests listed below |
| Run regressions | `cargo test --locked --features fake-server --test owner_lifecycle trust_administration_` | Five tests pass after approved fix |
| Full suite | `cargo test --locked --all-targets --features fake-server` | All targets pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Contract registry | `python scripts/release/check_schema.py` | Exit 0 |
| Stored state | `python scripts/release/check_stored_state.py` | Exit 0 |
| Diff | `git diff --check` | Exit 0 |

The registry check alone does not validate new envelopes. Tests must assert required fields, omitted optional fields, and the known error payloads described below.

## Git workflow

Leave changes uncommitted. Only create `advisor/009-decouple-trust-administration` if authorized. No push or PR. If later asked to commit, use the repository's imperative style, e.g. `Keep Trust revocation independent of executable resolution`.

## Steps

### 1. Verify the contract and the bounded implementation scope

Confirm that status still requires a string aggregate while Trust-change output permits omission:

```sh
python - <<'PY'
import json
from pathlib import Path
schemas = json.loads(Path('assets/contract/schemas.json').read_text())
for key in ('lspctl://schema/v1/success/trust-status', 'lspctl://schema/v1/success/trust-change'):
    assert key in schemas, key
    print(key)
    print(json.dumps(schemas[key], sort_keys=True))
PY
```

**Verify:** exit 0; the printed definitions and their references agree with the required/optional distinction above. If the current schema does not resolve that way, STOP and refresh the plan against the actual contract.

**Chosen boundary: bounded schema-compatible safety fix.** Implement named-grant/revocation isolation and structured incomplete-declaration errors. Named status retains the full-aggregate requirement and returns a structured unavailable/incomplete-declaration failure when any required declaration cannot resolve. Independent named-status success is explicitly deferred; retain that residual in the index instead of claiming audit finding 9 is completely resolved.

**Not authorized by this plan: complete named-status isolation.** That requires a contract decision for representing an unavailable aggregate (or separately exposing a clearly scoped digest), a revised implementation scope, and updated schema/contract tests. If the operator requires it now, STOP here and request that revision; do not invent it during implementation. Executing this plan as written means accepting the bounded scope above, not changing the meaning of the full aggregate.

### 2. Add isolated CLI regressions for the bounded scope

Add exactly these five tests to `tests/owner_lifecycle.rs`:

1. `trust_administration_grant_ignores_unrelated_unavailable_server`: with a valid selected project declaration, obtain its digest while configuration is resolvable; then add a second project declaration whose executable is missing. A named grant using the selected digest must succeed and persist only that grant; no Owner starts. Repeat with an incomplete unrelated declaration. Assert aggregate metadata is omitted when unavailable, never calculated from a subset. Wrong selected digest must still fail without writing a grant.
2. `trust_administration_revoke_ignores_executable_availability`: establish a grant, optionally start its fake-server Owner, then make the selected executable unavailable by changing only temporary configuration to an absent path (do not delete the built fake-server binary). Revoke must remove the record and signal the matching Owner; unrelated missing/incomplete declarations must not prevent it. Include a stored record whose declaration was removed and a repeated revoke. Check `trust list` to verify durable record removal without requiring declaration resolution.
3. `trust_administration_incomplete_declarations_return_json`: invoke Trust status/grant requiring an incomplete declaration and assert `server_declaration_incomplete`, the existing missing-fields payload, documented exit 3, and empty stderr rather than a panic. A declared but missing executable remains the distinct `server_executable_unavailable` failure. None may write a grant or start a server.
4. `trust_administration_preserves_all_grant_digest_and_denials`: with two valid project declarations, unfiltered status supplies the full aggregate; `grant --all` validates it, grants both, and rejects a stale aggregate without partial writes. Existing Denial replacement flags remain required. With any unavailable declaration, `--all` fails closed before state changes.
5. `trust_administration_status_preserves_full_aggregate_contract`: for the bounded option, a named status on fully resolvable configuration returns the same all-declarations aggregate as unfiltered status; when a sibling is unavailable it returns the existing structured error, not partial success, null, or a subset digest. This is explicit characterization of the deferred limitation, not a claim that named-status independence is fixed.

Use temporary project configuration and a known fake executable only. Obtain declared digests from successful status output before introducing unavailable siblings; do not copy fixed digests or write arbitrary production Trust state. Verify `session list --workspace <fixture workspace>` remains empty in administration-only cases; use the existing session-stop cleanup for the deliberate live-Owner case.

**Verify discovery:**

```sh
test "$(cargo test --locked --features fake-server --test owner_lifecycle trust_administration_ -- --list | grep -c '^trust_administration_.*: test$')" -eq 5
```

Expected: exit 0 and all five exact names registered.

**Verify red:** `cargo test --locked --features fake-server --test owner_lifecycle trust_administration_` fails the named-grant/revocation isolation and incomplete-declaration JSON assertions. Existing `--all`/status controls may pass. Compilation errors, missing fixtures, and zero-test results do not count as reproduction.

### 3. Return a structured failure for incomplete executable declarations

Replace the shared `resolve_server_executable` unwrap with a `Result`-returning missing-executable guard. Reuse the existing `server_declaration_incomplete` error shape shown above; a small private constructor reused by the existing selection guards is acceptable if it reduces duplicated error construction, but do not introduce an error abstraction layer.

Do not globally require every merged server to have an executable during configuration loading: partial declarations and invocation launch overrides remain legitimate. Preserve the declared-but-unavailable error and normal executable resolution behavior for completed servers.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle trust_administration_incomplete_declarations_return_json` must run one test and pass; `cargo test --locked --bin lspctl configuration::trust::tests::` also passes.

### 4. Isolate named grants and make revocation resolution-free

In `grant_trust`, choose named versus `--all` handling before resolving declarations. Named grants validate only the selected project-controlled declaration and its exact supplied digest before acquiring the existing lock/persisting the grant. `--all` retains full resolution, the existing digest algorithm, and atomic all-or-nothing state update. Preserve Denial replacement checks. Do not allow a named declaration absent from the project-controlled set to acquire a grant.

For optional metadata, either retain a complete aggregate when full resolution succeeds or omit it. Do not turn a metadata-resolution error into a failed named grant after successful persistence; do not write first and then report authorization failure. Selected record rendering must still use the selected current declaration.

In `revoke_trust`, remove the `current_declarations` dependency entirely from the destructive authorization-removal path. Preserve configuration parsing/canonical Workspace validation, the Workspace Application lock, Trust state update lock, and subsequent `signal_workspace_owners` behavior. Remove the named record regardless of whether its executable is available, its declaration is incomplete, or it is no longer declared. Return an untrusted record with available fields only, omitting unknown `currentDigest` and `aggregateDigest` rather than serializing null. Retain Owner signal successes/failures in the existing envelope; do not roll back revocation just because signalling fails.

Under the approved bounded option, keep full-aggregate `trust_status` semantics. The new resolver guard removes the panic but does not promise successful partial status.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle trust_administration_` runs exactly five tests and passes. `cargo test --locked --bin lspctl configuration::trust::tests::` passes, including stored-state compatibility.

### 5. Run final gates and state the remaining contract limitation

Run every command in the table and the discovery assertion. Review `git status --short` against the initial status and scope. Record test results and the still-deferred named-status behavior in this plan and its index row. If this plan is later replaced by a complete-isolation plan, follow that plan's replacement gates instead of declaring these five tests sufficient.

**Verify:** all applicable commands exit 0; `git diff --check` exits 0; there are no unapproved schema, state-format, digest-algorithm, or out-of-scope changes.

## Test plan

Five integration tests cover both newly fixed boundaries and preserved contract behavior. Use `Fixture` subprocess environment isolation, actual CLI JSON, stored record readback through `trust list`, and `session list`/Owner signal evidence. Unit tests for digest helpers alone would miss the eager-resolution command flow and panic envelope. Never log test environment contents or credentials. Preserve first-release Trust state fixtures without rewriting them.

## Done criteria

- [x] Step 1 confirms the required/optional aggregate distinction; the bounded scope is retained without an assumed contract change.
- [x] Five `trust_administration_*` tests are discovered and pass for the bounded option.
- [x] Revocation removes the persisted record despite missing/incomplete/removed executable declaration and signals the matching Owner.
- [x] A named grant ignores unrelated unavailable declarations while exact selected digest and Denial checks remain enforced.
- [x] Incomplete declaration errors are structured JSON, not panics; no administration-only test starts a language server.
- [x] `--all` digest algorithm, fail-closed behavior, state format, and status aggregate meaning remain unchanged under the bounded option.
- [x] Full tests, Clippy, formatting, contract, stored-state, and diff checks pass.
- [x] Only allowed files changed; plan/index accurately state whether named-status independence remains deferred. Do not mark the entire audit finding resolved if it does.

## Completion evidence

- Started from clean merged `main` at `d5ddf93`. Drift from `5268c6a` in scoped files was limited to reviewed lifecycle regressions for partial results, Owner maintenance, and raw Document scope; configuration/Trust implementation and read-only contract assets were unchanged. Baseline: 124 tests passed.
- Step 1 schema command exited 0: status requires a string `aggregateDigest`; Trust-change metadata may omit it. Retained the bounded schema-compatible decision, without contract or digest changes.
- Discovered exactly five named integration regressions. Before the fix, named grant/revoke failed on an unavailable sibling; incomplete declarations exited 101 instead of structured exit 3. Healthy aggregate/stale-digest/Denial controls ran before the incomplete-sibling cases failed.
- The shared executable resolver now returns the existing incomplete-declaration error, reusing a private constructor with selection guards. Its focused integration test and both Trust unit tests passed before proceeding to grant/revoke isolation.
- Named grants resolve only the selected project-controlled declaration for authorization, then attempt a complete aggregate solely as optional metadata. Missing/user-only selections return the existing `server_selection_failed` contract rather than inventing an aggregate for an absent declaration. Wrong digests and Denial replacement requirements remain enforced.
- Revocation retains configuration/Workspace validation, lock ordering, durable record removal, and Owner signalling, with no executable resolution. It reuses the untrusted-record renderer and omits unknown current/aggregate digests. Tests read durable state through `trust list`, including repeated revocation and removed declarations, and verify live Owner generations were signalled.
- Independent review identified a panic-cleanup gap in the live-Owner regression. The existing bounded cleanup guard now accepts a server name and is installed before startup; prior test callers retain `fake`.
- Final five integration regressions and both Trust unit tests pass. Full locked all-target fake-server suite: 129 passed, including 26 lifecycle tests. Clippy (`-D warnings`), formatting, schema registry, stored-state, and diff checks exited 0 on Linux. Native macOS/Windows verification remains for CI.
- Changes are limited to the three allowed implementation/test files and this plan/index metadata. No new dependencies, state-format changes, runtime changes, or contract-asset edits. Changes remain uncommitted.
- **Residual limitation:** named status still requires the complete all-declarations aggregate and fails structurally if a sibling is missing or incomplete. Independent named-status success remains deferred; audit finding 9 is not completely resolved.

## STOP conditions

Stop if the requested implementation includes successful independent named status despite unavailable aggregate inputs; that behavior is deliberately outside this bounded plan and requires an approved schema/semantic revision. Never emit a made-up or subset digest under an all-declarations field, make revocation conditional on executable resolution, change digest inputs, hash executable bytes, or replace revocation with Denial. Stop on unexplained drift, unavailable isolated user-state fixtures, scope expansion, or the same verification failure after two reasonable correction attempts.

## Maintenance notes

Review authorization removal before best-effort metadata rendering and signalling. Optional display metadata must not become a new precondition for a durable safety action. Full named-status independence remains a contract design choice; do not let a later cleanup silently implement it by dropping unresolved declarations from the aggregate.
