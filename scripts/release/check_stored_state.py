#!/usr/bin/env python3
"""Verify immutable same-major stored-state compatibility seeds."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path, PurePath


ROOT = Path(__file__).resolve().parents[2] / "tests/fixtures/stored-state"
REQUIRED = {"trust.json", "preview.json", "receipt.json", "recovery.json"}


def check_seed(directory: str, release: str, first_release: bool) -> None:
    root = ROOT / directory
    manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
    if set(manifest) != {"formatVersion", "release", "firstRelease", "files"}:
        raise SystemExit("stored-state manifest has unexpected fields")
    if manifest["formatVersion"] != 1 or manifest["release"] != release:
        raise SystemExit("stored-state manifest identity is invalid")
    if manifest["firstRelease"] is not first_release or not isinstance(manifest["files"], list):
        raise SystemExit("stored-state manifest release metadata is invalid")

    names: set[str] = set()
    for record in manifest["files"]:
        if set(record) != {"path", "sha256"} or not isinstance(record["path"], str):
            raise SystemExit("stored-state file entry is invalid")
        relative = PurePath(record["path"])
        path = root / record["path"]
        if (
            relative.is_absolute()
            or len(relative.parts) != 1
            or path.name in names
            or not path.is_file()
            or path.is_symlink()
        ):
            raise SystemExit("stored-state fixture path is invalid")
        names.add(path.name)
        digest = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
        if digest != record["sha256"]:
            raise SystemExit(f"stored-state fixture changed: {path.name}")
        value = json.loads(path.read_text(encoding="utf-8"))
        if value.get("formatVersion") != 1:
            raise SystemExit(f"stored-state fixture version is invalid: {path.name}")
    if names != REQUIRED:
        raise SystemExit(f"stored-state seed is incomplete: {names!r}")


def main() -> None:
    check_seed("v1", "0.1.0", True)
    check_seed("v0.1.1", "0.1.1", False)


if __name__ == "__main__":
    main()
