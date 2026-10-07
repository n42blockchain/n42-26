#!/usr/bin/env python3
"""Sample local fleet metrics with timestamps and explicit scrape failures."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
from pathlib import Path
import signal
import threading
import time
from urllib.request import urlopen


PREFIXES = (
    "n42_", "reth_n42_", "reth_engine_", "reth_payload_", "reth_transaction_pool_",
)


def scrape(port):
    started = time.monotonic()
    row = {"port": port, "time_ns": time.time_ns(), "metrics": {}}
    try:
        with urlopen(f"http://127.0.0.1:{port}/", timeout=2) as response:
            body = response.read(8 * 1024 * 1024 + 1)
        if len(body) > 8 * 1024 * 1024:
            raise ValueError("metrics response exceeds 8 MiB")
        for line in body.decode().splitlines():
            if not line.startswith(PREFIXES):
                continue
            # Labels may contain spaces. Prometheus' value follows the closing
            # label brace (or the metric name when there are no labels).
            at = line.rfind("}") + 1 if "{" in line else line.find(" ")
            if at <= 0:
                continue
            sample, value = line[:at].strip(), line[at:].strip().split()[0]
            number = float(value)
            if math.isfinite(number):
                row["metrics"][sample] = number
    except Exception as error:
        row["error"] = str(error)
        row["metrics"] = {}
    row["scrape_ms"] = (time.monotonic() - started) * 1000
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metrics-base", type=int, default=23600)
    parser.add_argument("--nodes", type=int, choices=(4, 7), default=7)
    parser.add_argument("--interval", type=float, default=2)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if not math.isfinite(args.interval) or args.interval < 1:
        parser.error("interval must be finite and at least one second")
    if not 1 <= args.metrics_base <= 65536 - args.nodes:
        parser.error("metrics ports must fit TCP port range")
    stop = threading.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda *_: stop.set())
    args.out.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation protects the provenance of a previous run.
    with args.out.open("x") as output, ThreadPoolExecutor(max_workers=args.nodes) as pool:
        while not stop.is_set():
            started = time.monotonic()
            rows = list(pool.map(scrape, range(args.metrics_base, args.metrics_base + args.nodes)))
            output.write(json.dumps({"time_ns": time.time_ns(), "nodes": rows}) + "\n")
            output.flush()
            stop.wait(max(0, args.interval - (time.monotonic() - started)))


if __name__ == "__main__":
    main()
