import hashlib, itertools, json, os, re, statistics, subprocess
from pathlib import Path
import argparse
parser=argparse.ArgumentParser(description='Compare the archived baseline and candidate in fresh processes.')
parser.add_argument('--binary',type=Path,required=True,help='Release test executable from cargo test --no-run')
parser.add_argument('--output',type=Path,required=True)
args=parser.parse_args()
out=args.output
out.mkdir(parents=True,exist_ok=True)
binary=args.binary.resolve()
(out/'binary-sha256.txt').write_text(hashlib.sha256(binary.read_bytes()).hexdigest()+'  '+str(binary)+'\n')
versions=['baseline','qmdb_read_view']
fields=['initialize_ms','derive_ms','process_max_rss_kib','reads_per_second_1','reads_per_second_16']
rows=[]
for keys in [200000,2000000]:
    for kind in ['account','storage']:
        for pair, order in enumerate(versions if pair % 2 == 0 else versions[::-1] for pair in range(6)):
            for version in order:
                env=dict(os.environ,N42_READ_BENCH_KEYS=str(keys),N42_READ_BENCH_KIND=kind)
                command=['/usr/bin/time','-f','process_max_rss_kib=%M',str(binary),version+'::tests::bench_immutable_read_views','--ignored','--nocapture','--test-threads=1','--exact']
                run=subprocess.run(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
                (out/f'{keys}-{kind}-{pair}-{version}.log').write_text(run.stdout)
                if run.returncode:
                    raise RuntimeError(run.stdout)
                row={'keys':keys,'kind':kind,'pair':pair,'version':version}
                for field in fields[:3]:
                    row[field]=float(re.search(field+r'=([0-9.]+)',run.stdout).group(1))
                for threads in [1,16]:
                    row[f'reads_per_second_{threads}']=float(re.search(r'threads='+str(threads)+r' reads=1000000 elapsed_ms=[0-9.]+ reads_per_second=([0-9.]+)',run.stdout).group(1))
                rows.append(row)
                print(json.dumps(row),flush=True)
                (out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
summary=[]
for keys in [200000,2000000]:
    for kind in ['account','storage']:
        for version in versions:
            values=[r for r in rows if r['keys']==keys and r['kind']==kind and r['version']==version]
            summary.append(dict(keys=keys,kind=kind,version=version,**{f:{'median':statistics.median(r[f] for r in values),'min':min(r[f] for r in values),'max':max(r[f] for r in values)} for f in fields}))
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary),flush=True)
