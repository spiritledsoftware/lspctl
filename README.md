# lspctl

[![Crates.io](https://img.shields.io/crates/v/lspctl.svg)](https://crates.io/crates/lspctl)
[![Acceptance](https://github.com/spiritledsoftware/lspctl/actions/workflows/ci.yml/badge.svg)](https://github.com/spiritledsoftware/lspctl/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/lspctl.svg)](LICENSE-MIT)

**Language-server code intelligence for coding agents and shell scripts.**

`lspctl` lets tools query installed Language Server Protocol servers without
embedding an editor. It returns one versioned JSON object per invocation and
keeps language servers warm between calls.

- Query definitions, references, hover, symbols, and diagnostics.
- Preview renames, formatting, code actions, and other filesystem changes
  before applying them.
- Reuse long-lived server sessions while keeping each CLI call scriptable.
- Inspect the complete command and output contract offline.
- Run on Linux, macOS, and Windows with any compatible language server.

## See lspctl in action

![Animated side-by-side terminal demo: baseline Pi searches Tokio source while Pi with lspctl inspects and applies a five-reference semantic rename Preview](assets/demo/lspctl-agent-rename.webp)

[Download the silent MP4](assets/demo/lspctl-agent-rename.mp4?raw=1).

Both Agents received the same prompt, model, Tokio commit, and prepared dependency state; the treatment added the released `lspctl` CLI and bundled skill, with rust-analyzer indexed before recording. [Read the methodology and sanitized transcripts](docs/demo.md).

## Install

### Homebrew

```sh
brew install spiritledsoftware/tap/lspctl
```

### Install script (Linux and macOS)

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/spiritledsoftware/lspctl/main/install.sh | sh
```

The script installs the latest release to `~/.local/bin` after verifying its
SHA-256 checksum. Set `LSPCTL_INSTALL_DIR` to choose another directory.

### Install script (Windows PowerShell)

```powershell
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; irm https://raw.githubusercontent.com/spiritledsoftware/lspctl/main/install.ps1 | iex
```

The script installs the x86-64 release to `%LOCALAPPDATA%\Programs\lspctl` after
verifying its SHA-256 checksum.

### Cargo

Install from [crates.io](https://crates.io/crates/lspctl) with Rust 1.89 or newer:

```sh
cargo install lspctl --locked
```

Verify the installation with `lspctl version | jq`. Prebuilt archives and
SHA-256 checksums are also available on the
[Releases page](https://github.com/spiritledsoftware/lspctl/releases).

`lspctl` launches language servers but does not install them. Install the server
you want to use separately. The examples below also use
[`jq`](https://jqlang.org/) to format and select JSON output.

## Quick start

### 1. Configure a language server

Find the native user configuration path:

```sh
lspctl schema config user | jq -r '.result.resolvedPath'
```

Create that file with a server declaration and route. For example, if
`rust-analyzer` is on `PATH`:

```toml
version = 1
default_server = "rust"

routes = [
  { server = "rust", language_id = "rust", extensions = [".rs"] },
]

[servers.rust]
executable = "rust-analyzer"
```

### 2. Query your code

Run these from a Rust workspace:

```sh
lspctl capabilities --workspace . --server rust | jq '.result'

lspctl definition --workspace . --server rust \
  --file src/main.rs --line 12 --column 8 | jq '.result'

lspctl references --workspace . --server rust \
  --file src/main.rs --line 12 --column 8 \
  --include-declaration true | jq '.result'
```

Source lines and columns are zero-based Unicode-scalar positions. A server may
answer an early navigation query before background indexing finishes. Inspect
progress and retry after it clears when an expected result is empty:

```sh
lspctl session status --workspace . --server rust | jq '.result.progress'
```

## Preview and apply changes

Mutation queries never edit files directly. They persist an immutable Preview
for inspection:

```sh
preview_id=$(
  lspctl rename --workspace . --server rust \
    --file src/main.rs --line 12 --column 8 --new-name replacement |
  jq -r '.result.previewId'
)

lspctl preview show "$preview_id" | jq -r '.result.diff'
lspctl apply "$preview_id" | jq
```

Before applying, `lspctl` rechecks the inspected Preview against the Workspace,
server configuration, authorization, and filesystem state. Every completed
Application records a durable Receipt; stale changes have no force-apply path.

## Project configuration and Trust

A repository may commit a `.lspctl.toml` using the same `version`, `routes`, and
`servers` structure as the user configuration. Project-controlled server launch
fields require an explicit, declaration-bound Trust grant before execution.

When Trust is required, inspect the structured `project_trust_required` error
and run its `error.data.requiredCommand` only after approving the reported
Workspace, server, and declaration digest. User configuration and explicit CLI
launch fields do not require a project Trust grant.

## Discover the JSON contract

The CLI is JSON-only, including help and errors:

```sh
lspctl help | jq                         # compact command index
lspctl help definition | jq              # one command
lspctl schema definition | jq            # exact input and output schemas
lspctl schema --full > lspctl-schema.json  # complete registry for tooling
```

Branch on stable fields such as `ok`, `error.code`, `error.retry`, and
`error.delivery` rather than parsing messages. The installed binary's schema is
the source of truth for its command syntax.

## Install the companion agent skill

Install the bundled workflow guidance into the current repository:

```sh
lspctl skill install
```

This writes `.agent/skills/lspctl`. Use `lspctl skill install --global` to install
it under your home directory instead. Existing unmanaged files are not replaced
without `--replace`.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features fake-server -- -D warnings
cargo test --locked --all-targets --features fake-server
```

See the [v0.1.2 release notes](docs/releases/v0.1.2.md) for the tested platform
floors, reference-server versions, and artifact provenance.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
