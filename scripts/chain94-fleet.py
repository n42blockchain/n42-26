#!/usr/bin/env python3
"""Isolated seven-Rust-node fleet on an explicitly selected, frozen Gov5 snapshot.

No command stops, opens for writing, or seeds from the live Gov5 node directories.
prepare needs Python cryptography for fresh, fleet-specific libp2p identities.
All mutable data and per-node private keys remain under --runtime (gitignored).
"""
import argparse
import concurrent.futures
import hashlib
import json
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import time
import urllib.request

REPO = Path(__file__).resolve().parents[1]
BASES = dict(http=22400, auth=22500, metrics=22600, consensus=32400, p2p=30400, mobile=9740)


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    with open(temporary, "w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def read_json(path):
    with open(path) as stream:
        return json.load(stream)


def peer_id(public):
    # identity multihash of protobuf PublicKey { Type: Ed25519, Data: public }.
    raw = b"\x00\x24\x08\x01\x12\x20" + public
    alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
    value, encoded = int.from_bytes(raw, "big"), ""
    while value:
        value, digit = divmod(value, 58)
        encoded = alphabet[digit] + encoded
    return "1" * (len(raw) - len(raw.lstrip(b"\x00"))) + encoded


def prepare(args):
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    runtime, source = args.runtime, args.source.resolve()
    leaf = args.leaf.resolve()
    template = source / "snapshot-reth-template"
    artifacts = source / "artifacts"
    resuming = args.resume_prepare and runtime.exists()
    if runtime.exists() and not resuming:
        raise ValueError("runtime already exists; use a new directory, never overwrite a fleet")
    if resuming and ((runtime / "manifest.json").exists() or list((runtime / "pids").glob("*.json"))):
        raise ValueError("only an incomplete, never-started preparation may resume")
    if runtime == source or source.is_relative_to(runtime) or runtime.is_relative_to(source):
        raise ValueError("runtime and source must be disjoint")
    manifest = read_json(artifacts / "state.jsonl.manifest.json")
    config = read_json(artifacts / "consensus-peer-bound.json")
    snapshot = read_json(artifacts / "consensus_state.json")
    if manifest["chain_id"] != 94 or not manifest["complete_state"]:
        raise ValueError("expected a complete chain-94 state snapshot")
    if len(config["initial_validators"]) != 7 or config["fault_tolerance"] != 2:
        raise ValueError("expected seven validators and f=2")
    if snapshot["execution_validated_head_hash"] != manifest["block_hash"]:
        raise ValueError("consensus and state snapshot anchors differ")
    if snapshot["locked_qc"]["block_hash"] != manifest["block_hash"]:
        raise ValueError("snapshot locked QC is not the applied head")
    if snapshot["last_committed_qc"]["block_hash"] != manifest["block_hash"]:
        raise ValueError("snapshot committed QC is not the applied head")
    if snapshot.get("scheduled_epoch_transition"):
        raise ValueError("scheduled validator changes require a separate migration")
    epoch, epoch_validators, fault_tolerance = snapshot["current_epoch_validators"]
    if len(epoch_validators) != 7 or fault_tolerance != 2:
        raise ValueError("snapshot must have seven epoch validators")
    match = re.search(r"(?:export\s+)?GENESIS_HASH=['\"]?(0x[0-9a-fA-F]{64})", (source / "env.sh").read_text())
    if not match:
        raise ValueError("source environment does not name its genesis hash")
    genesis_hash = match.group(1)
    if not leaf.is_file() or not (template / "db/mdbx.dat").is_file():
        raise ValueError("frozen leaf-form export or Reth template is missing")
    # Read only the explicitly named key file. Never print any of its contents.
    validators = {}
    for line in args.validators.read_text().splitlines():
        fields = line.split(",")
        if len(fields) == 4 and fields[0] in map(str, range(7)):
            if int(fields[0]) in validators:
                raise ValueError("duplicate validator index")
            validators[int(fields[0])] = fields
    if len(validators) != 7:
        raise ValueError("validator file must supply indices 0..6 exactly once")
    for index, validator in enumerate(config["initial_validators"]):
        if validators[index][1].lower() != validator["address"].lower():
            raise ValueError(f"validator address mismatch at index {index}")
        if len(bytes.fromhex(validators[index][3].removeprefix("0x"))) != 32:
            raise ValueError(f"invalid BLS key length at index {index}")
    runtime.mkdir(parents=True, mode=0o700, exist_ok=resuming)
    (runtime / "artifacts").mkdir(exist_ok=resuming)
    (runtime / "logs").mkdir(exist_ok=resuming)
    (runtime / "pids").mkdir(exist_ok=resuming)
    frozen = {}
    for name in ("genesis.json", "genesis-range.n42frng", "state.jsonl.header.rlp", "state.jsonl.manifest.json", "consensus_state.json"):
        src = artifacts / name
        subprocess.run(["cp", "--reflink=auto", str(src), str(runtime / "artifacts" / name)], check=True)
        frozen[name] = digest(src)
        if digest(runtime / "artifacts" / name) != frozen[name]:
            raise ValueError(f"copy changed: {name}")
    print("Pinning leaf-form export and frozen execution template...", flush=True)
    leaf_copy = runtime / "artifacts/snapshot.qmdb"
    subprocess.run(["cp", "--reflink=auto", "--sparse=always", str(leaf), str(leaf_copy)], check=True)
    frozen["snapshot.qmdb"] = digest(leaf)
    if digest(leaf_copy) != frozen["snapshot.qmdb"]:
        raise ValueError("leaf-form copy differs")
    template_hashes = {str(p.relative_to(template)): digest(p) for p in template.rglob("*") if p.is_file()}
    peers = []
    for index in range(7):
        node = runtime / f"node{index}"
        node.mkdir(mode=0o700, exist_ok=resuming)
        (node / "consensus").mkdir(exist_ok=resuming)
        secret = (Ed25519PrivateKey.from_private_bytes(bytes.fromhex((node / "p2p.key").read_text()))
                  if resuming and (node / "p2p.key").exists() else Ed25519PrivateKey.generate())
        peer = peer_id(secret.public_key().public_bytes_raw())
        peers.append(peer)
        (node / "p2p.key").write_text(secret.private_bytes_raw().hex())
        (node / "bls.key").write_text(validators[index][3].removeprefix("0x"))
        os.chmod(node / "p2p.key", 0o600)
        os.chmod(node / "bls.key", 0o600)
        config["initial_validators"][index]["p2p_peer_id"] = peer
        print(f"Copying frozen execution state for node {index}...", flush=True)
        if not (node / "reth").exists():
            subprocess.run(["cp", "-a", "--reflink=auto", "--sparse=always", str(template), str(node / "reth")], check=True)
        copied = {str(p.relative_to(node / "reth")): digest(p) for p in (node / "reth").rglob("*") if p.is_file()}
        if copied != template_hashes:
            raise ValueError(f"node {index} does not match the frozen template")
    # Peer IDs are transport bindings, not the BLS-signed committee. Keep all
    # consensus views, QCs, vote watermarks, keys and chain identity unchanged.
    for index, validator in enumerate(epoch_validators):
        if validator["address"] != config["initial_validators"][index]["address"]:
            raise ValueError("snapshot validator order differs")
        validator["p2p_peer_id"] = peers[index]
    for index in range(7):
        write_json(runtime / f"node{index}/consensus/consensus_state.json", snapshot)
    write_json(runtime / "consensus.json", config)
    write_json(runtime / "manifest.json", dict(
        version=1, chain_id=94, snapshot=manifest, source=str(source), leaf_source=str(leaf),
        artifacts_sha256=frozen, template_sha256=template_hashes, peers=peers, ports=BASES,
        genesis_hash=genesis_hash,
    ))
    print(f"Prepared {runtime}; no node has been started.")


def process_identity(pid):
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        # comm can contain spaces; fields after ')' begin at field 3.
        fields = raw[raw.rindex(")") + 2:].split()
        if fields[0] == "Z":
            return None
        return fields[19]
    except FileNotFoundError:
        return None


def owned_process(runtime, index):
    path = runtime / f"pids/node{index}.json"
    if not path.exists():
        return None
    record = read_json(path)
    pid = record["pid"]
    if process_identity(pid) != record["starttime"]:
        return None
    command = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\x00")
    if str(runtime / f"node{index}/reth").encode() not in command:
        raise ValueError(f"PID {pid} no longer belongs to this datadir; refusing to signal it")
    return pid


def start(args):
    runtime = args.runtime
    manifest = read_json(runtime / "manifest.json")
    binary = args.binary.resolve()
    if not binary.is_file():
        raise ValueError(f"binary missing: {binary}")
    if any(owned_process(runtime, i) for i in range(7)):
        raise ValueError("fleet is already running; stop it gracefully before restarting")
    for name, expected in manifest["artifacts_sha256"].items():
        if digest(runtime / "artifacts" / name) != expected:
            raise ValueError(f"frozen artifact changed: {name}")
    # Check every TCP/UDP port before starting any node. No existing process is stopped.
    ports = manifest["ports"]
    for kind, base in ports.items():
        types = [socket.SOCK_DGRAM] if kind == "mobile" else [socket.SOCK_STREAM]
        if kind == "consensus":
            types.append(socket.SOCK_DGRAM)
        for index in range(7):
            for socktype in types:
                with socket.socket(socket.AF_INET, socktype) as sock:
                    if socktype == socket.SOCK_STREAM:
                        # Match the server's restart semantics: TIME_WAIT from
                        # this fleet is not an active listener. listen() still
                        # rejects a competing bound/listening TCP socket.
                        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    try:
                        sock.bind(("127.0.0.1", base + index))
                        if socktype == socket.SOCK_STREAM:
                            sock.listen(1)
                    except OSError as error:
                        raise RuntimeError(f"port preflight failed: {kind} node{index} port {base + index}: {error}") from error
    snapshot = manifest["snapshot"]
    consensus_config = read_json(runtime / "consensus.json")
    run = dict(binary=str(binary), sha256=digest(binary), started=time.time(),
               block_interval_ms=consensus_config["slot_time_ms"],
               log_offsets={str(i): (runtime / f"logs/node{i}.log").stat().st_size
                            if (runtime / f"logs/node{i}.log").exists() else 0 for i in range(7)})
    write_json(runtime / "run.json", run)
    write_json(runtime / f"run-{int(run['started'])}.json", run)
    for index in range(7):
        node = runtime / f"node{index}"
        # Do not inherit another experiment's N42 mode, keys, peers or ports.
        env = {key: value for key, value in os.environ.items() if not key.startswith("N42_")}
        env.update({
            "N42_CONSENSUS_CONFIG": str(runtime / "consensus.json"),
            "N42_DATA_DIR": str(node / "consensus"),
            "N42_VALIDATOR_KEY": "@" + str(node / "bls.key"),
            "N42_P2P_KEY": "@" + str(node / "p2p.key"),
            "N42_GOV5_H2_PARTICIPANT": "1", "N42_GOV5_HEADER_PROFILE": "1",
            "N42_GOV5_LEGACY_SIGNING": "1", "N42_GOV5_NATIVE_PRODUCER": "1",
            "N42_GOV5_QMDB_EXECUTION": "1", "N42_GOV5_PRAGUE_TIME": "1746612311",
            "N42_GOV5_QMDB_LEAF_FORM": str(runtime / "artifacts/snapshot.qmdb"),
            "N42_GOV5_GENESIS_BOOTSTRAP": str(runtime / "artifacts/genesis-range.n42frng"),
            "N42_QMDB_BOOTSTRAP_BLOCK": str(snapshot["block_number"]),
            "N42_QMDB_BOOTSTRAP_BLOCK_HASH": snapshot["block_hash"],
            "N42_QMDB_BOOTSTRAP_ROOT": snapshot["state_root"],
            "N42_INTEROP_GENESIS_HASH": manifest["genesis_hash"],
            "N42_CONSENSUS_PORT": str(ports["consensus"] + index),
            "N42_STARHUB_PORT": str(ports["mobile"] + index),
            "N42_LISTEN_IP": "127.0.0.1", "N42_NO_AUTO_CONNECT": "1",
            "N42_ENABLE_MDNS": "0", "N42_ENABLE_DHT": "0", "N42_ENABLE_HTTP_RPC": "1",
            "N42_LOW_MEMORY": "1", "N42_COMPACT_BLOCK": "0",
            "N42_BLOCK_INTERVAL_MS": str(consensus_config["slot_time_ms"]),
            "N42_TRUSTED_PEERS": ",".join(f"/ip4/127.0.0.1/tcp/{ports['consensus']+i}/p2p/{peer}" for i, peer in enumerate(manifest["peers"]) if i != index),
            "RAYON_NUM_THREADS": "8", "TOKIO_WORKER_THREADS": "8",
            "RUST_LOG": "info",
        })
        command = [str(binary), "node", "--chain", str(runtime / "artifacts/genesis.json"),
                   "--datadir", str(node / "reth"), "--disable-discovery",
                   "--addr", "127.0.0.1", "--port", str(ports["p2p"] + index),
                   "--max-inbound-peers", "0", "--max-outbound-peers", "0",
                   "--http", "--http.addr", "127.0.0.1", "--http.port", str(ports["http"] + index),
                   "--authrpc.addr", "127.0.0.1", "--authrpc.port", str(ports["auth"] + index),
                   "--metrics", f"127.0.0.1:{ports['metrics'] + index}",
                   "--ipcdisable", "--log.file.directory", str(node / "logs"),
                   "--log.file.max-files", "0", "--color", "never"]
        with open(runtime / f"logs/node{index}.log", "ab", buffering=0) as log:
            child = subprocess.Popen(command, cwd=REPO, env=env, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        identity = process_identity(child.pid)
        write_json(runtime / f"pids/node{index}.json", dict(pid=child.pid, starttime=identity))
        print(f"node{index}: PID {child.pid}, RPC {ports['http'] + index}", flush=True)


def stop(args):
    targets = [(i, owned_process(args.runtime, i)) for i in range(7)]
    for _, pid in targets:
        if pid:
            os.kill(pid, signal.SIGINT)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        alive = [i for i, _ in targets if owned_process(args.runtime, i)]
        if not alive:
            print("All fleet nodes stopped gracefully; no data removed.")
            return
        time.sleep(1)
    raise RuntimeError(f"nodes {alive} still shutting down; no SIGKILL was sent")


def rpc(port, method, params):
    request = urllib.request.Request(f"http://127.0.0.1:{port}",
        data=json.dumps(dict(jsonrpc="2.0", id=1, method=method, params=params)).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        result = json.load(response)
    if "error" in result:
        raise ValueError(result["error"])
    return result["result"]


def sample(runtime):
    manifest = read_json(runtime / "manifest.json")
    ports = [manifest["ports"]["http"] + i for i in range(7)]
    with concurrent.futures.ThreadPoolExecutor(max_workers=7) as pool:
        heads = list(pool.map(lambda p: int(rpc(p, "eth_blockNumber", []), 16), ports))
        consensus = list(pool.map(lambda p: rpc(p, "n42_consensusStatus", []), ports))
        if not all(s["hasCommittedQc"] and s["validatorCount"] == 7 for s in consensus):
            raise ValueError("all seven nodes must report a committed QC and seven validators")
        commits = list(pool.map(committed_block, zip(ports, consensus)))
        committed_heights = [int(block["number"], 16) for block in commits]
        number = min(heads + committed_heights)
        blocks = list(pool.map(lambda p: rpc(p, "eth_getBlockByNumber", [hex(number), False]), ports))
    fields = ("hash", "stateRoot", "receiptsRoot", "transactionsRoot")
    identical = all(len({block[field] for block in blocks}) == 1 for field in fields)
    return dict(at=time.time(), heads=heads, comparison_height=number, identical=identical,
                committed_heights=committed_heights, committed_views=[s["latestCommittedView"] for s in consensus],
                **{field: blocks[0][field] for field in fields})


def committed_block(pair):
    port, consensus = pair
    # CommitQC publication can precede the asynchronous finalizing FCU by a
    # few milliseconds. Keep the exact sampled hash pinned while waiting;
    # never replace it with an unrelated latest head to hide a failed import.
    deadline = time.monotonic() + 2
    while True:
        block = rpc(port, "eth_getBlockByHash", [consensus["latestCommittedBlockHash"], False])
        if block is not None:
            return block
        if time.monotonic() >= deadline:
            raise ValueError("a committed QC has no executed block after the FCU grace period")
        time.sleep(0.05)


def verify(args):
    manifest = read_json(args.runtime / "manifest.json")
    baseline = sample(args.runtime)
    samples = [baseline]
    deadline = time.monotonic() + args.seconds
    while time.monotonic() < deadline:
        time.sleep(min(5, max(0, deadline - time.monotonic())))
        current = sample(args.runtime)
        samples.append(current)
        print(json.dumps(current), flush=True)
        if not current["identical"]:
            raise RuntimeError("fleet hash/root divergence")
    advanced = samples[-1]["comparison_height"] - baseline["comparison_height"]
    blocks = [rpc(manifest["ports"]["http"], "eth_getBlockByNumber", [hex(number), False])
              for number in range(baseline["comparison_height"] + 1, samples[-1]["comparison_height"] + 1)]
    producers = {block["miner"].lower() for block in blocks}
    expected = {v["address"].lower() for v in read_json(args.runtime / "consensus.json")["initial_validators"]}
    result = dict(status="PASS" if advanced >= args.min_blocks and all(s["identical"] for s in samples) and producers == expected else "FAIL",
                  advanced=advanced, snapshot=manifest["snapshot"], samples=samples,
                  producers=sorted(producers), all_seven_produced=producers == expected,
                  scope="seven committed-QC RPCs; matching block/state/receipt/transaction roots, progress and all seven producers")
    write_json(args.runtime / "verification.json", result)
    write_json(args.runtime / f"verification-{int(time.time())}.json", result)
    if result["status"] != "PASS":
        raise RuntimeError("insufficient common committed progress or incomplete producer rotation")
    print(f"PASS: {advanced} common blocks, all seven roots/hashes agree")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("prepare")
    prep.add_argument("--source", type=Path, required=True)
    prep.add_argument("--leaf", type=Path, required=True)
    prep.add_argument("--validators", type=Path, required=True)
    prep.add_argument("--resume-prepare", action="store_true", help="resume a never-started incomplete preparation, checking every copied file")
    start_parser = sub.add_parser("start")
    start_parser.add_argument("--binary", type=Path, default=REPO / "target/release/n42-node")
    sub.add_parser("stop")
    sub.add_parser("status")
    check = sub.add_parser("verify")
    check.add_argument("--seconds", type=int, default=60)
    check.add_argument("--min-blocks", type=int, default=7)
    args = parser.parse_args()
    args.runtime = args.runtime.resolve()
    if args.command == "status":
        print(json.dumps(sample(args.runtime), indent=2))
    else:
        globals()[args.command](args)


if __name__ == "__main__":
    main()
