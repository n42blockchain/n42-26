#!/usr/bin/env python3
"""Audit H2 common commits, QMDB reads and receipt-root-bound successful throughput.

Checks H2 commit reports, independent process/validator identities, and actual
QMDB adapter reports. Recomputes native header hashes and Gov5 receipt roots
from every node. Verifies boundary commit certificates against operator-supplied
H2-v4 configuration. Does not independently verify EVM execution, transaction
trie roots, hardware durability, or the provenance of the operator's trust file.
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
        raise AuditError("invalid hexadecimal byte data")
    result = bytes.fromhex(value[2:])
    if length is not None and len(result) != length:
        raise AuditError("invalid hexadecimal byte length")
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


def rlp_item(raw, start):
    if start >= len(raw):
        raise AuditError("truncated header RLP")
    prefix = raw[start]
    if prefix < 0x80:
        return raw[start:start+1], False, start+1
    is_list = prefix >= 0xc0
    base = 0xc0 if is_list else 0x80
    size = prefix-base
    position = start+1
    if size > 55:
        width = size-55
        encoded_size = raw[position:position+width]
        if len(encoded_size) != width or not encoded_size[0]:
            raise AuditError("invalid header RLP length")
        size = int.from_bytes(encoded_size, "big")
        if size < 56:
            raise AuditError("noncanonical long header RLP")
        position += width
    end = position+size
    if end > len(raw):
        raise AuditError("truncated header RLP payload")
    payload = raw[position:end]
    if not is_list and size == 1 and payload[0] < 0x80:
        raise AuditError("noncanonical short header RLP")
    return payload, is_list, end


def native_header_fields(raw):
    payload, is_list, end = rlp_item(raw, 0)
    if not is_list or end != len(raw):
        raise AuditError("native header must be exactly one RLP list")
    fields, position = [], 0
    while position < len(payload):
        value, is_list, position = rlp_item(payload, position)
        if is_list:
            raise AuditError("native header field is a list")
        fields.append(value)
        if len(fields) > 23:
            raise AuditError("too many native header fields")
    if len(fields) < 15:
        raise AuditError("missing native header fields")
    for index, width in {0:32, 1:32, 2:20, 3:32, 4:32, 5:32, 6:256, 13:32, 14:8}.items():
        if len(fields[index]) != width:
            raise AuditError("wrong native header field width")
    for index in (7, 8, 9, 10, 11, 15, 17, 18):
        if index < len(fields):
            value = fields[index]
            if len(value) > (32 if index == 7 else 8) or (value and value[0] == 0):
                raise AuditError("invalid native header integer")
    for index in (16, 19, 20, 21, 22):
        if index < len(fields) and len(fields[index]) not in (0, 32):
            raise AuditError("invalid native header optional hash")
    return fields


def verify_native_header(raw_hex, block):
    if raw_hex is None:
        raise AuditError("exact native header is unavailable")
    raw = data_bytes(raw_hex)
    fields = native_header_fields(raw)
    actual = keccak_chunks([raw])
    if actual != digest(block.get("hash")):
        raise AuditError("native header hash differs from committed block")
    for field, index in (("parentHash", 0), ("stateRoot", 3), ("transactionsRoot", 4), ("receiptsRoot", 5)):
        if "0x"+fields[index].hex() != digest(block.get(field)):
            raise AuditError(f"native header differs from RPC {field}")
    for field, index in (("number", 8), ("gasUsed", 10)):
        value = block.get(field)
        expected = value if type(value) is int else quantity(value)
        if int.from_bytes(fields[index], "big") != expected:
            raise AuditError(f"native header differs from RPC {field}")
    return dict(verified_header_hash=actual, native_header_rlp="0x"+raw.hex())


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


def trusted_config(value):
    # Qualification deliberately supports the static, chain-bound H2-v4 profile.
    # Other profiles remain available in the diagnostic verifier, not this gate.
    if (not isinstance(value, dict) or type(value.get("schema")) is not int
            or value["schema"] != 1 or value.get("profile") != "h2v4"
            or type(value.get("faultTolerance")) is not int or value["faultTolerance"] != 1
            or type(value.get("chainId")) is not int or not 0 <= value["chainId"] < 2**64
            or not isinstance(value.get("validators"), list) or len(value["validators"]) != 4):
        raise AuditError("trusted configuration requires static four-validator H2-v4 with f=1")
    keys = [public_key(key) for key in value["validators"]]
    if len(set(keys)) != 4 or digest(value.get("validatorChangesHash")) != "0x"+"00"*32:
        raise AuditError("invalid trusted roster or H2-v4 changes hash")
    return dict(schema=1, profile="h2v4", faultTolerance=1, chainId=value["chainId"],
                genesisHash=digest(value.get("genesisHash")), validators=keys,
                validatorChangesHash="0x"+"00"*32)


def config_hash(config):
    return hashlib.sha256(json.dumps(config, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def commit_binary():
    return Path(os.environ.get("N42_COMMIT_VERIFY_BIN", "target/release/n42-verify-commit")).resolve()


def verify_boundary_qc(node, trusted):
    status = node["status"]
    if (not isinstance(status, dict) or status.get("hasCommittedQc") is not True
            or type(status.get("validatorCount")) is not int or status["validatorCount"] != 4
            or type(status.get("latestCommittedView")) is not int
            or not 0 < status["latestCommittedView"] < 2**64
            or digest(status.get("latestCommittedBlockHash")) != node["block"]["hash"]):
        raise AuditError("boundary QC status differs from committed block")
    if (node["chain_id"] != trusted["chainId"] or node["genesis_hash"] != trusted["genesisHash"]
            or roster(node["validator_set"], 4) != trusted["validators"]):
        raise AuditError("boundary chain or ordered roster differs from trusted configuration")
    request = dict(trusted, expectedView=status["latestCommittedView"],
                   expectedBlockHash=node["block"]["hash"], qc=status.get("commitQc"))
    child = subprocess.run([str(commit_binary())], input=json.dumps(request),
                           capture_output=True, text=True, timeout=30)
    try:
        report = json.loads(child.stdout)
    except ValueError as error:
        raise AuditError("commit verifier returned invalid JSON") from error
    if (child.returncode != 0 or not isinstance(report, dict) or report.get("schema") != 1
            or report.get("verified") is not True or report.get("chainBoundSignature") is not True
            or report.get("profile") != "h2v4" or report.get("validatorCount") != 4
            or report.get("faultTolerance") != 1 or report.get("signerCount") not in (3, 4)
            or report.get("view") != request["expectedView"]
            or report.get("blockHash") != request["expectedBlockHash"]):
        raise AuditError("boundary commit certificate verification failed")
    return dict(request=request, verification=report)


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


def capture(urls, call=rpc, now=time.monotonic_ns, boot_id=None, *, trusted=None):
    trusted = trusted_config(trusted)
    if len(urls) != 4 or len(set(urls)) != len(urls):
        raise AuditError("need exactly four distinct validator RPC URLs")
    if boot_id is None:
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    verifier_hash = hashlib.sha256(commit_binary().read_bytes()).hexdigest()
    result = dict(schema=5, boot_id=boot_id, started_ns=now(), nodes=[],
                  trusted_config=trusted, trusted_config_sha256=config_hash(trusted),
                  commit_verifier_sha256=verifier_hash)
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
        header = verify_native_header(call(url, "n42_nativeHeader", [committed_hash]), block)
        after = read_status(call(url, "n42_stateReadStatus", [committed_hash]), committed_hash)
        result["nodes"].append(dict(url=url, chain_id=chain_id, status=status, block=block,
                                   genesis_hash=genesis["hash"], validator_set=validators,
                                   read_before=before, read_after=after, **header))
    if len({node["chain_id"] for node in result["nodes"]}) != 1:
        raise AuditError("validator chain IDs differ")
    validate_read_nodes(result["nodes"])
    for node in result["nodes"]:
        node["commit_proof"] = verify_boundary_qc(node, trusted)
    if hashlib.sha256(commit_binary().read_bytes()).hexdigest() != verifier_hash:
        raise AuditError("commit verifier changed during capture")
    result["finished_ns"] = now()
    return result


def audit(start, end, call=rpc, *, trusted=None):
    trusted = trusted_config(trusted)
    verifier_hash = hashlib.sha256(commit_binary().read_bytes()).hexdigest()
    if (start["schema"] != 5 or end["schema"] != 5
            or not start["boot_id"] or start["boot_id"] != end["boot_id"]):
        raise AuditError("boundary schema or host boot identity differs")
    urls = [node["url"] for node in start["nodes"]]
    if (len(urls) != 4 or len(set(urls)) != len(urls)
            or urls != [node["url"] for node in end["nodes"]]):
        raise AuditError("validator endpoint set changed")
    if not start["started_ns"] <= start["finished_ns"] < end["started_ns"] <= end["finished_ns"]:
        raise AuditError("non-monotonic or overlapping boundaries")
    elapsed = (end["finished_ns"] - start["started_ns"]) / 1e9
    if len({node["chain_id"] for snap in (start, end) for node in snap["nodes"]}) != 1:
        raise AuditError("chain identity changed")
    validate_read_nodes(start["nodes"])
    validate_read_nodes(end["nodes"])
    for snapshot in (start, end):
        if (snapshot.get("trusted_config") != trusted
                or snapshot.get("trusted_config_sha256") != config_hash(trusted)
                or snapshot.get("commit_verifier_sha256") != verifier_hash):
            raise AuditError("boundary trust configuration or commit verifier changed")
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
    proofs = []
    for snapshot in (start, end):
        for node in snapshot["nodes"]:
            proof = verify_boundary_qc(node, trusted)
            if node.get("commit_proof") != proof:
                raise AuditError("captured commit proof differs from re-verification")
            proofs.append(dict(boundary="start" if snapshot is start else "end", url=node["url"], **proof))

    def at(url, number, verify=True):
        block = block_summary(call(url, "eth_getBlockByNumber", [hex(number), False]))
        if block["number"] != number:
            raise AuditError(f"{url}: wrong block height for {number}")
        if verify:
            verify_native_header(call(url, "n42_nativeHeader", [block["hash"]]), block)
        return block

    # Recheck the actual captured hashes, not just heights. A committed reorg
    # or a body that has not reached the engine invalidates the measurement.
    for old, new in zip(start["nodes"], end["nodes"]):
        if (new["block"]["number"] < old["block"]["number"]
                or new["status"]["latestCommittedView"] < old["status"]["latestCommittedView"]):
            raise AuditError("H2 commit regressed")
        for node in (old, new):
            if at(node["url"], node["block"]["number"], verify=False) != node["block"]:
                raise AuditError(f"{node['url']}: captured commit changed")
            header = verify_native_header(node["native_header_rlp"], node["block"])
            if node["verified_header_hash"] != header["verified_header_hash"]:
                raise AuditError("captured native header verification differs")

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
            header = verify_native_header(call(url, "n42_nativeHeader", [block["hash"]]), remote)
            try:
                verified = receipt_summary(call(url, "eth_getBlockReceipts", [block["hash"]]), remote)
            except (AuditError, OSError, subprocess.SubprocessError) as error:
                raise AuditError(f"{url}: receipt verification at block {number}: {error}") from error
            if execution is not None and execution != verified:
                raise AuditError("validators disagree on receipt execution")
            execution = verified
        records.append(dict(**block, **execution, **header, receipt_nodes_verified=len(urls),
                            header_nodes_verified=len(urls)))
        previous = block
    for url in urls:
        if at(url, first) != anchor or at(url, last) != previous:
            raise AuditError(f"{url}: common committed chain differs")
    # Connect each actual signed boundary to the common interval. A QC over a
    # taller, divergent block cannot certify the interval merely by its height.
    ancestry = []
    for old, new in zip(start["nodes"], end["nodes"]):
        for lower, upper, edge in ((old["block"], anchor, "start"), (previous, new["block"], "end")):
            parent = lower
            path = []
            for number in range(lower["number"]+1, upper["number"]+1):
                block = at(old["url"], number, verify=False)
                header = verify_native_header(call(old["url"], "n42_nativeHeader", [block["hash"]]), block)
                if block["parentHash"] != parent["hash"]:
                    raise AuditError("broken ancestry between common interval and certified boundary")
                path.append(dict(**block, **header))
                parent = block
            if parent != upper:
                raise AuditError("certified boundary is not on the common committed chain")
            ancestry.append(dict(url=old["url"], boundary=edge, from_hash=lower["hash"],
                                 to_hash=upper["hash"], headers=path))
    count = sum(block["transactions"] for block in records)
    successful = sum(block["successful_transactions"] for block in records)
    if hashlib.sha256(keccak_binary().read_bytes()).hexdigest() != helper_hash:
        raise AuditError("Keccak helper changed during audit")
    if hashlib.sha256(commit_binary().read_bytes()).hexdigest() != verifier_hash:
        raise AuditError("commit verifier changed during audit")
    return dict(schema=5, scope="h2v4_commit_qcs_qmdb_only_native_headers_and_receipts", nodes=len(urls),
                read_evidence=read_evidence,
                trusted_config=trusted, trusted_config_sha256=config_hash(trusted),
                commit_verifier_sha256=verifier_hash, boundary_commit_proofs=proofs,
                boundary_ancestry=ancestry, anchor=anchor,
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
    for command in (cap, check):
        command.add_argument("--trusted-config", type=Path, required=True,
                             help="operator-authenticated static H2-v4 chain identity and ordered validator keys")
    args = parser.parse_args()
    report = dict(status="failed")
    try:
        trusted = trusted_config(json.loads(args.trusted_config.read_text()))
        if args.command == "capture":
            report = capture(args.rpc.split(","), trusted=trusted)
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
            report = audit(json.loads(args.start.read_text()), json.loads(args.end.read_text()), trusted=trusted)
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
