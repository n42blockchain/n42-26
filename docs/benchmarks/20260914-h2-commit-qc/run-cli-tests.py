"""Exercise the real commit verifier executable using synthetic signed fixtures."""
import argparse, copy, hashlib, json, subprocess
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--fixtures',type=Path,required=True)
p.add_argument('--out',type=Path,required=True)
a=p.parse_args(); binary=a.binary.resolve(); a.out.mkdir(parents=True,exist_ok=True)
cases=[]
for profile in ('native','h2v4','gov5legacy'):
 for phase in ('commit','prepare'):
  request=json.loads((a.fixtures/f'{profile}-{phase}.json').read_text())
  cases.append((f'{profile}-{phase}',request,phase=='commit'))
base=json.loads((a.fixtures/'h2v4-commit.json').read_text())
for field,value in [('chainId',95),('genesisHash','0x'+'00'*32),('expectedView',8),
                    ('expectedBlockHash','0x'+'00'*32),('faultTolerance',2),('faultTolerance',0),
                    ('profile','native'),('validatorChangesHash','0x'+'11'*32)]:
 request=copy.deepcopy(base);request[field]=value;cases.append((f'wrong-{field}-{value}',request,False))
for field,value in [('view',8),('blockHash','0x'+'33'*32),('signature','0x'+'00'*96),
                    ('signers',[True,True,False,False]),('signers',[1,True,True,False]),
                    ('signers',[True,True,True]),('signers',[True]*5)]:
 request=copy.deepcopy(base);request['qc'][field]=value;cases.append((f'wrong-qc-{field}-{len(cases)}',request,False))
request=copy.deepcopy(base);request['validators'][3]=request['validators'][0];cases.append(('duplicate-key',request,False))
request=copy.deepcopy(base);request['validators'][3]='00'*48;cases.append(('invalid-key',request,False))
request=copy.deepcopy(base);request['validators'][0],request['validators'][3]=request['validators'][3],request['validators'][0];cases.append(('wrong-roster-order',request,False))
request=copy.deepcopy(base);request['qc']['view']=request['expectedView']=0;cases.append(('genesis-sentinel',request,False))
results=[]
for name,request,valid in cases:
 run=subprocess.run([str(binary)],input=json.dumps(request),text=True,capture_output=True,timeout=10)
 report=json.loads(run.stdout)
 assert run.returncode==(0 if valid else 1),(name,run.returncode,run.stderr)
 assert report['verified'] is valid,(name,report)
 if valid:
  assert report['signerCount']==3 and report['validatorCount']==4
  assert report['chainBoundSignature']==(request['profile']=='h2v4')
 results.append(dict(case=name,expected_valid=valid,exit_code=run.returncode,request=request,report=report))
for name,payload in [('malformed-json',b'{'),('oversized-json',b' '*(1048576+1))]:
 run=subprocess.run([str(binary)],input=payload,capture_output=True,timeout=10)
 report=json.loads(run.stdout)
 assert run.returncode==1 and report['verified'] is False
 results.append(dict(case=name,expected_valid=False,exit_code=run.returncode,report=report))
(a.out/'cli-results.json').write_text(json.dumps(dict(fixture_only=True,binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),cases=results),indent=2)+'\n')
print(f'{len(results)} real CLI cases passed; fixtures are synthetic, not fleet evidence.')
