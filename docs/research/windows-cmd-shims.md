# Windows `.cmd` shims without a shell

Ticket: #65 "Can lspctl launch `.cmd` shim executables on Windows without a shell?"
Parent map: #62 "Map: zero-config language servers through Built-in servers".
Base commit: `69520ab`.

## Answer

Yes. A Built-in server that declares the bare name `typescript-language-server` works on Windows with no shell configuration.

- The `which` crate finds `typescript-language-server.cmd` through `PATHEXT`. It returns the absolute `.cmd` path.
- Rust std `Command` sees the `.cmd` extension. It then starts `%SystemRoot%\System32\cmd.exe /e:ON /v:OFF /d /c "..."` itself. lspctl does not start a shell. The Rust standard library does.
- Rust std escapes each argument for `cmd.exe`. It refuses arguments that it cannot escape safely: it returns `io::ErrorKind::InvalidInput` at spawn.
- The Owner Job Object also contains `node.exe` that the shim starts, because child processes inherit the job. One small race exists (see [Process-tree termination](#5-process-tree-termination)).

The constraints are in [Constraints for Built-in servers](#constraints-for-built-in-servers).

No Windows host was available for this research. The Windows behavior comes from source code, official documentation, and nightly CI logs. No local Windows run occurred.

## 1. Executable resolution (`which` 8.0.6)

lspctl code:

- `resolve_server_executable` sends a bare name (no `/` or `\`) to `which::which_in(declared, effective_path(server), cwd)`. It sends a name with a separator to `dunce::canonicalize` (`src/configuration.rs:451-462`).
- `effective_path` uses the server `environment.PATH` value if one exists. It compares the key without case on Windows. Otherwise it uses the lspctl process `PATH` (`src/configuration.rs:475-489`).
- `Cargo.lock:1197-1200` pins `which` 8.0.6. `Cargo.toml:36` declares `which = "8"`.

`which` 8.0.6 behavior on Windows (source in `~/.cargo/registry/src/index.crates.io-*/which-8.0.6/src`):

1. A bare name with no separator goes to `WhichFindIterator::new_paths` (`finder.rs:87-100`). The iterator searches only the `PATH` entries. It does not search the current directory. The `cwd` argument only makes relative `PATH` entries absolute (`finder.rs:170-185`).
2. `PATHEXT` comes from `std::env::var_os("PATHEXT")` of the **lspctl process**. The value is parsed once and cached in a `OnceLock` (`sys.rs:157-180`). A `PATHEXT` in the server `environment` has no effect on resolution.
3. For each `PATH` directory, `PathsIter` first yields the bare file name. Then it yields the name plus each `PATHEXT` extension in order (`finder.rs:245-292`). The Windows default order is `.COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;...` (comment at `sys.rs:160`).
4. `is_valid` requires that the path exists as a file or symlink (`checker.rs:32-75`). A path with no extension must also pass `GetBinaryTypeW`, so it must be a PE image (`checker.rs:14-30`, `sys.rs:220-238`).
5. A path with **any** extension passes the executable check with no more test (`checker.rs:19-20`).
6. `correct_casing` replaces the file name with the on-disk casing (`finder.rs:316-342`).

Result for an npm global install: the npm bin directory holds `typescript-language-server` (a `sh` script), `typescript-language-server.cmd`, and `typescript-language-server.ps1`. The extensionless `sh` script fails `GetBinaryTypeW`. `.COM`, `.EXE`, and `.BAT` do not exist. `.cmd` exists, so `which_in` returns the absolute `...\typescript-language-server.cmd` path. [INFERENCE: the three npm bin files come from npm `cmd-shim` behavior. This research did not observe a Windows npm directory.]

`PATH` directory order has priority over extension order. The first directory that holds any match wins.

Related facts:

- A bare name that contains a `.` can match a file with a non-executable extension, because of step 5. Then the spawn fails later.
- `.JS` is in the default `PATHEXT`. If a `PATH` directory holds `name.js` and no `.cmd`/`.exe`, `which` returns the `.js` file. Rust std then calls `CreateProcessW` on it, and the call fails. [INFERENCE: expected error `ERROR_BAD_EXE_FORMAT`, not observed.]
- `which` expands a leading `~` in a `PATH` entry to the home directory (`finder.rs:294-312`, applied at `finder.rs:177`). This is a home-directory expansion. `skills/lspctl/CONFIGURATION.md:39` says that lspctl "performs no shell, environment-variable, or home-directory expansion". This fact applies on all platforms.

## 2. How the Owner launches the path

Launch chain:

1. The CLI resolves the executable in `authorize_server` (`src/configuration/trust.rs:82`). It builds the child environment from the full inherited environment plus the server `environment` (`src/configuration.rs:508-529`).
2. `spawn_detached_owner` starts the Owner, `lspctl.exe __owner ...`. It sends the resolved `.cmd` path as a normal argument. It sets `current_dir`, `env_clear`, and the child environment, and uses `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW` (`src/session.rs:966-1042`). The Owner is a normal `.exe`, so batch rules do not apply to this step.
3. The Owner calls `SupervisedServerProcess::spawn(&bootstrap.executable, &settings.server_args)` (`src/session/owner_runtime.rs:624`). That function uses `tokio::process::Command::new(executable).args(arguments)` with piped stdio and `kill_on_drop(true)`. It sets no creation flags on Windows (`src/session/process_supervision.rs:16-51`). The server inherits the cwd and environment of the Owner.
4. `tokio::process::Command` wraps `std::process::Command` (`tokio-1.53.1/src/process/mod.rs:252,275,318`). Thus the std Windows spawn code applies.

Rust std Windows spawn. Rust 1.89.0 (MSRV, `Cargo.toml:5`) and local stable 1.98.1 have identical `spawn_with_attributes` (up to `cmd_str.push(0)`), `make_bat_command_line`, and `append_bat_arg`. A `diff` of the two source trees showed no change. Line numbers are for 1.89.0:

- [`library/std/src/sys/process/windows.rs`](https://github.com/rust-lang/rust/blob/1.89.0/library/std/src/sys/process/windows.rs):
  - `resolve_exe` (line 456): an absolute path that does not end in `.exe` gets `.exe` added as a probe. If `X.cmd.exe` does not exist, std uses the path as given (lines 467-491).
  - `spawn_with_attributes` (line 268): `has_bat_extension` does a case-insensitive check for `.bat` or `.cmd` (line 283). The check runs on the output of `GetFullPathNameW`. Thus trailing dots and spaces are removed first (lines 290-299, the CVE-2024-43402 fix).
  - If the file is a batch file, the application becomes `command_prompt()` (line 303). `command_prompt` builds `GetSystemDirectoryW() + "\cmd.exe"` (line 877). It does **not** read `ComSpec`.
- [`library/std/src/sys/args/windows.rs`](https://github.com/rust-lang/rust/blob/1.89.0/library/std/src/sys/args/windows.rs):
  - `make_bat_command_line` (line 293) starts the command line with `cmd.exe /e:ON /v:OFF /d /c "` (line 305). `/d` disables the `AutoRun` registry commands. `/v:OFF` disables delayed `!VAR!` expansion.
  - It returns `InvalidInput` if the script path contains `"` or ends in `\` (line 311).
  - It returns `InvalidInput` ("batch file arguments are invalid") for any argument that contains `\r` or `\n` (lines 327-332). It also returns an error for an argument that contains NUL (`ensure_no_nuls`).
  - `append_bat_arg` (line 220) quotes every argument that contains an ASCII character outside `[A-Za-z0-9#$*+-./:?@\_]`, a control character, or a trailing `\`, and every empty argument (lines 228-244). It doubles `"`. It replaces `%` with `%%cd:~,%`, so `cmd.exe` does not expand `%VAR%` (lines 261-279).
  - A `FIXME` in the source says that the round-trip of arguments through a batch script needs tests (line 321).
- The std module documentation says: "`CreateProcessW` ... when given a `.bat` file ... will automatically convert that into running `cmd.exe /c`". It also says that some special characters "might not be possible to safely escape ... will result in an error being returned at process spawn", that the set "might change between releases", and that "running batch scripts in this way may be removed in the future and so should not be relied upon" ([`library/std/src/process.rs:115-130`](https://github.com/rust-lang/rust/blob/1.89.0/library/std/src/process.rs#L115-L130)).

History:

- [CVE-2024-24576 advisory](https://blog.rust-lang.org/2024/04/09/cve-2024-24576/): before Rust 1.77.2, std escaped batch-file arguments incorrectly. An attacker who controlled the arguments could run arbitrary shell commands. Rust 1.77.2 made the escape more robust. It also made `Command` "return an `InvalidInput` error when it cannot safely escape an argument ... when spawning the process".
- [CVE-2024-43402 advisory](https://blog.rust-lang.org/2024/09/04/cve-2024-43402/): the 1.77.2 fix missed batch names with trailing whitespace or periods. Rust 1.81.0 fixed this.
- The MSRV is 1.89, so both fixes are always present.

The npm shim that `cmd.exe` runs. The current [`npm/cmd-shim` `lib/index.js`](https://github.com/npm/cmd-shim/blob/7667c245e7d9259b5f88b77fb71b497ffcc26976/lib/index.js#L92-L119) (v9.0.2) writes a `.cmd` file with this logic: `@ECHO off`, set `dp0=%~dp0`, use `%dp0%\node.exe` if it exists, else `node` from `PATH`, remove `.JS` from `PATHEXT`, then run `"%_prog%" <target script> %*` (lines 116-117). The `goto #_undefined_# 2>NUL || title %COMSPEC% & ...` prefix prevents the "Terminate batch job (Y/N)?" prompt.

Consequences:

- `node` must be on the `PATH` of the server environment, unless `node.exe` is next to the shim.
- `%*` forwards the arguments to `node` as `cmd.exe` parsed them. Rust's escape is designed for this pattern.
- `child.id()` and `SupervisedServerProcess::pid()` report the `cmd.exe` process, not `node.exe`. The Owner waits for `cmd.exe`. `cmd.exe` exits after `node.exe` exits. [INFERENCE: from shim logic, not observed.]
- If std refuses an argument, `server_start_failure` reports `server_start_failed` with `osCode: null`, because `raw_os_error()` is `None` for `InvalidInput` (`src/session/owner_runtime.rs:3008-3022`). The failure does not tell why.

## 3. The "no shell" rule and security

Documents:

- `skills/lspctl/CONFIGURATION.md:34`: "separate arguments, with no shell command string".
- `skills/lspctl/CONFIGURATION.md:39`: "`lspctl` performs no shell, environment-variable, or home-directory expansion. ... A bare name uses the owner's `PATH` and normal Windows executable resolution."
- `src/configuration.rs:442`: "Resolves a server executable without invoking a shell."

Assessment:

- lspctl code starts no shell, and has no shell command string. The rule is true for lspctl code.
- For a `.cmd` or `.bat` target, std starts `cmd.exe` implicitly. `cmd.exe` is a shell. Thus the process tree contains a shell when the executable is a batch file. The documents do not say this.
- `%` expansion and command separators (`&`, `|`, `<`, `>`) in arguments are neutralized by std quoting and the `%%cd:~,%` substitution. Thus the rule "no environment-variable expansion of arguments" stays true in effect. [INFERENCE: the Rust Security Response WG considers the post-1.77.2 escape safe or else refuses the argument. This research did not test it on Windows.]
- The batch file text itself does expand variables (`%~dp0`, `%PATHEXT%`, `%COMSPEC%`). That is the behavior of the installed shim, not of lspctl arguments.
- CONFIGURATION.md:39 says "the owner's `PATH`". The code uses the server `environment.PATH` if present, else the `PATH` of the CLI process that resolves. It uses the `PATHEXT` of the CLI process. The resolution runs in the CLI (`trust.rs:82`), not in the Owner.

Security for Built-in arguments (fixed strings, such as `--stdio`):

- The strings are constant and contain only safe characters. Std leaves `--stdio` unquoted. No injection is possible. [INFERENCE: every Built-in argument must be checked, but a fixed ASCII flag has no `cmd.exe` metacharacters.]
- The executable path comes from `PATH` on the user machine. A `.cmd` file on `PATH` can run any command. A `.exe` file on `PATH` can too. Thus the `.cmd` form does not add a new trust boundary for a Built-in server. The trust question is "which file on `PATH` runs", and it is the same for `.exe`.

Security for user-supplied arguments (project or user `args`):

- Project-controlled launch fields need a Trust grant (ADR 0002). A trusted project already controls the executable. Thus argument injection through `cmd.exe` gives it no new power.
- A user configuration that places untrusted text into `args` depends on the std escape. Std either escapes the text for `cmd.exe` or refuses it with `InvalidInput`. The Rust documentation says that the set of refused characters can change between releases. Thus an argument that works with one toolchain can fail after an upgrade.
- An argument that contains a newline always fails for a batch target. The same argument works for a `.exe` target.
- The Rust documentation says that implicit batch execution "may be removed in the future". If Rust removes it, a bare-name `.cmd` Built-in server stops working without any lspctl change.

## 4. CI evidence

Workflow `.github/workflows/nightly.yml`: the job `reference-servers` runs `os: [ubuntu-latest, macos-latest, windows-latest]` × `server: [rust-analyzer, typescript-language-server, basedpyright]`. It uses Node 24.6.0 and runs `python scripts/release/run_reference_matrix.py <server> --binary target/release/lspctl --output reference.json`.

How the scripts set the executable:

- `typescript-language-server`: `run_reference_matrix.py:64-77` runs `npm.cmd install --prefix tools/node typescript-language-server@... typescript@...`. Then it sends `--executable <shutil.which("node")>` and `--server-arg=<...>\node_modules\typescript-language-server\lib\cli.mjs --server-arg=--stdio`. Thus CI runs `node.exe` plus a script. **CI does not use the `.cmd` shim and does not use a bare name.** Commit `e951fee` added this form, and no commit in `scripts/` used a `.cmd` server path (`git log -S'.cmd' -- scripts/` is empty; `git log -S'cli.mjs'` shows only `e951fee`).
- `basedpyright`: `run_reference_matrix.py:81-85` uses the absolute path `Scripts\basedpyright-langserver.exe` in a venv. This is a pip `.exe` launcher. It passes in CI.
- `reference_smoke.py:145` writes the absolute executable path into the generated configuration.

Run evidence (`gh run list -R spiritledsoftware/lspctl --workflow nightly.yml`): the 20 latest runs (2026-09-09 to 2026-09-28) all have the conclusion `success`. In run [36405035040](https://github.com/spiritledsoftware/lspctl/actions/runs/36405035040) (2026-09-28), the jobs `windows-latest / typescript-language-server`, `windows-latest / basedpyright`, `windows-latest / rust-analyzer`, and `windows-latest / deterministic soak` all passed. The TypeScript job log shows `{"commands": 11, ..., "schemaValidated": true, "server": "typescript"}`.

Conclusion: CI proves that lspctl drives typescript-language-server on Windows through `node.exe`. CI proves that a pip `.exe` launcher works. CI does **not** prove the `.cmd` path or the bare-name resolution. No test in `tests/` or `scripts/` uses a `.cmd` or `.bat` server.

## 5. Process-tree termination

lspctl code (`src/session/process_supervision.rs`):

- After `spawn`, `create_kill_on_close_job(pid)` makes a job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. It opens the process with `PROCESS_SET_QUOTA | PROCESS_TERMINATE` and calls `AssignProcessToJobObject` (lines 141-186).
- `terminate_tree_soft` and `terminate_tree_force` both call `TerminateJobObject(job, 1)` (lines 121-133). `Drop` closes the job handle, and `KILL_ON_JOB_CLOSE` then kills the tree (lines 98-105).

Microsoft documentation ([`AssignProcessToJobObject` remarks](https://learn.microsoft.com/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject#remarks)): "By default, all child processes are associated with the immediate job and every job in the parent job chain." Nested jobs are supported from Windows 8.

Result:

- `cmd.exe` is in the lspctl job. `node.exe` that `cmd.exe` starts after the assignment is also in the job. `cmd.exe` does not use `CREATE_BREAKAWAY_FROM_JOB`, and the lspctl job does not set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`. Thus `TerminateJobObject` kills `cmd.exe` and `node.exe`. [INFERENCE: `cmd.exe` breakaway behavior is from documentation defaults, not from `cmd.exe` source.]
- `node.exe` (libuv) puts each non-detached child in its own global job and adds itself to that job ([libuv `src/win/process.c` at `2b4b918`, lines 69-121 and 1117-1166](https://github.com/libuv/libuv/blob/2b4b918d3381100854250c89d5159d4206daafb7/src/win/process.c#L69-L121)). libuv does not set `CREATE_BREAKAWAY_FROM_JOB`. With nested jobs, `node.exe` and `tsserver` stay in the lspctl job chain. `TerminateJobObject` on the outer job kills them.
- **Race.** Rust std spawns the process running (no `CREATE_SUSPENDED`), and lspctl assigns the job after `spawn` returns. A grandchild that starts before the assignment is not in the job. For a `.cmd` shim, `cmd.exe` must start, parse the batch file, and call `CreateProcess` for `node.exe` inside that window. The window is some microseconds. Starting `cmd.exe` takes much longer. [INFERENCE: timing was not measured. The risk is low but not zero on a loaded host.] This race also exists for `.exe` servers and their children, so it is not special to `.cmd`. libuv avoids it for detached children with `CREATE_SUSPENDED` (lines 1128-1129, 1169-1175). On stable Rust, `std` has no public API for the main thread handle or `PROC_THREAD_ATTRIBUTE_JOB_LIST`. [INFERENCE: from stable API docs; `raw_attribute` and `main_thread_handle` are unstable.]

Adjacent fact (not special to `.cmd`): the Owner has no console (`DETACHED_PROCESS`, `src/session.rs:1038-1041`). The server spawn sets no creation flags. `scripts/release/soak.py:68` says that "conhost.exe hosts console-subsystem servers". Thus Windows creates a new console for each console server. [INFERENCE: a visible console window can appear for `cmd.exe` and for `.exe` servers alike. CI does not show windows.] `CREATE_NO_WINDOW` on the server spawn would prevent this.

## Constraints for Built-in servers

A Built-in server with a bare name such as `typescript-language-server` works on Windows with no shell configuration if these conditions are true:

1. `PATHEXT` of the lspctl CLI process contains `.CMD` (Windows default). A server `environment.PATHEXT` does not change resolution.
2. The npm bin directory is on the effective `PATH`, and no earlier `PATH` directory holds a `.COM`, `.EXE`, or `.BAT` with the same name.
3. `node` is on the `PATH` of the server environment, or `node.exe` is next to the shim.
4. Built-in `args` contain no `\r`, `\n`, or NUL. Std refuses these for batch targets with `InvalidInput`. Fixed ASCII flags such as `--stdio` are safe.
5. The bare name contains no `.`. With a dot, `which` can return a non-executable file.
6. The implicit `cmd.exe` is a documented std behavior that std "may remove". The set of refused characters "might change between releases". A later Rust toolchain can break `.cmd` launch.
7. The PID that lspctl reports on Windows is `cmd.exe`, not the language server.
8. The job assignment race exists. It is low risk and exists for `.exe` servers too.

A pip `.exe` launcher (for example `basedpyright-langserver.exe`) is a normal PE file. It needs none of the batch rules. CI proves that it works with an absolute path. The bare-name path through `which` gives the same `.exe` (`.EXE` comes before `.CMD` in `PATHEXT`).
