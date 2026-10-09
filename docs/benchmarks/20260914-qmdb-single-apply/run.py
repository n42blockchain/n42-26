#!/usr/bin/env python3
"""Compare archived persistent-store implementations in fresh processes."""

import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
from pathlib import Path


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--tmpdir", type=Path, required=True,
                        help="Filesystem directory for temporary WAL fixtures")
    parser.add_argument("--keys", type=positive, default=200_000)
    parser.add_argument("--blocks", type=positive, default=3)
    parser.add_argument("--pairs", type=positive, default=6)
    parser.add_argument("--reads", choices=("0", "1", "0,1"), default="1")
    args = parser.parse_args()
    binary = args.binary.resolve()
    out = args.output
    out.mkdir(parents=True, exist_ok=True)
    args.tmpdir.mkdir(parents=True, exist_ok=True)
    modes = [int(value) for value in args.reads.split(",")]
    metadata = dict(binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    tmpdir=str(args.tmpdir.resolve()), keys=args.keys, blocks=args.blocks,
                    pairs=args.pairs, reads=modes, rebase_blocks=20_000)
    (out / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    rows = []
    for reads in modes:
        for pair in range(args.pairs):
            order = ["baseline", "optimized"] if pair % 2 == 0 else ["optimized", "baseline"]
            for version in order:
                env = dict(os.environ, TMPDIR=str(args.tmpdir.resolve()),
                           N42_QMDB_REBASE_BLOCKS="20000",
                           N42_COMMIT_BENCH_READS=str(reads),
                           N42_COMMIT_BENCH_KEYS=str(args.keys),
                           N42_COMMIT_BENCH_BLOCKS=str(args.blocks))
                command = [str(binary), version + "::tests::bench_persistent_commit_path",
                           "--ignored", "--exact", "--nocapture", "--test-threads=1"]
                run = subprocess.run(command, env=env, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT, text=True)
                (out / f"{reads}-{pair}-{version}.log").write_text(run.stdout)
                if run.returncode or "restart_verified=true" not in run.stdout:
                    raise RuntimeError(run.stdout)
                samples = re.findall(
                    r"commit_bench reads=(\d) keys=(\d+) updates=(\d+) generation=(\d+) "
                    r"elapsed_ms=([0-9.]+) root=(0x[0-9a-f]{64}) wal_bytes=(\d+)", run.stdout)
                if len(samples) != args.blocks:
                    raise RuntimeError("wrong sample count: " + run.stdout)
                for got_reads, keys, updates, generation, ms, root, size in samples:
                    if int(got_reads) != reads or int(keys) != args.keys:
                        raise RuntimeError("benchmark configuration differs")
                    rows.append(dict(reads=reads, pair=pair, version=version, keys=int(keys),
                                     updates=int(updates), generation=int(generation),
                                     elapsed_ms=float(ms), root=root, wal_bytes=int(size)))
                print(json.dumps(dict(reads=reads, pair=pair, version=version,
                                      elapsed_ms=[float(s[4]) for s in samples])), flush=True)
                (out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
    for generation in range(2, args.blocks + 2):
        identities = {(row["root"], row["wal_bytes"]) for row in rows
                      if row["generation"] == generation}
        if len(identities) != 1:
            raise RuntimeError(f"root/WAL disagreement at generation {generation}: {identities}")
    summary = []
    for reads in modes:
        for version in ["baseline", "optimized"]:
            values = [r["elapsed_ms"] for r in rows
                      if r["reads"] == reads and r["version"] == version]
            summary.append(dict(reads=reads, version=version, count=len(values),
                                median_ms=statistics.median(values),
                                min_ms=min(values), max_ms=max(values)))
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
