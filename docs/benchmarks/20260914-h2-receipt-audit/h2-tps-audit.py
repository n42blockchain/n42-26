#!/usr/bin/env python3
"""Audit H2 common commits, QMDB reads and receipt-root-bound successful throughput.

Checks H2 commit reports, independent process/validator identities, and actual
QMDB adapter reports. Recomputes Gov5 native receipt roots from all four nodes.
It does not independently verify BLS certificates, header hashes, EVM execution,
transaction trie roots, or hardware durability.
"""

import argparse
import contextlib
import json
import math
import hashlib
import os
import re
import subprocess
import time
import urllib.request
from pathlib import Path


class AuditError(ValueError):
    pass


def rpc(url, method, params):
    body = json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params)).encode()
    request = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)
    if (not isinstance(result, dict) or result.get("jsonrpc") != "2.0"
            or result.get("id") != 1 or result.get("error") is not None
            or "result" not in result):
        raise AuditError(f"{url}: invalid/error response to {method}")
    return result["result"]


def quantity(value):
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]+", value):
        raise AuditError(f"invalid quantity: {value!r}")
    return int(value, 16)


def digest(value):
    if not isinstance(value, str) or not re.fullmatch(r"0x[0-9a-fA-F]{64}", value):
        raise AuditError(f"invalid hash: {value!r}")
    return value.lower()


def data_bytes(value, length=None):
    if not isinstance(value, str) or not re.fullmatch(r"0x(?:[0-9a-fA-F]{2})*", value):
        raise AuditError("invalid receipt byte data")
    result = bytes.fromhex(value[2:])
    if length is not None and len(result) != length:
        raise AuditError("invalid receipt byte length")
    return result


def rlp(value):
    if isinstance(value, list):
        payload = b"".join(rlp(item) for item in value)
        offset = 0xc0
    else:
        payload = value.to_bytes((value.bit_length()+7)//8, "big") if isinstance(value, int) else value
        if len(payload) == 1 and payload[0] < 0x80:
            return payload
        offset = 0x80
    if len(payload) < 56:
        return bytes([offset+len(payload)]) + payload
    size = len(payload).to_bytes((len(payload).bit_length()+7)//8, "big")
    return bytes([offset+55+len(size)]) + size + payload


def keccak_binary():
    return Path(os.environ.get("N42_KECCAK_BIN", "target/release/n42-keccak")).resolve()


def keccak_chunks(chunks):
    # The helper hashes the stream in bounded buffers. SHA3-256 has different
    # padding and must never be substituted for Keccak-256.
    with subprocess.Popen([str(keccak_binary())], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        try:
            for chunk in chunks:
                child.stdin.write(chunk)
            child.stdin.close()
            child.stdin = None
            value, _ = child.communicate(timeout=30)
            if child.returncode != 0 or len(value) != 32:
                raise AuditError("Keccak helper failed or returned an invalid digest")
            return "0x" + value.hex()
        except BaseException:
            child.kill()
            if child.stdin is not None:
                with contextlib.suppress(OSError):
                    child.stdin.close()
                child.stdin = None
            child.wait()
            raise


def receipt_summary(receipts, block):
    """Verify positional RPC metadata and reconstruct Gov5's native receipt root."""
    transactions = [digest(tx) for tx in block["transactions"]]
    if not isinstance(receipts, list) or len(receipts) != len(transactions):
        raise AuditError("receipt count differs from committed transaction count")
    block_hash, number = digest(block["hash"]), quantity(block["number"])
    stats = dict(successful_transactions=0, failed_transactions=0)

    def encoded():
        previous_gas, log_index = 0, 0
        for index, (receipt, tx_hash) in enumerate(zip(receipts, transactions)):
            if (not isinstance(receipt, dict) or digest(receipt.get("blockHash")) != block_hash
                    or quantity(receipt.get("blockNumber")) != number
                    or quantity(receipt.get("transactionIndex")) != index
                    or digest(receipt.get("transactionHash")) != tx_hash):
                raise AuditError("receipt identity/order differs from committed block")
            status = quantity(receipt.get("status"))
            cumulative = quantity(receipt.get("cumulativeGasUsed"))
            if status not in (0, 1):
                raise AuditError("receipt has no binary execution status")
            if (not previous_gas <= cumulative < 2**64
                    or quantity(receipt.get("gasUsed")) != cumulative-previous_gas):
                raise AuditError("receipt gas accounting is inconsistent")
            previous_gas = cumulative
            logs = receipt.get("logs")
            if not isinstance(logs, list):
                raise AuditError("missing receipt logs")
            encoded_logs = []
            for log in logs:
                if (not isinstance(log, dict) or log.get("removed") is not False
                        or digest(log.get("blockHash")) != block_hash
                        or quantity(log.get("blockNumber")) != number
                        or digest(log.get("transactionHash")) != tx_hash
                        or quantity(log.get("transactionIndex")) != index
                        or quantity(log.get("logIndex")) != log_index):
                    raise AuditError("receipt log identity/order differs from committed block")
                topics = log.get("topics")
                if not isinstance(topics, list) or len(topics) > 4:
                    raise AuditError("invalid receipt log topics")
                encoded_logs.append([data_bytes(log.get("address"), 20),
                                     [data_bytes(topic, 32) for topic in topics],
                                     data_bytes(log.get("data"))])
                log_index += 1
            stats["successful_transactions" if status else "failed_transactions"] += 1
            # Gov5 replay-v2 excludes the Ethereum bloom and typed envelope.
            yield rlp([status, cumulative, encoded_logs])
        if previous_gas != quantity(block.get("gasUsed")):
            raise AuditError("receipt cumulative gas differs from block gasUsed")

    root = keccak_chunks(encoded())
    if root != digest(block.get("receiptsRoot")):
        raise AuditError("recomputed Gov5 receipt root differs from committed block")
    return dict(**stats, verified_receipts_root=root)


def block_summary(block):
    if not isinstance(block, dict):
        raise AuditError("committed block is unavailable in the execution layer")
    transactions = block.get("transactions")
    if not isinstance(transactions, list):
        raise AuditError("missing transaction list")
    for tx in transactions:
        digest(tx)
    if len({digest(tx) for tx in transactions}) != len(transactions):
        raise AuditError("duplicate transaction hash in block")
    summary = {field: digest(block.get(field)) for field in
               ("hash", "parentHash", "stateRoot", "receiptsRoot", "transactionsRoot")}
    summary.update(number=quantity(block.get("number")), transactions=len(transactions),
                   gasUsed=quantity(block.get("gasUsed")))
    return summary


READ_COUNTERS = ("accountReads", "storageReads", "accountComparisons", "storageComparisons",
                 "mismatches", "readErrors", "providerErrors", "pinnedProviders",
                 "unavailableProviders")
READ_IDENTITY = ("schema", "instanceId", "processId", "mode", "backend", "coverage",
                 "walEnabled", "chainId", "genesisHash", "baseBlockHash", "baseRoot",
                 "validatorPublicKey")


def public_key(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-fA-F]{96}", value):
        raise AuditError("invalid validator public key")
    return value.lower()


def read_status(value, requested):
    if (not isinstance(value, dict) or type(value.get("schema")) is not int or value.get("schema") != 1
            or value.get("mode") != "only" or value.get("backend") != "gov5_qmdb_binary"
            or value.get("coverage") != "exact_block_provider" or value.get("walEnabled") is not True):
        raise AuditError("qualification requires a WAL-backed QMDB only adapter")
    result = dict(value)
    for field in ("instanceId", "genesisHash", "baseBlockHash", "baseRoot"):
        result[field] = digest(value.get(field))
    for field in ("processId", "chainId"):
        if type(value.get(field)) is not int or not 0 <= value[field] < 2**64:
            raise AuditError(f"invalid read status {field}")
    if not result["processId"]:
        raise AuditError("missing process identity")
    result["validatorPublicKey"] = public_key(value.get("validatorPublicKey"))
    counters = value.get("counters")
    if not isinstance(counters, dict):
        raise AuditError("missing QMDB read counters")
    for field in READ_COUNTERS:
        if type(counters.get(field)) is not int or not 0 <= counters[field] < 2**64:
            raise AuditError(f"invalid QMDB counter {field}")
    if requested is None:
        if value.get("requestedBlockHash") is not None or value.get("durableRoot") is not None:
            raise AuditError("unexpected QMDB requested block")
    else:
        if digest(value.get("requestedBlockHash")) != requested:
            raise AuditError("QMDB durable root refers to another block")
        result["durableRoot"] = digest(value.get("durableRoot"))
    return result


def roster(value, count):
    if not isinstance(value, dict) or not isinstance(value.get("active"), list):
        raise AuditError("missing active validator roster")
    active = value["active"]
    if (len(active) != count or any(not isinstance(entry, dict) or type(entry.get("index")) is not int
                                    for entry in active)
            or sorted(entry["index"] for entry in active) != list(range(count))):
        raise AuditError("invalid active validator indices")
    keys = [public_key(entry.get("publicKey")) for entry in sorted(active, key=lambda x: x["index"])]
    if len(set(keys)) != count:
        raise AuditError("duplicate validator public keys in roster")
    return keys


def read_sequence(snapshots):
    first = snapshots[0]
    for old, new in zip(snapshots, snapshots[1:]):
        if any(old[field] != new[field] for field in READ_IDENTITY):
            raise AuditError("QMDB process, validator, or adapter identity changed")
        for field in READ_COUNTERS:
            if new["counters"][field] < old["counters"][field]:
                raise AuditError(f"QMDB counter regressed: {field}")
    return {field: snapshots[-1]["counters"][field] - first["counters"][field]
            for field in READ_COUNTERS}


def validate_read_nodes(nodes):
    instances, validators, rosters, geneses = [], [], [], []
    for node in nodes:
        before = read_status(node["read_before"], None)
        after = read_status(node["read_after"], node["block"]["hash"])
        read_sequence([before, after])
        if before["chainId"] != node["chain_id"] or before["genesisHash"] != node["genesis_hash"]:
            raise AuditError("QMDB identity differs from execution chain")
        if after["durableRoot"] != node["block"]["stateRoot"]:
            raise AuditError("QMDB durable root differs from H2 committed execution root")
        keys = roster(node["validator_set"], len(nodes))
        if before["validatorPublicKey"] not in keys:
            raise AuditError("reporting process is not an active validator")
        instances.append(before["instanceId"])
        validators.append(before["validatorPublicKey"])
        rosters.append(tuple(keys))
        geneses.append(before["genesisHash"])
    if len(set(instances)) != len(nodes):
        raise AuditError("RPC URLs alias the same process instance")
    if len(set(validators)) != len(nodes):
        raise AuditError("RPC processes use duplicate validator identities")
    if len(set(rosters)) != 1 or len(set(geneses)) != 1:
        raise AuditError("validator rosters or genesis identities differ")


def capture(urls, call=rpc, now=time.monotonic_ns, boot_id=None):
    if len(urls) < 4 or len(set(urls)) != len(urls):
        raise AuditError("need at least four distinct validator RPC URLs")
    if boot_id is None:
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    result = dict(schema=3, boot_id=boot_id, started_ns=now(), nodes=[])
    for url in urls:
        # Bracket the H2 observation. The audit uses the outer counter samples
        # so all measured commits are covered even while other nodes progress.
        before = read_status(call(url, "n42_stateReadStatus", [None]), None)
        chain_id = quantity(call(url, "eth_chainId", []))
        genesis = block_summary(call(url, "eth_getBlockByNumber", ["0x0", False]))
        if genesis["number"] != 0:
            raise AuditError("wrong genesis block height")
        validators = call(url, "n42_validatorSet", [])
        status = call(url, "n42_consensusStatus", [])
        if (not isinstance(status, dict) or status.get("hasCommittedQc") is not True
                or status.get("validatorCount") != len(urls)
                or type(status.get("latestCommittedView")) is not int
                or status["latestCommittedView"] < 0):
            raise AuditError(f"{url}: missing H2 commit or unexpected validator count")
        committed_hash = digest(status.get("latestCommittedBlockHash"))
        block = block_summary(call(url, "eth_getBlockByHash", [committed_hash, False]))
        if block["hash"] != committed_hash:
            raise AuditError(f"{url}: execution block differs from H2 committed hash")
        after = read_status(call(url, "n42_stateReadStatus", [committed_hash]), committed_hash)
        result["nodes"].append(dict(url=url, chain_id=chain_id, status=status, block=block,
                                   genesis_hash=genesis["hash"], validator_set=validators,
                                   read_before=before, read_after=after))
    result["finished_ns"] = now()
    if len({node["chain_id"] for node in result["nodes"]}) != 1:
        raise AuditError("validator chain IDs differ")
    validate_read_nodes(result["nodes"])
    return result


def audit(start, end, call=rpc):
    if (start["schema"] != 3 or end["schema"] != 3
            or not start["boot_id"] or start["boot_id"] != end["boot_id"]):
        raise AuditError("boundary schema or host boot identity differs")
    urls = [node["url"] for node in start["nodes"]]
    if (len(urls) < 4 or len(set(urls)) != len(urls)
            or urls != [node["url"] for node in end["nodes"]]):
        raise AuditError("validator endpoint set changed")
    if not start["started_ns"] <= start["finished_ns"] < end["started_ns"] <= end["finished_ns"]:
        raise AuditError("non-monotonic or overlapping boundaries")
    elapsed = (end["finished_ns"] - start["started_ns"]) / 1e9
    if len({node["chain_id"] for snap in (start, end) for node in snap["nodes"]}) != 1:
        raise AuditError("chain identity changed")
    validate_read_nodes(start["nodes"])
    validate_read_nodes(end["nodes"])
    read_evidence = []
    for old, new in zip(start["nodes"], end["nodes"]):
        if roster(old["validator_set"], len(urls)) != roster(new["validator_set"], len(urls)):
            raise AuditError("validator roster changed during the window")
        delta = read_sequence([old["read_before"], old["read_after"],
                               new["read_before"], new["read_after"]])
        for field in ("mismatches", "readErrors", "providerErrors", "unavailableProviders"):
            if delta[field]:
                raise AuditError(f"{old['url']}: QMDB {field} advanced by {delta[field]}")
        if not delta["pinnedProviders"] or not delta["accountReads"]:
            raise AuditError(f"{old['url']}: no observed QMDB provider/account read progress")
        read_evidence.append(dict(url=old["url"], instance_id=old["read_before"]["instanceId"],
                                  validator_public_key=old["read_before"]["validatorPublicKey"],
                                  counters_delta=delta))

    def at(url, number):
        block = block_summary(call(url, "eth_getBlockByNumber", [hex(number), False]))
        if block["number"] != number:
            raise AuditError(f"{url}: wrong block height for {number}")
        return block

    # Recheck the actual captured hashes, not just heights. A committed reorg
    # or a body that has not reached the engine invalidates the measurement.
    for old, new in zip(start["nodes"], end["nodes"]):
        if (new["block"]["number"] < old["block"]["number"]
                or new["status"]["latestCommittedView"] < old["status"]["latestCommittedView"]):
            raise AuditError("H2 commit regressed")
        for node in (old, new):
            if at(node["url"], node["block"]["number"]) != node["block"]:
                raise AuditError(f"{node['url']}: captured commit changed")

    # Exclude everything any node had already committed at the start; include
    # only heights every node committed by its end observation. The denominator
    # contains all boundary RPC time, so sampling delay cannot inflate TPS.
    first = max(node["block"]["number"] for node in start["nodes"])
    last = min(node["block"]["number"] for node in end["nodes"])
    if last <= first:
        raise AuditError("no common committed progress")
    anchor = at(urls[0], first)
    previous = anchor
    records = []
    helper_hash = hashlib.sha256(keccak_binary().read_bytes()).hexdigest()
    if keccak_chunks([]) != "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470":
        raise AuditError("Keccak helper failed the empty-input vector")
    for number in range(first + 1, last + 1):
        raw = call(urls[0], "eth_getBlockByNumber", [hex(number), False])
        block = block_summary(raw)
        if block["number"] != number:
            raise AuditError("wrong committed block height")
        if block["parentHash"] != previous["hash"]:
            raise AuditError(f"broken ancestry at {number}")
        execution = None
        for url in urls:
            remote = raw if url == urls[0] else call(url, "eth_getBlockByHash", [block["hash"], False])
            if (block_summary(remote) != block
                    or [digest(tx) for tx in remote["transactions"]] != [digest(tx) for tx in raw["transactions"]]):
                raise AuditError(f"{url}: common committed chain differs")
            try:
                verified = receipt_summary(call(url, "eth_getBlockReceipts", [block["hash"]]), remote)
            except (AuditError, OSError, subprocess.SubprocessError) as error:
                raise AuditError(f"{url}: receipt verification at block {number}: {error}") from error
            if execution is not None and execution != verified:
                raise AuditError("validators disagree on receipt execution")
            execution = verified
        records.append(dict(**block, **execution, receipt_nodes_verified=len(urls)))
        previous = block
    for url in urls:
        if at(url, first) != anchor or at(url, last) != previous:
            raise AuditError(f"{url}: common committed chain differs")
    count = sum(block["transactions"] for block in records)
    successful = sum(block["successful_transactions"] for block in records)
    if hashlib.sha256(keccak_binary().read_bytes()).hexdigest() != helper_hash:
        raise AuditError("Keccak helper changed during audit")
    return dict(schema=3, scope="h2_rpc_commits_qmdb_only_and_gov5_receipt_roots", nodes=len(urls),
                read_evidence=read_evidence,
                keccak_binary_sha256=helper_hash,
                start_block=first, end_block=last, committed_blocks=last-first,
                committed_transactions=count, measurement_seconds=elapsed,
                successful_committed_transactions=successful, failed_committed_transactions=count-successful,
                successful_committed_tps=successful/elapsed,
                strict_committed_tps=count/elapsed, blocks=records,
                start_boundary_seconds=(start["finished_ns"]-start["started_ns"])/1e9,
                end_boundary_seconds=(end["finished_ns"]-end["started_ns"])/1e9)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    cap = commands.add_parser("capture")
    cap.add_argument("--rpc", required=True, help="comma-separated validator RPC URLs")
    cap.add_argument("--out", type=Path, required=True)
    cap.add_argument("--verify-receipts", action="store_true",
                     help="preflight the receipt endpoint and native roots before loading the fleet")
    check = commands.add_parser("audit")
    check.add_argument("--start", type=Path, required=True)
    check.add_argument("--end", type=Path, required=True)
    check.add_argument("--out", type=Path, required=True)
    check.add_argument("--min-tps", type=float, default=1_000_000)
    args = parser.parse_args()
    report = dict(status="failed")
    try:
        if args.command == "capture":
            report = capture(args.rpc.split(","))
            if args.verify_receipts:
                report["receipt_preflight"] = []
                for node in report["nodes"]:
                    block_hash = node["block"]["hash"]
                    raw = rpc(node["url"], "eth_getBlockByHash", [block_hash, False])
                    if block_summary(raw) != node["block"]:
                        raise AuditError("preflight committed block changed")
                    verified = receipt_summary(rpc(node["url"], "eth_getBlockReceipts", [block_hash]), raw)
                    report["receipt_preflight"].append(dict(url=node["url"], block_hash=block_hash, **verified))
        else:
            if not math.isfinite(args.min_tps) or args.min_tps <= 0:
                raise AuditError("minimum TPS must be finite and positive")
            report = audit(json.loads(args.start.read_text()), json.loads(args.end.read_text()))
            report["min_tps"] = args.min_tps
            report["status"] = "passed" if report["successful_committed_tps"] >= args.min_tps else "below_target"
        args.out.write_text(json.dumps(report, indent=2) + "\n")
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        args.out.write_text(json.dumps(dict(status="failed", error=str(error)), indent=2) + "\n")
        parser.exit(1, f"H2 TPS audit failed: {error}\n")
    if report.get("status") == "below_target":
        parser.exit(1, f"H2 successful committed TPS {report['successful_committed_tps']:.6f} < {args.min_tps:.6f}\n")


if __name__ == "__main__":
    main()
