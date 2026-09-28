# Research: the first set of Built-in servers

- Ticket: [#64](https://github.com/spiritledsoftware/lspctl/issues/64), a child of map [#62](https://github.com/spiritledsoftware/lspctl/issues/62).
- Base commit: `69520ab` (lspctl 0.1.2).
- Date of research: 2026-09-28.
- Terms: see `CONTEXT.md`. A **Built-in server** is a server declaration and its routes that ship inside lspctl.

## Question

Which servers are in the first set of Built-in servers? For each server: executable name on each OS (with Windows shim forms), stdio args, language IDs and file extensions, root markers, install commands, behavior with no `initialization_options` and no `settings`, Workspace-local install paths, and overlaps. Also: can lspctl copy data from Helix, nvim-lspconfig, OpenCode or mason-registry?

## Answer

Use these 13 servers as the first set:

1. rust-analyzer (reference matrix)
2. typescript-language-server (reference matrix)
3. basedpyright (reference matrix)
4. gopls
5. clangd
6. lua-language-server
7. bash-language-server
8. yaml-language-server
9. taplo
10. marksman
11. zls
12. sourcekit-lsp (Swift only)
13. terraform-ls

Each of these servers answered lspctl 0.1.2 Queries with no `initializationOptions` and with `settings = {}` in a smoke run on macOS (see [Evidence method](#evidence-method)). Only the first 3 are in the reference matrix. The other 10 are not tested in CI.

Do not copy data from the aggregators. Take each fact from the server's own docs or source, and from the VS Code language definitions (MIT). The reasons are in [Prior-art data sources](#prior-art-data-sources-and-licenses).

Two facts are urgent:

- `npm install typescript` now gives TypeScript 7.0.2. TypeScript 7 has no `lib/tsserver.js`. typescript-language-server 6.0.1 then fails in `initialize`. It works only when TypeScript 6 or older is resolvable. See [TypeScript 7](#typescript-7-and-the-typescript-built-in-server).
- lspctl allows only one server for each file extension. Two Built-in servers for the same extension make an `ambiguous` route (see [lspctl constraints](#lspctl-constraints-that-apply)). Thus the first set has exactly one server for each extension.

## Evidence method

- **Primary sources.** Each server's README, docs, source files, and package manifests (npm, PyPI, Homebrew API, `microsoft/winget-pkgs`, `ScoopInstaller/Main`). Links are in the tables and in [Sources](#sources).
- **Smoke run.** On 2026-09-28 (macOS 27.0 arm64), a sub-agent ran lspctl 0.1.2 (`69520ab`) against each candidate. Each run used a user config with only `executable`, `args` and one route. There were no `initialization_options`, and `settings` was `{}`. Thus every `workspace/configuration` item got `null`. HOME and XDG dirs were isolated. Each fixture had 2 files. One file referred to a symbol in the other file. The run did `capabilities`, `workspace-symbols` (before and after a file was opened), `document-symbols`, `hover`, cross-file `definition`, `published-diagnostics` and, where supported, `document-diagnostics`. The run recorded the files that each server wrote into the Workspace. The smoke run is evidence for macOS only. [INFERENCE] Linux and Windows behavior is the same for these Queries, but nobody observed it.
- **Language IDs.** Cross-checked with the LSP 3.17 table in [`_specifications/lsp/3.17/types/textDocumentItem.md@de9a671`](https://github.com/microsoft/language-server-protocol/blob/de9a671ae6ba374cc748a29c1c620cbc536302ff/_specifications/lsp/3.17/types/textDocumentItem.md) and with the VS Code language definitions in [`microsoft/vscode/extensions/*/package.json@ab179c7`](https://github.com/microsoft/vscode/tree/ab179c730eb371cfefe06e576f9ec710856da047/extensions).

## lspctl constraints that apply

These facts come from the code at `69520ab`. They decide what "works with zero config" means.

| Constraint | Evidence | Effect on the set |
|---|---|---|
| A route has one `language_id` and extensions. Each extension must start with `.`. | `src/configuration.rs:1140-1152` | Bare file names (`Dockerfile`, `Makefile`, `Gemfile`) cannot be routed. Multi-dot suffixes (`.tfvars`, `.d.ts`) are valid. |
| Route selection uses the longest matching suffix. If more than one server has that suffix, selection fails. | `src/configuration.rs:1198-1220`, `unique_route_server` at `:1180-1196` | Only one Built-in server can own an extension, unless #70 adds an ordered candidate list. |
| `workspace/configuration` returns the static `settings`. With `settings = {}` each item gets `null`. | `src/session/owner_runtime.rs:2403-2446` | A server must accept `null` sections. |
| Server-to-client requests other than `workspace/configuration`, `workspace/workspaceFolders`, `workspace/diagnostic/refresh`, `workspace/applyEdit`, `window/showMessageRequest` and `window/showDocument` get `MethodNotFound`. This includes `client/registerCapability`. | `src/session/owner_runtime.rs:2381-2401` | Servers that need dynamic registration or custom requests do not work. |
| The capability profile disables dynamic registration. It does not offer `didChangeWatchedFiles`. lspctl has no file watcher. | `assets/contract/initialize-capabilities.json`; ADR 0003 | Servers see changes to files that are not open only through their own watchers. rust-analyzer uses its own watcher in this case (`crates/rust-analyzer/src/config.rs`, `FilesWatcher::Server`). |
| The default server `cwd` is the Workspace. | `src/configuration.rs:708-713` | Wrappers that derive state from the cwd (rustup, jdtls) see the Workspace. |
| A bare executable name resolves with `which::which_in` against the server `PATH`. A path with `/` or `\` is canonicalized. | `src/configuration.rs:443-473` | Built-in servers must use bare names. See [New questions](#new-questions). |
| On Windows, a bare name finds `.cmd` and `.exe` through `PATHEXT`. Bare names must not contain `.`. npm-based servers need `node` on `PATH`. | Resolution of [#65](https://github.com/spiritledsoftware/lspctl/issues/65) | npm shims work on Windows. None of the 13 names contains `.`. |

## Recommended first set

"Tested" means that the reference matrix (`nightly.yml`, `assets/reference-servers.json`) runs the server on Linux, macOS and Windows. "Smoke" is the 2026-09-28 macOS run.

| # | Server | Executable: Linux, macOS | Executable: Windows | Args | Routes (`languageId`: extensions) | Reference-matrix tested | Smoke (macOS, no settings) |
|---|---|---|---|---|---|---|---|
| 1 | rust-analyzer | `rust-analyzer` | `rust-analyzer.exe` | none | `rust`: `.rs` | **Yes** (2026-08-31) | All Queries pass |
| 2 | typescript-language-server | `typescript-language-server` | `typescript-language-server.cmd` (npm; also `.ps1`) | `--stdio` | `typescript`: `.ts .mts .cts`; `typescriptreact`: `.tsx`; `javascript`: `.js .mjs .cjs`; `javascriptreact`: `.jsx` | **Yes** (6.0.0 with TypeScript 6.0.3). CI runs `node cli.mjs`, not the `.cmd` shim. | Pass with TypeScript 6.0.3. **Fails** with TypeScript 7.0.2. Cold `workspace-symbols` gives `No Project.` |
| 3 | basedpyright | `basedpyright-langserver` | `basedpyright-langserver.exe` (pip, in `Scripts\`) or `.cmd` (npm) | `--stdio` | `python`: `.py .pyi` | **Yes** (1.39.10) | All Queries pass |
| 4 | gopls | `gopls` | `gopls.exe` | none (bare `gopls` runs `serve`) | `go`: `.go` | No | All Queries pass |
| 5 | clangd | `clangd` | `clangd.exe` | none | `c`: `.c`; `cpp`: `.cpp .cc .cxx .c++ .hpp .hh .hxx .h++ .h`; `objective-c`: `.m`; `objective-cpp`: `.mm` | No | All Queries pass with `compile_commands.json`. Cold `workspace-symbols` gives `[]`. |
| 6 | lua-language-server | `lua-language-server` | `lua-language-server.exe` | none | `lua`: `.lua` | No | All Queries pass |
| 7 | bash-language-server | `bash-language-server` | `bash-language-server.cmd` (npm) | `start` | `shellscript`: `.sh .bash` | No | All Queries pass |
| 8 | yaml-language-server | `yaml-language-server` | `yaml-language-server.cmd` (npm) | `--stdio` | `yaml`: `.yaml .yml` | No | Partial: symbols, same-file definition, diagnostics. Hover is empty without a schema. No `workspace-symbols`. |
| 9 | taplo | `taplo` | `taplo.exe` | `lsp stdio` | `toml`: `.toml` | No | Partial: symbols, diagnostics, formatting. Hover is empty without a schema. No definition. |
| 10 | marksman | `marksman` | `marksman.exe` | `server` | `markdown`: `.md .markdown` | No | All Queries pass |
| 11 | zls | `zls` | `zls.exe` | none | `zig`: `.zig .zon` | No | All Queries pass. Published diagnostics were empty for a type error. |
| 12 | sourcekit-lsp | `sourcekit-lsp` | `sourcekit-lsp.exe` [INFERENCE] | none | `swift`: `.swift` | No | All Queries pass from the toolchain path or `/usr/bin`. Fails through a mise `latest/bin` symlink. |
| 13 | terraform-ls | `terraform-ls` | `terraform-ls.exe` | `serve` | `terraform`: `.tf`; `terraform-vars`: `.tfvars` | No | All Queries pass. Cold `workspace-symbols` gives `[]`. |

### Language IDs outside the LSP 3.17 table

The LSP 3.17 table includes `rust`, `typescript`, `typescriptreact`, `javascript`, `javascriptreact`, `python`, `go`, `c`, `cpp`, `objective-c`, `objective-cpp`, `lua`, `shellscript`, `yaml`, `markdown` and `swift`. It does not include `toml`, `zig`, `terraform` or `terraform-vars`. These 4 IDs come from the servers' own docs:

- `toml`: taplo. The smoke run used `toml`, and taplo answered.
- `zig`: zls. The smoke run used `zig`, and zls answered.
- `terraform` and `terraform-vars`: [terraform-ls `docs/USAGE.md`](https://github.com/hashicorp/terraform-ls/blob/main/docs/USAGE.md). It also defines `terraform-stack` (`.tfcomponent.hcl`), `terraform-deploy` (`.tfdeploy.hcl`), `terraform-search` (`.tfquery.hcl`), `terraform-policy` (`.policy.hcl`) and `terraform-policytest` (`.policytest.hcl`). The same doc says: do not send `*.tf.json`, `*.tfvars.json` or other HCL files. The first set leaves out the `.hcl` routes. lspctl can route them, because the longest suffix wins. But no smoke run tested them.

### Extension choices

- The extensions follow the VS Code language definitions, which are MIT. Examples: `.h` is `cpp` in [`extensions/cpp/package.json`](https://github.com/microsoft/vscode/blob/ab179c730eb371cfefe06e576f9ec710856da047/extensions/cpp/package.json). `.mts` and `.cts` are `typescript` in `extensions/typescript-basics/package.json`.
- The first set leaves out rare or ambiguous suffixes: `.es6`, `.pac`, `.i`, `.inc`, `.command`, `.zsh` and dot-file names such as `.bashrc`. `.inc` is in bash-language-server's `globPattern`, but C and PHP also use it.
- typescript-language-server does not guess `.mts`, `.cts`, `.mjs` or `.cjs` from the URI (`src/document.ts`, `getModeFromFileUri`). Thus the route must send the correct `languageId`.

## Details per server

### Stdio args and "no settings" behavior

| Server | Args source | No `initializationOptions`, `null` settings | Workspace writes and side effects (smoke) |
|---|---|---|---|
| rust-analyzer | [Other editors](https://rust-analyzer.github.io/book/other_editors.html): run bare `rust-analyzer` | Works. Every setting has a default ([configuration](https://rust-analyzer.github.io/book/configuration.html)). With no dynamic registration, it uses its own watcher (`config.rs`, `FilesWatcherDef::Client` falls back to `FilesWatcher::Server`). | Writes `Cargo.lock` and `target/` (`cargo check`). |
| typescript-language-server | README: `--stdio` is "required" | Works only if it finds TypeScript with `lib/tsserver.js`. Order: `initializationOptions.tsserver.path`, then the Workspace `node_modules/typescript`, then `require.resolve('typescript')` from its own install (`src/tsServer/versionProvider.ts`). Else `initialize` fails with "Could not find a valid TypeScript installation". | None |
| basedpyright | [language-server-settings.md](https://github.com/DetachHead/basedpyright/blob/a8a92cd35f7b1f683ebed3ad573c4aacefa79bc6/docs/configuration/language-server-settings.md): `basedpyright-langserver --stdio` | Works. With no settings, it uses `./.venv` if it exists, else `python` on `PATH` ([import-resolution.md:76-84](https://github.com/DetachHead/basedpyright/blob/a8a92cd35f7b1f683ebed3ad573c4aacefa79bc6/docs/usage/import-resolution.md), [better-defaults.md](https://github.com/DetachHead/basedpyright/blob/a8a92cd35f7b1f683ebed3ad573c4aacefa79bc6/docs/benefits-over-pyright/better-defaults.md)). | None |
| gopls | [cmd package doc](https://pkg.go.dev/golang.org/x/tools/gopls/internal/cmd): with no args, gopls runs `serve` | Works. All settings have defaults ([go.dev/gopls](https://go.dev/gopls/)). gopls has no default server-side watcher (`fileWatcher` defaults to `off`, [v0.22.0 notes](https://go.dev/gopls/release/v0.22.0)). Thus it does not see disk changes to files that are not open. | None |
| clangd | [installation](https://clangd.llvm.org/installation): bare `clangd` | Works. Without a compilation database it uses fallback flags. Cross-file results need `compile_commands.json` or `compile_flags.txt` ([design/compile-commands](https://clangd.llvm.org/design/compile-commands)). | None seen |
| lua-language-server | [Getting Started](https://github.com/LuaLS/lua-language-server/wiki/Getting-Started): no transport flag | Works. Config order: `--configpath`, then `.luarc.json`, then client settings. Defaults apply when all are absent. | None |
| bash-language-server | `server/src/cli.ts`: only `start`; stdio is hard-coded | Works. All options are optional (`server/src/config.ts`). `shellcheck` and `shfmt` are optional. Without them, lint diagnostics and formatting are not available. | None |
| yaml-language-server | README and `src/server.ts`: `--stdio` | Works. `setConfiguration` skips the block when `settings.yaml` is `null` (`src/languageserver/handlers/settingsHandlers.ts`). `schemaStoreEnabled` defaults to `true`. Thus the server downloads the SchemaStore catalog over the network. | Network: SchemaStore catalog |
| taplo | [language-server docs](https://taplo.tamasfe.dev/cli/usage/language-server.html): `taplo lsp stdio` | Works. The `lsp` feature is on by default in `taplo-cli` 0.10.0 (`crates/taplo-cli/Cargo.toml`). The npm build has no LSP. | Network: schema catalog fetch (log: `failed to fetch catalog`) |
| marksman | [docs/features.md](https://github.com/artempyanykh/marksman/blob/main/docs/features.md), README vim example: `server` | Works. It falls back to single-file mode without a root. | None |
| zls | README: bare `zls` | Works. `zig_exe_path` unset means `zig` from `PATH` (`src/Config.zig`). Zig and ZLS versions must match ([install](https://zigtools.org/zls/install/)). | Writes `.zig-cache/` |
| sourcekit-lsp | [Editor Integration.md](https://github.com/swiftlang/sourcekit-lsp/blob/main/Documentation/Editor%20Integration.md): stdin and stdout, no flags | Works. The README says that cross-module features need a recent build or background indexing. | Writes `.build/index-build` |
| terraform-ls | [docs/USAGE.md](https://github.com/hashicorp/terraform-ls/blob/main/docs/USAGE.md): `serve` | Works. All settings are optional ([docs/SETTINGS.md](https://github.com/hashicorp/terraform-ls/blob/main/docs/SETTINGS.md)). Since 0.29 it indexes each directory with `*.tf` files. | None |

### Root markers

Root markers only select a server. They never move the Workspace root (map #62).

| Server | Markers that the server itself uses or documents | Source |
|---|---|---|
| rust-analyzer | `Cargo.toml`, `rust-project.json` | [configuration](https://rust-analyzer.github.io/book/configuration.html), [non-Cargo projects](https://rust-analyzer.github.io/book/non_cargo_based_projects.html) |
| typescript-language-server | `tsconfig.json`, `jsconfig.json` (tsserver searches up from each file). `package.json` is a common extra marker, but the server does not use it. | [tsserver wiki](https://github.com/microsoft/TypeScript/wiki/Standalone-Server-(tsserver)) |
| basedpyright | `pyrightconfig.json`, `pyproject.toml`. OpenCode also uses `setup.py`, `setup.cfg`, `requirements.txt`, `Pipfile`, but pyright does not document these. | [pyright configuration.md](https://github.com/microsoft/pyright/blob/main/docs/configuration.md); OpenCode `server.ts:488` |
| gopls | `go.work`, then `go.mod` | [go.dev/gopls/workspace](https://go.dev/gopls/workspace) |
| clangd | `compile_commands.json` (also in `build/`), `compile_flags.txt`, `.clangd` | [clangd config](https://clangd.llvm.org/config), [compile-commands](https://clangd.llvm.org/design/compile-commands) |
| lua-language-server | `.luarc.json` | [Configuration File](https://github.com/LuaLS/lua-language-server/wiki/Configuration-File) |
| bash-language-server | None | `server/src/config.ts` |
| yaml-language-server | None | README "Schema resolution priority" |
| taplo | `.taplo.toml`, `taplo.toml` (config search) | `crates/taplo-lsp/src/config.rs` |
| marksman | `.marksman.toml`, or a VCS root such as `.git` | `docs/features.md` |
| zls | `build.zig` | [per-build config](https://zigtools.org/zls/configure/per-build/) |
| sourcekit-lsp | `Package.swift` (also `compile_commands.json`, but clangd owns that marker in this set) | [README](https://github.com/swiftlang/sourcekit-lsp/blob/main/README.md) |
| terraform-ls | No marker file. It indexes directories with `*.tf`. [INFERENCE] `.terraform.lock.hcl` is a usable marker, because `terraform init` writes it. | `docs/SETTINGS.md` |

### Install commands

"—" means that the manager has no package, or that we found none. Package names in `winget` and `scoop` were checked in `microsoft/winget-pkgs` and `ScoopInstaller/Main` on 2026-09-28. Homebrew names were checked with the `formulae.brew.sh` API.

| Server | brew | npm | pip, pipx, uv | go, cargo, rustup | winget | scoop (Main) | apt | Other |
|---|---|---|---|---|---|---|---|---|
| rust-analyzer | `rust-analyzer` | — | — | `rustup component add rust-analyzer` | `Rustlang.rust-analyzer` | `rust-analyzer` | `rust-analyzer` (Debian trixie and later) | [GitHub releases](https://github.com/rust-lang/rust-analyzer/releases) |
| typescript-language-server | `typescript-language-server` (depends on brew `typescript`, which is 7.0.2) | `npm i -g typescript-language-server typescript@6` | — | — | — | — | — | — |
| basedpyright | `basedpyright` | `npm i -g basedpyright` | `pip install basedpyright`, `pipx install basedpyright`, `uv tool install basedpyright` | — | — | — | — | PyPI wheel includes Node through `nodejs-wheel-binaries` |
| gopls | `gopls` | — | — | `go install golang.org/x/tools/gopls@latest` | — | — | `gopls` (Ubuntu universe, Debian) | — |
| clangd | `llvm` (keg-only, so not on `PATH` by default: `keg_only: provided_by_macos` in the formula API) | — | (third-party `clangd` wheel) | — | `LLVM.LLVM` | `clangd`, `llvm` | `clangd` | [clangd releases](https://github.com/clangd/clangd/releases); Xcode CLT has `/usr/bin/clangd` (observed in smoke) |
| lua-language-server | `lua-language-server` | — | — | — | `LuaLS.lua-language-server` | `lua-language-server` | — | [GitHub releases](https://github.com/LuaLS/lua-language-server/releases) |
| bash-language-server | `bash-language-server` | `npm i -g bash-language-server` | — | — | — | — | — | snap `bash-language-server --classic`; dnf `nodejs-bash-language-server` |
| yaml-language-server | `yaml-language-server` | `npm i -g yaml-language-server` | — | — | — | — | — | — |
| taplo | `taplo` | (no LSP in npm build) | — | `cargo install --locked taplo-cli` | `tamasfe.taplo` | `taplo` | — | — |
| marksman | `marksman` | — | — | — | `Artempyanykh.Marksman` | `marksman` | — | snap; [GitHub releases](https://github.com/artempyanykh/marksman/releases) |
| zls | `zls` (depends on `zig`) | — | — | — | `zigtools.zls` | `zls` | — | [zigtools.org/zls/install](https://zigtools.org/zls/install/) |
| sourcekit-lsp | (part of Xcode or the Swift toolchain) | — | — | — | `Swift.Toolchain` | — | — | Linux: `swiftly init` ([swift.org/install/linux](https://www.swift.org/install/linux/)) |
| terraform-ls | `terraform-ls` (core) or `hashicorp/tap/terraform-ls` | — | — | — | `Hashicorp.TerraformLanguageServer` | `terraform-ls` | `terraform-ls` (HashiCorp apt repo) | [releases.hashicorp.com](https://releases.hashicorp.com/terraform-ls/) |

Suggested single install hint for each OS (for #73). Use a command that exists on that OS:

| Server | macOS | Linux | Windows |
|---|---|---|---|
| rust-analyzer | `rustup component add rust-analyzer` | `rustup component add rust-analyzer` | `rustup component add rust-analyzer` |
| typescript-language-server | `npm i -g typescript-language-server typescript@6` | same | same |
| basedpyright | `uv tool install basedpyright` | same | same |
| gopls | `go install golang.org/x/tools/gopls@latest` | same | same |
| clangd | `xcode-select --install` or `brew install llvm` | `apt install clangd` | `winget install LLVM.LLVM` |
| lua-language-server | `brew install lua-language-server` | GitHub release archive | `winget install LuaLS.lua-language-server` |
| bash-language-server | `npm i -g bash-language-server` | same | same |
| yaml-language-server | `npm i -g yaml-language-server` | same | same |
| taplo | `brew install taplo` | `cargo install --locked taplo-cli` | `winget install tamasfe.taplo` |
| marksman | `brew install marksman` | GitHub release binary | `winget install Artempyanykh.Marksman` |
| zls | `brew install zls` | [zigtools.org/zls/install](https://zigtools.org/zls/install/) | `winget install zigtools.zls` |
| sourcekit-lsp | `xcode-select --install` | `swiftly init` | `winget install Swift.Toolchain` |
| terraform-ls | `brew install terraform-ls` | HashiCorp apt repo: `apt install terraform-ls` | `winget install Hashicorp.TerraformLanguageServer` |

### Workspace-local install paths

A Workspace-local executable needs a Trust grant (ADR 0002, map #62).

| Server | Conventional Workspace-local path | Documented preference for a project-local version |
|---|---|---|
| typescript-language-server | `node_modules/.bin/typescript-language-server` (Windows: `node_modules\.bin\typescript-language-server.cmd`) | The server prefers the Workspace `node_modules/typescript` for tsserver (`versionProvider.ts`). The smoke run confirmed this: a global server with TypeScript 7 worked when the Workspace had TypeScript 6. |
| basedpyright | `.venv/bin/basedpyright-langserver` (Windows: `.venv\Scripts\basedpyright-langserver.exe`); also `node_modules/.bin` | Yes. The VS Code extension's `importStrategy` "defaults to looking in your project for the basedpyright pypi package" ([pypi-package-vscode-pinning.md](https://github.com/DetachHead/basedpyright/blob/a8a92cd35f7b1f683ebed3ad573c4aacefa79bc6/docs/benefits-over-pyright/pypi-package-vscode-pinning.md)). |
| bash-language-server, yaml-language-server | `node_modules/.bin/<name>` | Not documented |
| rust-analyzer | None. The rustup proxy reads `rust-toolchain.toml` in the cwd, which is the Workspace ([rustup overrides](https://rust-lang.github.io/rustup/overrides.html)). | Indirect: the proxy runs the project's toolchain. If that toolchain lacks the component, the proxy exits with an error ([rustup#3846](https://github.com/rust-lang/rustup/issues/3846)). |
| gopls, clangd, lua-language-server, taplo, marksman, zls, sourcekit-lsp, terraform-ls | None | Not documented. lua-language-server must not be symlinked onto `PATH`, because it finds its scripts relative to its own directory ([Getting Started](https://github.com/LuaLS/lua-language-server/wiki/Getting-Started)). |

## Overlaps

| Language | Servers | Choice for the first set | Reason |
|---|---|---|---|
| TypeScript, JavaScript | typescript-language-server, `tsc --lsp --stdio` (TypeScript 7), vtsls, `deno lsp` | typescript-language-server | It is in the reference matrix. See [TypeScript 7](#typescript-7-and-the-typescript-built-in-server). |
| Python | basedpyright, pyright, ty, ruff, python-lsp-server, jedi-language-server | basedpyright | Reference matrix. It uses `./.venv` by default. The PyPI package includes Node. Zed uses basedpyright as its default ([ty editors doc](https://docs.astral.sh/ty/editors/)). |
| C, C++, Objective-C | clangd, sourcekit-lsp | clangd for `.c .cpp .h .m .mm` and related suffixes; sourcekit-lsp for `.swift` only | clangd needs no Swift toolchain. sourcekit-lsp also claims C-family files ([Editor Integration.md](https://github.com/swiftlang/sourcekit-lsp/blob/main/Documentation/Editor%20Integration.md)). |
| Ruby | ruby-lsp, solargraph | None in the first set | See [Excluded servers](#excluded-servers). |
| Markdown | marksman, vscode-markdown-language-server | marksman | vscode-markdown-language-server needs VS Code-only requests (smoke). |

## TypeScript 7 and the TypeScript Built-in server

- The npm `latest` tag of `typescript` is 7.0.2 (published 2026-07-08, `npm view typescript dist-tags`). The Homebrew `typescript` formula is also 7.0.2.
- The TypeScript 7.0.2 tarball has no `tsserver` file. The 6.0.3 tarball has `package/lib/tsserver.js` (checked with `tar tz` on the npm tarballs).
- typescript-language-server needs `tsserver.js`. With only TypeScript 7 resolvable, `initialize` fails (smoke row `tsls-ts7`).
- The Homebrew formula links brew's `typescript` into the server's `node_modules` ([formula](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript-language-server.rb)). [INFERENCE] `brew install typescript-language-server` therefore fails in the same way when the Workspace has no TypeScript 6.
- TypeScript 7 has its own server: `tsc --lsp --stdio`. `--stdio` is required, and other transports print "only stdio is supported" ([`cmd/tsgo/lsp.go@89d5d5b`](https://github.com/microsoft/typescript-go/blob/89d5d5b2849a0db0957065889ca58536fa6d2e4a/cmd/tsgo/lsp.go), `main.go` handles `--lsp`). It passed all smoke Queries. Its diagnostics are pull-only. The typescript-go README still lists the language service as "in progress".
- `tsc` is a bad bare name for a Built-in server. TypeScript 6 and older also install `tsc`, but that `tsc` has no `--lsp`. A name on `PATH` does not tell lspctl which version it is.

Recommendation: keep typescript-language-server as the Built-in server for 0.2.0, because it is the only tested TypeScript server. Make its install hint `typescript@6`. Open a follow-up ticket to decide about TypeScript 7.

## Excluded servers

| Server | Reason |
|---|---|
| pyright (`pyright-langserver --stdio`) | Same engine and routes as basedpyright. Passed smoke. It is a good second candidate if #70 adds ordered candidates. The PyPI `pyright` wrapper downloads Node at first run ([pypi.org/project/pyright](https://pypi.org/project/pyright/)). |
| vtsls (`vtsls --stdio`) | Overlaps typescript-language-server. It bundles TypeScript 5.9.3 (smoke), which is older than the reference 6.0.3. Single maintainer. The README calls it "best-effort". Last release 0.3.0 on 2025-12-24. |
| `tsc --lsp --stdio` (TypeScript 7) | Overlaps. The name `tsc` is ambiguous. See above. |
| deno (`deno lsp`) | Needs `enable` in `initializationOptions` or settings. It stays off without it ([deno lsp docs](https://docs.deno.com/runtime/reference/cli/lsp/)). Also overlaps TypeScript routes. |
| vscode-json-language-server (`--stdio`) | Third-party repackage with 1 maintainer ([vscode-langservers-extracted](https://github.com/hrsh7th/vscode-langservers-extracted)). In lspctl today, only `document-symbols` gives a useful result: hover is empty without a schema, there is no definition, and `document-diagnostics` fails because of the `documentSelector: null` problem (see [New questions](#new-questions)). Formatting needs `initializationOptions.provideFormatter: true` (`jsonServer.ts`). Reconsider after the diagnostics fix. |
| vscode-css-language-server, vscode-html-language-server | Same package and the same diagnostics problem. html-ls logs an unhandled exception on `client/registerCapability` (smoke). |
| vscode-markdown-language-server | Every Query fails: `Unsupported server request: markdown/parse` (smoke). |
| vscode-eslint-language-server | Needs settings. Pull diagnostics fail with `The "path" argument must be of type string` (smoke). |
| ruby-lsp | With a `Gemfile`, it runs `bundle install` over the network and writes `.ruby-lsp/` into the Workspace (`lib/ruby_lsp/setup_bundler.rb`). It exits during `initialize` if there is a `Gemfile` but no `Gemfile.lock` (smoke). Version 0.x. |
| solargraph (`solargraph stdio`) | Overlaps ruby-lsp. Community use moved to ruby-lsp. |
| ruff (`ruff server`) | Lint and format only. No definition, symbols or general hover ([features](https://docs.astral.sh/ruff/editors/features/), smoke). A second `.py` route would be ambiguous. |
| ty (`ty server`) | Beta. "ty uses 0.0.x versioning" ([README](https://github.com/astral-sh/ty/blob/main/README.md)). |
| python-lsp-server, jedi-language-server | Jedi-based. Weaker type inference. Useful features need plugins and config. |
| jdtls | Needs a unique `-data` directory for each Workspace (README). Static args cannot give that. The `jdtls` Python wrapper derives the default from the **base name** of the cwd only (`sha1(os.path.basename(os.getcwd()))`, [`jdtls.py:83-109@b484381`](https://github.com/eclipse-jdtls/eclipse.jdt.ls/blob/b4843814510c5a45c2eb1b6d9b551e9a99966c08/org.eclipse.jdt.ls.product/scripts/jdtls.py#L83-L109)). Two Workspaces with the same directory name share one data directory. Also needs Java 21 or later. |
| dockerfile-language-server (`docker-langserver --stdio`) | The file name `Dockerfile` has no extension. lspctl cannot route it. |
| kotlin-lsp | Partly closed source. Pre-release. Large install. |
| intelephense | Proprietary ([intelephense.com/docs](https://intelephense.com/docs)). |
| phpactor, elixir-ls, haskell-language-server, ocamllsp, csharp-ls, Roslyn LSP | Not smoke-tested. Each needs a language toolchain or a project build model. Candidates for a later set. |

## Prior-art data sources and licenses

lspctl is `MIT OR Apache-2.0` (`Cargo.toml:9`).

| Source | License | Can lspctl copy the data? |
|---|---|---|
| Helix `languages.toml` ([LICENSE@079a789](https://github.com/helix-editor/helix/blob/079a789e8cb08ead67f19e1971a1b7438b37354b/LICENSE)) | MPL-2.0 | Do not copy. MPL-2.0 is file-level copyleft. Copied parts stay under MPL-2.0 and need source availability. Thus a copied data file cannot be `MIT OR Apache-2.0`. |
| nvim-lspconfig ([LICENSE.md@a9bb4d5](https://github.com/neovim/nvim-lspconfig/blob/a9bb4d5f4276aa5cddc015faa3ecc5a7b5a26b14/LICENSE.md)) | Apache-2.0 | Possible with attribution. [INFERENCE] Copied parts are Apache-2.0 only, so a user cannot take them under the MIT option. Avoid it. |
| OpenCode ([LICENSE@083ed26](https://github.com/anomalyco/opencode/blob/083ed266e058dc3d2d1b377ff5540859d79de110/LICENSE); list in `packages/opencode/src/lsp/server.ts`; docs at [opencode.ai/docs/lsp](https://opencode.ai/docs/lsp/)) | MIT | Allowed with the copyright notice. OpenCode installs servers automatically, which lspctl does not do. Use it only as a cross-check. |
| mason-registry ([LICENSE@2ee949a](https://github.com/mason-org/mason-registry/blob/2ee949accc537ffea6a867903d61a2a5a80951b6/LICENSE)) | Apache-2.0 | Same as nvim-lspconfig. Its data is mostly download recipes. lspctl does not install servers. |
| VS Code language definitions ([microsoft/vscode](https://github.com/microsoft/vscode)) | MIT | Allowed with the notice. Best source for language ID and extension maps. |
| LSP specification ([License.txt](https://github.com/microsoft/language-server-protocol/blob/gh-pages/License.txt)) | CC-BY-4.0 | Cite it. It is the list of known language IDs. |

[INFERENCE] Plain facts (an executable name, an extension, an argument) are not normally protected by copyright. A copied file, or the selection and arrangement of a large table, can be. This is not legal advice. The safe rule: write each Built-in declaration from the server's own docs, cite that source, and use aggregators only to find leads.

## New questions

1. **`document-diagnostics` refuses `diagnosticProvider.documentSelector: null`.** `src/query.rs:349-352` takes `documentSelector` from the options, and `selector_matches` (`src/query.rs:913-916`) returns `false` for a non-array. LSP 3.17 says that `null` means "use the client-side document selector". Result: `capabilities` says diagnostics are supported, but `document-diagnostics` gives `capability_unavailable` for vscode-json/css/html-language-server and ruby-lsp. `lspctl raw --method textDocument/diagnostic` works for the same servers (smoke).
2. **An executable path with `/` is canonicalized.** `src/configuration.rs:451-458` calls `dunce::canonicalize`. A multicall shim then runs as its target. Example: `~/.cargo/bin/rust-analyzer` is a symlink to `rustup`. With that absolute path, lspctl starts `rustup` itself, and `initialize` fails. A bare name works. This affects Workspace-local paths (#69).
3. **TypeScript 7.** Should the TypeScript Built-in server change from typescript-language-server to `tsc --lsp --stdio`, or should both be ordered candidates? How does lspctl tell TypeScript 7 `tsc` from older `tsc`?
4. **Cold `workspace-symbols`.** clangd, terraform-ls, vtsls and tsgo return `[]`, and typescript-language-server returns `No Project.`, until a Document is open (smoke). Phase 3 selects a server for Workspace-wide commands. It may need to open one file first.
5. **Network and Workspace writes.** yaml-language-server and taplo fetch schema catalogs by default. rust-analyzer, zls and sourcekit-lsp write build directories into the Workspace. Should Built-in declarations set static `settings` to turn off network fetches (for example `yaml.schemaStore.enable = false`)?

## Sources

Server sources (primary):

- rust-analyzer: [book](https://rust-analyzer.github.io/book/), [releases](https://github.com/rust-lang/rust-analyzer/releases), [rustup proxies](https://rust-lang.github.io/rustup/concepts/proxies.html).
- typescript-language-server: [repo](https://github.com/typescript-language-server/typescript-language-server) (`src/tsServer/versionProvider.ts`, `src/document.ts`, README), [Homebrew formula](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript-language-server.rb).
- TypeScript 7: [typescript-go@89d5d5b](https://github.com/microsoft/typescript-go/tree/89d5d5b2849a0db0957065889ca58536fa6d2e4a) (`cmd/tsgo/main.go`, `cmd/tsgo/lsp.go`, README), [announcement](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0/).
- basedpyright: [repo@a8a92cd](https://github.com/DetachHead/basedpyright/tree/a8a92cd35f7b1f683ebed3ad573c4aacefa79bc6) (`docs/`, `pyproject.toml`).
- pyright: [docs](https://github.com/microsoft/pyright/tree/main/docs), [PyPI wrapper](https://pypi.org/project/pyright/).
- gopls: [go.dev/gopls](https://go.dev/gopls/), [workspace](https://go.dev/gopls/workspace), [v0.22.0](https://go.dev/gopls/release/v0.22.0).
- clangd: [installation](https://clangd.llvm.org/installation), [config](https://clangd.llvm.org/config), [compile-commands](https://clangd.llvm.org/design/compile-commands).
- lua-language-server: [wiki](https://github.com/LuaLS/lua-language-server/wiki/Getting-Started).
- bash-language-server: [repo](https://github.com/bash-lsp/bash-language-server) (`server/src/cli.ts`, `server/src/config.ts`).
- yaml-language-server: [repo](https://github.com/redhat-developer/yaml-language-server) (`src/server.ts`, `src/languageserver/handlers/settingsHandlers.ts`).
- vscode-langservers-extracted: [repo](https://github.com/hrsh7th/vscode-langservers-extracted); upstream `extensions/json-language-features/server/src/jsonServer.ts` in [microsoft/vscode](https://github.com/microsoft/vscode).
- taplo: [language server docs](https://taplo.tamasfe.dev/cli/usage/language-server.html), `crates/taplo-cli/Cargo.toml`.
- marksman: [docs](https://github.com/artempyanykh/marksman/tree/main/docs).
- zls: [repo](https://github.com/zigtools/zls), [zigtools.org/zls](https://zigtools.org/zls/).
- sourcekit-lsp: [repo](https://github.com/swiftlang/sourcekit-lsp), [swift.org/install](https://www.swift.org/install/).
- terraform-ls: [docs/USAGE.md](https://github.com/hashicorp/terraform-ls/blob/main/docs/USAGE.md), [docs/SETTINGS.md](https://github.com/hashicorp/terraform-ls/blob/main/docs/SETTINGS.md).
- ruby-lsp: [repo](https://github.com/Shopify/ruby-lsp) (`exe/ruby-lsp`, `lib/ruby_lsp/setup_bundler.rb`, `lib/ruby_lsp/server.rb`).
- ruff: [editor features](https://docs.astral.sh/ruff/editors/features/). ty: [editors](https://docs.astral.sh/ty/editors/).
- jdtls: [eclipse.jdt.ls@b484381](https://github.com/eclipse-jdtls/eclipse.jdt.ls/tree/b4843814510c5a45c2eb1b6d9b551e9a99966c08).
- Package indexes: [formulae.brew.sh](https://formulae.brew.sh/), [microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs/tree/master/manifests), [ScoopInstaller/Main](https://github.com/ScoopInstaller/Main/tree/master/bucket), [npm registry](https://registry.npmjs.org/), [PyPI](https://pypi.org/).

lspctl sources at `69520ab`: `src/configuration.rs`, `src/query.rs`, `src/session/owner_runtime.rs`, `assets/contract/initialize-capabilities.json`, `assets/reference-servers.json`, `.github/workflows/nightly.yml`, `scripts/release/run_reference_matrix.py`, `docs/adr/0003-synchronize-filesystem-snapshots-without-a-watcher.md`.
