#!/usr/bin/env python3
"""Audit and compare a seven-validator Reth old/new/old performance campaign."""

import json
import sys
from pathlib import Path


TAGS = ("reth-a1", "reth-b", "reth-a2")


def read_tsv(path: Path) -> dict[str, str]:
    return dict(line.split("\t", 1) for line in path.read_text().splitlines() if "\t" in line)


def summarize(campaign: Path) -> dict:
    legs: dict[str, dict] = {}
    workload_hashes, trusted_hashes, run_configurations, artifact_config_hashes = set(), set(), set(), set()
    for tag in TAGS:
        result = campaign / f"result-{tag}"
        summary = read_tsv(result / "qualification" / "summary.tsv")
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
        tx_count = int(summary["successful_committed_transactions"])
        if len(windows) != 4 or sum(windows) != tx_count:
            raise ValueError(f"{tag} windows do not cover all scored transactions")
        verify = (result / "verify-after.log").read_text()
        if "PASS:" not in verify or "all 7 roots/hashes agree" not in verify:
            raise ValueError(f"{tag} failed post-run seven-node verification")
        if "PASS: post-restart state agrees on all 7 validators" not in verify:
            raise ValueError(f"{tag} failed post-restart state verification")

        leg = read_tsv(result / "leg.tsv")
        if leg.get("tag") != tag:
            raise ValueError(f"{tag} leg metadata tag mismatch")
        if not all(leg.get(key) for key in ("options", "max_txs_per_block",
                                             "gov5_reuse_builder_execution", "rpc_max_response_mb")):
            raise ValueError(f"{tag} has incomplete runtime configuration")
        workload, binary = None, None
        for line in (result / "binary-workload-sha256.txt").read_text().splitlines():
            digest, filename = line.split(maxsplit=1)
            if filename.endswith("/n42-node"):
                binary = digest
            elif filename.endswith("presigned-48m-20260927.bin"):
                workload = digest
        if not binary or not workload:
            raise ValueError(f"{tag} missing binary/workload identity")
        source = json.loads((result / "source-manifest.json").read_text())
        if not all(source.get(k) for k in ("reth_revision", "reth_source_sha256", "lockfile_sha256", "source_tree_sha256")):
            raise ValueError(f"{tag} has incomplete source manifest")

        workload_hashes.add(workload)
        trusted_hashes.add(summary.get("trusted_config_sha256"))
        run_configurations.add((leg.get("options"), leg.get("max_txs_per_block"),
                                leg.get("gov5_reuse_builder_execution"), leg.get("rpc_max_response_mb")))
        config_entries = tuple(sorted(
            line.split(maxsplit=1)[0]
            for line in (result / "run-config-sha256.txt").read_text().splitlines()
        ))
        if len(config_entries) != 5:
            raise ValueError(f"{tag} is missing immutable runtime configuration hashes")
        artifact_config_hashes.add(config_entries)
        first_tps = windows[0] / 15
        sustained_tps = sum(windows[1:]) / 45
        legs[tag] = {
            "binary_sha256": binary,
            "successful_committed_tps": float(summary["successful_committed_tps"]),
            "successful_transactions": tx_count,
            "measurement_seconds": float(summary["measurement_seconds"]),
            "first_window_tps": first_tps,
            "sustained_windows_tps": [count / 15 for count in windows[1:]],
            "sustained_tps": sustained_tps,
            "windows_15s": windows,
            "stages": timeline.get("stages", {}),
            "source": source,
        }

    if len(workload_hashes) != 1:
        raise ValueError("A/B/A legs used different signed workloads")
    if len(trusted_hashes) != 1 or None in trusted_hashes:
        raise ValueError("A/B/A legs used different trusted validator configs")
    if len(run_configurations) != 1:
        raise ValueError("A/B/A legs used different run configurations")
    if len(artifact_config_hashes) != 1:
        raise ValueError("A/B/A legs used different genesis/account/validator configuration hashes")
    if legs["reth-a1"]["binary_sha256"] != legs["reth-a2"]["binary_sha256"]:
        raise ValueError("A bookends used different binaries")
    if legs["reth-a1"]["source"] != legs["reth-a2"]["source"]:
        raise ValueError("A bookends used different source manifests")
    if legs["reth-a1"]["source"]["reth_revision"] == legs["reth-b"]["source"]["reth_revision"]:
        raise ValueError("B did not use a different Reth revision")
    if legs["reth-a1"]["binary_sha256"] == legs["reth-b"]["binary_sha256"]:
        raise ValueError("A and B used the same n42-node binary")

    a_total = (legs["reth-a1"]["successful_committed_tps"] + legs["reth-a2"]["successful_committed_tps"]) / 2
    a_sustained = (legs["reth-a1"]["sustained_tps"] + legs["reth-a2"]["sustained_tps"]) / 2
    drift = abs(legs["reth-a2"]["successful_committed_tps"] - legs["reth-a1"]["successful_committed_tps"])
    drift_pct = drift / a_total * 100 if a_total else float("inf")
    sustained_drift_pct = (
        abs(legs["reth-a2"]["sustained_tps"] - legs["reth-a1"]["sustained_tps"])
        / a_sustained * 100 if a_sustained else float("inf")
    )
    uplift = (legs["reth-b"]["sustained_tps"] - a_sustained) / a_sustained * 100 if a_sustained else float("inf")
    valid = drift_pct <= 5 and sustained_drift_pct <= 5
    return {
        "campaign": str(campaign), "legs": legs,
        "a_mean_total_tps": a_total, "a_mean_sustained_tps": a_sustained,
        "a_bookend_drift_pct": drift_pct, "a_sustained_bookend_drift_pct": sustained_drift_pct,
        "b_sustained_uplift_pct": uplift,
        "bookends_within_5pct": valid,
        "decision": "candidate-improved" if valid and uplift > max(drift_pct, sustained_drift_pct) else
                    "candidate-not-improved" if valid else "inconclusive-bookend-drift",
    }


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {Path(sys.argv[0]).name} CAMPAIGN_DIR", file=sys.stderr)
        return 2
    report = summarize(Path(sys.argv[1]))
    (Path(sys.argv[1]) / "reth-ab-summary.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "legs"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
