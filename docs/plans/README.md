# Implementation plans

Generated on **2026-09-05**, against commit **`5268c6a`**, for the explicitly selected audit findings **1–14**. The numbered filenames preserve the audit finding IDs; the table below gives the recommended execution order. Plans live here at the maintainer's request, rather than the improve skill's default root directory.

**These are handoffs, not implemented fixes.** Each plan is self-contained: current code evidence, design constraints, scoped steps, named regression tests, verification gates, and STOP conditions. Read the entire selected plan before execution. Do not execute the whole backlog automatically.

The existing [README agent demo plan](readme-agent-demo.md) is unrelated and preserved unchanged.

## Execution order and status

Effort: **S** hours, **M** roughly a day, **L** multiple days, including tests. Risk describes the implementation, not the severity of leaving the defect unfixed. Scheduling below prioritizes filesystem safety, then small correctness/security fixes, then shared Owner/diagnostic work.

| Order | Plan / audit finding | Title | Priority | Effort | Risk | Depends on | Status |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | [001](001-confine-skill-install-recovery.md) | Confine skill-install recovery and validate existing ancestors | P1 | M | MED | — | DONE |
| 2 | [002](002-bind-recursive-directory-membership.md) | Bind exact recursive directory membership | P1 | M | MED | — | DONE |
| 3 | [003](003-preserve-external-changes-during-rollback.md) | Preserve external changes during rollback | P1 | L | HIGH | 002 | TODO |
| 4 | [004](004-preserve-partial-result-chunks.md) | Preserve partial-result chunks across the Owner boundary | P1 | S | LOW | — | TODO |
| 5 | [005](005-prevent-owner-maintenance-starvation.md) | Prevent maintenance starvation under notification traffic | P1 | S | LOW | — | TODO |
| 6 | [006](006-release-prejournal-preview-reservations.md) | Release Preview reservations before durable transaction ownership | P1 | S | LOW | — | TODO |
| 7 | [007](007-enforce-raw-document-scope.md) | Enforce explicit server selection for external raw Documents | P1 | S | LOW | — | TODO |
| 8 | [008](008-bound-owner-handshakes.md) | Bound unauthenticated Owner admission and handshakes | P1 | S | LOW | — | TODO |
| 9 | [009](009-decouple-trust-administration.md) | Decouple Trust administration from unrelated executable availability | P1 | M | MED | — | TODO |
| 10 | [011](011-bound-language-server-writes.md) | Bound language-server writes and make incomplete delivery fatal | P1 | M | MED | 005 | TODO |
| 11 | [010](010-deliver-document-synchronization-transitions.md) | Deliver committed Document synchronization transitions | P1 | M | MED | 011 | TODO |
| 12 | [012](012-render-ordered-preview-diffs.md) | Render Preview diffs against ordered virtual filesystem state | P2 | M | MED | — | TODO |
| 13 | [013](013-persist-related-diagnostic-reports.md) | Persist related diagnostic reports across CLI calls | P2 | M | MED | 004 | TODO |
| 14 | [014](014-correct-recursive-glob-boundaries.md) | Respect directory boundaries in recursive globs | P2 | S | LOW | — | TODO |

Status values: **TODO**, **IN PROGRESS**, **DONE**, **BLOCKED** (add one-line cause), **REJECTED** (add rationale, such as independently fixed), or **STALE** (requires plan refresh). A green test filter matching zero tests is not evidence of completion. Record actual command results in the individual plan before marking DONE. Native platform verification still belongs in CI when unavailable locally.

## Dependency notes

```text
002 ──► 003        Directory certificates must exist before safe recursive undo.
005 ──► 011 ──► 010
                   Timer progress, then bounded/fatal writes, then sync ordering.
004 ──► 013        Diagnostic persistence consumes complete partial chunks.
```

**Plan 009 has a deliberate contract boundary:** it fixes resolution-independent revocation, selected-only grants, and incomplete-declaration JSON errors. Independent named-status success remains deferred because the current status schema requires a complete all-declarations aggregate digest. It must not fabricate a subset digest; completing 009 does not claim that residual limitation is resolved.

These are correctness/verification dependencies, not permission to skip reading a plan. Independent plans can run in a different order, but **do not edit shared files concurrently in the same worktree**:

- **Mutation group:** 002, 003, 006, 012 share `src/mutation/*` and inline tests. In particular, 006 must retain 003's durable transaction/progress ownership, and 012 must retain staging verification and directory certificates.
- **Owner/Query group:** 004, 005, 008, 010, 011, 013, 014 share `owner_runtime.rs`, Query helpers, and/or fake-server fixtures. 007 and 009 can also touch lifecycle tests. Reuse helpers introduced by completed plans rather than adding parallel fixture frameworks.
- **Installer group:** 001 is independent of Mutation Recovery; its installation journal must not be folded into the Workspace transaction system.

Every plan is stamped at the same audit commit. Previously completed plans will legitimately shift excerpts and test counts. Review their diffs, preserve their regression tests, and map the current symbols before executing the next plan. Stop and refresh the plan if an assumption changed; do not revert a prerequisite to recover the old excerpt. Plans are specifications, not guarantees that a high-risk implementation can proceed without a new decision.

## Baseline and verification

Repository shape: one Rust 2024 Cargo package, **Rust 1.89 minimum**, CLI plus hidden per-Session Owner, Linux/macOS/Windows targets. Dependencies are already locked; no installation or dependency update is part of these plans. Public commands emit one versioned JSON envelope. Unit tests are mostly inline; `fake-server` enables the independent LSP fixture and Owner lifecycle integration tests.

Verified at `5268c6a` during the audit:

| Gate | Command | Observed result |
| --- | --- | --- |
| Full tests/build | `cargo test --locked --all-targets --features fake-server` | 84 passed (run with `--offline` using installed dependencies) |
| Type/lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Passed (run with `--offline`) |
| Format | `cargo fmt --all -- --check` | Passed |
| Public registry | `python scripts/release/check_schema.py` | Passed |
| Stored-state seed integrity | `python scripts/release/check_stored_state.py` | Passed |
| Release registry | `python scripts/release/check_gates.py --require-complete` | Passed |
| Release scripts | `python tests/release_scripts.py` | 10 passed; Windows-only parse test skipped on Linux |
| Dependency advisories | `cargo audit` | Refreshed RustSec database; no vulnerabilities reported |

The existing green baseline does not reproduce the newly identified defects. Each plan specifies a small failing regression before its fix. The schema registry comparison is not comprehensive validation of every runtime envelope (audit finding 15); do not describe it that way.

No native Windows/macOS runs, real-server matrix, soak benchmarks, or release publication were performed for this planning work. No issues were published; the user did not request `--issues`.

## Implementation boundaries

- The current change creates planning documents only; source, tests, dependencies, contract assets, and the existing demo plan are unchanged.
- Future executors may modify only their plan's implementation scope, plus their own status/evidence and corresponding index row. Record pre-existing work and leave it untouched.
- Use the operator-provided branch/worktree. Create a branch, commit, push, or open a PR only with separate authorization. The default handoff is an uncommitted reviewed diff.
- Preserve the domain terms **Owner**, **Query**, **Document**, **Preview**, **Application**, **Recovery**, and **Receipt**. Honor the ADR decisions quoted in each plan.
- Do not rewrite immutable first-release fixtures or silently upgrade an old Preview into new authorization. Plans 002 and 003 explicitly favor safe stale/Recovery outcomes when legacy evidence cannot prove safety.
- Do not print authentication tokens, real user-state contents, or source secrets in tests/evidence. Fixtures must own and clean up their temporary state and processes.

## Findings considered and rejected

These are not additional implementation plans:

- **Replace per-Session serial Owners with a global service or concurrent dispatcher:** rejected; ADR 0001 deliberately chooses isolated serial Owners. The scheduling and transport defects have narrower fixes.
- **Add a filesystem watcher or incremental synchronization:** rejected for this scope; ADR 0003 intentionally uses filesystem snapshots and close/open replacement. Lost events are a correctness bug within that design, not evidence that a watcher is needed.
- **Enable dynamic capabilities or replace the custom transport with an editor stack:** rejected; ADRs 0005/0006 explicitly choose a fixed profile and guarded transport. Fix its boundaries rather than undoing the design.
- **Promise fully atomic cross-platform multi-resource writes:** rejected; ADR 0004 explicitly requires rollback or inspectable Recovery instead. False safety claims are not an improvement.
- **Hash executable bytes as a Trust identity requirement:** rejected; ADR 0002 binds declaration and resolved path, deliberately not executable contents.
- **Split large modules or add new caches merely because files are large:** not justified by measured cost. Keep changes at the existing shared seams.
- **Treat ordinary bundled agent guidance as malicious prompt injection:** not a finding by itself; it is expected documentation, not authority over the audit. No suspicious repository instruction was promoted into this backlog.

## Deferred and unselected work

- Independently successful named Trust status when another declaration is unavailable requires a separate contract decision; see the explicit boundary in plan 009.
- Audit **15**, broader runtime-envelope schema validation, and **16**, behavioral Windows-installer tests, were not selected. Preserve them as follow-up candidates, not implicit prerequisites for every plan.
- Release dependency hash pinning and third-party Action SHA pinning remain optional follow-up hardening; no dependency advisory or compromised artifact was demonstrated in this audit.
- A read-only configuration doctor and native code signing/notarization were direction options, not selected features.
- General hostile concurrent filesystem race hardening, Unicode glob behavior, and URI-decoding semantics are outside these numbered fixes unless a plan explicitly stops for a prerequisite decision.
