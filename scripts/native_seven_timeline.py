#!/usr/bin/env python3
"""Join audited seven-node transaction blocks to their logged wall-clock stages."""

import argparse
import datetime as dt
import json
import re
import statistics
from pathlib import Path


HASH = re.compile(r"\b(?:block_hash|hash)=(0x[0-9a-f]+)\b")
TIMESTAMP = re.compile(r"^(\S+)")
STAGES = {
    "N42_CADENCE: build_start->broadcast": ("leader_broadcast_ms", "build_start_to_broadcast_ms"),
    "validated normalized Gov5 leader payload": ("leader_validation_ms", "elapsed_ms"),
    "N42_COMPRESS: payload compressed": ("compression_ms", "compress_ms"),
    "N42_FOLLOWER_IMPORT: block_data->accepted": ("follower_import_ms", "follower_import_ms"),
}


def _timestamp_ns(line: str) -> int | None:
    match = TIMESTAMP.search(line)
    if not match:
        return None
    try:
        instant = dt.datetime.fromisoformat(match.group(1).replace("Z", "+00:00"))
    except ValueError:
        return None
    elapsed = instant - dt.datetime(1970, 1, 1, tzinfo=dt.timezone.utc)
    return ((elapsed.days * 86400 + elapsed.seconds) * 1_000_000_000
            + elapsed.microseconds * 1000)


def build_timeline(
    campaign: Path,
    tag: str,
    *,
    audit_name: str = "h2-audit.json",
    start_marker: str = "ingest-start.ns",
) -> dict:
    qualification = campaign / f"result-{tag}" / "qualification"
    audit = json.loads((qualification / audit_name).read_text())
    start_ns = int((qualification / start_marker).read_text().strip())
    blocks = [
        {
            "hash": block["hash"],
            "successful_transactions": block["successful_transactions"],
            "node0_commit_ns": None,
            "leader_broadcast_ms": None,
            "leader_validation_ms": None,
            "packing_ms": None,
            "builder_finish_ms": None,
            "payload_built_ms": None,
            "compression_ms": None,
            "payload_to_engine_ms": None,
            "validation_to_compress_done_ms": None,
            "follower_import_ms": {},
        }
        for block in audit["blocks"]
        if block["successful_transactions"] > 0
    ]
    by_hash = {block["hash"]: block for block in blocks}
    for index in range(7):
        log = campaign / f"runtime-{tag}" / "logs" / f"node{index}.log"
        if not log.exists():
            continue
        active_build = None
        with log.open(errors="replace") as stream:
            for line in stream:
                if "N42_TIMEOUT_VIEW: leader_build_start" in line:
                    active_build = {}
                if active_build is not None:
                    if "N42_PAYLOAD_PACK: tx packing complete" in line:
                        duration = re.search(r"\bpacking_ms=(\d+)\b", line)
                        if duration:
                            active_build["packing_ms"] = int(duration.group(1))
                    elif "N42_FINISH_BREAKDOWN:" in line:
                        duration = re.search(r"\btotal_finish_ms=(\d+)\b", line)
                        if duration:
                            active_build["builder_finish_ms"] = int(duration.group(1))
                    elif "payload built elapsed_ms=" in line:
                        duration = re.search(r"\belapsed_ms=(\d+)\b", line)
                        if duration:
                            active_build["payload_built_ms"] = int(duration.group(1))
                            active_build["payload_built_ns"] = _timestamp_ns(line)
                    elif "Received new payload from consensus engine" in line and active_build.get("payload_built_ns") is not None and "payload_to_engine_ms" not in active_build:
                        received_ns = _timestamp_ns(line)
                        if received_ns is not None:
                            active_build["payload_to_engine_ms"] = max(
                                0, (received_ns - active_build["payload_built_ns"]) // 1_000_000
                            )
                found = HASH.search(line)
                completed_build = None
                if "N42_CADENCE: build_start->broadcast" in line:
                    completed_build, active_build = active_build, None
                if not found or found.group(1) not in by_hash:
                    continue
                block = by_hash[found.group(1)]
                if "validated normalized Gov5 leader payload" in line:
                    block["validation_complete_ns"] = _timestamp_ns(line)
                if "N42_COMPRESS: payload compressed" in line:
                    block["compression_complete_ns"] = _timestamp_ns(line)
                if completed_build is not None:
                    block.update(completed_build)
                if index == 0 and "N42_CADENCE: inter-block commit interval" in line:
                    block["node0_commit_ns"] = _timestamp_ns(line)
                for marker, (field, duration_key) in STAGES.items():
                    if marker not in line:
                        continue
                    duration = re.search(r"\b" + duration_key + r"=(\d+)\b", line)
                    if duration is None:
                        continue
                    value = int(duration.group(1))
                    if field == "follower_import_ms":
                        block[field][f"node{index}"] = value
                    else:
                        block[field] = value
    windows = [0, 0, 0, 0]
    unmatched = 0
    for block in blocks:
        validated = block.get("validation_complete_ns")
        compressed = block.get("compression_complete_ns")
        if validated is not None and compressed is not None and compressed >= validated:
            block["validation_to_compress_done_ms"] = (compressed - validated) // 1_000_000
        committed = block["node0_commit_ns"]
        if committed is None:
            unmatched += 1
            continue
        window = (committed - start_ns) // 15_000_000_000
        if 0 <= window < len(windows):
            windows[window] += block["successful_transactions"]
    stage_summary = {}
    for field in ("packing_ms", "builder_finish_ms", "payload_built_ms", "payload_to_engine_ms", "leader_broadcast_ms", "leader_validation_ms", "compression_ms", "validation_to_compress_done_ms", "follower_import_ms"):
        values = []
        for block in blocks:
            value = block[field]
            if isinstance(value, dict):
                values.extend(value.values())
            elif value is not None:
                values.append(value)
        stage_summary[field] = {
            "count": len(values),
            "missing_count": (6 * len(blocks) if field == "follower_import_ms" else len(blocks)) - len(values),
            "median_ms": statistics.median(values) if values else None,
            "p90_ms": sorted(values)[int(0.9 * (len(values) - 1))] if values else None,
        }
    return {
        "tag": tag,
        "ingest_start_ns": start_ns,
        "audited_transaction_blocks": len(blocks),
        "unmatched_audited_transaction_blocks": unmatched,
        "successful_commit_windows_15s": windows,
        "stages": stage_summary,
        "blocks": blocks,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", type=Path)
    parser.add_argument("tag")
    parser.add_argument("--score", action="store_true", help="use the warmed scored audit and boundary")
    args = parser.parse_args(argv)
    result = build_timeline(
        args.campaign, args.tag,
        audit_name="h2-score-audit.json" if args.score else "h2-audit.json",
        start_marker="score-start.ns" if args.score else "ingest-start.ns",
    )
    output = args.campaign / f"result-{args.tag}"
    stem = "timeline-score" if args.score else "timeline"
    (output / f"{stem}.json").write_text(json.dumps(result, indent=2) + "\n")
    rows = ["stage\tcount\tmedian_ms\tp90_ms"]
    for name, values in result["stages"].items():
        rows.append(f"{name}\t{values['count']}\t{values['median_ms']}\t{values['p90_ms']}")
    (output / f"{stem}.tsv").write_text("\n".join(rows) + "\n")
    print(json.dumps({key: result[key] for key in ("tag", "unmatched_audited_transaction_blocks", "successful_commit_windows_15s", "stages")}))
    return 0 if result["unmatched_audited_transaction_blocks"] == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
