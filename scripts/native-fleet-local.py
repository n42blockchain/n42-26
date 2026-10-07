#!/usr/bin/env python3
"""Measure a frozen native fleet without touching any other running fleet.

Uses the reference's own argument builders, isolated fresh data, and exact child
process groups. Run E1 and E7 separately with the same --rate and --seconds.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import secrets
import signal
import shlex
import socket
import subprocess
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--layers', type=int, choices=[1, 7], required=True)
    parser.add_argument('--rate', type=int, default=100000)
    parser.add_argument('--seconds', type=int, default=30)
    parser.add_argument('--dataset', type=Path, help='Shared pregenerated dataset directory for both layouts')
    parser.add_argument('--plan-only', action='store_true', help='Generate isolated keys and launch provenance without starting a fleet')
    parser.add_argument('--workers', type=int, default=2)
    parser.add_argument('--interval-ms', type=int, default=100)
    parser.add_argument('--block-txs', type=int, default=163000)
    parser.add_argument('--per-sender-txs', type=int, default=50000)
    args = parser.parse_args()
    capacity=128*args.per_sender_txs
    if min(args.rate,args.seconds,args.workers,args.interval_ms,args.block_txs,args.per_sender_txs) <= 0 or args.rate*(args.seconds+3) > capacity:
        parser.error('Choose positive parameters and a dataset covering rate*(seconds+3)')
    ref, root = args.reference.resolve(), args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    driver=Path(__file__).read_bytes()
    (root/'driver.py').write_bytes(driver)
    bins = ref / 'target/release'
    dataset = (args.dataset or root.parent/('native-transfer-dataset' if args.per_sender_txs==50000 else f'native-transfer-dataset-{args.per_sender_txs}')).resolve()
    txargs=['--chain-id','1143','--senders','128','--pertx',str(args.per_sender_txs),'--offset','900000',
            '--recipients','200000','--gas','21000','--gasprice','100000000000000',
            '--conc','8','--rpcbatch','500','--alg','ed25519','--gateway-key','seed:n42-bench-gateway']
    for executable in [bins/'n42', bins/'examples/h2_validator', bins/'examples/tx_flood', bins/'examples/h2_keygen']:
        if not executable.is_file():
            raise RuntimeError(f'Missing built binary: {executable}')
    sources=[p for base in [ref/'bin/n42/src',ref/'crates/n42'] for p in base.rglob('*.rs') if '/tests/' not in str(p)]
    newest=max(p.stat().st_mtime for p in sources)
    if not args.plan_only and (bins/'n42').stat().st_mtime < newest:
        raise RuntimeError('Native node is older than the source; complete the isolated build before measuring')
    ports = [base+i for base in [28200, 28600, 29200, 29300, 29700, 29800, 30500, 30700] for i in range(7)]
    for port in ports:
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
            sock.bind(('127.0.0.1', port))
    genesis = json.loads((ref/'crates/chainspec/res/genesis/n42_fleet7_bench.json').read_text())
    genesis['gasLimit'] = hex(args.block_txs*21000)
    genesis['config']['hotstuff']['leaderTenure'] = 1024
    (root/'genesis.json').write_text(json.dumps(genesis, indent=2)+'\n')
    (root/'jwt.hex').write_text(secrets.token_hex(32))
    (root/'jwt.hex').chmod(0o600)
    env = {k:v for k,v in os.environ.items() if not k.startswith(('N42_', 'F7_', 'RETH_'))}
    # Import the measured profile as a unit. A partial set of pipeline switches
    # can route frame blocks through an incompatible ordinary-payload path.
    runner=(ref/'scripts/fleet7-runs/run-loop342.sh').read_text()
    reference_env={}
    for variable in ['C','R','D','A2']:
        match=re.search(r"^"+variable+r"='([^']*)'",runner,re.M)
        if not match: raise RuntimeError(f'Missing reference profile variable {variable}')
        for token in shlex.split(match.group(1)):
            if '=' in token:
                key,value=token.split('=',1); reference_env[key]=value
    measured=next(line for line in runner.splitlines() if line.startswith('legf P '))
    for token in shlex.split(measured):
        if '=' in token:
            key,value=token.split('=',1)
            if key.startswith('N42_'): reference_env[key]=value
    env.update(reference_env)
    env.update({
        'F7_ROOT':str(root), 'F7_GENESIS':str(root/'genesis.json'), 'F7_BIN':str(bins),
        'F7_EL_MAP':','.join(str(i if args.layers == 7 else 0) for i in range(7)),
        'F7_PROFILE':'bench', 'F7_PIN':'0', 'F7_CORES_PER_NODE':str(args.workers), 'F7_WORKERS':str(args.workers),
        'F7_HTTP_BASE':'28200', 'F7_AUTH_BASE':'28600', 'F7_INGEST_BASE':'29200',
        'F7_PAYLOAD_BASE':'29300', 'F7_P2P_BASE':'29700', 'F7_MOBILE_BASE':'29800',
        'F7_DEVP2P_BASE':'30500', 'F7_NO_TX_GOSSIP':'1', 'F7_INGEST':'1',
        'F7_DIRECT_PUSH':'1', 'F7_MOBILE':'0', 'F7_BLOCK_INTERVAL_MS':str(args.interval_ms),
        'F7_VIEW_TIMEOUT_MS':'2000', 'F7_BENCH_GASCEIL':str(args.block_txs*21000),
        'F7_EL_EXTRA':f'--builder.interval {args.interval_ms} --builder.deadline 3 --prune.transaction-lookup.full --rpc.max-response-size 512',
        'F7_BENCH_POOL_SLOTS':str(max(500000,args.block_txs*3)), 'F7_BENCH_POOL_MB':'1024',
        'N42_IMPORT_ONCE':'1', 'N42_FRAME_BLOCKS':'1', 'N42_BUILD_CHAIN':'1',
        'N42_MAX_GOSSIP_MB':'64',
        'N42_TX_QUEUE':'1', 'N42_TX_INGEST_ASYNC':'1', 'N42_TX_INGEST_DIRECT':'1',
        'N42_TX_INGEST_RECOVER_PARALLEL':str(args.workers), 'N42_PARALLEL_BUILD':'1',
        'N42_TX_INGEST_HIGH_WATER':str(max(500000,args.block_txs*3)*5//6),
        'N42_FOLLOWER_PARALLEL':'1', 'N42_FOLLOWER_GRAFT':'1', 'N42_BUILD_ON_SEAL':'1',
        'N42_QMDB_READS':'on', 'N42_QMDB_ENTRY_FILE':'1', 'N42_QMDB_RETAIN_DEPTH':'16',
        'N42_HASHED_TABLES':'off', 'N42_ACCOUNT_HISTORY':'off',
        'N42_SF_PARALLEL_ENCODE':'1', 'N42_SF_EARLY_WRITEBACK':'1', 'N42_PERSIST_QMDB_IN_SCOPE':'1',
        'N42_PLAN_AHEAD':'1', 'N42_PULL_BY_FRAMES':'1', 'N42_ANSWER_LAYOUT_ONLY':'1',
        'N42_BUILDER_PULLER':'1024', 'N42_ED25519_BATCH':'256',
        'N42_ROAD_RUNTIME':'1', 'N42_FIELDS_AT_SEAL':'1', 'N42_BUILD_COLLECT_IN_PLACE':'1',
        'N42_FOLLOWER_FIELDS_EARLY':'1', 'N42_FOLLOWER_EXEC_EARLY':'1',
        'N42_FOLLOWER_BUILD_PATH':'1', 'N42_FOLLOWER_ROOT_ON_BUILD_POOL':'1',
        'N42_FOLLOWER_FRAME_SCAN':'1', 'N42_BUILD_START_ASYNC':'1',
        'F7_LEADER_TENURE':'1024', 'TOKIO_WORKER_THREADS':'2',
        'N42_BODY_ONCE':'1', 'N42_BUILD_ON_OUTPUT':'1', 'N42_SEAL_AT_EXEC':'1',
        'N42_STATE_AFTER_PULL':'1', 'N42_ENGINE_TAKE_SEALED':'1',
        'N42_TENURE_FIRST_ON_OUTPUT':'1', 'N42_COMMIT_FCU_ASYNC':'1',
        'N42_TAKE_COMPACT':'1', 'N42_COMPACT_BODY':'1', 'N42_OUTPUT_SHARDS':'8',
        'N42_BLOCK_BY_DESCRIPTION':'1', 'N42_FOLLOWER_COPY_ASIDE':'1',
        'N42_OUTPUT_INDEX':'1', 'N42_OUTPUT_INDEX_LIVE':'1', 'N42_LEADER_LAYERS':'3',
        'N42_PARALLEL_BUILD_THREADS':str(args.workers), 'N42_FRAME_ATTEST_MIN':'1',
        'N42_FAST_TRANSFER':'1', 'N42_FOLLOWER_DIRECT_IMPORT':'1',
        'N42_TX_QUEUE_BATCH':'1024', 'N42_TX_QUEUE_DRAINER':'1', 'N42_TX_QUEUE_RUN':'64',
        'N42_FRAME_GATEWAYS':'0x87f7d242c742c0990ded1018a88b1296dbd89adbab9dff0bda008678beb2aa12',
        'RAYON_NUM_THREADS':str(args.workers), 'RUST_LOG':'info',
        'MALLOC_CONF':'narenas:2,dirty_decay_ms:2000,muzzy_decay_ms:0,background_thread:true,oversize_threshold:0',
        'RETH_ENGINE_NUM_STATE_MASKING_BLOCKS':'0',
    })
    # Generate outside the measured window; replay avoids measuring signing throughput.
    expected={'transactions':capacity,'arguments':txargs}
    if dataset.exists():
        if not (dataset/'local-complete.json').is_file() or json.loads((dataset/'local-complete.json').read_text()) != expected:
            raise RuntimeError(f'Incomplete or differently configured dataset: {dataset}')
    else:
        dataset.mkdir(parents=True)
        with (root/'pregen.log').open('wb') as output:
            subprocess.run([str(bins/'examples/tx_flood'),*txargs,'--pregen-out',str(dataset),'--pregen-txs',str(capacity)],env=env,stdout=output,stderr=subprocess.STDOUT,check=True)
        (dataset/'local-complete.json').write_text(json.dumps(expected,indent=2))
    helper = 'source "$1/scripts/fleet7-env.sh"\n'
    def bash(code, *values):
        return subprocess.check_output(['/bin/bash','-c', helper+code, 'fleet-helper', str(ref), *map(str,values)], env=env)
    bash('for ((i=0;i<7;i++)); do f7_place_keys "$i"; done\nf7_record_genesis')
    commands=[]
    for i in range(args.layers):
        argv=bash('f7_el_args "$2"\nprintf "%s\\0" "${F7_EL_ARGS[@]}"',i).decode().rstrip('\0').split('\0')
        commands.append(('el',i,[str(bins/'n42'),*argv]))
    for i in range(7):
        argv=bash('f7_load_peerids\nf7_validator_args "$2"\nprintf "%s\\0" "${F7_V_ARGS[@]}"',i).decode().rstrip('\0').split('\0')
        commands.append(('validator',i,[str(bins/'examples/h2_validator'),*argv,'--worker-threads','2']))
    record={'reference':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ref,text=True).strip(),
            'layers':args.layers,'validators':7,'rate':args.rate,'seconds':args.seconds,
            'environment':{k:v for k,v in env.items() if k.startswith(('N42_','F7_','RETH_','RAYON_','TOKIO_','MALLOC_'))},
            'commands':commands,
            'reference_profile':'scripts/fleet7-runs/run-loop342.sh: legf P with C/R/D/A2',
            'driver_sha256':hashlib.sha256(driver).hexdigest(),
            'dataset':str(dataset), 'dataset_transactions':capacity,
            'macos_unavailable_diagnostics':['thread page faults','TCP kernel receive timestamps','Linux signal stack dumps'],
            'binary_sha256':{str(p.relative_to(bins)):hashlib.sha256(p.read_bytes()).hexdigest() for p in [bins/'n42',bins/'examples/h2_validator',bins/'examples/tx_flood']}}
    (root/'provenance.json').write_text(json.dumps(record,indent=2))
    if args.plan_only:
        print(f'Prepared {args.layers}-layer launch plan at {root}/provenance.json')
        return
    children=[]
    handles=[]
    def launch(name, argv, childenv):
        log=(root/f'{name}.log').open('wb'); handles.append(log)
        process=subprocess.Popen(argv,env=childenv,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        children.append((name,process)); return process
    def rpc(i, method, params, timeout=5):
        request=urllib.request.Request(f'http://127.0.0.1:{28200+i}',data=json.dumps({'jsonrpc':'2.0','id':1,'method':method,'params':params}).encode(),headers={'Content-Type':'application/json'})
        result=json.loads(urllib.request.urlopen(request,timeout=timeout).read())
        if 'error' in result: raise RuntimeError(result['error'])
        return result['result']
    def checkpoint():
        heads=[int(rpc(i,'eth_blockNumber',[]),16) for i in range(args.layers)]
        height=min(heads)
        common_qc=None
        logs=[root/f'validator-{i}.log' for i in range(7)]
        if all(p.is_file() for p in logs):
            votes=[dict((block,int(view)) for view,block in re.findall(r'COMMIT view=(\d+) block=(0x[0-9a-fA-F]+)',p.read_text(errors='replace'))) for p in logs]
            common=set.intersection(*(set(v) for v in votes))
            if not common: raise RuntimeError('No common validator commit yet')
            committed=None
            for candidate in sorted(common,key=lambda block:votes[0][block],reverse=True):
                block=rpc(0,'eth_getBlockByHash',[candidate,False])
                if block is not None:
                    common_qc=candidate; committed=block; break
            if committed is None: raise RuntimeError('No common validator commit is readable yet')
            height=min(height,int(committed['number'],16))
        blocks=[rpc(i,'eth_getBlockByNumber',[hex(height),False]) for i in range(args.layers)]
        agree=all(len({b[field] for b in blocks})==1 for field in ['hash','stateRoot','receiptsRoot','transactionsRoot'])
        if not agree: raise RuntimeError(f'Canonical root mismatch at {height}')
        processes=[]
        for name,process in children:
            if process.poll() is not None: continue
            line=subprocess.check_output(['ps','-p',str(process.pid),'-o','time=,rss=,%cpu='],text=True).strip()
            if not line: continue
            clock,rss,cpu=line.split()
            seconds=0.0
            for value in clock.split(':'): seconds=seconds*60+float(value)
            processes.append({'name':name,'pid':process.pid,'cpu_seconds':seconds,'rss_kib':int(rss),'rolling_cpu_percent':float(cpu)})
        return {'at':time.time(),'height':height,'heads':heads,'blocks':blocks,'agreement':agree,'processes':processes,'common_validator_commit':common_qc}
    def checkpoint_light():
        votes=[]
        for i in range(7):
            text=(root/f'validator-{i}.log').read_text(errors='replace')
            votes.append(dict((block,int(view)) for view,block in re.findall(r'COMMIT view=(\d+) block=(0x[0-9a-fA-F]+)',text)))
        common=set.intersection(*(set(v) for v in votes))
        chains=[]
        for i in range(args.layers):
            text=(root/f'el-{i}.log').read_text(errors='replace')
            chain={}
            for number,block,txs in re.findall(r'Block added to canonical chain number=(\d+) hash=(0x[0-9a-fA-F]+)[^\n]*?\btxs=(\d+)',text):
                chain[int(number)]={'hash':block,'txs':int(txs)}
            chains.append(chain)
        by_hash={data['hash']:number for number,data in chains[0].items()}
        for candidate in sorted(common,key=lambda block:votes[0][block],reverse=True):
            height=by_hash.get(candidate)
            if height is not None and all(chain.get(height,{}).get('hash')==candidate for chain in chains):
                return {'at':time.time(),'height':height,'hash':candidate,'heads':[max(chain) for chain in chains],
                        'agreement':True,'canonical_counts':{str(n):data['txs'] for n,data in chains[0].items() if n<=height},
                        'common_validator_commit':candidate}
        raise RuntimeError('No common canonical commit in execution logs yet')
    try:
        for kind,i,argv in commands[:args.layers]:
            childenv=dict(env,N42_TX_INGEST=f'127.0.0.1:{29200+i}',N42_PAYLOAD_SERVE=f'127.0.0.1:{29300+i}',N42_INGEST_SHARD=f'{i}/{args.layers}',N42_SENDER_CACHE_MULT='2')
            launch(f'el-{i}',argv,childenv)
        deadline=time.monotonic()+180
        while True:
            try:
                checkpoint(); break
            except Exception:
                if time.monotonic()>deadline or any(p.poll() is not None for _,p in children): raise
                time.sleep(1)
        validators=commands[args.layers:]
        # Followers must listen before the first leader can publish block 1.
        # A late follower can otherwise receive Decide without the body and
        # remain at execution genesis while later bodies wait for that parent.
        for _,i,argv in validators[1:]: launch(f'validator-{i}',argv,env)
        deadline=time.monotonic()+60
        for i in range(1,7):
            while True:
                with socket.socket() as probe:
                    probe.settimeout(0.3)
                    if probe.connect_ex(('127.0.0.1',29700+i)) == 0: break
                if time.monotonic()>deadline: raise RuntimeError(f'Follower {i} did not open its consensus listener')
                time.sleep(0.1)
        _,i,argv=validators[0]; launch(f'validator-{i}',argv,env)
        time.sleep(15)
        flood=[str(bins/'examples/tx_flood'),'--rpc',','.join(f'http://127.0.0.1:{28200+i}' for i in range(args.layers)),
               '--ingest',','.join(f'127.0.0.1:{29200+i}' for i in range(args.layers)),'--ingest-all',
               *txargs,'--rate',str(args.rate),'--replay',str(dataset)]
        producer=launch('flood',flood,env)
        deadline=time.monotonic()+180
        while 'funding      : mined through nonce' not in (root/'flood.log').read_text(errors='replace'):
            # Different reference versions align the label differently.
            if 'mined through nonce' in (root/'flood.log').read_text(errors='replace'): break
            if producer.poll() is not None or time.monotonic()>deadline: raise RuntimeError('Flood funding failed or timed out')
            time.sleep(1)
        begin=checkpoint_light(); time.sleep(args.seconds); end=checkpoint_light()
        (root/'window.json').write_text(json.dumps({'begin':begin,'end':end},indent=2))
        os.killpg(producer.pid,signal.SIGINT); producer.wait(timeout=15)
        count=sum(end['canonical_counts'][str(n)] for n in range(begin['height']+1,end['height']+1))
        verified_count=0
        with (root/'measured-blocks.jsonl').open('w') as output:
            for n in range(begin['height']+1,end['height']+1):
                block=rpc(0,'eth_getBlockByNumber',[hex(n),False],timeout=30); verified_count+=len(block['transactions'])
                output.write(json.dumps(block)+'\n')
        if verified_count != count: raise RuntimeError('Post-window canonical transaction count differs from execution log count')
        deadline=time.monotonic()+45
        while True:
            commits=[set(re.findall(r'COMMIT view=\d+ block=(0x[0-9a-fA-F]+)',(root/f'validator-{i}.log').read_text(errors='replace'))) for i in range(7)]
            common_commits=set.intersection(*commits)
            if end['hash'] in common_commits: break
            if time.monotonic()>deadline:
                raise RuntimeError('Measured canonical endpoint was not committed by all seven validators')
            time.sleep(1)
        final=checkpoint()
        endpoints=[]
        for point in [begin,end]:
            blocks=[rpc(i,'eth_getBlockByNumber',[hex(point['height']),False],timeout=30) for i in range(args.layers)]
            if any(block['hash'] != point['hash'] for block in blocks): raise RuntimeError('Measured committed endpoint changed canonical hash')
            if not all(len({block[field] for block in blocks})==1 for field in ['hash','stateRoot','receiptsRoot','transactionsRoot']):
                raise RuntimeError('Post-window endpoint root mismatch')
            point['blocks']=blocks
            endpoints.append(blocks)
        hashes=end['blocks'][0]['transactions']
        receipt_checks=[]
        # The benchmark prunes transaction-hash lookup. Read receipts by block.
        receipts=rpc(0,'eth_getBlockReceipts',[hex(end['height'])],timeout=30) if hashes else []
        if len(receipts) != len(hashes): raise RuntimeError('Endpoint receipt count differs from canonical transaction count')
        for index in sorted({0,len(hashes)//2,len(hashes)-1}) if hashes else []:
            receipt=receipts[index]
            if receipt is None or receipt.get('status') != '0x1' or receipt['blockHash'] != end['blocks'][0]['hash'] or receipt['transactionHash'] != hashes[index]:
                raise RuntimeError(f'Measured endpoint receipt is unavailable or failed: {hashes[index]}')
            receipt_checks.append(receipt)
        summary={'begin':begin,'end':end,'final':final,'canonical_txs':count,'canonical_tps':count/(end['at']-begin['at']),
                 'common_validator_commit_hashes':sorted(common_commits),'sampled_execution_receipts':receipt_checks}
        first={p['name']:p for p in begin.get('processes',[])}
        usage={}
        for process in end.get('processes',[]):
            kind=process['name'].split('-')[0]
            usage[kind]=usage.get(kind,0)+process['cpu_seconds']-first[process['name']]['cpu_seconds']
        summary['cpu_seconds_by_role']=usage
        import_errors={}
        runtime_warnings={}
        for i in range(args.layers):
            text=(root/f'el-{i}.log').read_text(errors='replace')
            import_errors[str(i)]={pattern:text.count(pattern) for pattern in ['Invalid payload','compact body refused','no gov5 header variant','failed to apply pre-execution changes']}
            runtime_warnings[str(i)]={pattern:text.count(pattern) for pattern in ['QMDB compaction failed','no state found for block','QMDB reader declined a read','no published output for the parent']}
        summary['import_error_counts']=import_errors
        summary['no_detected_import_errors']=not any(n for errors in import_errors.values() for n in errors.values())
        summary['runtime_warning_counts']=runtime_warnings
        (root/'summary.json').write_text(json.dumps(summary,indent=2))
        print(json.dumps({'layers':args.layers,'canonical_txs':count,'canonical_tps':summary['canonical_tps']}),flush=True)
    finally:
        for _,process in reversed(children):
            if process.poll() is None: os.killpg(process.pid,signal.SIGINT)
        for name,process in reversed(children):
            try: process.wait(timeout=45)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL); process.wait()
        (root/'cleanup.json').write_text(json.dumps({name:{'pid':p.pid,'returncode':p.returncode} for name,p in children},indent=2))
        for handle in handles: handle.close()


if __name__ == '__main__':
    main()
