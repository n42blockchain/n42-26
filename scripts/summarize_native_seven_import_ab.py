#!/usr/bin/env python3
"""Validate and summarize the warmed follower-import A/B/A campaign."""

import json
import sys
from pathlib import Path


TAGS = ("import-a1", "import-b", "import-a2")


def summarize(campaign: Path) -> dict:
    legs = {}
    binary_hashes = set()
    workload_hashes = set()
    trusted_hashes = set()
    for tag in TAGS:
        result = campaign / f"result-{tag}"
        qualification = result / "qualification"
        summary = dict(
            line.split("\t", 1)
            for line in (qualification / "summary.tsv").read_text().splitlines()[1:]
            if "\t" in line
        )
        timeline = json.loads((result / "timeline-score.json").read_text())
        if summary.get("throughput_status") != "passed":
            raise ValueError(f"{tag} score audit did not pass")
        if summary.get("validators") != "7":
            raise ValueError(f"{tag} did not audit seven validators")
        if summary.get("failed_committed_transactions") != "0":
            raise ValueError(f"{tag} has failed committed transactions")
        if timeline.get("unmatched_audited_transaction_blocks") != 0:
            raise ValueError(f"{tag} has unmatched audited transaction blocks")
        windows = timeline.get("successful_commit_windows_15s", [])
        if len(windows) != 4 or sum(windows) != int(summary["successful_committed_transactions"]):
            raise ValueError(f"{tag} windows do not cover all scored transactions")
        leg = dict(line.split("\t", 1) for line in (result / "leg.tsv").read_text().splitlines())
        expected_options = "--parallel-build" if tag != "import-b" else "--parallel-build --parallel-import"
        if leg.get("options") != expected_options:
            raise ValueError(f"{tag} has unexpected execution options")
        hashes = {}
        for line in (result / "binary-workload-sha256.txt").read_text().splitlines():
            digest, filename = line.split(maxsplit=1)
            if filename.endswith("/n42-node"):
                hashes["binary"] = digest
            if filename.endswith("presigned-48m-20260927.bin"):
                hashes["workload"] = digest
        if set(hashes) != {"binary", "workload"}:
            raise ValueError(f"{tag} is missing binary/workload identity")
        binary_hashes.add(hashes["binary"])
        workload_hashes.add(hashes["workload"])
        trusted_hashes.add(summary["trusted_config_sha256"])
        verify = (result / "verify-after.log").read_text()
        if "PASS:" not in verify or "all 7 roots/hashes agree" not in verify:
            raise ValueError(f"{tag} failed post-run seven-node verification")
        legs[tag] = {
            "successful_committed_tps": float(summary["successful_committed_tps"]),
            "successful_transactions": int(summary["successful_committed_transactions"]),
            "measurement_seconds": float(summary["measurement_seconds"]),
            "windows_15s": windows,
            "stages": timeline["stages"],
        }

    if len(binary_hashes) != 1:
        raise ValueError("A/B/A legs used different n42-node binaries")
    if len(workload_hashes) != 1:
        raise ValueError("A/B/A legs used different signed workloads")
    if len(trusted_hashes) != 1:
        raise ValueError("A/B/A legs used different trusted validator configs")

    a1 = legs["import-a1"]["successful_committed_tps"]
    b = legs["import-b"]["successful_committed_tps"]
    a2 = legs["import-a2"]["successful_committed_tps"]
    a_mean = (a1 + a2) / 2
    drift_pct = abs(a2 - a1) / a_mean * 100 if a_mean else float("inf")
    uplift_pct = (b - a_mean) / a_mean * 100 if a_mean else float("inf")
    valid_ab = drift_pct <= 5.0
    return {
        "campaign": str(campaign),
        "legs": legs,
        "a_mean_tps": a_mean,
        "a_bookend_drift_pct": drift_pct,
        "b_uplift_pct": uplift_pct,
        "bookends_within_5pct": valid_ab,
        "decision": "candidate-improved" if valid_ab and b > a_mean else
                    "candidate-not-improved" if valid_ab else "inconclusive-bookend-drift",
    }


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} CAMPAIGN_DIR", file=sys.stderr)
        return 2
    report = summarize(Path(sys.argv[1]))
    output = Path(sys.argv[1]) / "import-ab-summary.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "legs"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
