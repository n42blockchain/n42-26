#!/usr/bin/env python3
"""Summarize a frozen executable's A/B/A timing rows; never report chain TPS."""
import argparse
import json
import math
import statistics
from pathlib import Path


def summarize(lines, prefix):
    groups = {tag: [] for tag in ('warmup', 'a1', 'b', 'a2')}
    counts = set()
    seen = set()
    gas = set()
    for line in lines:
        if not line.startswith(prefix + ' '):
            continue
        row = json.loads(line[len(prefix) + 1:])
        tag = row['tag']
        if tag not in groups:
            raise ValueError('unknown leg')
        key = (tag, row['iteration'])
        if key in seen:
            raise ValueError('duplicate sample')
        if not isinstance(row['iteration'], int) or not 0 <= row['iteration'] < 10:
            raise ValueError('invalid iteration')
        seen.add(key)
        count, duration = row['transactions'], row['duration_ns']
        if not isinstance(count, int) or count <= 0:
            raise ValueError('invalid transaction count')
        if not isinstance(duration, int) or duration <= 0:
            raise ValueError('invalid duration')
        counts.add(count)
        if prefix == 'TRANSFER_PROBE':
            if row['gas_used'] != count * 21_000:
                raise ValueError('unexpected transfer gas')
            gas.add(row['gas_used'])
        groups[tag].append(duration / 1_000_000)
    if len(counts) != 1 or any(len(values) != 10 for values in groups.values()):
        raise ValueError('need four complete legs of ten samples with identical counts')
    result = {
        'scope': ('native frame admission microbenchmark; excludes execution, QMDB, consensus and persistence; trusted gateway policy differs between A and B'
                  if prefix == 'NATIVE_FRAME_PROBE' else 'execution microbenchmark; excludes canonical consensus and persistence'),
        'transactions_per_sample': counts.pop(),
        'warmup_excluded': True,
        'legs': {tag: {'samples': len(values), 'median_ms': statistics.median(values),
                        'mean_ms': statistics.mean(values), 'p90_ms': sorted(values)[math.ceil(.9 * len(values)) - 1]}
                 for tag, values in groups.items()},
    }
    a1, b, a2 = (result['legs'][tag]['median_ms'] for tag in ('a1', 'b', 'a2'))
    reference = (a1 + a2) / 2
    result['candidate_duration_change_pct'] = (b / reference - 1) * 100
    result['bookend_duration_drift_pct'] = (a2 / a1 - 1) * 100
    result['candidate_faster_than_both_bookends'] = b < min(a1, a2)
    result['capacity_conclusion'] = 'none; repeat trials and end-to-end qualification required'
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('log', type=Path)
    parser.add_argument('--prefix', choices=['PROBE', 'TRANSFER_PROBE', 'NATIVE_FRAME_PROBE'], required=True)
    args = parser.parse_args()
    print(json.dumps(summarize(args.log.read_text().splitlines(), args.prefix), indent=2))


if __name__ == '__main__':
    main()
