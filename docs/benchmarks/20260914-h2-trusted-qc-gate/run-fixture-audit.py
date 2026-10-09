"""Archive a synthetic four-node audit using real Keccak and BLS executables."""
import argparse
import importlib.util
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--repo", type=Path, required=True)
parser.add_argument("--out", type=Path, required=True)
args = parser.parse_args()
spec = importlib.util.spec_from_file_location("fixtures", args.repo / "scripts/test-h2-tps-audit.py")
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)
fleet = fixtures.Fleet()
start = fleet.capture(0, 1_000_000_000)
fleet.heads = [13, 14, 15, 13]
end = fleet.capture(61_000_000_000, 63_000_000_000)
report = fixtures.audit.audit(start, end, fleet.rpc, trusted=fleet.trusted)
assert report["successful_committed_tps"] == 9 / 63
assert len(report["boundary_commit_proofs"]) == 8
assert sum(len(path["headers"]) for path in report["boundary_ancestry"]) == 3
args.out.mkdir(parents=True, exist_ok=True)
for name, value in (("trusted-config", fleet.trusted), ("start", start), ("end", end), ("audit", report)):
    (args.out / f"{name}.json").write_text(json.dumps(value, indent=2) + "\n")
(args.out / "fixture-only.json").write_text(json.dumps(dict(fixture_only=True, actual_tps_measured=False)) + "\n")
print("Synthetic 9/63 TPS, eight real BLS certificate checks, three boundary ancestry headers; not fleet evidence.")
