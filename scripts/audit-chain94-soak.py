#!/usr/bin/env python3
"""Read-only continuous chain94 audit; never starts, stops or resets nodes."""
import argparse
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import time

spec = importlib.util.spec_from_file_location('h2_audit', Path(__file__).with_name('h2-tps-audit.py'))
h2 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h2)
FIELDS = ('hash', 'parentHash', 'stateRoot', 'receiptsRoot', 'transactionsRoot', 'gasUsed', 'transactions')


def matching_blocks(blocks, number, parent=None):
    if not blocks or any(not isinstance(b, dict) or h2.quantity(b.get('number')) != number for b in blocks):
        raise h2.AuditError('missing or wrongly numbered block')
    for block in blocks:
        h2.block_summary(block)
    for field in FIELDS:
        if any(b.get(field) != blocks[0].get(field) for b in blocks[1:]):
            raise h2.AuditError(f'cross-node mismatch at {number}: {field}')
    if parent is not None and blocks[0]['parentHash'] != parent:
        raise h2.AuditError(f'noncontinuous parent at {number}')
    return blocks[0]


def header_qc(block, count):
    """Gov5 embeds LastCommittedQC in header extra; mobile CE is separate."""
    extra = h2.data_bytes(block.get('extraData'))
    if len(extra) > 4096 or len(extra) <= 108 or extra[:4] != b'N42H':
        raise h2.AuditError('header has no bounded embedded commit QC')
    data = extra[12:-96]
    if len(data) < 8:
        raise h2.AuditError('truncated header QC')
    view = int.from_bytes(data[:8], 'little'); offset = 8
    def field(expected):
        nonlocal offset
        if offset+4 > len(data):raise h2.AuditError('truncated QC field')
        size = int.from_bytes(data[offset:offset+4], 'little'); offset += 4
        if size != expected or offset+size > len(data):raise h2.AuditError('invalid QC field length')
        value = data[offset:offset+size]; offset += size
        return value
    block_hash = field(32); signature = field(96); bitmap = field(2+(count+7)//8)
    packed = bitmap[2:]
    if offset != len(data) or int.from_bytes(bitmap[:2], 'little') != count or (count % 8 and packed[-1] >> (count % 8)):
        raise h2.AuditError('noncanonical QC bitmap or trailing data')
    return dict(view=view,blockHash='0x'+block_hash.hex(),signature='0x'+signature.hex(),
                signers=[bool(packed[i//8] & (1 << (i % 8))) for i in range(count)])


def checkpoint_qc(snapshot, count):
    """Read the public commit certificate from the existing Rust checkpoint format."""
    qc = snapshot['last_committed_qc']
    bits = qc['signers']
    if not isinstance(bits, list) or len(bits) != 2 or bits[0] != count:
        raise h2.AuditError('checkpoint signer count differs')
    packed = bytes(bits[1]); signature = bytes(qc['aggregate_signature'])
    if len(packed) != (count+7)//8 or (count % 8 and packed[-1] & ((1 << (8-count % 8))-1)) or len(signature) != 96:
        raise h2.AuditError('invalid checkpoint signature/bitmap')
    return dict(view=qc['view'],blockHash=qc['block_hash'],signature='0x'+signature.hex(),
                signers=[bool(packed[i//8] & (0x80 >> (i % 8))) for i in range(count)])


def verify_qc(qc, trusted, binary):
    request = dict(trusted, expectedView=qc['view'], expectedBlockHash=qc['blockHash'], qc=qc)
    result = subprocess.run([str(binary)], input=json.dumps(request), text=True,
                            capture_output=True, timeout=15)
    value = json.loads(result.stdout)
    if (result.returncode or value.get('verified') is not True
            or value.get('profile') != trusted['profile']
            or value.get('certificatePhase', 'commit') != trusted.get('certificatePhase', 'commit')
            or value.get('view') != qc['view'] or value.get('blockHash') != qc['blockHash']
            or value.get('validatorCount') != len(trusted['validators'])
            or value.get('faultTolerance') != trusted['faultTolerance']
            or value.get('signerCount', 0) < len(trusted['validators'])-trusted['faultTolerance']):
        raise h2.AuditError('commit QC verification failed')
    return value


def node_commit(node, mode, head, trusted, verifier):
    """Authenticate this adapter's certificate and bind it to a canonical block."""
    if mode == 'n42-26':
        status = h2.rpc(node['url'], 'n42_consensusStatus', [])
        if not status.get('hasCommittedQc') or status.get('validatorCount') != 7:
            raise h2.AuditError('missing committed QC or incorrect validator count')
        qc = status['commitQc']
        if qc['blockHash'] != status['latestCommittedBlockHash'] or qc['view'] != status['latestCommittedView']:
            raise h2.AuditError('committed status/QC mismatch')
    elif mode == 'n42-rs':
        qc = checkpoint_qc(json.loads(Path(node['checkpoint']).read_text()), 7)
    elif mode == 'gov5':
        # Rust producers use the existing optional-QC header profile. Inspect
        # at most one committee rotation for a Go-produced carrier; its QC
        # still authenticates an available canonical ancestor on this node.
        for height in range(head, max(-1, head-8), -1):
            header = h2.rpc(node['url'], 'eth_getBlockByNumber', [hex(height), False])
            if not isinstance(header, dict) or h2.quantity(header.get('number')) != height:
                raise h2.AuditError('missing or wrongly numbered QC carrier')
            extra = h2.data_bytes(header.get('extraData'))
            if len(extra) == 108 and extra[:4] == b'N42H':
                continue
            qc = header_qc(header, 7)
            break
        else:
            raise h2.AuditError('no embedded commit QC within bounded committee rotation')
    else:
        raise h2.AuditError('unknown node commit adapter')
    proof = verify_qc(qc, trusted, verifier)
    target = h2.read_committed_block(node['url'], qc['blockHash'])
    number = h2.quantity(target['number'])
    canonical = h2.rpc(node['url'], 'eth_getBlockByNumber', [hex(number), False])
    # RPC/checkpoint commits can advance after the sampled head; only an
    # embedded certificate is constrained by the exact header carrying it.
    if target['hash'] != qc['blockHash'] or canonical['hash'] != qc['blockHash'] or (mode == 'gov5' and number > head):
        raise h2.AuditError('commit QC target is not an available canonical ancestor')
    return number, proof


def resources(pids):
    rows = []
    for pid in pids:
        base = Path('/proc')/str(pid)
        status = (base/'status').read_text()
        row = dict(pid=pid, fds=len(list((base/'fd').iterdir())))
        for line in status.splitlines():
            if line.startswith(('VmRSS:', 'Threads:')):
                key, value = line.split(':', 1); row[key] = int(value.split()[0])
        rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--seconds', type=int, default=3600)
    parser.add_argument('--min-blocks', type=int, default=1000)
    parser.add_argument('--interval', type=float, default=5)
    parser.add_argument('--stall-seconds', type=int, default=180)
    args = parser.parse_args()
    if args.seconds <= 0 or args.interval <= 0 or args.min_blocks <= 0:
        parser.error('duration, interval and minimum blocks must be positive')
    cfg = json.loads(args.config.read_text()); nodes = cfg['nodes']; trusted = cfg['trusted']
    if cfg.get('mode') not in ('gov5', 'n42-26', 'n42-rs', 'mixed'):
        parser.error('supported adapters: gov5, n42-26, n42-rs, mixed with per-node adapters')
    if cfg['mode'] == 'mixed' and any(n.get('mode') not in ('gov5', 'n42-26', 'n42-rs') for n in nodes):
        parser.error('mixed nodes must each specify a supported adapter')
    if len(nodes) != 7 or len(trusted['validators']) != 7 or trusted['faultTolerance'] != 2:
        parser.error('this contract requires seven validators and f=2')
    args.output.mkdir(parents=True, exist_ok=False)
    os.environ['N42_KECCAK_BIN'] = str(Path(cfg['keccak']).resolve())
    verifier = Path(cfg['verifier']).resolve()
    start = time.monotonic(); deadline = start+args.seconds; last_progress = start
    first = last = parent = None; producers = set(); totals = dict(successful_transactions=0, failed_transactions=0)
    report = dict(status='RUNNING', scope='committed chain/receipt/QC consistency; durability requires restart verification')
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=7) as pool, (args.output/'samples.jsonl').open('x') as log:
            def parallel(method, params):
                return list(pool.map(lambda node: h2.rpc(node['url'], method, params), nodes))
            identities = parallel('eth_chainId', [])
            if any(h2.quantity(i) != trusted['chainId'] for i in identities):
                raise h2.AuditError('chain ID mismatch')
            genesis = parallel('eth_getBlockByNumber', ['0x0', False])
            if any(b['hash'] != trusted['genesisHash'] for b in genesis):
                raise h2.AuditError('genesis identity mismatch')
            while time.monotonic() < deadline:
                heads = [h2.quantity(h) for h in parallel('eth_blockNumber', [])]
                current = min(heads)
                commits = list(pool.map(
                    lambda pair: node_commit(pair[0], pair[0]['mode'] if cfg['mode'] == 'mixed' else cfg['mode'], pair[1], trusted, verifier),
                    zip(nodes, heads),
                ))
                proofs = [proof for _, proof in commits]
                current = min([current]+[number for number, _ in commits])
                if first is None:
                    first = last = current
                    parent = matching_blocks(parallel('eth_getBlockByNumber', [hex(current), False]), current)['hash']
                if current < last:
                    raise h2.AuditError('common committed height regressed')
                for number in range(last+1, current+1):
                    blocks = parallel('eth_getBlockByNumber', [hex(number), False])
                    block = matching_blocks(blocks, number, parent)
                    receipts = parallel('eth_getBlockReceipts', [block['hash']])
                    summaries = [h2.receipt_summary(r, b) for r, b in zip(receipts, blocks)]
                    if any(s != summaries[0] for s in summaries[1:]):
                        raise h2.AuditError('cross-node receipt mismatch')
                    for key in totals: totals[key] += summaries[0][key]
                    failed = [{key:r.get(key) for key in ('transactionHash','gasUsed','from','to','contractAddress')} for r in receipts[0] if h2.quantity(r['status']) == 0]
                    producers.add(block['miner'].lower()); parent = block['hash']; last = number
                    last_progress = time.monotonic()
                    log.write(json.dumps(dict(event='block',number=number,hash=parent,receipts=summaries[0],failed_receipts=failed,boundary_qc=proofs))+'\n')
                if time.monotonic()-last_progress > args.stall_seconds:
                    raise h2.AuditError('committed progress stalled')
                sample=dict(event='sample',time=time.time(),heads=heads,common=last,resources=resources(cfg.get('pids',[])))
                log.write(json.dumps(sample)+'\n');log.flush();print(json.dumps(sample),flush=True)
                time.sleep(min(args.interval,max(0,deadline-time.monotonic())))
        expected=set(a.lower() for a in cfg['validator_addresses'])
        if last-first < args.min_blocks or producers != expected or totals['successful_transactions'] == 0:
            raise h2.AuditError('insufficient committed blocks, leader coverage or successful transaction workload')
        report['status']='PASS'
    except Exception as error:
        report.update(status='FAIL',error=str(error));raise
    finally:
        report.update(first=first,last=last,elapsed_seconds=time.monotonic()-start,producers=sorted(producers),**totals)
        (args.output/'summary.json').write_text(json.dumps(report,indent=2)+'\n')

if __name__ == '__main__':
    main()
