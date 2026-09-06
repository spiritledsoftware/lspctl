# Plan 002: Bind recursive directory membership into exact Preview preconditions

> **Executor instructions:** Read this entire plan, then execute the steps in order. Run each verification gate; do not treat a filtered run containing zero tests as success. Leave changes uncommitted unless the operator separately authorizes a commit. Update plan 002 in `docs/plans/README.md` when complete, unless a reviewer owns the index.
>
> **Drift check, run first:** `git diff --stat 5268c6a..HEAD -- src/mutation/planner.rs src/mutation/application.rs src/mutation.rs src/mutation/state.rs docs/plans/002-bind-recursive-directory-membership.md docs/plans/README.md`. Read any changed implementation against the excerpts below. Known changes from another selected plan may be mapped to their equivalent symbols and verified; STOP on unexplained semantic drift. Do not revert earlier safety fixes.

## Status

- **Status:** DONE
- **Priority:** P1
- **Effort:** M
- **Risk:** MED
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05
- **Audit finding:** 2

## Why this matters

An Application currently rechecks only paths recorded when its Preview was created. Directory identity and metadata do not bind directory membership, so adding a new file beneath a recursively deleted or renamed directory need not make the Preview stale. An overwrite-rename also omits destination-only descendants from its plan. Moving those trees into undo storage and subsequently deleting undo storage can destroy bytes the Agent never inspected or authorized.

## Current state

- `src/mutation/planner.rs` owns `ManifestEntry`, the ordered `VirtualWorkspace`, bounded tree loading, and filesystem inspection.
- `src/mutation/application.rs` revalidates, stages, commits, compares manifests, and exposes Recovery.
- `src/mutation.rs` rechecks proposals and refreshes Preview presentation; every path must agree on membership semantics.
- `src/mutation/state.rs` defines immutable version-1 stored records and their compatibility tests.

`src/mutation/planner.rs:559–574` reads a fixed list, not the current set of descendants:

```rust
for path in paths {
    if let Err(problem) = self.validate_existing_ancestors(path, 0) {
        problems.push(problem);
        continue;
    }
    match self.inspect_resource(path, 0, false) {
        Ok(state) => manifest.push(state.manifest),
        Err(problem) => problems.push(problem),
    }
}
```

`src/mutation/planner.rs:925–933` loads the complete source, but only the destination root:

```rust
if let Err(problem) = self.load_resource_tree(workspace, &old_path, index) {
    problems.push(problem);
    return;
}
if let Err(problem) = self.load_exact_resource(workspace, &new_path, index, false) {
    problems.push(problem);
    return;
}
```

`src/mutation/planner.rs:1400–1404` gives directories no content precondition:

```rust
let content_digest = if resource_kind == ResourceKind::File {
    Some(hash_file(path, index)?)
} else {
    None
};
```

`src/mutation/application.rs:1995–1998` intentionally treats omitted digests as unconstrained:

```rust
} else if expected.content_digest.is_some()
    && expected.content_digest != actual.content_digest
{
    Some("resource_content")
```

Consequently merely populating new observations with directory digests does **not** protect old Previews that lack them. The existing `lspctl://schema/v1/output/manifest-entry` in `assets/contract/schemas.json` permits an optional SHA-256 `contentDigest` for every resource kind; no new JSON field is required.

### Conventions and design constraints

`CONTEXT.md:33–39` defines Preview as “An identified, immutable representation of one Mutation that an Agent can inspect before authorizing it,” and Application as “An Agent-authorized attempt to commit one Mutation to the filesystem.” Use these terms; do not call a newly observed tree the authorized tree.

`docs/adr/0004-apply-exact-previews-with-recoverable-transactions.md:3` says: “Application revalidates every bound identity and filesystem precondition, stages and journals the complete plan, records an at-most-once Receipt, and either reaches the intended manifest, rolls back, or exposes Recovery with the exact observed state.” It explicitly rejects perfectly atomic arbitrary cross-platform changes. This plan closes the Preview-to-Application gap; it does not promise lockout of external editors after a check.

`docs/adr/0007-use-capability-based-workspace-filesystem-access.md:3` requires opening the canonical Workspace through `cap-std` and using `cap-fs-ext` for no-follow access. Use the existing capability root and error/limit conventions, not a separate unrestricted tree walker.

Test exemplar, `src/mutation/planner.rs:2358–2373`:

```rust
let workspace = TempDir::new().unwrap();
let file = workspace.path().join("main.rs");
fs::write(&file, "a😀b\r\n").unwrap();
let uri = Url::from_file_path(&file).unwrap().to_string();
let (previews, mutation) = settings();
let planner = WorkspaceEditPlanner::open(
    workspace.path(), PositionEncoding::Utf16, &previews, &mutation,
).unwrap();
```

Use this inline `#[cfg(test)]` module pattern. For durable Application tests, reuse the local `MutationStateStore::open_at` and `ApplicationContext` construction in `exact_text_preview_applies_once_and_returns_same_receipt` (`src/mutation/application.rs:2282–2366`). Never touch real user state.

## Commands you will need

Run from the repository root; Rust 1.89+ and existing locked dependencies are required. No installation or new dependency is part of this plan.

| Purpose | Command | Expected result |
|---|---|---|
| List Mutation tests | `cargo test --locked --bin lspctl mutation:: -- --list` | Exit 0; named tests listed |
| Focused tests | `cargo test --locked --bin lspctl directory_membership` | After implementation, all selected tests pass, nonzero count |
| Mutation tests | `cargo test --locked --bin lspctl mutation::` | All pass |
| Full tests | `cargo test --locked --all-targets --features fake-server` | All pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Format check | `cargo fmt --all -- --check` | Exit 0 |
| Schema compatibility | `python scripts/release/check_schema.py` | Exit 0 |
| Immutable fixture integrity | `python scripts/release/check_stored_state.py` | Exit 0 |

## Scope

**Implementation files allowed:** `src/mutation/planner.rs`, `src/mutation/application.rs`, `src/mutation.rs`, `src/mutation/state.rs` (compatibility tests only).

**Metadata exception:** this plan's status/verification notes and its row in `docs/plans/README.md`.

**Out of scope:** public schema/catalog changes, stored-state version bumps, immutable `tests/fixtures/stored-state/v1/*`, language-server behavior, locking external editors, introducing a watcher, transaction-provenance redesign (plan 003), unrelated planner cleanups. Do not edit the existing README demo plan.

## Git workflow

Work on the operator's chosen branch. If separately asked to create one, use `advisor/002-directory-membership`. Leave the diff uncommitted by default. Authorized commit messages should match the repository's imperative style, e.g. `Bind recursive directory membership to Previews`. Do not push or open a PR.

## Steps

### Step 1: Add deterministic stale-tree regressions

Add `directory_membership_rejects_added_descendant` in the application test module. Create a nested tree, persist a recursive-delete Preview, then create a new descendant before calling `apply_preview`. Assert `preview_stale`, unchanged contents of both old and new files, and no transaction or Receipt created.

Add `directory_membership_covers_overwritten_destination` in the planner test module. Plan a directory overwrite-rename where the destination has a nested file absent from the source. Assert that destination-only paths appear in `before_manifest`, appear as missing in `intended_manifest`, and contribute to entry/rollback limits. Add a subsequent Application variant that modifies an existing destination-only file between Preview and Application and requires stale rejection.

**Verify:** `cargo test --locked --bin lspctl directory_membership -- --list` must list both exact test names. `cargo test --locked --bin lspctl directory_membership` must fail on membership/manifest assertions against the current implementation, not compilation or setup. If it passes unchanged, STOP and reassess the reproduction.

### Step 2: Bind bounded directory child sets using existing manifest fields

Implement one deterministic directory-membership digest using the existing `content_digest` field. Hash a domain-tagged canonical representation of sorted **immediate child names and resource kinds**, not absolute paths, iteration order, timestamps, or a concatenation with ambiguous separators. Each nested directory gets its own digest and complete descendants remain individually bound by the manifest. Reuse `digest_canonical_value` and existing no-follow resource validation.

Enumerate through the Workspace capability. Enforce existing `max_entries` and `max_recursion_depth` ceilings; fail closed if a child cannot be represented or inspected. Never follow symlinks/reparse points. Do not start enumerating the whole Workspace merely because a single file is edited.

In `plan_rename_operation`, fully load an existing overwritten destination directory before removing its virtual subtree. Preserve existing `ignoreIfExists` behavior and resource-kind validation. Ensure new source/destination descendants count toward limits before copying/staging anything.

At finalization, derive intended directory child digests from the **complete ordered virtual state** after all operations; copying a directory's original digest is wrong if a later operation adds, deletes, or renames a child. Preserve missing-entry tombstones so later operations cannot accidentally reload an already deleted child from the physical tree.

**Verify:** `cargo test --locked --bin lspctl directory_membership` passes the Step 1 cases; `cargo test --locked --bin lspctl mutation::planner::tests` passes existing ordering, no-op, and validation tests.

### Step 3: Revalidate membership consistently and fail closed on legacy recursive Previews

Use the same observation semantics in `apply_preview` preflight; `persist_preview`, `create_callback_preview`, and `apply_preauthorized_workspace_edit` proposal rechecks; and `refresh_preview_presentation`. Require membership certificates on existing directories consumed by a delete or rename (including an overwritten destination). For a version-1 Preview lacking a needed directory digest, retain readability but report it stale and require a fresh Preview; never silently enrich/re-authorize its immutable canonical plan. Use existing `preview_stale`/stale-reason fields rather than a new error-code family.

New transactions retain the certified before/intended manifests, so their post-commit and Recovery observations include membership. Legacy Receipts remain readable unchanged. Legacy Recovery records must also remain inspectable: in `reconcile_recovery_status` and `recover_transaction`, when matching their previously issued manifest digest, project directory content fields according to the journal's existing template rather than silently changing its digest vocabulary. Do **not** label a legacy journal certified or automatically upgrade it to authorize newly discovered descendants. Plan 003 will separately make rollback conservative when provenance is absent.

Add `directory_membership_legacy_preview_requires_recreation`, `directory_membership_intended_tree_matches_ordered_operations`, and `directory_membership_limits_and_no_follow`. The ordered case must include directory rename followed by a nested resource change; the limit case must exercise entry count and depth on overwritten destinations. Assert an unmodified certified recursive Application still succeeds and preserves at-most-once Receipt behavior.

**Verify:** the list command must show all five `directory_membership` names specified above. Run `cargo test --locked --bin lspctl directory_membership` and `cargo test --locked --bin lspctl mutation::state::tests::first_release_stored_state_fixtures_remain_readable`; both exit 0. `python scripts/release/check_stored_state.py` exits 0 without changing fixtures.

### Step 4: Run the full gates and record compatibility behavior

Run every full-test/lint/format/schema/fixture command in the command table. Record the actual new test names and results in this plan's completion notes; mark the index row DONE only after all gates pass. On a single-OS workstation, retain explicit Windows/macOS CI follow-up instead of claiming native validation.

**Verify:** all commands exit 0; `git diff --check` exits 0; `git status --short` shows no new changes outside the allowed implementation and metadata paths relative to the starting worktree.

## Test plan

The required named tests are:

1. `directory_membership_rejects_added_descendant` — recursive delete and source-directory rename reject an added descendant before any write.
2. `directory_membership_covers_overwritten_destination` — destination-only descendants are bound and charged; modification rejects Application.
3. `directory_membership_legacy_preview_requires_recreation` — readable old recursive Preview is stale; ordinary legacy text-only Preview behavior remains unchanged.
4. `directory_membership_intended_tree_matches_ordered_operations` — unchanged certified operations apply; nested virtual mutations yield the observed final directory digest.
5. `directory_membership_limits_and_no_follow` — existing depth/entry ceilings and supported-platform symlink/reparse rejection remain fail-closed.

Also retain the exact at-most-once Application and immutable-state fixture tests. Tests must compare preserved bytes, not merely error categories. Do not use sleeps to manufacture races.

## Done criteria

- [x] All five named `directory_membership` tests are listed and pass.
- [x] Full tests, Clippy, format, schema, stored-state integrity, and `git diff --check` exit 0.
- [x] Added/changed unpreviewed descendants cause stale rejection before a transaction is created.
- [x] New intended directory digests reflect the ordered final tree, and unchanged recursive Applications pass.
- [x] Legacy recursive Previews are not silently upgraded; immutable fixtures remain byte-identical.
- [x] Only in-scope files changed, and the index status is updated.

## STOP conditions

STOP and report if an existing contract consumer forbids directory `contentDigest`, a safe legacy-Recovery digest projection cannot be expressed without rewriting immutable records, complete virtual membership cannot be established for a supported operation sequence, or a new schema/version/dependency appears necessary. Also STOP on unexplained drift, a gate failing twice after a reasonable fix attempt, or a required out-of-scope edit. Never weaken a check to make an old unsafe Preview apply.

## Execution record — 2026-09-05

Implemented on `main`, starting from clean commit `d33e9f0`. The drift check found no Mutation implementation changes from `5268c6a`; only the previously committed plan/index differed. The operator authorized implementation and a commit, not a push.

- Immediate child names and resource kinds use sorted canonical JSON with the domain `lspctl-directory-membership-v1`. Enumeration uses the Workspace capability and existing no-follow checks and limits; ordinary text edits do not enumerate their parent tree.
- Overwritten destination trees are fully bound and charged to limits. Missing-entry tombstones prevent ordered operations from reloading deleted physical resources. Intended directory digests are derived from the final virtual child sets; virtual parents permit nested resource changes after a directory rename.
- `inspect_manifest` observes only certificate-bound directories. All five Preview proposal/presentation/Application rechecks use this observation and `preview_manifest_mismatches`; uncertified legacy recursive Previews require recreation. Ignored destination directories remain unconstrained rather than being unnecessarily enumerated.
- Recovery chooses existing directory certificates across the journal's before/intended/observed templates. Legacy digest vocabulary remains unchanged, while certified journals reject subsequently added descendants. No schema, stored-state version, dependency, or immutable fixture changed. Post-check races and conservative rollback provenance remain plan 003 work.

### Regression evidence

The initial two regressions failed on the expected assertions: recursive delete applied despite an added descendant, and overwrite-rename omitted destination-only paths. Both then passed. Legacy recursive Preview rejection and legacy Recovery digest preservation each had separate red/green runs. Review reproduced an ignored-destination recheck regression, then verified the template-aware fix.

`cargo test --offline --locked --bin lspctl directory_membership -- --list` lists **8 tests**, covering every required name:

- Application: `directory_membership_rejects_added_descendant` — added descendants block recursive delete and source-directory rename; original/new bytes remain and no transaction or Receipt is created.
- Planner and Application: `directory_membership_covers_overwritten_destination` — destination-only descendants and tombstones are bound and charged; changed destination-only bytes block Application.
- Application: `directory_membership_legacy_preview_requires_recreation` — delete/rename/overwrite legacy Previews remain immutable and stale; legacy text-only Application still succeeds.
- Application: `directory_membership_intended_tree_matches_ordered_operations` — overwrite-directory rename, nested delete/create/rename, ignored deletion of an old path, and another directory rename apply with correct final membership and one at-most-once Receipt.
- Planner: `directory_membership_limits_and_no_follow` — overwritten destination entry/depth ceilings, affected target counts, ignored rename behavior, and no-follow checks. Linux exercised symlinks; the Windows junction branch was cross-compiled, not run locally.
- Application: `directory_membership_legacy_recovery_preserves_digest_vocabulary` — staged/committing × accept/rollback × legacy/certified journals retain the proper digest vocabulary; certified Recovery rejects added descendants and preserves their bytes.
- Application: `directory_membership_preserves_ignored_destination` — a mixed ignored rename plus text edit applies without enumerating the oversized untouched destination.

### Final gates

All commands below exited 0. Cargo commands used the existing locked dependency cache with `--offline`.

| Gate | Result |
| --- | --- |
| `cargo test --offline --locked --bin lspctl directory_membership` | 8 passed |
| `cargo test --offline --locked --bin lspctl mutation::` | 25 passed |
| `cargo test --offline --locked --bin lspctl mutation::state::tests::first_release_stored_state_fixtures_remain_readable` | 1 passed |
| `cargo test --offline --locked --all-targets --features fake-server` | 100 passed |
| `cargo check --offline --locked --all-targets --features fake-server` | Passed |
| Same check with `--target x86_64-pc-windows-gnu` | Passed; compile-only |
| `cargo clippy --offline --locked --all-targets --features fake-server -- -D warnings` | Passed |
| Same Clippy command with `--target x86_64-pc-windows-gnu` | Passed; compile-only |
| `cargo fmt --all -- --check` | Passed |
| `PYTHONDONTWRITEBYTECODE=1 python scripts/release/check_schema.py` | Passed |
| `PYTHONDONTWRITEBYTECODE=1 python scripts/release/check_stored_state.py` | Passed; fixtures unchanged |
| `git diff --check` and allowed-path check | Passed |

Two-axis review against `d33e9f0`: Standards identified one fixture-setup duplication, resolved with a small test helper; Spec identified the ignored-destination compatibility defect, resolved with a regression and template-aware observation. Both independent re-reviews reported no remaining findings.

**Platform follow-up:** Native Windows/macOS CI is still required. Windows cross-compilation/linting is not evidence of native filesystem behavior. No push or CI run was performed for this implementation.

## Maintenance notes

Review hashing determinism, virtual membership after overwrite/rename, legacy-record behavior, and bounded/no-follow enumeration especially closely. Execute plan 003 after this plan because rollback must consume the strengthened manifests. Plans 006 and 012 share Mutation files: serialize edits and retain their tests, but they are not semantic prerequisites. Post-check filesystem races remain recoverable rather than perfectly atomic, as ADR 0004 requires.
