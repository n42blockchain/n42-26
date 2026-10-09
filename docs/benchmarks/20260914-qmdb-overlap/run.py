"""Compare serial/overlapped read-view construction, including durable commit, in fresh processes."""
import argparse
import hashlib
import json
import os
import re
import statistics
import subprocess
from pathlib import Path

def positive(value):
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--binary", type=Path, required=True)
p.add_argument("--out", type=Path, required=True)
p.add_argument("--tmpdir", type=Path, required=True)
p.add_argument("--preparation", choices=["none", "retain"], default="none")
p.add_argument("--pairs", type=positive, default=6)
p.add_argument("--blocks", type=positive, default=3)
p.add_argument("--keys", type=positive, nargs="+", default=[200000, 2000000])
args = p.parse_args()
binary = args.binary.resolve()
args.out.mkdir(parents=True, exist_ok=True)
args.tmpdir.mkdir(parents=True, exist_ok=True)
binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
(args.out / "run.json").write_text(json.dumps(dict(binary_sha256=binary_hash, keys=args.keys,
    pairs=args.pairs, blocks=args.blocks, read_views=True, preparation=args.preparation, tmpdir=str(args.tmpdir.resolve())), indent=2) + "\n")
rows = []
for keys in args.keys:
    for pair in range(args.pairs):
        for mode in (("0", "1") if pair % 2 == 0 else ("1", "0")):
            env = dict(os.environ, TMPDIR=str(args.tmpdir.resolve()), N42_QMDB_REBASE_BLOCKS="20000",
                N42_COMMIT_BENCH_READS="1", N42_COMMIT_BENCH_KEYS=str(keys),
                N42_COMMIT_BENCH_BLOCKS=str(args.blocks), N42_COMMIT_BENCH_PREPARATION=args.preparation, N42_COMMIT_BENCH_OVERLAP=mode)
            result = subprocess.run([str(binary), "store::tests::bench_persistent_commit_path",
                "--ignored", "--exact", "--nocapture", "--test-threads=1"],
                env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            (args.out / f"{keys}-{pair}-{mode}.log").write_text(result.stdout)
            assert result.returncode == 0 and "commit_bench restart_verified=true" in result.stdout, result.stdout
            totals = re.findall(r"commit_bench reads=1 keys=(\d+) updates=(\d+) generation=(\d+) elapsed_ms=([\d.]+) root=(0x[0-9a-f]{64}) wal_bytes=(\d+)", result.stdout)
            phases = re.findall(r"prepare_bench mode=(\w+) retained=(true|false) generation=(\d+) candidate_ms=([\d.]+) commit_ms=([\d.]+)", result.stdout)
            overlaps = re.findall(r"overlap_bench generation=(\d+) joined=(\d+)", result.stdout)
            assert len(totals) == len(phases) == len(overlaps) == args.blocks, result.stdout
            assert f"overlap_bench enabled={str(mode == '1').lower()}" in result.stdout
            for (generation, joined), total in zip(overlaps, totals):
                assert generation == total[2]
                assert int(joined) == int(mode == "1" and args.preparation == "none"), result.stdout
            for total, phase in zip(totals, phases):
                k, updates, generation, elapsed, root, wal = total
                m, retained, g, candidate, commit = phase
                assert int(k) == keys and int(updates) == min(keys, 147000)
                assert generation == g and m == args.preparation and (retained == "true") == (args.preparation == "retain")
                rows.append(dict(keys=keys, pair=pair, mode=mode, generation=int(g), updates=int(updates),
                    elapsed_ms=float(elapsed), candidate_ms=float(candidate), commit_ms=float(commit), root=root, wal_bytes=int(wal)))
            print(keys, pair, mode, [float(t[3]) for t in totals], flush=True)
            (args.out / "samples.json").write_text(json.dumps(rows, indent=2) + "\n")
for keys in args.keys:
    for generation in range(2, args.blocks+2):
        assert len({(r["root"], r["wal_bytes"]) for r in rows if r["keys"] == keys and r["generation"] == generation}) == 1
assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary_hash
summary = []
for keys in args.keys:
    for mode in ("0", "1"):
        selected = [r for r in rows if r["keys"] == keys and r["mode"] == mode]
        row = dict(keys=keys, mode=mode, samples=len(selected))
        for field in ("elapsed_ms", "candidate_ms", "commit_ms"):
            values = [r[field] for r in selected]
            row[field] = dict(median=statistics.median(values), minimum=min(values), maximum=max(values))
        summary.append(row)
(args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2), flush=True)
