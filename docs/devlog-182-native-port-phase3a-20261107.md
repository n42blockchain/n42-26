# Devlog 182 — Native-path port phase 3a: per-node CPU cuts and where the block cycle goes (2026-11-07)

Branch `feat/native-port`, on top of phase 2 (`614ca5e`,
[devlog-181](devlog-181-native-port-phase2-20261107.md)). Same bench as phase 2: 7 validators
on one host, 2 G gas limit, 20,000 Ed25519 senders × 80 nonces = 1.6 M attested 0x50 transfers.
Scratch scripts and raw outputs: `.artifacts/native-port-p0/` (`run{7..10}-*`, `tn7{b,c,p,r}/`).

## 1. Design decisions

- **Follower verification: measured, not yet changed.** n42-rs delivers every frame to every
  node, so each node's `AltSigSenderCache` is warm when the block arrives. `native-flood
  --broadcast` reproduces that without node changes (run7). Whether to keep that model (a
  gateway fans frames out, no consensus change) or port n42-rs frame-aligned blocks (a block
  names frames; consensus rule) is open; see section 6.
- **Mobile packets only when a phone is attached.** The packet costs a state replay, codec and
  zstd on every node and every block. With no session on this node's StarHub it is skipped.
  A phone that joins later starts from the next block, and its cache sync still carries the
  codes sent so far. Alternative considered: build once per block on one node and relay.
  Rejected for now because it changes who serves phones.
- **The state diff only when a JMT/Twig sidecar exists.** Leader and followers decoded the
  compact execution output a second time on every block to derive a `StateDiff` that only
  the sidecars read. When no sidecar is configured, as on the authoritative-QMDB route, the
  decode is skipped.
- **Compact execution output as MessagePack.** JSON wrote every hash, balance and address as
  hex. The new form is a version byte `0x01` and then positional MessagePack. Receipts get a
  small wire struct of their own, because the receipt's serde quantity encoding does not read
  back in a binary format. The decoder still accepts JSON (it starts with `{`), so a node
  reads blocks from an older leader; an older follower cannot read a new leader's blob, so
  every node of a network must be upgraded together. Alternatives: bincode (cannot read
  alloy's flattened `Log`), named MessagePack (larger and slower than positional).
- **0x50 in the parallel transfer prefix.** A 0x50 transaction executes as an EIP-1559 call
  from its key's address (`n42_tx_types::execution`), and receipts take their type from the
  envelope. So a plain 0x50 transfer is eligible for `execute_for_build` like types 0-2.
  Before this, the first 0x50 transaction ended the prefix and the whole block went serial.

## 2. Implementation

- `n42-network` `StarHubHandle::active_sessions` (shares the hub's session table) and
  `ShardedStarHubHandle::active_sessions` (sum over shards). `mobile_packet_loop` checks it
  before each block and counts skips in `n42_mobile_packet_skipped_no_phones_total`.
- `n42-consensus-service` `execution_bridge.rs`: `state_diff_ready_tx` is
  `Option<Sender>`, `Some` only with `jmt` or `twig`; the decode task runs only then.
- `n42-consensus-service` `orchestrator/mod.rs`: `CompactBlockExecution::{encode, decode}`,
  `CompactExecutionWire`, `ReceiptWire`. Call sites: `exec_cache.rs` (serialize, inject) and
  `consensus_loop.rs` (`extract_state_diff_from_execution_output`). The ZK path hashes the
  decompressed bytes as an opaque blob, so it is unaffected. New dependency: `rmp-serde 1.3`.
- `n42-node` `parallel_payload.rs`: plain = `tx_type <= 2 || tx_type == 0x50`, plus the
  existing checks (no input, no access list, not to/from the beneficiary).
- `native-flood --broadcast`: every endpoint gets every frame; the `--connections` of one
  endpoint split the sender groups between them; the accepted count is reported per node.

Tests: codec round trip (MessagePack and legacy JSON, a bundle with storage, a contract,
receipts with logs, both receipt types), plus an ignored speed test. n42-consensus-service,
n42-node and n42-network test suites pass; clippy `-D warnings` is clean; E2E 5, 8 and 12
(mobile verification, mobile EVM, Blockscout RPC) pass 3/3.

Codec on one 95,238-transfer block (release, `compact_codec_speed`):

| | JSON | MessagePack |
|---|---|---|
| size (uncompressed) | 48.8 MB | 21.1 MB |
| encode | 57-62 ms | 20 ms |
| decode | 107 ms | 46 ms |

## 3. Measurements

| Run | Change | Committed | Notes |
|---|---|---|---|
| run6 | phase 2 baseline (asm keccak) | 51.6k TPS | full blocks every 1.3-1.5 s |
| run7 | `--broadcast` | 43.2k TPS | follower import 15-25% → ~7% CPU; ingest +6-8%; one host now admits every frame 7 times, so supply limits it (32k tx/s per node) |
| run8 | mobile skip, state-diff gate, MessagePack | 50.0k TPS | node CPU −20% (samples 7.3k → 5.5-6.0k per 10 s); mobile packets 0% |
| run9 | + `N42_PARALLEL_BUILD/FAST_TRANSFER/PARALLEL_IMPORT=1` | 30.2k TPS | testnet default `N42_MAX_TXS_PER_BLOCK=48000` caps blocks; cycle still ~1.5 s |
| run10 | + `N42_MAX_TXS_PER_BLOCK=100000` | 61.5k TPS over blocks 26..41 | packing of 95k transfers 140-320 ms (was ~900 ms serial); whole build still 800-1,000 ms |

Every full-block run keeps a 1.4-1.5 s cycle. The CPU savings and the faster packing did not
move it, so the cycle is bound by latency, not by node CPU.

### Where a cycle goes (run9 leader of view 50, 48k transfers; run10 for 95k)

| Step | Time |
|---|---|
| wait for the slot boundary | 50 ms |
| the new leader imports the parent (`new_payload`, compact cache hit; MPT state root ~280 ms inside) | ~550 ms |
| FCU rejected for timestamp (block timestamps run ~27 s ahead of the wall clock), retry, second slot wait | ~100 ms |
| packing (parallel prefix) | 69 ms (48k) / 140-320 ms (95k) |
| builder finish: synchronous MPT `StateRoot` over the overlay DB trie, block assembly | ~400 ms (48k) / ~480 ms (95k) |
| serialize + compress + send | ~80 ms |
| two vote rounds | ~120 ms |

The leader rotates every view, so each new leader first imports the previous leader's block
and then computes a full state root for its own. Both roots are MPT in this testnet; the
performance route is specified on QMDB (docs/04, section 3), which needs the H2-v4 participant
mode with the Gov5 header profile and a bootstrap bundle (`scripts/chain94-fleet.py` flow).
`scripts/testnet.sh` does not set that up.

## 4. Problems and fixes

- MessagePack failed on receipts ("invalid length 1, expected 64 bits"): their serde quantity
  encoding does not round-trip in a binary format → `ReceiptWire`.
- `cargo fmt` / `rustfmt` on `orchestrator/mod.rs` reformats its submodules (pre-existing
  unformatted code in `consensus_loop.rs`, `star_hub.rs`); those hunks were reverted so the
  diff holds only this change.
- `pkill -f <pattern>` matched the shell running it; the testnet is stopped by its exact pid.

## 5. Status

- [x] Follower verification cost measured with frames to all nodes (`--broadcast`)
- [x] Mobile packets skipped without phones; state-diff decode only with a sidecar
- [x] Compact execution output in MessagePack, JSON still read
- [x] 0x50 in the parallel transfer prefix
- [x] Cycle breakdown: parent import + MPT state roots dominate, not CPU

## 6. Next (decisions needed)

1. **0x50 testnet on QMDB.** Bring the 7-node 0x50 testnet up in H2-v4 participant mode with
   QMDB execution, so both roots in the cycle are QMDB (n42-rs: 45-51 ms). This is the route's
   documented base and the largest single term.
2. **Leader pipeline.** Let the next leader start packing on the parent's execution output
   (it has the compact output before its own `new_payload` finishes) instead of waiting for
   the import, and fix the timestamp drift that costs an FCU retry.
3. **Follower verification.** Either frames to every node (gateway fan-out, no consensus
   change, ~2× bandwidth) or n42-rs frame-aligned blocks (a block names frames; followers
   assemble by reference; a consensus rule shared with Gov5).
4. Raise `N42_MAX_TXS_PER_BLOCK` in the 0x50 profile of `testnet.sh` (48,000 caps a 2 G block
   at half).
