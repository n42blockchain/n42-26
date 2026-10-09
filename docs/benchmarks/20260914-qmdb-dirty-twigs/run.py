"""Alternate fresh-process QMDB dirty-twig measurements."""
from pathlib import Path
import argparse, os, subprocess, re, json, statistics, hashlib

def positive(text):
 value=int(text)
 if value <= 0: raise argparse.ArgumentTypeError('must be positive')
 return value

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--output',type=Path,required=True)
p.add_argument('--pairs',type=positive,default=6)
p.add_argument('--blocks',type=positive,default=3)
p.add_argument('--keys',type=positive,nargs='+',default=[200000,2000000])
p.add_argument('--ordered',type=int,choices=[0,1],nargs='+',default=[0])
args=p.parse_args(); binary=args.binary.resolve(); out=args.output
out.mkdir(parents=True,exist_ok=True)
(out/'run.json').write_text(json.dumps(dict(binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),pairs=args.pairs,blocks=args.blocks,keys=args.keys,ordered=args.ordered),indent=2)+'\n')
rows=[]
for keys in args.keys:
 for ordered in args.ordered:
  for pair in range(args.pairs):
   for version in (('before','batch','after'), ('batch','after','before'), ('after','before','batch'))[pair % 3]:
    env=dict(os.environ,N42_UPDATE_KEYS=str(keys),N42_UPDATE_BLOCKS=str(args.blocks),N42_UPDATE_VERSION=version,N42_UPDATE_SORTED=str(ordered))
    run=subprocess.run([str(binary)],env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
    (out/f'{keys}-{ordered}-{pair}-{version}.log').write_text(run.stdout)
    if run.returncode: raise RuntimeError(run.stdout)
    matches=re.findall(r'dirty_twigs version=(\w+) keys=(\d+) generation=(\d+) updates=(\d+) elapsed_ms=([\d.]+) root=([0-9a-f]{64}) undo=([0-9a-f]{64}) rollback_verified=true',run.stdout)
    if len(matches)!=args.blocks: raise RuntimeError(run.stdout)
    values=[]
    for ver,k,g,u,t,r,d in matches:
     assert ver==version and int(k)==keys and int(u)==min(keys,147000)
     rows.append(dict(version=ver,keys=int(k),ordered=ordered,pair=pair,generation=int(g),updates=int(u),elapsed_ms=float(t),root=r,undo_digest=d))
     values.append(float(t))
    print(keys,ordered,pair,version,values,flush=True)
    (out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
for keys in args.keys:
 for generation in range(2,args.blocks+2):
  assert len({(r['root'],r['undo_digest']) for r in rows if r['keys']==keys and r['generation']==generation})==1
summary=[]
for keys in args.keys:
 for ordered in args.ordered:
  for version in ('before','batch','after'):
   values=[r['elapsed_ms'] for r in rows if r['keys']==keys and r['ordered']==ordered and r['version']==version]
   summary.append(dict(keys=keys,ordered=ordered,version=version,count=len(values),median_ms=statistics.median(values),min_ms=min(values),max_ms=max(values)))
(out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
print(json.dumps(summary,indent=2),flush=True)
