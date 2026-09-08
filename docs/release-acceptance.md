# Release acceptance

The CI matrix runs current stable and Rust 1.89 on Ubuntu, macOS, and Windows.
Each cell checks formatting, Clippy with warnings denied, tests, and that
`lspctl schema --full` exactly exposes the checked contract assets. Product
checks have no retry step. A clean rerun is allowed only for an infrastructure
failure and must retain both attempts.

Linux also checks `cargo package --locked` and a locked source installation.
`cargo deny` blocks RustSec vulnerabilities, non-crates.io sources, and
unreviewed licenses for the five release targets. See
[dependency review](dependency-review.md).

## Candidate artifacts

`scripts/release/package.py` builds a target-native release binary and invokes
`build_archive.py`. The latter emits a deterministic target-named `.tar.gz`
or `.zip`, its SHA-256 sidecar, and a payload manifest. Archives contain the
binary, README, both licenses, `skills/lspctl/`, and a manifest with the release
version, target, source commit, Rust version, skill digest, and per-file
checksums. The script rejects missing or symlinked skill payloads.

`build_skill_archive.py` creates a separate versioned companion-skill ZIP with
a checksum and skill/schema digests. `package.py --skill-only` derives the
schema from the built release binary before archiving the skill.

`release.yml` uses native hosted runners for the five supported targets. Linux
builds and smokes inside architecture-native manylinux 2.28 containers; macOS
sets a 12.0 deployment target; Windows uses the MSVC target. Static audits
reject a GLIBC symbol above 2.28, a Mach-O minimum above macOS 12, or an
unreviewed Windows runtime import/PE version. Hosted native runners also smoke
every extracted archive before build-provenance attestation and upload.

## Extended acceptance

`nightly.yml` runs exact versions from `assets/reference-servers.json` for
rust-analyzer, typescript-language-server, and basedpyright on all three OS
families. Each cell proves Trust, initialization, owner reuse, navigation,
diagnostics, Preview inspection, Application, Receipt persistence, graceful
stop, and validates every output against the embedded schemas.

The same workflow runs a 1,000-operation immediate-response soak. It enforces
the two-second cold and 250 ms warm-p95 ceilings, one server child, bounded
memory and handles, zero queue depth after quiescence, Document/diagnostic LRU
churn, and graceful stop within five seconds.

`tests/fixtures/stored-state/v1` is the immutable first-release compatibility
seed. `tests/fixtures/stored-state/v0.1.1` explicitly covers v0.1.1 with the
same payloads: the stored types did not change, and both deserialization tests
were run against the immutable v0.1.1 tag before copying its fixtures. The
candidate checks both seeds and exercises legacy recursive-Preview rejection
and legacy Recovery ownership restrictions in the Mutation tests.

Every future stable release must add fixtures and candidate migration coverage
for all earlier releases in the same major version. The JSON digest checker
and Rust deserialization tests prevent accidental format drift.

`release-gates.json` is the authoritative machine-readable list. Candidate
workflows fail if any gate is pending; gates are never represented by empty or
passing placeholder tests.
