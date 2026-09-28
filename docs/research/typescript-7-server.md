# Research: TypeScript 7 native language server as a Built-in server

- Ticket: [#78](https://github.com/spiritledsoftware/lspctl/issues/78), a child of map [#62](https://github.com/spiritledsoftware/lspctl/issues/62).
- Base commit: `69520ab` (lspctl 0.1.2).
- Date of research: 2026-09-28. Host: macOS 27.0 arm64, Node v26.10.0.
- Terms: see `CONTEXT.md`. A **Built-in server** is a server declaration and its routes that ship inside lspctl.
- Prior findings: [`docs/research/builtin-server-set.md`](https://github.com/spiritledsoftware/lspctl/blob/research/builtin-server-set/docs/research/builtin-server-set.md) (#64).

## Question

npm `typescript@latest` is 7.0.2. It has no `lib/tsserver.js`. typescript-language-server 6.0.1 cannot initialize unless TypeScript 6 or older is resolvable. Homebrew links typescript 7.0.2. Find out:

1. Does `tsc --lsp --stdio` (TypeScript 7) work with the lspctl capability profile? Which Queries work? What are the executable name, the args and the Windows form?
2. How can lspctl tell TypeScript 7 `tsc` from an older `tsc` without a shell? What does each method cost?
3. Does typescript-language-server have a release or a plan for TypeScript 7? What does vtsls do?
4. What is the preference order and the detection rule for the TypeScript and JavaScript Built-in server?

## Answer

1. **Yes.** TypeScript 7.0.2 `tsc --lsp --stdio` passed 12 of 13 smoke steps with lspctl 0.1.2, with no `initialization_options` and with `settings = {}`. It passed through the npm Node launcher, through the native executable, through a Workspace-local npm install and through a Workspace-local pnpm install. `published-diagnostics` gives `[]`. Use `document-diagnostics` for this server. The executable is `tsc`. The args are `--lsp` and `--stdio`, in this order. On Windows, npm gives `tsc.cmd`, and the native executable is `tsc.exe`.
2. **Read the `package.json` of the `typescript` package, and use a `--version` probe as the fallback.** A `package.json` read starts no process, so it can occur before a Trust grant. It works for npm, pnpm and Homebrew layouts. It does not work for version-manager shims. The probe `tsc --version` works for all layouts. It costs one process start (10 ms native, 40–110 ms through Node). It runs the executable, so a Workspace-local probe needs a Trust grant first. nvim-lspconfig uses the probe (major version 7 or higher).
3. **No.** typescript-language-server has no release and no published plan for TypeScript 7. Its README says that TypeScript 7 "will hopefully supersede this project" and tells users to install `typescript@6`. Version 6.0.0 only adds a warning when the Workspace TypeScript has no `tsserver.js`. vtsls bundles TypeScript 5.9.3, removed its `useTsgo` option, and its maintainer tells users to use tsgo directly.
4. **Make two Built-in servers for the same TypeScript and JavaScript routes, with a gate on each one.** The Workspace TypeScript version decides the order. See [Recommended preference order](#recommended-preference-order-and-detection-rule). This needs the ordered-candidate selection that "How does lspctl pick one Built-in server when more than one is installed?" (#70) decides. Without #70, one route can have only one server.

## Evidence method

### Setup

All commands used an isolated HOME and XDG tree under `/tmp/ts7smoke`. No real user state changed.

```sh
R=/tmp/ts7smoke
export HOME=$R/home XDG_CONFIG_HOME=$R/xdg/config XDG_DATA_HOME=$R/xdg/data \
  XDG_STATE_HOME=$R/xdg/state XDG_CACHE_HOME=$R/xdg/cache npm_config_cache=$R/npmcache
npm i -g --prefix $R/g7 typescript@7.0.2
npm i -g --prefix $R/g6 --no-audit --no-fund typescript@6.0.3 typescript-language-server@6.0.1
# Workspace-local copies of the fixture:
(cd $R/ws7local && npm i -D typescript@7.0.2)
(cd $R/ws7pnpm && pnpm add -D typescript@7.0.2)   # pnpm 12.6.0
```

The fixture `ws7` has `tsconfig.json` (`strict`, `module: nodenext`, `noEmit`, `include: ["src"]`), `package.json` (`"type": "module"`), `src/lib.ts` (interface `Greeter`, function `greet`) and `src/main.ts`. `main.ts` imports `greet`, calls it, and has one type error (`const  value: number = "oops";`, with 2 spaces for the format test).

The smoke script ran `lspctl` 0.1.2 (`/Users/ianpascoe/src/lspctl/target/debug/lspctl`, built from `69520ab`) with `PATH=<node dir>:/usr/bin:/bin`. The user config had one server and one route:

```toml
version = 1
default_server = "ts"
routes = [
  { server = "ts", language_id = "typescript", extensions = [".ts", ".mts", ".cts"] },
]

[servers.ts]
executable = "<path under test>"
args = ["--lsp", "--stdio"]
```

Each Query also had the explicit invocation fields:

```sh
lspctl <query> ... --workspace $R/ws7 --ignore-project-config --server ts \
  --server-executable <path under test> --server-arg=--lsp --server-arg=--stdio --request-timeout 60s
```

Note: `--server-arg --lsp` (with a space) fails with `invalid_arguments`. Use `--server-arg=--lsp`.

### Variants

| Label | Executable | Launch chain |
|---|---|---|
| `ts7-npm-global` | `$R/g7/bin/tsc` | npm symlink → `typescript/bin/tsc` (Node) → `process.execve` of the native executable |
| `ts7-native` | `$R/g7/lib/node_modules/typescript/node_modules/@typescript/typescript-darwin-arm64/lib/tsc` | Mach-O arm64 executable, 23.6 MB. No Node. |
| `ts7-workspace-local` | `$R/ws7local/node_modules/.bin/tsc` | npm symlink → `../typescript/bin/tsc` (Node) |
| `ts7-pnpm-local` | `$R/ws7pnpm/node_modules/.bin/tsc` | pnpm `#!/bin/sh` wrapper script (not a symlink) → Node |
| `tsls-ts6-control` | `$R/g6/bin/typescript-language-server`, args `--stdio` | typescript-language-server 6.0.1 with TypeScript 6.0.3 next to it |

Also, with the bare name `tsc` and `PATH=$R/g7/bin:...`, `definition` and a `javascript` route (`.js .mjs .cjs`) passed. `document-symbols` on `src/util.js` gave `twice` and `four`.

### Results

Positions are zero-based. "Pass" means `ok: true` with the expected result.

| Step | All 4 TypeScript 7 variants | typescript-language-server 6.0.1 + TypeScript 6.0.3 |
|---|---|---|
| `capabilities` | Pass. Providers: definition, references, hover, documentSymbol, workspaceSymbol, rename (prepare), formatting, codeAction (`quickfix`, `source.organizeImports`, `source.removeUnusedImports`, `source.sortImports`, `source.fixAll`), diagnostic (`identifier: typescript`, `interFileDependencies: true`, no workspace diagnostics) | Pass |
| `workspace-symbols` before any Document is open | `[]` | Error `server_error`: `No Project.` |
| `definition` (`main.ts` 3:12) | Pass. `LocationLink` to `lib.ts` 4:16 | Pass |
| `references` (`lib.ts` 4:16, with declaration) | Pass. Declaration, import and call | Pass |
| `hover` | Pass. `(alias) function greet(g: Greeter): string` | Pass |
| `document-symbols` (`lib.ts`) | Pass. Hierarchical: `Greeter` with `name`, `greet` | Pass |
| `workspace-symbols` after a Document is open | Pass | Pass |
| `prepare-rename` | Pass. Placeholder `greet` | Pass |
| `rename` to `salute` | Pass. The Preview changes `lib.ts` and both uses in `main.ts` | Pass |
| `format` (`tabSize: 2`) | Pass. Removes the extra space | Pass |
| `code-actions`, range with no diagnostic in context | `[]` (no `refactor` kinds are advertised) | Pass. Refactor actions |
| `code-actions` with diagnostic TS2304 in context (separate run, `src/fix.ts`) | Pass. `Add import from "./lib.js"` (`quickfix`) | Not run |
| `document-diagnostics` | Pass. TS2322 on 2:7–2:12 | Error `capability_unavailable` (no `diagnosticProvider`) |
| `published-diagnostics` | `[]` after the 2 s wait | Pass. TS2322 |

Each TypeScript 7 Query took 0.01–0.14 s. The fixture had no new files after the runs (`git status --short --ignored` was empty). The server wrote `$HOME/Library/Caches/typescript/7.0/package.json`. This is the typings cache for automatic type acquisition. It is outside the Workspace. The launcher runs `npm` for type acquisition (`cmd/tsgo/lsp.go:58-62`) [INFERENCE: only for JavaScript projects that need typings; not seen in the smoke run].

### Other observed facts

| Command | Result |
|---|---|
| `g6/bin/tsc --lsp --stdio` (TypeScript 6.0.3) | Prints `error TS5023: Unknown compiler option '--lsp'.` and the same for `--stdio` to stdout. Exit 1. |
| The same through lspctl (`hover`) | `initialization_failed`, reason `JSON-RPC header ended before its blank line`, `retry: after_change` |
| `g7/bin/tsc --lsp` (no `--stdio`) | `only stdio is supported`, exit 1 |
| `g7/bin/tsc --stdio --lsp` (wrong order) | Runs the compiler, which prints TS5023 for both flags. `--lsp` must be the first argument (`cmd/tsgo/main.go:20-24`). |
| `tsc --version` | TypeScript 7: `Version 7.0.2`. TypeScript 6: `Version 6.0.3`. |
| typescript-language-server 6.0.1 with a Workspace that has only TypeScript 7.0.2 in `node_modules` | It works, but with its own sibling TypeScript 6.0.3. It sends `window/showMessage`: "The TypeScript of the workspace (TypeScript 7.0.2 at ...) provides no tsserver.js. Using (bundled) TypeScript 6.0.3 instead - language features will not reflect the TypeScript of the workspace." |

## 1. TypeScript 7 as a stdio LSP server

### Source facts (TypeScript `v7.0.2`, commit [`1e4744d`](https://github.com/microsoft/TypeScript/tree/1e4744d68260a7cb91b62b12edc3f6a2187faaf1/tsc))

- TypeScript 7 source is now in `microsoft/TypeScript` under `tsc/`. The Homebrew formula builds from the `v7.0.2` tag of that repo ([`Formula/t/typescript.rb`](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript.rb)).
- `--lsp` must be `args[0]`. Then `runLSP` parses the other flags (`tsc/cmd/tsgo/main.go:18-26`).
- Without `--stdio`, the server prints "only stdio is supported" and exits 1 (`tsc/cmd/tsgo/lsp.go:20-35`). `--pipe` and `--socket` are parsed but not used.
- The server uses the `processId` from `initialize` as a parent watchdog. It checks every 5 s and stops when the parent is gone (`tsc/cmd/tsgo/lsp.go:74-108`). lspctl sends `processId: std::process::id()` (`src/session/owner_runtime.rs:806`).
- File watching (`tsc/internal/lsp/server.go:1198-1227`): lspctl does not offer dynamic registration for `didChangeWatchedFiles`. Thus tsgo uses its own in-process watcher only on backends with fast recursive watching. `HasFastRecursiveBackend` is true only for `windows` and `fsevents` (`tsc/internal/fswatch/watcher.go:247-254`). On macOS the smoke log showed `Added new watch`. **On Linux, file watching is off.** [INFERENCE] On Linux, tsgo does not see disk changes to files that lspctl has not opened, until lspctl reopens them (ADR 0003).
- After `initialized`, tsgo sends `workspace/configuration` for `js/ts`, `typescript`, `javascript` and `editor` (`server.go:365-395`). With `settings = {}`, lspctl answers `null` for each item, and tsgo uses its defaults.
- Then tsgo sends `client/registerCapability` for `workspace/didChangeConfiguration`, and returns an error from `initialized` if the request fails (`server.go:1272-1288`). lspctl answers `MethodNotFound` to this request (`src/session/owner_runtime.rs:2381-2401`). The error only goes to the log (`server.go:708-714`), and the session exists before the request (`server.go:1245-1266`). The smoke run confirms that all Queries work after this. [INFERENCE] The only effect is that the `compilerOptionsForInferredProjects` step and `StartPerformanceTelemetry` at `server.go:1290-1297` do not run. The lspctl `--trace-protocol` output did not show server-to-client requests, so the exchange itself was not observed.
- `diagnosticProvider` has no `workspaceDiagnostics` (`server.go:1096-1101`). Push diagnostics are on by default (`PushDiagnosticsEnabled: !disablePushDiagnostics`, `server.go:1256`). But the smoke run got no pushed diagnostics for `main.ts` in 2 s. [INFERENCE] tsgo pushes only project-level diagnostics (for example, `tsconfig.json` errors) and serves file diagnostics by pull.
- The TypeScript 7 README marks "Language service (LSP)" as **in progress**: "Nearly all features implemented" ([`tsc/README.md`](https://github.com/microsoft/TypeScript/blob/1e4744d68260a7cb91b62b12edc3f6a2187faaf1/tsc/README.md), line 44).

### Executable name and args

| OS | Install | Executable that lspctl resolves | Args |
|---|---|---|---|
| Linux, macOS | `npm i -g typescript` | `tsc` (symlink to `lib/node_modules/typescript/bin/tsc`, a Node script) | `--lsp`, `--stdio` |
| macOS, Linux | `brew install typescript` | `tsc` (symlink to the native `@typescript/typescript-<os>-<cpu>/lib/tsc`; the formula says "Prefer the native executable over the Node launcher") | `--lsp`, `--stdio` |
| Linux, macOS | Workspace-local npm, pnpm, yarn | `node_modules/.bin/tsc` (npm: symlink; pnpm: `#!/bin/sh` script) | `--lsp`, `--stdio` |
| Windows | `npm i -g typescript` | `tsc.cmd` in the npm prefix (found through PATHEXT, #65) | `--lsp`, `--stdio` |
| Windows | Workspace-local | `node_modules\.bin\tsc.cmd` | `--lsp`, `--stdio` |
| Windows | native only | `node_modules\@typescript\typescript-win32-x64\lib\tsc.exe` (or `-win32-arm64`), from `getExePath.js` | `--lsp`, `--stdio` |

npm metadata for `typescript@7.0.2`: `bin: {"tsc": "bin/tsc"}`, `exports["."]: "./lib/version.cjs"`, and 20 optional platform packages `@typescript/typescript-<os>-<cpu>` (`npm view typescript@7.0.2 bin exports optionalDependencies`). `typescript@6.0.3` has `bin: {"tsc", "tsserver"}`.

The Node launcher (`typescript/lib/tsc.js`) finds the native executable with `getExePath()`. On POSIX with Node ≥ 22.15 it replaces itself with `process.execve`. On Windows it uses `execFileSync` with `stdio: "inherit"`, so a Node process stays as the parent of `tsc.exe` (`lib/tsc.js` in the 7.0.2 tarball). [INFERENCE] On Windows, lspctl stops the `cmd.exe` and Node processes. The parent watchdog then stops `tsc.exe` within about 5 s. This is input for "Which Windows launch fixes must ship with Built-in `.cmd` servers?" (#77).

The name `tsgo` comes from the `@typescript/native-preview` package. Its `latest` tag is `7.0.0-dev.20260707.2`, and it gets no new releases after 7.0 (`npm view @typescript/native-preview dist-tags`). nvim-lspconfig deprecated its `tsgo` config in favor of `tsc` ([`lsp/tsgo.lua`](https://github.com/neovim/nvim-lspconfig/blob/a9bb4d5f4276aa5cddc015faa3ecc5a7b5a26b14/lsp/tsgo.lua)). Do not add `tsgo` as a Built-in executable.

## 2. How to tell TypeScript 7 `tsc` from an older `tsc`

Every TypeScript version installs an executable named `tsc`. Only TypeScript 7 and later accept `--lsp`. If lspctl starts an older `tsc` with `--lsp --stdio`, the result is `initialization_failed` with a reason about a JSON-RPC header. This reason does not tell the user the real cause.

| Method | How | Works for | Fails for | Cost |
|---|---|---|---|---|
| A. `package.json` next to the resolved binary | Resolve `tsc` (`which_in`, `src/configuration.rs:461`). Canonicalize it. Go up at most 2 directories to the first `package.json`. Accept if `name` is `typescript` or starts with `@typescript/typescript-`, and the major of `version` is 7 or more. | npm global POSIX (`typescript/bin/tsc` → `typescript/package.json`, observed). npm local `.bin/tsc` symlink (observed). Homebrew (`@typescript/typescript-<os>-<cpu>/lib/tsc` → its `package.json`, version `7.0.2`) [INFERENCE from the formula; not installed on the host]. | pnpm and Windows `.cmd` wrappers are not symlinks. Version-manager shims (mise, asdf, Volta) point to a shim program [INFERENCE]. | 1 `canonicalize`, up to 2 `stat`, 1 small file read (≈1–2 KB). No process. |
| B. Workspace `node_modules/typescript/package.json` | Read `<Workspace>/node_modules/typescript/package.json` directly. Do not follow `.bin`. | npm, pnpm (the path is a symlink into `.pnpm/typescript@7.0.2/...`, observed), Windows. | Yarn Plug'n'Play has no `node_modules` [INFERENCE]. Only for the Workspace-local candidate. | 1 file read. No process. No Trust grant, because lspctl reads a file and does not run it. |
| C. Windows npm global layout | For `<dir>\tsc.cmd`, read `<dir>\node_modules\typescript\package.json`. | npm global on Windows [INFERENCE from the npm `cmd-shim` layout; not observed]. | Other managers. | 1 file read. |
| D. `--version` probe | Run the resolved executable with the single arg `--version`, no shell, a short timeout, and no stdin. Parse `Version <major>.<minor>.<patch>` from stdout. Accept exit 0 and major ≥ 7. | All layouts, including shims and `.cmd` files (Rust std runs `.cmd` through `cmd.exe` with safe escaping, #65). | An executable that hangs or is not `tsc`. | 1 process start. Measured on macOS: native 7.0.2 ≈ 0.01 s; Node launcher 7.0.2 0.04–0.11 s; 6.0.3 ≈ 0.05 s. It runs code: a Workspace-local probe needs a Trust grant first (ADR 0002). Cache the result by canonical path, size and mtime. |
| E. Start with `--lsp --stdio` and look at the failure | Start the server and treat `initialization_failed` as "not TypeScript 7". | Any `tsc`. | Gives no clear reason. Starts a full server. Cannot tell a TypeScript 6 `tsc` from a broken TypeScript 7. | 1 failed Owner start per attempt. Do not use. |
| F. Absence of `tsserver` | TypeScript 6 `package.json` has a `tsserver` bin, and `lib/tsserver.js` exists. TypeScript 7 has neither. | Same layouts as A and B. | Same as A and B. | Same as A. Use only as a cross-check. The version is simpler. |

Prior art: nvim-lspconfig uses method D. It tries `<root>/node_modules/.bin/tsc`, then `tsc` on `PATH`, then the same for `tsgo`. It accepts the first one whose `--version` has major ≥ 7. It caches the result for each root ([`lsp/tsc.lua`](https://github.com/neovim/nvim-lspconfig/blob/a9bb4d5f4276aa5cddc015faa3ecc5a7b5a26b14/lsp/tsc.lua), `supports_lsp` and `root_dir`; last change to the file `3fc5c45`, 2026-08-17). Its doc says: "An older binary in `node_modules/.bin` is skipped in favour of one on `$PATH` that does support it".

**Recommendation:** Use B for the Workspace-local candidate, because it starts no process before the Trust grant. Use A for the `PATH` candidate, and use D only when A finds no `typescript` `package.json`. D is safe for a `PATH` executable, because lspctl would start that executable anyway without a Trust grant. Never use E.

## 3. typescript-language-server and vtsls

### typescript-language-server

- Latest release: [v6.0.1](https://github.com/typescript-language-server/typescript-language-server/releases/tag/v6.0.1) (2026-09-24). It has only 2 crash fixes. [v6.0.0](https://github.com/typescript-language-server/typescript-language-server/releases/tag/v6.0.0) (2026-08-20) requires Node ≥ 22.22.2, and adds "notify when typescript with no tsserver API found" ([#1120](https://github.com/typescript-language-server/typescript-language-server/pull/1120)).
- #1120 fixes [issue #1110](https://github.com/typescript-language-server/typescript-language-server/issues/1110), "Silent fallback to unrelated global TypeScript when workspace uses TypeScript 7". The issue author says: "This is **not** a request to add TS7 language support". The fix only warns. The smoke run shows the warning text.
- The source still needs `tsserver.js` (`src/tsServer/versionProvider.ts:176-204` at [`cf9c47b`](https://github.com/typescript-language-server/typescript-language-server/blob/cf9c47ba0cf7c29448309c088765f5c16eabe6ce/src/tsServer/versionProvider.ts)).
- The maintainers closed the Renovate PR "update devdependency typescript to v7" without a merge ([#1112](https://github.com/typescript-language-server/typescript-language-server/pull/1112), 2026-08-26). Renovate now ignores all TypeScript 7.x updates.
- The README says: "Currently Microsoft is working on TypeScript 7 ... that will include the LSP implementation and will hopefully supersede this project." The install command is `npm install -g typescript-language-server typescript@6`.
- No open issue or PR has a plan to support TypeScript 7 (GitHub search `tsgo`, `typescript 7`, `native`, `tsserver`, 2026-09-28).
- Homebrew `typescript-language-server` depends on `typescript` (7.0.2) and links it into its own `node_modules` ([formula](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript-language-server.rb)). [INFERENCE] The Homebrew install fails in `initialize` unless the Workspace has TypeScript 6 or older. The formula test sends `initialize` with `rootUri: null` and only checks for a `Content-Length` header, so it does not catch this.

Conclusion: typescript-language-server will work only with TypeScript ≤ 6. Its maintainers expect TypeScript 7 to replace it.

### vtsls

- Latest release: server [v0.3.0](https://github.com/yioneko/vtsls/releases/tag/server-v0.3.0) (2025-12-24). Commits after that are only dependency updates (last: `8284c90`, 2026-09-22).
- It bundles `typescript` 5.9.3 (`packages/service/package.json:31`). It can use the Workspace TypeScript only with `vtsls.autoUseWorkspaceTsdk` (README "TypeScript version"). [INFERENCE] A TypeScript 7 Workspace has no `tsserver.js`, so this option cannot help.
- The maintainer removed `typescript.experimental.useTsgo` "for incompatibility" ([#292](https://github.com/yioneko/vtsls/issues/292)). The maintainer also wrote: "Since tsgo now follows LSP I think it is better to directly use it without another layer of wrapper" ([#254](https://github.com/yioneko/vtsls/issues/254)).

Conclusion: vtsls is not a path to TypeScript 7. Keep it out of the Built-in servers (as #64 decided).

## Recommended preference order and detection rule

### Two Built-in servers

| Built-in server | Executable | Args | Gate |
|---|---|---|---|
| `typescript-native` (working name) | `tsc` | `--lsp`, `--stdio` | The resolved `tsc` is TypeScript ≥ 7 (method A or D; B for Workspace-local). |
| `typescript-language-server` | `typescript-language-server` | `--stdio` | The executable resolves. [INFERENCE] lspctl cannot check cheaply that a TypeScript ≤ 6 `tsserver.js` is resolvable next to it. |

Both declare the same routes: `typescript` (`.ts .mts .cts`), `typescriptreact` (`.tsx`), `javascript` (`.js .mjs .cjs`), `javascriptreact` (`.jsx`). This is only possible with ordered candidates (#70).

### Step 1: find the Workspace TypeScript major (no process, no Trust grant)

1. Read `<Workspace>/node_modules/typescript/package.json`. If it exists and has a valid `version`, use its major. This is the installed version.
2. Else, read `<Workspace>/package.json`. Look for `typescript` in `devDependencies`, then `dependencies`. [INFERENCE] Use the major only when the range fixes it (for example `^7.0.2`, `~6.0.3`, `7.x`). Ignore it when it is not installed, because the project is not ready.
3. Else, the Workspace TypeScript is **unknown** (plain JavaScript, or no install yet).

Use only the Workspace root. Root markers never move the Workspace root (map #62). [INFERENCE] Monorepo packages with a different TypeScript version are out of scope for the first release.

### Step 2: order the candidates

| Workspace TypeScript | Order | Reason |
|---|---|---|
| **≥ 7** | 1. Workspace-local `node_modules/.bin/tsc` (needs a Trust grant) → 2. `tsc` on `PATH` that passes the gate → 3. `typescript-language-server` on `PATH` | Candidate 1 is the project's own compiler. typescript-language-server would use a different TypeScript and only warn (#1110), so it is last. |
| **≤ 6** | 1. `typescript-language-server` on `PATH` → 2. Workspace-local `node_modules/.bin/typescript-language-server` (needs a Trust grant) → 3. `tsc` on `PATH` that passes the gate | typescript-language-server loads the Workspace `node_modules/typescript/lib/tsserver.js` first (`versionProvider.ts`), so it uses the project's compiler. Never use the Workspace `.bin/tsc` here: it has no `--lsp`. TypeScript 7 as a fallback gives nearly the same results for TypeScript 6 projects ("Same files and module resolution as TS 6.0", `tsc/README.md`). |
| **unknown** | 1. `tsc` on `PATH` that passes the gate → 2. `typescript-language-server` on `PATH` | `npm i -g typescript` and `brew install typescript` now give TypeScript 7. With no Workspace TypeScript, typescript-language-server works only if a TypeScript ≤ 6 is next to it. |

Which order is better for `PATH` in the "≤ 6" row is a trade-off: typescript-language-server matches the project compiler, and `tsc` 7 is faster and needs no Node when it comes from Homebrew. This research recommends the project compiler first.

### Step 3: detection rule for each candidate

- `tsc` on `PATH`: resolve with the server `PATH` (`which_in`). Canonicalize. Method A. If A finds no `typescript` `package.json`, method D. Pass if the major is ≥ 7. If it fails, skip the candidate. Report it in `candidates` with the reason (for example, "tsc 6.0.3 has no --lsp"). Cache the result by canonical path, size and mtime.
- Workspace-local `tsc`: method B only. Pass if the major is ≥ 7 and `node_modules/.bin/tsc` (Windows: `tsc.cmd`) exists. Then require a Trust grant before the start (#69). No probe before the grant.
- `typescript-language-server`: resolve only. [INFERENCE] Optional extra check without a process: for a Workspace with TypeScript ≤ 6, `node_modules/typescript/lib/tsserver.js` exists. For other Workspaces, lspctl cannot see which TypeScript the server resolves. An `initialize` failure with "Could not find a valid TypeScript installation" is then the only signal.
- Executable paths: #64 found that lspctl canonicalizes a path that contains `/` (`src/configuration.rs:451-458`). For npm `.bin/tsc` this gives `typescript/bin/tsc`, which works (smoke). The pnpm `.bin/tsc` is a regular script file, so canonicalization does not change it, and it works (smoke).

### Install hints (for "How do failures give install guidance?", #73)

| Candidate | macOS | Linux | Windows |
|---|---|---|---|
| `tsc` (TypeScript 7) | `brew install typescript` or `npm i -g typescript` | `npm i -g typescript` | `npm i -g typescript` |
| typescript-language-server | `npm i -g typescript-language-server typescript@6` | same | same |
| Workspace-local | `npm i -D typescript` (then `lspctl trust grant`) | same | same |

## Differences that agents must know

| Query | `tsc --lsp --stdio` 7.0.2 | typescript-language-server 6.0.1 |
|---|---|---|
| Diagnostics | `document-diagnostics` works. `published-diagnostics` gives `[]`. | `published-diagnostics` works. `document-diagnostics` gives `capability_unavailable`. |
| `workspace-symbols` before a Document is open | `[]` (no error) | Error `No Project.` |
| `code-actions` | Quick fixes and `source.*` only. No refactors. | Quick fixes, source actions and refactors |
| Files changed on disk that are not open | Seen on macOS and Windows (own watcher). Not seen on Linux. | tsserver watches files itself [INFERENCE] |

## Consequences for the map

- "How does lspctl pick one Built-in server when more than one is installed?" (#70): the TypeScript routes need 2 ordered candidates and a per-candidate gate that can reject an installed executable (a `tsc` older than 7). The order depends on Workspace data (the Workspace TypeScript major), not only on what is on `PATH`.
- "Which servers are in the first set of Built-in servers?" (#64, closed): change the TypeScript entry from "typescript-language-server only" to "TypeScript 7 `tsc` and typescript-language-server, ordered". If #70 does not add ordered candidates for 0.2.0, keep typescript-language-server and the `typescript@6` install hint.
- "How does a Built-in server use a Workspace-local executable under a Trust grant?" (#69): the Workspace-local `tsc` must pass a version check before the grant. Only a file read (method B) is safe before the grant. The Workspace `.bin/tsc` of TypeScript ≤ 6 must never be a candidate.
- "How do failures give install guidance?" (#73): a rejected `tsc` needs its own reason in `candidates` (found, but version < 7). Hints in the table above.
- "What does the server inspection command show?" (#74): show the detected TypeScript version and the gate result for each candidate.
- "Must lspctl open a Document before Workspace-wide Queries?" (#79): confirmed for TypeScript 7 (`[]` before a Document is open).
- "Which Windows launch fixes must ship with Built-in `.cmd` servers?" (#77): npm `tsc.cmd` keeps a Node parent process on Windows. tsgo stops within about 5 s after the parent is gone.
- "How do we test Built-in servers in CI?" (#75): the reference matrix tests only typescript-language-server with TypeScript 6.0.3. Add `tsc` 7 to the matrix if it becomes a Built-in server. Linux behavior differs (no file watcher).
- "What does the agent setup prompt say?" (#76): tell agents to use `document-diagnostics` for TypeScript 7 and `published-diagnostics` for typescript-language-server.

## New questions

1. Can a Built-in declaration have a **gate** (a version check) as data, or must it be code? `tsc` is the first Built-in executable name that is shared with a program that is not a language server.
2. Should lspctl run a `--version` probe at all? It is a new kind of process start (not an Owner). It needs a timeout, a cache and a place in the contract.
3. On Linux, TypeScript 7 has no file watcher with lspctl. Should lspctl offer `workspace.didChangeWatchedFiles.dynamicRegistration`? This conflicts with ADR 0003 and the fixed capability profile (ADR 0005).

## Sources

- TypeScript 7 source: [`microsoft/TypeScript@1e4744d` (`v7.0.2`)](https://github.com/microsoft/TypeScript/tree/1e4744d68260a7cb91b62b12edc3f6a2187faaf1/tsc): `cmd/tsgo/main.go`, `cmd/tsgo/lsp.go`, `internal/lsp/server.go`, `internal/fswatch/watcher.go`, `_packages/native-preview/lib/getExePath.js`, `README.md`.
- npm registry: `npm view typescript dist-tags`, `npm view typescript@7.0.2 bin exports optionalDependencies`, `npm view typescript@6.0.3 bin`, `npm view @typescript/native-preview dist-tags`, `npm view @vtsls/language-server`.
- Homebrew: [formula API `typescript`](https://formulae.brew.sh/api/formula/typescript.json) (7.0.2), [`typescript.rb`](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript.rb), [`typescript-language-server.rb`](https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/t/typescript-language-server.rb).
- typescript-language-server: [README](https://github.com/typescript-language-server/typescript-language-server/blob/cf9c47ba0cf7c29448309c088765f5c16eabe6ce/README.md), [releases](https://github.com/typescript-language-server/typescript-language-server/releases), [#1110](https://github.com/typescript-language-server/typescript-language-server/issues/1110), [#1120](https://github.com/typescript-language-server/typescript-language-server/pull/1120), [#1112](https://github.com/typescript-language-server/typescript-language-server/pull/1112), [`versionProvider.ts@cf9c47b`](https://github.com/typescript-language-server/typescript-language-server/blob/cf9c47ba0cf7c29448309c088765f5c16eabe6ce/src/tsServer/versionProvider.ts).
- vtsls: [README](https://github.com/yioneko/vtsls), [releases](https://github.com/yioneko/vtsls/releases), [#254](https://github.com/yioneko/vtsls/issues/254), [#292](https://github.com/yioneko/vtsls/issues/292), `packages/service/package.json`.
- nvim-lspconfig: [`lsp/tsc.lua`](https://github.com/neovim/nvim-lspconfig/blob/a9bb4d5f4276aa5cddc015faa3ecc5a7b5a26b14/lsp/tsc.lua), [`lsp/tsgo.lua`](https://github.com/neovim/nvim-lspconfig/blob/a9bb4d5f4276aa5cddc015faa3ecc5a7b5a26b14/lsp/tsgo.lua) (Apache-2.0; used as prior art only, no data copied).
- lspctl at `69520ab`: `src/configuration.rs:443-473`, `src/session/owner_runtime.rs:806`, `:2381-2446`, `assets/contract/initialize-capabilities.json`, `docs/adr/0003-synchronize-filesystem-snapshots-without-a-watcher.md`.
