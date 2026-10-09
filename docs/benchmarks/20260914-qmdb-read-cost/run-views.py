"""Alternate derived-read-index implementations; measure updates and account reads."""
import argparse, hashlib, json, os, re, statistics, subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
p.add_argument('--keys',type=int,nargs='+',default=[200000,2000000])
p.add_argument('--kind', choices=['account','storage'], default='account')
p.add_argument('--updates', type=int, default=147000)
p.add_argument('--pairs',type=int,default=6)
p.add_argument('--variants',nargs='+',default=['qmdb','fold','ordered','ordered256'])
a=p.parse_args();a.out.mkdir(parents=True,exist_ok=True);binary=a.binary.resolve();rows=[]
assert a.updates>0 and a.pairs>0 and all(n>0 for n in a.keys)
assert a.updates == 147000 or set(a.variants) <= {"qmdb", "hash4096"}
hash_before=hashlib.sha256(binary.read_bytes()).hexdigest()
(a.out/'run.json').write_text(json.dumps(dict(binary_sha256=hash_before,keys=a.keys,pairs=a.pairs,variants=a.variants,kind=a.kind,updates=a.updates,sorted_operations=True),indent=2)+'\n')
for keys in a.keys:
 for pair in range(a.pairs):
  versions=a.variants[pair%len(a.variants):]+a.variants[:pair%len(a.variants)]
  for version in versions:
   module={'qmdb':'qmdb_read_view','fold':'fold_read_view','ordered':'ordered_read_view','ordered256':'ordered256_read_view','paths':'paths_read_view','foldpaths':'foldpaths_read_view','hash256':'hash256_read_view','hash1024':'hash1024_read_view','hash4096':'hash4096_read_view'}[version]
   env=dict(os.environ,N42_READ_BENCH_KEYS=str(keys),N42_READ_BENCH_KIND=a.kind,N42_READ_BENCH_UPDATES=str(a.updates))
   result=subprocess.run([str(binary),module+'::tests::bench_immutable_read_views','--ignored','--exact','--nocapture','--test-threads=1'],env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
   (a.out/f'{keys}-{pair}-{version}.log').write_text(result.stdout)
   assert result.returncode==0,result.stdout
   init=re.search(r'initialize_ms=([\d.]+)',result.stdout);derive=re.search(r'updates=(\d+) derive_ms=([\d.]+)',result.stdout)
   reads=re.findall(r'threads=(\d+) reads=(\d+) elapsed_ms=([\d.]+) reads_per_second=(\d+)',result.stdout)
   assert init and derive and len(reads)==2 and int(derive[1])==min(keys,a.updates),result.stdout
   row=dict(keys=keys,pair=pair,version=version,initialize_ms=float(init[1]),derive_ms=float(derive[2]),reads={threads:dict(count=int(count),elapsed_ms=float(elapsed),per_second=int(rate)) for threads,count,elapsed,rate in reads})
   rows.append(row);print(keys,pair,version,row['derive_ms'],row['reads'],flush=True)
   (a.out/'samples.json').write_text(json.dumps(rows,indent=2)+'\n')
assert hashlib.sha256(binary.read_bytes()).hexdigest()==hash_before
summary=[]
for keys in a.keys:
 for version in a.variants:
  selected=[r for r in rows if r['keys']==keys and r['version']==version]
  fields={field:[r[field] for r in selected] for field in ['initialize_ms','derive_ms']}
  fields.update({f'read_{n}_ms':[r['reads'][n]['elapsed_ms'] for r in selected] for n in ['1','16']})
  summary.append(dict(keys=keys,version=version,count=len(selected),metrics={name:dict(median=statistics.median(values),minimum=min(values),maximum=max(values)) for name,values in fields.items()}))
(a.out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary,indent=2),flush=True)
