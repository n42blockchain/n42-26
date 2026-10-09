#!/usr/bin/env python3
"""Capture a dirty Reth checkout without writing to its source tree."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Any


def _git(source: Path, *args: str, binary: bool = False) -> bytes | str:
    result = subprocess.run(
        ["git", "-C", str(source), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"source must be a readable Git repository: {detail}")
    return result.stdout if binary else result.stdout.decode("utf-8").strip()


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _status_entries(status: bytes) -> list[dict[str, Any]]:
    fields = status.split(b"\0")
    entries: list[dict[str, Any]] = []
    index = 0
    while index < len(fields):
        record = fields[index]
        index += 1
        if not record:
            continue
        if len(record) < 4 or record[2:3] != b" ":
            raise ValueError("Git returned an unsupported porcelain status record")
        xy = record[:2].decode("ascii", errors="strict")
        path = os.fsdecode(record[3:])
        entry: dict[str, Any] = {
            "path": path,
            "status": xy,
            "tracked": xy != "??",
            "captured": False,
        }
        if "R" in xy or "C" in xy:
            if index >= len(fields) or not fields[index]:
                raise ValueError("Git returned an incomplete rename/copy status record")
            entry["source_path"] = os.fsdecode(fields[index])
            index += 1
        entries.append(entry)
    return entries


def _path_digest(path: Path) -> str | None:
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if path.is_symlink():
        return _sha256(os.fsencode(os.readlink(path)))
    if not path.is_file():
        return None
    if not info.st_mode:
        return None
    return _sha256(path.read_bytes())


def _inside(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def prepare(source: Path, destination: Path) -> dict[str, Any]:
    """Preserve Git changes and untracked source files under destination.

    The source checkout is read-only from this function's perspective. The
    destination must be new and outside the source repository.
    """
    source = Path(source).expanduser().resolve()
    if not source.exists():
        raise FileNotFoundError(source)
    if not source.is_dir():
        raise ValueError(f"source must be a Git repository directory: {source}")

    top = Path(str(_git(source, "rev-parse", "--show-toplevel"))).resolve()
    if source != top:
        raise ValueError(f"source must be the Git repository root: {top}")

    head = str(_git(source, "rev-parse", "HEAD"))
    status_before = bytes(_git(source, "status", "--porcelain=v1", "--untracked-files=all", "-z", binary=True))
    entries = _status_entries(status_before)
    dirty_patch = bytes(_git(source, "diff", "--binary", "--full-index", "HEAD", "--", binary=True))

    destination = Path(destination).expanduser()
    if not destination.is_absolute():
        destination = Path.cwd() / destination
    destination = destination.absolute()
    resolved_parent = destination.parent.resolve()
    resolved_destination = resolved_parent / destination.name
    if _inside(resolved_destination, source):
        raise ValueError("destination must be outside the source Git repository")
    if os.path.lexists(destination):
        raise FileExistsError(f"destination already exists: {destination}")

    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.mkdir()
    try:
        (destination / "current-dirty.patch").write_bytes(dirty_patch)
        untracked_root = destination / "untracked-source"
        patch_digest = _sha256(dirty_patch)

        for entry in entries:
            path = source / entry["path"]
            entry["sha256"] = _path_digest(path)
            if entry["tracked"]:
                entry["captured"] = True  # Its content diff is in current-dirty.patch.
                continue

            parts = Path(entry["path"]).parts
            if entry["path"] == "target" or "target" in parts or path.is_symlink():
                entry["reason"] = "build_output_or_symlink"
                continue
            if not path.is_file():
                entry["reason"] = "not_a_regular_source_file"
                continue

            relative = Path(*parts)
            output = untracked_root / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, output)
            if _path_digest(output) != entry["sha256"]:
                raise RuntimeError(f"untracked source changed while copying: {entry['path']}")
            entry["captured"] = True

        head_after = str(_git(source, "rev-parse", "HEAD"))
        status_after = bytes(_git(source, "status", "--porcelain=v1", "--untracked-files=all", "-z", binary=True))
        if head_after != head or status_after != status_before:
            raise RuntimeError("source Git state changed while the preservation snapshot was being made")
        for entry in entries:
            if _path_digest(source / entry["path"]) != entry["sha256"]:
                raise RuntimeError(f"source file changed while snapshotting: {entry['path']}")

        manifest: dict[str, Any] = {
            "schema": 1,
            "source_path": str(source),
            "head": head,
            "status_sha256": _sha256(status_before),
            "dirty_patch_sha256": patch_digest,
            "entry_count": len(entries),
            "entries": entries,
        }
        (destination / "current-source-manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return manifest
    except BaseException:
        shutil.rmtree(destination, ignore_errors=True)
        raise


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path("../reth"))
    parser.add_argument("--destination", type=Path, default=Path(".artifacts/reth-upgrade"))
    args = parser.parse_args(argv)
    try:
        manifest = prepare(args.source, args.destination)
    except (FileNotFoundError, FileExistsError, ValueError, RuntimeError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(
        f"Preserved {manifest['entry_count']} dirty entries from "
        f"{manifest['head']} at {args.destination}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
