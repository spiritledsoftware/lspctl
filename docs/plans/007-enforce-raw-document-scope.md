# Plan 007: Require explicit Workspace and server selection for outside-Workspace raw Documents

> **Executor instructions:** Execute only after authorization. Follow every gate, including test discovery before filtered test runs. Stop on the conditions below; do not widen this into a Workspace redesign. When finished, update this plan's status and its row in `docs/plans/README.md`.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/session.rs tests/owner_lifecycle.rs`
> Run `git status --short` too. Compare changed symbols with the excerpts; reviewed unrelated/prerequisite changes are acceptable, but unexplained changes to invocation selection or dispatch need a refreshed plan. Preserve existing work.

## Status

- **Status:** DONE
- **Audit finding:** 7
- **Priority:** P1
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** security
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

The common Document scope guard permits outside-Workspace reads only when both the Workspace and server were explicitly selected. The raw dispatcher currently passes “Workspace explicitly selected” into the parameter meaning “server explicitly selected.” Thus `--workspace` alone can synchronize an external file through an inferred/default server. Preserve the existing scope contract by passing the right invocation fact, not by adding a second policy.

## Current state

- `src/session.rs` constructs `OwnerQueryDispatcher` and checks raw `--sync-file` paths before reading and dispatching them.
- `src/workspace.rs` implements the common canonical-path scope guard; **read it, do not modify it**.
- `tests/owner_lifecycle.rs` provides an isolated cross-platform fake-server fixture and CLI subprocess assertions.
- `test_support/fake_lsp_server.rs:148–193,225–230` already supports standard initialization and `textDocument/hover` returning null. No new fake-server scenario is needed.

The ordinary `--file` path gets the predicate right (`src/session.rs:140–142`):

```rust
validate_document_scope(&workspace, &path, invocation.has_option("--server"), false)?;
```

The dispatcher instead stores and passes the wrong fact (`:270`, `:349`, `:362`):

```rust
explicit_workspace: invocation.has_option("--workspace"),
// ...
validate_document_scope(self.workspace, path, self.explicit_workspace, false)
    .map_err(DispatchFailure::from)?;
```

The existing policy is explicit (`src/workspace.rs:396–416`):

```rust
pub(crate) fn validate_document_scope(
    workspace: &Workspace,
    path: &Path,
    server_explicitly_selected: bool,
    mutation: bool,
) -> Result<PathBuf, ContractFailure> {
    let canonical = dunce::canonicalize(path)
        .map_err(|_| workspace_failure("A target path cannot be resolved.", &[path.into()]))?;
    if canonical.starts_with(&workspace.root)
        || (!mutation && workspace.explicitly_selected && server_explicitly_selected)
    {
        Ok(canonical)
```

The denial message is `An outside-Workspace Document requires explicit Workspace and server selection.` Preserve its structured failure constructor rather than creating a new error code.

`CONTEXT.md:58–64` defines **Workspace** as “The filesystem tree presented to a language server as the context for queries and mutations,” and **Document** as a filesystem-backed file whose snapshot has been presented to a language server. ADR `docs/adr/0003-synchronize-filesystem-snapshots-without-a-watcher.md:3` states: “The filesystem is authoritative” and snapshots are replaced with `didClose`/`didOpen`, without a watcher. This fix changes scope admission only, not synchronization semantics.

Test convention (`tests/owner_lifecycle.rs:84–96,116–126`):

```rust
let output = self.output_with_environment(arguments, environment);
assert!(output.status.success(), "command failed: {}",
    String::from_utf8_lossy(&output.stdout));
assert!(output.stderr.is_empty());
serde_json::from_slice(&output.stdout).unwrap()
```

`Fixture::with_server_arguments` writes a **user-local** fake server plus `default_server = "fake"` and `.rs` routing in temporary configuration. Its environment isolates Linux XDG, macOS HOME, and Windows APPDATA/LOCALAPPDATA. Reuse that setup; never grant Trust to a repository-controlled executable during this test.

## Scope

**Only implementation files allowed:**

- `src/session.rs` — store and pass the explicit server-selection predicate.
- `tests/owner_lifecycle.rs` — two regression tests and a minimal test-only command helper if needed.

**Metadata exception:** this file and its row in `docs/plans/README.md`.

**Out of scope:** `src/workspace.rs`, fake-server implementation, schemas, Trust grants, routing redesign, raw parameter URI rewriting, moving Owner startup, Mutation admission, dependencies, or any blanket ban on external read-only Queries. Explicitly selecting both Workspace and server must remain sufficient for the read-only case.

## Commands you will need

Run from the repository root with existing Rust 1.89+ and locked dependencies. No installation step is needed.

| Purpose | Command | Expected result |
|---|---|---|
| Discover regressions | `cargo test --locked --features fake-server --test owner_lifecycle raw_document_scope_ -- --list` | Exactly two named tests listed below |
| Run regressions | `cargo test --locked --features fake-server --test owner_lifecycle raw_document_scope_` | Two tests pass after the fix |
| Lifecycle suite | `cargo test --locked --features fake-server --test owner_lifecycle` | All tests pass |
| Full suite | `cargo test --locked --all-targets --features fake-server` | All targets pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Contract | `python scripts/release/check_schema.py` | Exit 0 |
| Diff | `git diff --check` | Exit 0 |

## Git workflow

Leave changes uncommitted. Only create `advisor/007-enforce-raw-document-scope` if authorized to create a branch. No push or PR. If subsequently asked to commit, match short imperative repository messages, e.g. `Enforce explicit server selection for raw Documents`.

## Steps

### 1. Add a CLI selection matrix before changing the predicate

Add `raw_document_scope_requires_explicit_workspace_and_server` in `tests/owner_lifecycle.rs`.

Create `.rs` files both inside the fixture Workspace and beside it under the fixture's temporary root. Put the subprocess current directory **inside the fixture Workspace** when `--workspace` is absent, rather than relying on the test runner's repository directory. Use `Command::current_dir`, not process-global `set_current_dir`; reuse the fixture's isolated environment. Supply the outside file as an absolute native path.

Exercise all four combinations of explicit `--workspace` and explicit `--server fake` for:

- Raw Query: `raw --method fixture/scope --sync-file <outside.rs>`.
- Ordinary Query control: `hover --file <outside.rs> --line 0 --column 0`.

Both paths must deny the first three combinations through the common scope failure, produce parseable JSON with empty stderr, and succeed only when both flags are present. The standard fake-server fallback serves the raw fixture method; hover's null response is a valid success. The default server in user configuration ensures absent `--server` is inferred rather than failing selection for an unrelated reason. Stop each test-created Owner using the existing fixture helper, including cases denied after Owner startup.

Add `raw_document_scope_allows_in_workspace_documents`: verify the raw and ordinary paths accept an inside `.rs` file with neither explicit selection flag. This protects normal inferred/default routing.

**Verify discovery:**

```sh
test "$(cargo test --locked --features fake-server --test owner_lifecycle raw_document_scope_ -- --list | grep -c '^raw_document_scope_.*: test$')" -eq 2
```

Expected: exit 0 and exactly the two named tests registered. Compilation failures or zero selected tests are not acceptable.

**Verify red:** `cargo test --locked --features fake-server --test owner_lifecycle raw_document_scope_` must fail the raw outside-file case with explicit Workspace but inferred server. Inside-file and ordinary-path controls should pass. Do not accept selection/setup failures as reproduction.

### 2. Carry the correct predicate through the dispatcher

Rename the dispatcher's `explicit_workspace` field to an unambiguous server-selection name, such as `server_explicitly_selected`. Initialize it with `invocation.has_option("--server")` and pass it unchanged as the third argument to `validate_document_scope` inside `OwnerQueryDispatcher::dispatch`.

The Workspace already stores its own explicit-selection fact. Do not add duplicate booleans or loosen canonical containment. Keep raw params byte/JSON semantics, error mapping, Document refresh ordering, and the ordinary `--file` caller unchanged.

**Verify:** `cargo test --locked --features fake-server --test owner_lifecycle raw_document_scope_` runs two tests and exits 0. Then `cargo test --locked --features fake-server --test owner_lifecycle` exits 0.

### 3. Run final gates and record completion

Run full tests, Clippy, formatting, contract validation, and diff hygiene from the command table. Run the discovery assertion again after any test rename. Compare `git status --short` against the initial status and the scope, then update this plan and its index row.

**Verify:** all table commands exit 0; `git status --short` shows only allowed changes plus recorded pre-existing work.

## Test plan

Two table-driven integration tests are sufficient. They cover all explicit-selection combinations for both raw and ordinary external Documents, an internal-Document control, structured failure output, and cleanup. They must exercise invocation-to-dispatch behavior; testing only `validate_document_scope` would miss the actual wrong argument. Follow `Fixture` and existing raw synchronization coverage at `tests/owner_lifecycle.rs:236–274`; do not mutate global environment or install a real language server.

## Done criteria

- [x] The discovery assertion exits 0 with exactly two `raw_document_scope_*` tests.
- [x] Both tests pass, including the previously accepted external raw Query with inferred server now being denied.
- [x] Ordinary external reads remain allowed when both flags are explicit; inside-Workspace inferred reads still work.
- [x] Full test suite, Clippy, formatting, schema check, and `git diff --check` exit 0.
- [x] No out-of-scope implementation files changed and no test-created Owner remains running.
- [x] This plan's status and index row are updated.

## Completion evidence

- Initial worktree was clean. Drift from `5268c6a` was confined to lifecycle tests and helpers from completed plans 004 and 005; invocation selection and dispatch were unchanged.
- Discovery assertion exited 0 with exactly the two specified tests. Before the fix, the outside raw Query with explicit Workspace and inferred server returned exit 0 instead of 3; the ordinary selection matrix and inside-Workspace controls passed.
- After the fix, both regression tests passed and the lifecycle suite passed all 21 tests. Each matrix case stops its Owner, verifies an empty Session list, and retains panic cleanup.
- `cargo test --locked --all-targets --features fake-server`: 120 tests passed.
- `cargo clippy --locked --all-targets --features fake-server -- -D warnings`, `cargo fmt --all -- --check`, `python scripts/release/check_schema.py`, and `git diff --check`: all exited 0.
- Only the two permitted implementation files and this plan/index metadata changed. Changes remain uncommitted. Verification ran on Linux; native macOS/Windows verification remains for CI.

## STOP conditions

Stop if no-`--workspace` fixtures do not select the intended temporary Workspace, if the regression fails during language/server selection rather than scope checking, or if the ordinary `--file` path has changed its contract. Stop before altering the shared policy, Trust, startup order, or schemas. Also stop on unexplained drift, out-of-scope requirements, or the same verification failure after two reasonable fixes.

## Maintenance notes

Selection predicates describe facts about an invocation, not whether a value is eventually available. Review future raw dispatcher fields for the same confusion. Keep the matrix at the CLI boundary; the common scope helper alone cannot protect against a caller passing the wrong boolean.
