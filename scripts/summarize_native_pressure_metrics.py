#!/usr/bin/env python3
"""Summarize successful pressure scrapes; missing metrics are never zero-filled."""

import argparse
import json
from pathlib import Path


def summarize(path, start_ns=None, end_ns=None):
    if start_ns is not None and end_ns is not None and end_ns <= start_ns:
        raise ValueError("metric window must have positive duration")
    series = {}
    errors = []
    snapshots = 0
    with path.open() as source:
        rows = (json.loads(line) for line in source)
        for row in rows:
            selected = [node for node in row["nodes"]
                        if (start_ns is None or node["time_ns"] >= start_ns)
                        and (end_ns is None or node["time_ns"] <= end_ns)]
            if not selected:
                continue
            snapshots += 1
            for node in selected:
                if "error" in node:
                    errors.append({"port": node["port"], "time_ns": node["time_ns"], "error": node["error"]})
                    continue
                samples = series.setdefault(str(node["port"]), {})
                for sample, value in node["metrics"].items():
                    samples.setdefault(sample, []).append((node["time_ns"], value))
    nodes = {}
    for port, samples in series.items():
        metrics = {}
        means = {}
        for sample, values in samples.items():
            first_ns, first = values[0]
            last_ns, last = values[-1]
            name, _, labels = sample.partition("{")
            result = {"samples": len(values), "first": first, "last": last,
                      "min": min(value for _, value in values), "max": max(value for _, value in values),
                      "seconds": (last_ns - first_ns) / 1e9}
            if name.endswith(("_total", "_sum", "_count", "_bucket")):
                reset = any(after < before for (_, before), (_, after) in zip(values, values[1:]))
                result["counter_reset"] = reset
                result["delta"] = None if reset else last - first
            metrics[sample] = result
            if name.endswith("_sum"):
                count_sample = name[:-4] + "_count" + ("{" + labels if labels else "")
                counts = samples.get(count_sample)
                # A mean requires aligned samples and monotone sum/count.
                if counts and [at for at, _ in counts] == [at for at, _ in values]:
                    count_reset = any(b < a for (_, a), (_, b) in zip(counts, counts[1:]))
                    delta_count = counts[-1][1] - counts[0][1]
                    if not result["counter_reset"] and not count_reset and delta_count > 0:
                        key = name[:-4] + ("{" + labels if labels else "")
                        means[key] = {"observations": delta_count,
                                      "mean": (last - first) / delta_count}
        nodes[port] = {"metrics": metrics, "histogram_means": means}
    return {"snapshots": snapshots, "scrape_errors": errors, "nodes": nodes,
            "window": {"start_ns": start_ns, "end_ns": end_ns},
            "scope": "per-port successful scrape ranges; histogram mean units follow source metric; quantiles are not reconstructed"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--start-marker", type=Path, help="Unix-nanosecond ingest start marker")
    parser.add_argument("--end-marker", type=Path, help="Unix-nanosecond ingest end marker")
    args = parser.parse_args()
    start = int(args.start_marker.read_text().strip()) if args.start_marker else None
    end = int(args.end_marker.read_text().strip()) if args.end_marker else None
    args.out.write_text(json.dumps(summarize(args.input, start, end), indent=2) + "\n")


if __name__ == "__main__":
    main()
