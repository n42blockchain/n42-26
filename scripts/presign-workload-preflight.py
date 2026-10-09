#!/usr/bin/env python3
"""Check workload identity and capacity before a timed H2 qualification.

This checks N42T framing metadata, not transaction signatures or execution.
The stress loader checks the complete framing before releasing its start gate.
"""
import argparse
import decimal
import hashlib
import json
import math
from pathlib import Path
import struct

HEADER = struct.Struct("<4sBQIQ")


def inspect(path, chain_id, duration, minimum_tps):
    rate = decimal.Decimal(str(minimum_tps))
    if duration <= 0 or not rate.is_finite() or rate <= 0:
        raise ValueError("duration and minimum TPS must be positive and finite")
    required = math.ceil(rate * duration)
    with open(path, "rb") as source:
        before = source_stat(source)
        header = source.read(HEADER.size)
        if len(header) != HEADER.size:
            raise ValueError("truncated N42T header")
        magic, version, file_chain, groups, count = HEADER.unpack(header)
        if magic != b"N42T" or version != 2:
            raise ValueError("binary ingest requires N42T version 2")
        if file_chain != chain_id:
            raise ValueError(f"presigned chain {file_chain} != trusted chain {chain_id}")
        if groups == 0 or HEADER.size + groups * 8 + count * 23 > before[2]:
            raise ValueError("impossible N42T group/transaction counts for file size")
        if count < required:
            raise ValueError(f"workload has {count} transactions; at least {required} required for {duration}s at {rate} successful TPS")
        source.seek(0)
        digest = hashlib.file_digest(source, "sha256").hexdigest()
        if source_stat(source) != before or path_stat(path) != before:
            raise ValueError("presigned file changed during preflight")
    return dict(schema=1, path=str(Path(path).resolve()), sha256=digest,
                bytes=before[2], version=version, chainId=file_chain, rpcGroups=groups,
                transactions=count, minimumTransactions=required,
                scope="file identity and header capacity; signatures, full framing, running-chain nonces and execution not verified")


def identity(stat):
    return stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns


def source_stat(source):
    import os
    return identity(os.fstat(source.fileno()))


def path_stat(path):
    return identity(Path(path).stat())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", required=True)
    parser.add_argument("--trusted-config", required=True)
    parser.add_argument("--chain-id", type=int, required=True)
    parser.add_argument("--duration", type=int, required=True)
    parser.add_argument("--minimum-tps", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    trusted = json.loads(Path(args.trusted_config).read_text())
    if trusted["chainId"] != args.chain_id:
        raise ValueError("N42_CHAIN_ID does not match the authenticated trusted configuration")
    result = inspect(args.file, args.chain_id, args.duration, args.minimum_tps)
    Path(args.out).write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
