# Can the v1 contract generator reproduce the current contract assets?

Ticket: #63. Map: #62 "Map: zero-config language servers through Built-in servers".
Base commit: `69520ab`. Date of the investigation: 2026-09-28.

## Answer

Yes. The generator reproduces the current `assets/contract/schemas.json` byte for byte. It needs one change: the generator hard-codes the old product name `lspc` in 23 lines of `check.py` and in 18 lines of `contract-fixtures.json`. Commit `c719886` ("Rename lspc to lspctl") renamed the catalog and `schemas.json`, but it did not rename the generator. The generator was never in the main history, so the rename could not reach it.

Without the name change, the generator emits 167 schema URIs under `lspc://` and 76 under `lspctl://`. That output does not match the current file. After the name change, the output has the SHA-256 digest `41b9903009d0a8f880ff1bb3b77ec05c1e934eeeecb5a9996ed1232f7b539b69`. The current file has the same digest.

No commit contains a hand edit to `schemas.json` that the generator cannot reproduce. No commit after `baaead1` changes a generator.

## Where the generator is

- `check.py` exists only at commits `aa65677` and `baaead1`. Both are on the local branch `prototype/v1-machine-contract` (`git log --all -- '*check.py'`).
- `baaead1` is not an ancestor of `main` (`git merge-base --is-ancestor baaead1 main` fails). The main history starts again at `79a21f6` → `d69ae99`. `docs/prototypes/` does not exist in the main tree.
- No ref on `origin` contains `baaead1`. `git ls-remote origin` lists only `main`, `refs/pull/*`, and tags. The GitHub API still resolves the commit (`gh api repos/spiritledsoftware/lspctl/commits/baaead1` returns the SHA). [INFERENCE] GitHub can remove an unreachable commit at any time. A restore must copy the files into the main history.
- The prototype directory at `baaead1` has these files: `README.md`, `catalog.json`, `check.py`, `contract-fixtures.json`, `initialize-capabilities.json`, `project-config.toml`, `user-config.toml`.

## 1. Does `check.py` emit a schema registry?

Yes. `check.py` at `baaead1` has an `--emit-schemas` mode (`check.py:1138-1141`):

```python
if sys.argv[1:] == ["--emit-schemas"]:
    json.dump(compile_schemas(catalog), sys.stdout, separators=(",", ":"), sort_keys=True)
    print()
    return 0
```

- Invocation: `python3 check.py --emit-schemas > schemas.json`. The prototype README gives the same command (`README.md:102-105`).
- `main()` always runs all checks first (`check_catalog`, `check_config`, `check_capability_profile`, `check_fixtures`, `check.py:1129-1136`). If one check fails, the script prints `FAIL:` lines and exits with 1. It does not emit schemas.
- The checks read four sibling files through fixed paths (`check.py:16-21`): `catalog.json`, `user-config.toml`, `project-config.toml`, `initialize-capabilities.json`, `contract-fixtures.json`. The generator needs all of them, not only the catalog.
- `compile_schemas` (`check.py:554-603`) builds the registry: config schemas and the skill-install marker (`compile_config_schemas`, `check.py:229-319`), one schema per output shape, and four schemas per command (`cli/<path>`, `command/<path>`, `command/<path>/success`, `command/<path>/failure`, `check.py:471-551`).
- The checks do not look for the `"prototype": true` marker. The same script accepts the catalog with or without it.

## 2. Experiments

All runs used Python 3.14.7 on macOS arm64 in temp directories. The main worktree was not changed.

### 2a. `baaead1` generator on its own catalog, compared with `schemas.json` at `97985ac`

- Command: `python3 check.py` → `ok: 41 commands, 76 shapes, 68 errors`. Then `python3 check.py --emit-schemas`.
- Result: **byte-identical** to `git show 97985ac:assets/contract/schemas.json` (730,850 bytes each).
- The catalog at `97985ac` differs from the `baaead1` catalog only by the removed line `"prototype": true` (semantic diff with `json.tool --sort-keys`).

### 2b. `baaead1` generator on the current catalog (input change only)

- Change: copy `assets/contract/catalog.json` at `69520ab` over the prototype `catalog.json`. There is no code change. (An equal one-line alternative is to change `CATALOG_PATH` at `check.py:17`.)
- All checks pass: `ok: 41 commands, 76 shapes, 68 errors`.
- Result: **not identical.** A semantic JSON diff finds 334 top-level differences: 167 keys only in the output and 167 keys only in the current file.
- Cause: the generator writes `lspc://schema/v1/cli/...`, `lspc://schema/v1/command/...`, `lspc://schema/v1/config/...`, and `lspc://schema/v1/skill-install/marker` from hard-coded strings. The 76 output-shape schemas take their `$id` from the catalog `schemaUri` fields, which are already `lspctl://`. The generator also writes `x-lspc-*` keys and `"manager": {"const": "lspc"}`.
- If you replace `lspc` with `lspctl` in this output (`sed -E 's/lspc([:"-])/lspctl\1/g'`), the result is **semantically equal** to the current file (0 differences). The bytes still differ, because renamed keys sort to new positions. A re-serialization with `sort_keys=True` then gives identical bytes.

### 2c. Generator with the rename applied

- Change 1: in `check.py`, apply `sed -E 's/lspc([:"-])/lspctl\1/g'`. This changes 23 lines: 301-303, 305, 314, 404-405, 479-482, 539-542, 545, 1007-1009, 1014-1017.
- Change 2: apply the same expression to `contract-fixtures.json`. This changes 18 lines. Without it, 18 fixture checks fail with `fixture ... names an unknown schema`, and the script emits nothing.
- Result: **byte-identical** to the current `assets/contract/schemas.json` (733,787 bytes, SHA-256 `41b99030…7b539b69`).

### 2d. Every commit that changed `assets/contract`

For each commit, the run used the commit's catalog. It used the original generator before `c719886` and the renamed generator from `c719886`. Each output was compared with the `schemas.json` of the same commit.

| Commit | Change to `assets/contract` | Generator output vs. committed `schemas.json` | Class |
|---|---|---|---|
| `97985ac` Implement complete CLI and schema catalog | Adds the catalog (marker removed), `schemas.json`, capability profile | byte-identical | Generator output |
| `33769a0` Improve CLI onboarding and query readiness evidence | Catalog: adds `"serverProgress": "array<shape:progressRecord>?"` to the `query-context` shape. `schemas.json`: adds `properties.serverProgress` to `lspc://schema/v1/output/query-context` (1 semantic difference). README text changes. | byte-identical | Catalog change that the generator reproduces |
| `04dacbc` Prepare v0.1.0 release | Catalog: `binaryVersion` `1.0.0` → `0.1.0`. `schemas.json` unchanged. | byte-identical | Catalog change with no schema effect |
| `c719886` Rename lspc to lspctl | Catalog: 196 lines, all `lspc` → `lspctl`. `schemas.json`: all names renamed. README text changes. | byte-identical only with the renamed generator | Rename applied outside the generator (see below) |
| `ba7dad2` Prepare v0.1.1 release | Catalog: `binaryVersion` `0.1.0` → `0.1.1` | byte-identical | Catalog change with no schema effect |
| `69520ab` Prepare v0.1.2 release | Catalog: `binaryVersion` `0.1.1` → `0.1.2`. README text changes. | byte-identical | Catalog change with no schema effect |

Notes on `c719886`:

- A text replacement on the `04dacbc` `schemas.json` (`sed 's/lspc/lspctl/g'`) gives bytes identical to the `c719886` file. The renamed generator gives the same bytes. [INFERENCE] The history cannot show which method the author used. The generator that did the rename, if one existed, is not in any ref.
- This is the only change that the `baaead1` generator cannot reproduce without an edit. It is a generator change that is not in history, but it is a mechanical rename only.
- `binaryVersion` does not appear in `schemas.json`, so version bumps do not change it. The runtime overrides `binaryVersion` with `CARGO_PKG_VERSION` in `schema` output (`src/contract.rs:151`).

### 3. Generator changes after `baaead1`

- `git log --all -- '*check.py' '*generat*'` lists only `aa65677` and `baaead1` for `check.py`. No other file with "generat" in its name exists in any ref.
- `build.rs` changed in `d69ae99` and `c719886`. It only resolves the build commit and target (`build.rs:3-28`). It does not generate or read contract assets.
- No script under `scripts/release/` writes `schemas.json`.
- Result: no commit after `baaead1` modified a schema generator.

## 4. What a restored generator needs

- **Python version:** 3.11 or later. The script imports `tomllib` (`check.py:11`), which Python 3.11 added. Python 3.10 fails with `ModuleNotFoundError: No module named 'tomllib'`. Python 3.11, 3.12, 3.13, and 3.14 all give the same SHA-256 digest for the renamed generator.
- **Dependencies:** standard library only: `hashlib`, `json`, `math`, `re`, `sys`, `tomllib`, `copy`, `pathlib` (`check.py:4-13`).
- **Inputs:** the catalog plus `user-config.toml`, `project-config.toml`, `initialize-capabilities.json`, and `contract-fixtures.json`. The current `assets/contract/initialize-capabilities.json` is byte-identical to the `baaead1` copy. The other three files exist only at `baaead1`.
- **Rename:** apply `lspc` → `lspctl` to `check.py` (23 lines) and `contract-fixtures.json` (18 lines), as in 2c. The config TOML files contain no `lspc` string.
- **Serialization:** `json.dump(..., separators=(",", ":"), sort_keys=True)` and then `print()`. The result is one line of compact JSON with no spaces, all object keys sorted by code point at every level, and one trailing `\n`. `ensure_ascii` keeps its default (`True`). The current file matches this format: 1 line, 733,787 bytes, ends with `}}\n`, no `\u` escapes, and the catalog has no non-ASCII bytes.
- **Order:** object key order comes only from `sort_keys`. Array order comes from the catalog order (for example `required`, `oneOf`, error code lists). A restored generator must keep the catalog array order.
- **Semantic equality is not enough for bytes.** 2b shows that the same schemas with other key names give other bytes. A byte check must compare the generator output, not a rewritten copy.

## 5. What the current checks verify

### `scripts/release/check_schema.py`

- It runs `cargo run --locked --quiet -- schema --full` (`check_schema.py:19-24`).
- It checks that the envelope has `schemaVersion` 1 and `ok` true (`check_schema.py:27-28`).
- It checks that `result.catalog` equals `assets/contract/catalog.json` and that `result.schemas` equals `assets/contract/schemas.json`, as parsed JSON values (`check_schema.py:29-32`).
- CI runs it (`.github/workflows/ci.yml:33-34`) and the release workflow runs it (`.github/workflows/release.yml:38`).

It does not verify:

- that `schemas.json` is the compiled form of `catalog.json`. The binary embeds both files with `include_str!` (`src/contract.rs:44`, `src/contract.rs:53`), so the check only proves that the binary serves the files that are on disk;
- byte format, key order, or the trailing newline;
- the catalog invariants that `check.py` checked (cross-references, error profiles, fixtures, capability profile).

### `tests/cli_contract.rs`

- `version` and `--version` give the same envelope with version `0.1.2` (`tests/cli_contract.rs:6-30`).
- `-V` gives a structured `invalid_arguments` failure (`tests/cli_contract.rs:33-53`).
- `schema` lists 41 catalog commands; `trust --help` lists 5 commands and empty `schemas` (`tests/cli_contract.rs:56-88`).
- `schema --full` has exactly 243 schemas and 41 commands. `schema definition` contains `lspctl://schema/v1/cli/definition`, `lspctl://schema/v1/command/definition`, and the `serverProgress` item `$ref` to `lspctl://schema/v1/output/progress-record`. An unknown subject exits with 2 (`tests/cli_contract.rs:91-121`).

It does not verify:

- the content of most of the 243 schemas;
- that the registry agrees with the catalog;
- that runtime outputs validate against the schemas. Only `scripts/release/reference_smoke.py:94-116` validates outputs with `jsonschema` (`Draft202012Validator`), in the nightly reference matrix. If `jsonschema` is not installed, that function returns `False` and skips validation unless the caller passes `--require-schema-validation` (`reference_smoke.py:204-206`).

## Consequences for Built-in servers

- If a later ticket changes the catalog (for example new commands, options, error codes, or output shapes for Built-in servers), `schemas.json` must change too. No tool in the main history does this today. A hand edit to a 733,787-byte single-line file is not practical.
- The smallest restore is: copy the `baaead1` prototype files into the main history, apply the `lspc` → `lspctl` rename to `check.py` and `contract-fixtures.json`, and point the input paths at `assets/contract/`. The first run must give SHA-256 `41b99030…7b539b69`.
- A check that fails when `schemas.json` is not the generator output of `catalog.json` does not exist. `check_schema.py` and `tests/cli_contract.rs` cannot detect drift between the two files.
- A catalog change that adds a config field (for example a Built-in server opt-out) also changes the config schemas, because `compile_config_schemas` reads `catalog["config"]` (`check.py:229-319`). The fixtures and TOML samples may then need updates for the checks to pass.
