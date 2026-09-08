# Plan 012: Render Preview text from the same ordered state used for staging

> **Executor instructions:** Follow the steps and verification gates in order. Fix the shared ordered-text semantics, not just the string renderer. Leave changes uncommitted unless separately authorized, and update plan 012 in `docs/plans/README.md` when complete unless the reviewer owns the index.
>
> **Drift check, run first:** `git diff --stat 5268c6a..HEAD -- src/mutation.rs src/mutation/application.rs src/mutation/planner.rs docs/plans/012-render-ordered-preview-diffs.md docs/plans/README.md`. Earlier plans may have changed manifest or staging safety. Read those diffs and map the excerpts to the corresponding live functions; preserve their invariants and tests. STOP on unexplained semantic drift.

## Status

- **Status:** DONE — six regressions and all acceptance gates pass
- **Priority:** P2
- **Effort:** M
- **Risk:** MED
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05
- **Audit finding:** 12

## Why this matters

A Preview is the Agent's authorization evidence. Its renderer currently rereads the physical path separately for every text operation, although canonical byte offsets refer to the virtual text produced by earlier operations. Create→edit can show no diff, rename→edit has missing-path problems, and repeated edits can show incorrect hunks. Projection must agree with actual staged output without writing to the Workspace merely to display it.

## Current state

- `src/mutation.rs` creates/loads immutable Previews and computes presentation, including `preview_diff` and `contextual_text_diff`.
- `src/mutation/application.rs` already evaluates canonical operations in order while staging text: `stage_text_outputs`, `virtual_text`, `resolve_physical_path`, and `apply_canonical_text_edits`.
- `src/mutation/planner.rs` parses ordered WorkspaceEdit operations into canonical byte edits and keeps `VirtualWorkspace`/`ResourceState`.

`src/mutation.rs:1052–1070` ignores earlier virtual operations:

```rust
for operation in &stored.preview.plan.operations {
    match operation {
        CanonicalOperation::Text { path, edits, .. } => {
            let bytes = fs::read(path).ok()?;
            let old = std::str::from_utf8(&bytes).ok()?;
            let mut new = String::new();
            let mut cursor = 0;
            for edit in edits {
                let start = usize::try_from(edit.start_byte).ok()?;
                let end = usize::try_from(edit.end_byte).ok()?;
                new.push_str(old.get(cursor..start)?);
                new.push_str(&edit.new_text);
                cursor = end;
            }
            new.push_str(old.get(cursor..)?);
            output.push_str(&contextual_text_diff(path, old, &new));
        }
```

By contrast, staging starts with shared virtual text, missing-path markers, and rename aliases (`src/mutation/application.rs:1186–1206`):

```rust
let mut texts = BTreeMap::<PathBuf, Vec<u8>>::new();
let mut unavailable = Vec::<PathBuf>::new();
let mut aliases = Vec::<(PathBuf, PathBuf)>::new();
let mut staged_bytes = 0_u64;
for operation in operations {
    match operation {
        CanonicalOperation::Text { path, before_digest, after_digest, edits, .. } => {
            let before = virtual_text(path, &texts, &unavailable, &aliases)
```

It validates before/after digests, enforces staging limits, then stores each result (`:1215–1275`). Its resource branches set creates to empty text, relocate cached text on rename, and mark deletions unavailable (`:1278–1311`). Reuse this interpreter instead of writing a third one for presentation.

There is one related planning prerequisite for the promised rename→edit case: a moved `ResourceState` may have no text loaded. `src/mutation/planner.rs:1255–1261` then reads from the **virtual destination path**, which need not exist yet:

```rust
if load_text
    && workspace.entries[path].text.is_none()
    && workspace.entries[path].manifest.exists
    && workspace.entries[path].manifest.resource_kind == ResourceKind::File
{
    let bytes = read_text_file(path, self.preview_limits.max_document_text_bytes, index)?;
    workspace.entries.get_mut(path).unwrap().text = Some(bytes);
}
```

Fix this narrowly with in-memory original text-source bookkeeping; do not prewarm the test with an unrelated earlier edit to hide the failure.

### Design constraints and exemplar

`CONTEXT.md:33–35`: Preview is “An identified, immutable representation of one Mutation that an Agent can inspect before authorizing it.” `docs/adr/0004-apply-exact-previews-with-recoverable-transactions.md:3` requires the exact immutable canonical Preview; no rebase, force, subset, or edit-at-apply behavior is authorized. `docs/adr/0007-use-capability-based-workspace-filesystem-access.md:3` keeps no-follow Workspace access at the Mutation seam. Rendering must remain non-mutating and fail closed on stale or unresolvable inputs.

Keep existing contextual unified formatting (`src/mutation.rs:1091–1097`):

```rust
TextDiff::from_lines(old, new)
    .unified_diff()
    .context_radius(3)
    .header(&path, &path)
    .to_string()
```

Use the application test exemplar `text_output_is_complete_private_staging_before_commit` (`src/mutation/application.rs:2451–2522`):

```rust
stage_transaction(&transaction, &transaction.operations, &mutation_limits).unwrap();
assert_eq!(
    fs::read(staged_text_path(&artifact_directory, 0)).unwrap(),
    b"longer\n"
);
assert_eq!(fs::read(&file).unwrap(), b"old\n");
cleanup_transaction_artifacts(&transaction).unwrap();
```

Construct real canonical plans with `WorkspaceEditPlanner` and `create_preview_record`; use `TempDir`/`MutationStateStore::open_at` as in the existing application tests. New presentation tests may live in a local `#[cfg(test)] mod tests` at the end of `src/mutation.rs`. No live language server is required.

## Commands you will need

Run from the root using existing Rust 1.89+ tooling and dependencies.

| Purpose | Command | Expected result |
|---|---|---|
| List focused tests | `cargo test --locked --bin lspctl ordered_preview -- --list` | All named regressions listed |
| Focused tests | `cargo test --locked --bin lspctl ordered_preview` | After fix, nonzero count and all pass |
| Mutation tests | `cargo test --locked --bin lspctl mutation::` | All pass |
| Full tests | `cargo test --locked --all-targets --features fake-server` | All pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Format | `cargo fmt --all -- --check` | Exit 0 |
| Schemas | `python scripts/release/check_schema.py` | Exit 0 |
| Immutable stored fixtures | `python scripts/release/check_stored_state.py` | Exit 0 |

## Scope

**Implementation files allowed:** `src/mutation.rs`, `src/mutation/application.rs`, `src/mutation/planner.rs`.

**Metadata exception:** this plan's verification/status notes and its row in `docs/plans/README.md`.

**Out of scope:** public Preview/schema changes, new persistent text snapshots, stored-state version changes, immutable fixtures, a generic virtual-filesystem framework, Owner/LSP changes, new diff libraries/dependencies, applying temporary edits to generate a diff, stale-Preview rebase, and unrelated resource-operation optimizations.

## Git workflow

Use the operator-selected branch; only create `advisor/012-ordered-preview-diffs` if separately asked. Leave changes uncommitted. If a commit is authorized, use the repository's imperative style, e.g. `Render Preview diffs from ordered text state`. Do not push or open a PR.

## Steps

### Step 1: Add ordered-sequence regressions with actual canonical plans

Add these tests in `src/mutation.rs`'s test module:

- `ordered_preview_create_then_edit`: create a nonexistent file, then insert text. Require a `create` notice plus the inserted text in the diff, and no file on disk after rendering.
- `ordered_preview_repeated_text_edits`: first lengthen a line; then edit a position that exists only in the longer intermediate line. Require the second hunk to use that intermediate text rather than the original disk contents.
- `ordered_preview_rename_then_edit`: rename a file, then edit its destination without any earlier text edit. Require successful planning, a rename notice, the correct hunk at the destination path, and unchanged physical source/no physical destination before Application.

Check all expected exact inserted/deleted lines, not just `diff.is_some()`. Use a Unicode/CRLF case so canonical byte offsets are not accidentally reinterpreted as character indices.

**Verify:** `cargo test --locked --bin lspctl ordered_preview -- --list` lists all three names. The focused run fails on renderer assertions or the documented rename-planning missing-path bug, not invalid test WorkspaceEdits or compilation. If all pass unchanged, STOP and reassess.

### Step 2: Preserve lazy physical text sources through planner renames

Add only in-memory source bookkeeping to `ResourceState` (or an equivalent small existing virtual-state field): physical inspection remembers where unloaded text comes from; a rename changes the virtual manifest path but retains that source; creates/overwrite-creates already hold empty text; loaded text wins over any physical source. `load_exact_resource` must read an unloaded moved file from that tracked source, using existing document-size/no-follow checks. A delete must not allow a later virtual read to resurrect an unavailable resource from disk.

Do not serialize the new bookkeeping into `CanonicalPlan`. Preserve versioned Document preconditions, replacement byte ranges, same-position insertion order, no-op suppression, and existing limits. A directory rename followed by a nested text edit must resolve its original nested source correctly.

**Verify:** add `ordered_preview_planner_tracks_renamed_text_sources` in the planner tests, covering file rename, directory rename with a nested file, and chained rename. Run its unique filter: at least one test passes. The rename→edit presentation test may still fail at rendering at this intermediate step; planner tests must all pass.

### Step 3: Extract the existing staging interpreter and share it with presentation

Extract the ordered evaluation portion of `stage_text_outputs` into one crate-visible Mutation helper housed in `application.rs`, with a searchable name such as `visit_canonical_text_outputs`. Keep virtual text, missing roots, alias resolution, canonical edit application, before/after digest validation, and byte-limit enforcement in that **single** implementation.

Have the helper visit operations in order, delivering each text operation's index/path and borrowed before/after bytes to a small caller-supplied sink, and visiting resource operations without text bytes. This lets the Preview sink emit resource notices in place without accumulating another operation-sized text copy. The staging sink ignores resource visits and retains today's private file creation, permissions, write/flush, and filename-by-operation-index behavior. The Preview sink renders the corresponding hunk without creating transaction artifacts. Resource notices remain in canonical operation order. Avoid collecting an additional unbounded copy of every intermediate document; preserve existing configured limits and fail closed on conversion/digest/input errors.

Render **ordered per-operation hunks**, each relative to the virtual text immediately before that operation, plus existing create/rename/delete notices. This is the minimal truthful representation matching canonical execution, including repeated edits. Do not claim the entire display is one standalone patch against the original physical tree; net original-to-final coalescing and rename-lineage formatting are deliberately not required. Preserve contextual radius 3 and do not drop a necessary earlier hunk merely because a later operation edits the same file.

`preview_diff` must stop rereading `fs::read(path)` for every Text arm. Keep the existing stale preflight in `refresh_preview_presentation`, and check before/after canonical digests during projection so an intervening change cannot produce a misleading diff. On stale/unavailable inputs, omit the diff as today; never mutate the canonical Preview to make it render. Preserve access-time behavior of the existing staging reader.

**Verify:** all three Step 1 tests pass. Run `cargo test --locked --bin lspctl mutation::application::tests::text_output_is_complete_private_staging_before_commit` and `cargo test --locked --bin lspctl mutation::planner::tests`; both pass with nonzero test counts. Inspect `git diff --stat`: there is one shared evaluator, not a copied virtual interpreter added to `mutation.rs`.

### Step 4: Assert projection/staging equivalence and non-mutating failures

Add `ordered_preview_matches_staged_outputs` and `ordered_preview_preserves_stale_and_noop_behavior`.

For equivalence, run the same canonical sequences through the shared visitor and real `stage_transaction`. Compare emitted after-bytes/digests with each staged `text-{operationIndex}` file, including create→edit, rename→edit, repeated edits, directory rename→nested edit, overwrite-create→edit, and delete→create→edit. Verify resource notices remain ordered, physical Workspace files are unchanged before commit, and staging preserves private ownership and limits. Do not parse unified diff hunks back into bytes as the sole oracle; compare the common projected bytes to actual staged files and separately assert exact display lines.

For stale/no-op behavior, modify a bound physical input before presentation and require no misleading diff; verify exact no-op edits are still omitted by the planner. Cover invalid canonical ranges/digests through private unit inputs and ensure they fail rather than panic or emit fabricated text. Keep binary-only resource operations representable as resource notices without trying to decode their file contents.

**Verify:** the list command shows six required named `ordered_preview_*` tests. `cargo test --locked --bin lspctl ordered_preview` and `cargo test --locked --bin lspctl mutation::` exit 0 with nonzero counts.

### Step 5: Run complete verification and update the index

Run the full test, Clippy, format, schema, and fixture checks above. Keep existing CLI rename contextual-diff coverage green in the full suite. Record tests/platforms actually run; update the index only after every gate passes.

**Verify:** all gates and `git diff --check` exit 0; `git status --short` contains only allowed changes relative to the starting worktree.

## Test plan

The six required tests are the three initial rendering regressions, `ordered_preview_planner_tracks_renamed_text_sources`, `ordered_preview_matches_staged_outputs`, and `ordered_preview_preserves_stale_and_noop_behavior`. Together they cover fresh creates, unmaterialized rename targets, repeated offsets, nested/chained renames, overwrites, recreate sequences, Unicode/CRLF, stale inputs, no-ops, display limits, and no filesystem writes while rendering. Reuse existing isolated inline Mutation tests rather than adding a new fixture framework.

## Done criteria

- [x] All six named tests are listed and pass; the original regressions fail before the fix.
- [x] Create→edit and rename→edit produce truthful nonempty diffs before any physical Application.
- [x] Repeated text operations render against their own virtual before-state, and projected after-bytes equal staged after-bytes.
- [x] One shared ordered evaluator powers presentation and staging; no third interpreter exists.
- [x] Stale/malformed input cannot fabricate a diff, and rendering does not write Workspace files or transaction artifacts.
- [x] Full tests, Clippy, format, schema, fixture, and diff checks exit 0.
- [x] Only scoped files changed; index row 012 is coordinated by the root reviewer.

## Implementation and verification evidence

- Starting HEAD `e83786f` (merged PR #59), initially clean. Reviewed drift from `5268c6a`: prior plans added directory-membership certificates, reservation ownership, and proven commit/rollback effects. These are preserved; the ordered staging interpreter itself still matched this plan's excerpt. No unexplained semantic drift.
- Initial valid RED: `cargo test --locked --bin lspctl ordered_preview` failed the create/repeated presentation assertions (`None`), and rename→edit failed opening the nonexistent virtual destination at operation 1. Unicode/CRLF WorkspaceEdits compiled and passed validation up to those intended failures. The planner regression independently failed the same missing-source path. Logs: `/tmp/48-render-red.log`, `/tmp/48-planner-red.log`.
- `ResourceState.text_source` is in-memory only. Physical file inspection initializes it; moved file/directory states retain it; empty creates and tombstones clear it. Lazy reads retain ancestor/no-follow checks and per-Document limits. The required planner test covers file, nested directory, and both chained rename forms, plus deletion non-resurrection and size rejection.
- `visit_canonical_text_outputs` is the single ordered evaluator shared by staging and presentation. It retains digest checks, virtual text/rename aliases/missing roots, cumulative staged-byte limits, and the existing access-time-preserving reader. The staging sink retains exclusive private output creation, permissions, flushes, and operation-index filenames. The presentation sink emits ordered resource notices and contextual radius-3 hunks without staging artifacts. Canonical byte conversions, ordering, bounds, and UTF-8 boundaries fail rather than panic.
- Six named `ordered_preview_*` tests pass. Equivalence covers create→edit, rename→edit, repeated edits, directory rename→nested edit, overwrite-create→edit, and delete→create→edit, comparing projected bytes and canonical digests to real staged files. Tests separately assert exact display lines/order, unchanged physical manifests, ownership marker/private Unix permissions, and cumulative staging/display limits. Stale/missing input, malformed ranges/digests, no-ops, and binary-only resource notices are covered without Workspace writes.
- Scoped GREEN on Linux: discovery lists all **6** required names; `ordered_preview` **6**, `mutation::` **44**, planner **9**, and existing private-staging exemplar **1** pass. Clippy with `-D warnings`, formatting, schema, immutable stored-state, and whitespace checks exit **0**. Logs: `/tmp/48-discovery-green.log`, `/tmp/48-step4.log`, `/tmp/48-mutation.log`, `/tmp/48-planner-all.log`, `/tmp/48-staging.log`, `/tmp/48-clippy.log`, `/tmp/48-format.log`, `/tmp/48-schema.log`, `/tmp/48-state.log`.
- Root independently reviewed the production and test diffs without blocking findings. Native path audit confirmed these tests share the planner's lexical root/file-URI convention (no Owner snapshot canonicalization mismatch). Windows/macOS/MSRV have not been run locally.
- Combined full GREEN after #49 completed its intended RED phase: `cargo test --locked --all-targets --features fake-server` passes **152** tests (104 unit + 4 CLI + 3 fake-server fixture + 38 lifecycle + 3 installation), including all six #48 regressions and existing contextual-diff/Mutation coverage. Evidence: `/tmp/49-full-green.log`, independently inspected. Combined Clippy, formatting, schema, immutable stored-state, and whitespace gates also pass.
- Only the three scoped Mutation files and this plan were changed for #48; the coordinator owns the index. Concurrent #49 changes are separately scoped. Changes remain uncommitted.

- Final coordinator verification after all three plans: **155** tests pass (107 unit + 4 CLI + 3 documentation + 38 lifecycle + 3 installation), exact regression discovery counts **6/2/5/3**, and Clippy, formatting, schema, stored-state, and whitespace gates pass. Logs: `/tmp/48-50-root-full-green.log`, `/tmp/48-50-root-clippy.log`. Independent coordinator review found no blocking findings; the shared index now marks plans 012–014 DONE. Changes remain uncommitted; native Windows/macOS/MSRV CI was not run.

## STOP conditions

STOP if rendering requires a changed public Preview contract, persisting full source snapshots, applying temporary edits, or broadening into a general virtual filesystem. STOP if the minimal source bookkeeping cannot preserve existing canonical/version preconditions, or if a sequence needs changed Mutation authorization semantics rather than presentation/source resolution. Also STOP on unexplained drift, required out-of-scope edits, or a verification gate failing twice after a reasonable fix attempt. Never bypass a digest check to generate a prettier diff.

## Maintenance notes

Any new canonical resource operation must update the shared evaluator and its staging-equivalence test. Keep the planner's coordinate conversion separate from evaluation of already-canonical byte edits. Ordered per-operation display is intentional; single net-file coalescing is deferred because it needs additional resource-lineage policy. Serialize edits with plans 002/003/006, which share the same Mutation files, without treating them as semantic prerequisites.
