#!/usr/bin/env python3
"""Measure default-feature Rust workspace source lines and enforce a 70% gate."""

import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


def summarize(report):
    """Use executable line counts, never an average of percentages."""
    try:
        files = [file for unit in report["data"] for file in unit["files"]]
        seen = set()
        total = covered = 0
        for file in files:
            name = file["filename"]
            lines = file["summary"]["lines"]
            count, hit = lines["count"], lines["covered"]
            if name in seen or not isinstance(count, int) or not isinstance(hit, int):
                raise ValueError("duplicate file or invalid line counts")
            if not 0 <= hit <= count:
                raise ValueError("invalid line counts")
            seen.add(name)
            total += count
            covered += hit
        if not total:
            raise ValueError("no executable source lines in coverage report")
        return {"lines": total, "covered": covered, "files": files}
    except (KeyError, TypeError) as error:
        raise ValueError("missing or malformed LLVM coverage data") from error


def gate_status(summary, minimum, test_status):
    if test_status:
        return 2
    return 0 if summary["covered"] * 100 >= minimum * summary["lines"] else 1


def collect_objects(log, workspace_members):
    objects = set()
    build_finished = False
    for line in log.read_text().splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue  # libtest output shares stdout with Cargo JSON messages.
        if not isinstance(message, dict):
            continue
        if message.get("reason") == "build-finished":
            build_finished = message.get("success") is True
        if (message.get("reason") == "compiler-artifact"
                and message.get("package_id") in workspace_members
                and message.get("executable")):
            objects.add(message["executable"])
    if not build_finished or not objects:
        raise ValueError("build did not finish successfully or produced no coverage objects")
    return sorted(objects)


def llvm_tool(name, major, sysroot, host):
    candidates = [os.environ.get(name.upper().replace("-", "_")),
                  str(sysroot / "lib/rustlib" / host / "bin" / name),
                  shutil.which(f"{name}-{major}"), shutil.which(name)]
    for candidate in candidates:
        if not candidate or not Path(candidate).is_file():
            continue
        version = subprocess.check_output([candidate, "--version"], text=True)
        if re.search(rf"LLVM version {major}\.", version, re.IGNORECASE):
            return candidate
    raise ValueError(f"{name} matching rustc's LLVM {major} is required; "
                     "install llvm-tools-preview or set LLVM_COV and LLVM_PROFDATA")


def run(args):
    metadata_cmd = ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked",
                    "--manifest-path", str(args.manifest_path.resolve())]
    if args.offline:
        metadata_cmd.append("--offline")
    metadata = json.loads(subprocess.check_output(metadata_cmd, text=True))
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    sources = []
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        directory = Path(package["manifest_path"]).parent
        # Integration/E2E harnesses are tests, not production source. All crate
        # and bin sources stay in scope, including tools with no tests at all.
        if directory.relative_to(root).parts[0] == "tests":
            continue
        sources.extend(directory.glob("src/**/*.rs"))
    if not sources:
        raise ValueError("workspace has no source files")

    rust_version = subprocess.check_output(["rustc", "-vV"], text=True)
    major = re.search(r"LLVM version: (\d+)", rust_version).group(1)
    host = re.search(r"host: (\S+)", rust_version).group(1)
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    cov = llvm_tool("llvm-cov", major, sysroot, host)
    profdata = llvm_tool("llvm-profdata", major, sysroot, host)

    output = args.output_dir.resolve() if args.output_dir else root / ".artifacts/coverage"
    output.mkdir(parents=True, exist_ok=True)
    # Each run owns its profiles. Never mix a previous build's counters into a
    # new report, and never recursively delete a caller-supplied target path.
    run_dir = Path(tempfile.mkdtemp(prefix="run-", dir=output))
    profiles = run_dir / "profraw"
    profiles.mkdir()
    env = os.environ.copy()
    if env.get("RUSTC_WORKSPACE_WRAPPER"):
        raise ValueError("unset RUSTC_WORKSPACE_WRAPPER before running coverage")
    wrapper = Path(__file__).resolve().with_name("coverage-rustc.sh")
    env.update(RUSTC_WORKSPACE_WRAPPER=str(wrapper),
               LLVM_PROFILE_FILE=str(profiles / "%p-%m.profraw"))
    env.setdefault("CARGO_TARGET_DIR", str(output / "target"))
    command = ["cargo", "test", "--workspace", "--locked", "--no-fail-fast",
               "--message-format=json", "--manifest-path", str(root / "Cargo.toml")]
    if args.offline:
        command.append("--offline")
    print(f"Running workspace tests; logs and reports: {run_dir}", flush=True)
    with (run_dir / "tests.log").open("w") as log:
        result = subprocess.run(command, cwd=root, env=env, stdout=log)
    objects = collect_objects(run_dir / "tests.log", members)
    raw = sorted(profiles.glob("*.profraw"))
    if not raw:
        raise ValueError("tests produced no LLVM profiles")
    # A response file avoids the OS argument-size limit in large workspaces.
    profile_list = run_dir / "profiles.txt"
    profile_list.write_text("\n".join(json.dumps(str(path)) for path in raw) + "\n")
    merged = run_dir / "coverage.profdata"
    subprocess.run([profdata, "merge", "-sparse", f"@{profile_list}", "-o", str(merged)],
                   check=True)
    export_args = [f"--instr-profile={merged}"]
    for obj in objects:
        export_args.extend(["--object", obj])
    export_args.extend(["--sources", *map(str, sorted(sources))])
    response = run_dir / "export-args.txt"
    response.write_text("\n".join(json.dumps(arg) for arg in export_args) + "\n")
    with (run_dir / "coverage.json").open("w") as report:
        subprocess.run([cov, "export", f"@{response}"], stdout=report, check=True)
    summary = summarize(json.loads((run_dir / "coverage.json").read_text()))
    groups = defaultdict(lambda: [0, 0])
    mapped = set()
    for file in summary["files"]:
        path = Path(file["filename"])
        relative = path.relative_to(root)
        mapped.add(path)
        key = "/".join(relative.parts[:2])
        lines = file["summary"]["lines"]
        groups[key][0] += lines["covered"]
        groups[key][1] += lines["count"]
    lines = ["Rust workspace source line coverage (default features, host target).",
             "Includes inline test modules; excludes integration/E2E harness source and dependencies.",
             "This is not branch coverage or coverage of all platform/feature combinations.", ""]
    for name, (hit, count) in sorted(groups.items()):
        percent = hit * 100 / count if count else 0
        lines.append(f"{name:40} {hit:6}/{count:<6} {percent:7.2f}%")
    percent = summary["covered"] * 100 / summary["lines"]
    lines.extend(["", f"TOTAL: {summary['covered']}/{summary['lines']} = {percent:.4f}%",
                  f"Required: {args.min_lines:g}%", f"Cargo test exit status: {result.returncode}"])
    unmapped = sorted(str(path.relative_to(root)) for path in set(sources) - mapped)
    (run_dir / "unmapped-sources.json").write_text(json.dumps(unmapped, indent=2) + "\n")
    lines.append(f"Files without coverage mappings: {len(unmapped)} (see unmapped-sources.json)")
    status = gate_status(summary, args.min_lines, result.returncode)
    lines.append("PASS" if status == 0 else "FAIL: tests failed or line coverage is below the threshold")
    text = "\n".join(lines) + "\n"
    (run_dir / "summary.txt").write_text(text)
    print(text, end="")
    return status


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest-path", type=Path, default=Path("Cargo.toml"))
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--min-lines", type=float, default=70)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if not 0 <= args.min_lines <= 100:
        parser.error("--min-lines must be between 0 and 100")
    try:
        return run(args)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"Coverage failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
