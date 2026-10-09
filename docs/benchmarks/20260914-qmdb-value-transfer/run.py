from pathlib import Path
import os, subprocess, re, json, statistics, hashlib
p=Path(__file__).resolve().parent
binary=p/'target/release/value-transfer'
rows=[]
for keys in (200_000, 2_000_000):
 for pair in range(6):
  for version in (('before','after') if pair%2==0 else ('after','before')):
   env=dict(os.environ,N42_UPDATE_KEYS=str(keys),N42_UPDATE_BLOCKS='3',N42_UPDATE_VERSION=version)
   run=subprocess.run([str(binary)],env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
   (p/f'{keys}-{pair}-{version}.log').write_text(run.stdout)
   assert run.returncode==0,run.stdout
   matches=re.findall(r'value_transfer version=(\w+) keys=(\d+) generation=(\d+) updates=(\d+) elapsed_ms=([\d.]+) root=([0-9a-f]{64}) undo=([0-9a-f]{64}) rollback_verified=true',run.stdout)
   assert len(matches)==3,run.stdout
   values=[]
   for ver,k,g,u,t,r,d in matches:
    rows.append(dict(version=ver,keys=int(k),pair=pair,generation=int(g),updates=int(u),elapsed_ms=float(t),root=r,undo_digest=d))
    values.append(float(t))
   print(keys,pair,version,values,flush=True)
   (p/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
for keys in (200_000,2_000_000):
 for generation in range(2,5):
  assert len({(r['root'],r['undo_digest']) for r in rows if r['keys']==keys and r['generation']==generation})==1
summary=[]
for keys in (200_000,2_000_000):
 for version in ('before','after'):
  values=[r['elapsed_ms'] for r in rows if r['keys']==keys and r['version']==version]
  summary.append(dict(keys=keys,version=version,count=len(values),median_ms=statistics.median(values),min_ms=min(values),max_ms=max(values)))
(p/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
(p/'binary.json').write_text(json.dumps({'sha256':hashlib.sha256(binary.read_bytes()).hexdigest()},indent=2)+'\n')
print(json.dumps(summary,indent=2),flush=True)
