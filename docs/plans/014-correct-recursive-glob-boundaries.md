# Plan 014: Match recursive directory globs only at directory boundaries

> **Executor instructions:** Implement only after authorization and follow each verification gate. Stop on the conditions below instead of replacing the glob engine or broadening pattern semantics. When finished, update this plan's status and its row in `docs/plans/README.md`.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/query.rs src/session/owner_runtime.rs`
> Run `git status --short` and compare any changes with the excerpts. Other plans may have legitimately changed these large files; review those changes, but STOP if the matching algorithm/call contracts differ materially. Do not overwrite unrelated work.

## Status

- **Status:** DONE — three regressions and all acceptance gates pass
- **Audit finding:** 14
- **Priority:** P2
- **Effort:** S
- **Risk:** LOW
- **Depends on:** none
- **Category:** bug
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

`**/main.rs` currently matches `/workspace/domain.rs`: the recursive matcher strips the pattern slash and retries `main.rs` in the middle of a filename. The matcher feeds both Document capability selectors and file-operation registrations, so this admits Documents/notifications a server did not register for. Correct one shared boundary transition while retaining the existing bounded matcher and zero-directory behavior.

## Current state

- `src/query.rs:948–1002` exposes `protocol_glob_matches` and uses the same private matcher for absolute and relative Document selectors.
- `src/query.rs:1062–1107` contains the memoized recursive matcher.
- `src/session/owner_runtime.rs:2092–2154` checks file-operation registration filters through `crate::query::protocol_glob_matches(glob, uri.path(), ignore_case)`.
- Both files have inline `#[cfg(test)]` modules; no new test framework or fake server is needed.

Faulty branch (`src/query.rs:1074–1084`):

```rust
let recursive = pattern.get(pattern_index + 1) == Some(&b'*');
let mut next_pattern = pattern_index + if recursive { 2 } else { 1 };
let recursive_directory = recursive && pattern.get(next_pattern) == Some(&b'/');
if recursive_directory {
    next_pattern += 1;
}
glob_matches_at(pattern, path, next_pattern, path_index, states)
    || (path_index < path.len()
        && (recursive || path[path_index] != b'/')
        && glob_matches_at(pattern, path, pattern_index, path_index + 1, states))
```

Every byte consumed by the recursive arm retries the suffix after `**/`, even when no slash was crossed. The nearby comment (`:946–947`) promises: “One memoization cell per pattern/path byte pair and at most 256 brace expansions bounds matching of server-controlled selector patterns.” Preserve that bound and the memoization key.

Test exemplar (`src/query.rs:2374–2375`):

```rust
assert!(glob_matches(b"**/main.[r-t][!x]", b"/workspace/main.rs"));
assert!(!glob_matches(b"**/main.[!r]s", b"/workspace/main.rs"));
```

`capability_gates_subfeatures_commands_and_document_selectors` (`:2300–2376`) builds a provider selector and uses `compose(...).unwrap_err().code` to inspect `capability_unavailable`. `file_operation_filters_honor_kind_glob_and_case_options` (`src/session/owner_runtime.rs:2832–2866`) builds an initialize-result filter and asserts `file_operation_registered` for rename old/new URIs and resource kind. Extend those conventions with separate named regressions.

`CONTEXT.md:9–15` calls an Agent's semantic request a **Query** and the session's fixed capabilities a **Capability profile**. ADR `docs/adr/0005-use-a-fixed-static-capability-profile.md:3` says the profile “is fixed for a session, disables dynamic registration and configuration overrides, and also constrains raw requests.” This plan corrects matching of that fixed profile; do not add dynamic registration or configuration switches.

## Scope

**Only implementation files allowed:**

- `src/query.rs` — the `**/` transition and inline matcher/selector regression tests.
- `src/session/owner_runtime.rs` — **test module only**, adding file-operation filter regression coverage.

**Metadata exception:** this plan and its row in `docs/plans/README.md`.

**Out of scope:** Unicode `?` semantics, percent-decoding URI paths, path normalization, brace expansion limits, general pattern grammar changes, resource limit configuration, replacing the matcher with a dependency, production Owner lifecycle code, dynamic registration, public schemas. Bare `**` without a following slash and ordinary `*` retain their current semantics.

## Commands you will need

Run from repository root with existing Rust 1.89+ and locked dependencies; no installation step.

| Purpose | Command | Expected result |
|---|---|---|
| Discover regressions | `cargo test --locked --bin lspctl recursive_glob_ -- --list` | Three new tests listed |
| Regressions | `cargo test --locked --bin lspctl recursive_glob_` | Three tests pass after fix |
| Query tests | `cargo test --locked --bin lspctl query::tests::` | All selected tests pass |
| Owner unit tests | `cargo test --locked --bin lspctl session::owner_runtime::tests::` | Existing and new filters pass |
| Full suite | `cargo test --locked --all-targets --features fake-server` | All targets pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Diff | `git diff --check` | Exit 0 |

## Git workflow

Leave changes uncommitted. If explicitly authorized to create a branch, use `advisor/014-correct-recursive-glob-boundaries`. No push or PR. If later asked to commit, use an imperative message such as `Respect directory boundaries in recursive globs`.

## Steps

### 1. Reproduce the mismatch at the shared matcher and its consumers

In `src/query.rs::tests`, add:

- `recursive_glob_respects_directory_boundaries`: a table-driven `protocol_glob_matches` test with positive `**/main.rs` matches against `main.rs`, `/main.rs`, `src/main.rs`, and `/workspace/src/main.rs`; negative matches against `domain.rs`, `/workspace/domain.rs`, and `/workspace/main.rs.bak`. Include `src/**/main.rs` against zero- and multi-directory paths and reject `src/domain.rs`. Cover unchanged ordinary `*` refusing directory separators, bare `**` crossing them, braces/classes, and the existing ASCII case option. Assert both matched result and pattern/path context in failures.
- `recursive_glob_document_selectors_reject_filename_suffixes`: build a Document snapshot URI ending in `/workspace/domain.rs`, use selector `**/main.rs`, and assert denial through the same selector/compose path used in `capability_gates_subfeatures_commands_and_document_selectors`. Positive `/workspace/main.rs` must be admitted. Add the equivalent `RelativePattern` with `baseUri` to ensure both selector entry paths reuse the correction.

In `src/session/owner_runtime.rs::tests`, add `recursive_glob_file_operation_filters_reject_filename_suffixes`. Use `file_operation_registered` with `didRename` filters patterned `**/main.rs`: reject when both old/new URIs are nonmatching (`domain.rs` included), and accept when either rename endpoint really ends in the `main.rs` path component. Include a non-rename file-operation filter case and preserve `matches`/`ignoreCase` behavior.

**Verify discovery:**

```sh
test "$(cargo test --locked --bin lspctl recursive_glob_ -- --list | grep -c 'recursive_glob_.*: test$')" -eq 3
```

Expected: exit 0 and all three exact names above present. Zero tests is failure, not success.

**Verify red:** `cargo test --locked --bin lspctl recursive_glob_` must fail the `domain.rs` negative assertions on the original implementation, rather than a fixture/compilation error.

### 2. Correct only the recursive-directory transition

Handle `recursive_directory` distinctly from the ordinary star transition in `glob_matches_at`. Permit the suffix following `**/` at the zero-directory entry boundary and after consumed directory separators, never at an arbitrary interior byte of a component. Keep consuming through components so multi-directory matches still work; do not fix the suffix false-positive by requiring at least one directory.

For the normal path-component form of `**/`, the suffix attempt must be gated by `path_index == 0` or a preceding `/`, while the recursive consume arm still advances. Check the prefixed `src/**/main.rs` cases explicitly. If an alternative shape is clearer, use it only if it keeps one memoized state per pattern/path byte pair and preserves the listed positives/negatives. Do not add a regex/glob dependency or change ordinary-star/bare-recursive behavior.

**Verify:** `cargo test --locked --bin lspctl recursive_glob_` runs exactly three tests and passes. Run the existing Query and Owner unit-test commands from the table; both must pass.

### 3. Run final gates and inspect scope

Run full tests, Clippy, formatting, discovery, and diff hygiene. Confirm that the only production change is the shared Query matcher; Owner changes are confined to its test module. Record result and any unexecuted platform coverage in this plan and its index row.

**Verify:** every command in the table exits 0; `git diff --check` exits 0 and `git status --short` shows only allowed changes plus recorded pre-existing work.

## Test plan

Three narrowly scoped unit tests cover the algorithm and both independent consumer paths. Assert the exact `domain.rs` rejection and the zero-/multi-directory positives so a fix cannot trade overmatching for undermatching. No subprocess or real language server is necessary. Retain current brace/class and case-insensitive coverage; Unicode and URI decoding are intentionally separate investigations.

## Done criteria

- [x] Discovery assertion returns 0 with exactly three new `recursive_glob_*` tests.
- [x] `**/main.rs` does not match `/workspace/domain.rs`, while all listed component-boundary positives pass.
- [x] Document selector and file-operation consumer regressions pass.
- [x] Existing Query/Owner tests, full suite, Clippy, formatting, and diff hygiene all pass.
- [x] Production Owner runtime, schemas, dependencies, and unrelated glob semantics are untouched by this plan.
- [x] This plan records completion; the coordinating reviewer owns the index update.

## Verification evidence

- Starting HEAD: `e83786f` (detached `origin/main`). Plans 012/013 were concurrently authorized uncommitted work and were preserved. Drift inspection against `5268c6a` found no Query matcher changes; Owner changes belong to landed lifecycle/synchronization plans and retain the same file-operation filter contract. Plan 013's concurrent changes only widen two unrelated Query helper visibilities and update diagnostic caching in the Owner.
- Added exactly the three specified regressions. Discovery assertion exits **0** (`/tmp/50-discovery.log`). The initial focused run failed all three on the intended `domain.rs` admission: direct matcher returned true, Document composition admitted the suffix, and rename filtering admitted `domain.rs -> other.rs` (`/tmp/50-red.log`). No fixture or compilation failure was counted as RED.
- The only production change for this plan gates the `**/` suffix attempt on the beginning of the path or a preceding `/`. Recursive consumption and the memoization key are unchanged. Table coverage preserves zero/multiple directories, ordinary `*`, bare `**`, braces/classes, and ASCII case handling. Consumer tests cover absolute/relative selectors, either rename endpoint, and create filters with resource-kind and case options.
- Focused regressions: **3** pass (`/tmp/50-green.log`); Query tests: **18** pass (`/tmp/50-query.log`); Owner unit tests: **7** pass (`/tmp/50-owner.log`).
- Full `cargo test --locked --all-targets --features fake-server`: **155** pass (107 unit + 4 CLI + 3 documentation + 38 lifecycle + 3 installation), including concurrent plans 012/013 (`/tmp/50-full-green.log`). Clippy with `-D warnings`, formatting, discovery assertion, and `git diff --check` all exit **0** (`/tmp/50-clippy.log`).
- Scope inspection confirms this plan changes only the shared Query matcher, its two inline tests, the Owner test module, and this metadata. No dependencies, schemas, normalization, or new glob semantics were introduced. Verified locally on Linux Rust **1.98.1**; native macOS/Windows and Rust 1.89 were not run locally. Changes remain uncommitted for review.

- Final coordinator verification after all three plans: **155** tests pass (107 unit + 4 CLI + 3 documentation + 38 lifecycle + 3 installation), exact regression discovery counts **6/2/5/3**, and Clippy, formatting, schema, stored-state, and whitespace gates pass. Logs: `/tmp/48-50-root-full-green.log`, `/tmp/48-50-root-clippy.log`. Independent coordinator review found no blocking findings; the shared index now marks plans 012–014 DONE. Changes remain uncommitted; native Windows/macOS/MSRV CI was not run.

## STOP conditions

Stop if the matcher has materially changed since the recorded SHA, if the minimal correction requires a different memoization key or unbounded search, or if a failing test concerns Unicode/path-decoding semantics rather than recursive directory boundaries. Do not invent semantics for unusual embedded globstar forms to expand the patch. Report any such ambiguity separately. Also stop on out-of-scope changes or the same verification failure after two reasonable corrections.

## Maintenance notes

The shared matcher is the correct seam: fixing only Document selectors would leave file-operation filters wrong. Preserve tests for zero directories whenever this branch changes. Reviewer attention belongs on component boundaries and bounded work, not a wholesale pattern-engine rewrite.
