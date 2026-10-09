"""Measure consecutive canonical EOA execution and durable QMDB commits."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--tmpdir', type=Path, required=True)
p.add_argument('--accounts', type=int, nargs='+', default=[200000, 2000000])
p.add_argument('--transactions', type=int, default=50000)
p.add_argument('--blocks', type=int, default=3)
p.add_argument('--runs', type=int, default=3)
a = p.parse_args()
assert a.transactions > 0 and 0 < a.blocks < 1000 and a.runs > 0
assert all(n >= 2*a.transactions+1 for n in a.accounts)
a.out.mkdir(parents=True, exist_ok=True)
a.tmpdir.mkdir(parents=True, exist_ok=True)
binary = a.binary.resolve()
binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
metadata = dict(binary_sha256=binary_hash, accounts=a.accounts, transactions=a.transactions,
                blocks=a.blocks, runs=a.runs, tmpdir=str(a.tmpdir.resolve()),
                scope='canonical_execution_delta_qmdb_durable_commit', status='running')
(a.out/'run.json').write_text(json.dumps(metadata, indent=2)+'\n')
rows = []
for run in range(a.runs):
    sizes = a.accounts if run % 2 == 0 else list(reversed(a.accounts))
    for accounts in sizes:
        env = dict(os.environ, TMPDIR=str(a.tmpdir.resolve()), N42_QMDB_REBASE_BLOCKS='20000',
                   N42_CANON_BENCH_ACCOUNTS=str(accounts), N42_CANON_BENCH_TXS=str(a.transactions),
                   N42_CANON_BENCH_BLOCKS=str(a.blocks))
        result = subprocess.run([str(binary), 'qmdb_state_reader::canonical_bench::bench_canonical_qmdb_execution_and_commit',
                                '--ignored', '--exact', '--nocapture', '--test-threads=1'], env=env,
                               text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        (a.out/f'{accounts}-{run}.log').write_text(result.stdout)
        assert result.returncode == 0 and 'canonical_qmdb_restart_verified=true' in result.stdout, result.stdout
        samples = [json.loads(line.split('canonical_qmdb_sample ',1)[1]) for line in result.stdout.splitlines()
                   if 'canonical_qmdb_sample ' in line]
        configs = [json.loads(line.split('canonical_qmdb_config ',1)[1]) for line in result.stdout.splitlines()
                   if 'canonical_qmdb_config ' in line]
        assert len(samples) == a.blocks and len(configs) == 1
        config = configs[0]
        assert config['accounts'] == accounts and config['transactions'] == a.transactions
        assert config['reads'] == 'only' and config['strategy'] == 'canonical_sequential'
        assert config['disjointAddresses'] and config['distinctSenders'] == config['distinctRecipients'] == a.transactions
        for number, sample in enumerate(samples, 1):
            assert sample['number'] == number and sample['transactions'] == sample['successful'] == a.transactions
            assert sample['operations'] == 2*a.transactions+1 and sample['accountReads'] >= 2*a.transactions+1
            assert sample['totalMs'] > 0 and sample['walBytes'] > 0
            sample.update(accounts=accounts, run=run)
            rows.append(sample)
        print(accounts, run, [(s['executionMs'], s['commitMs'], s['totalMs']) for s in samples], flush=True)
        (a.out/'samples.json').write_text(json.dumps(rows, indent=2)+'\n')
assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary_hash
summary = []
for accounts in a.accounts:
    selected = [r for r in rows if r['accounts'] == accounts]
    for number in range(1, a.blocks+1):
        assert len({(r['root'],r['blockHash'],r['walBytes']) for r in selected if r['number']==number}) == 1
    metrics = {}
    for name in ['executionMs','deltaMs','commitMs','totalMs','recoveryMsExcluded']:
        values = [r[name] for r in selected]
        metrics[name] = dict(median=statistics.median(values), minimum=min(values), maximum=max(values))
    summary.append(dict(accounts=accounts, samples=len(selected), metrics=metrics,
                        local_successful_execution_and_commit_per_second=sum(r['successful'] for r in selected)*1000/sum(r['totalMs'] for r in selected)))
(a.out/'summary.json').write_text(json.dumps(summary, indent=2)+'\n')
metadata['status'] = 'complete'
(a.out/'run.json').write_text(json.dumps(metadata, indent=2)+'\n')
print(json.dumps(summary, indent=2), flush=True)
