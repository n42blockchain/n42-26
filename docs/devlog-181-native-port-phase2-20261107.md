# Devlog 181 — Native-path port phase 2: frame ingest, Ed25519 flood, first 0x50 profile (2026-11-07)

Branch `feat/native-port` (worktree `.artifacts/reth-upgrade-281/native-port`), on top of
phase 1 (`0d148e0`, [devlog-180](devlog-180-native-port-phase1-20261107.md)).
Source of the ported design: n42-rs tag `fleet7-x22-5.2M-20261024` (read only).

## 1. Design decisions

- **Native frames on the existing ingest socket, not a new port.** The tx-ingest wire
  already distinguishes batches by the count word; bit 30 (`0x4000_0000`) now marks a
  native frame. Old clients never set it (batch caps are far below 2^30), so the batch
  path is unchanged and one listener serves both.
- **Whole-frame admission.** A frame is admitted or deferred as a unit. Partial admission
  would need per-tx acks and would break the gateway attestation, which signs the frame
  root, not single transactions.
- **Attestation first, verification as fallback.** `AdmittedFrame::authenticate` (phase 1
  `n42-tx-types/admission.rs`) accepts a frame when at least `N42_FRAME_ATTEST_MIN` known
  gateways (`N42_FRAME_GATEWAYS`) signed `keccak256("n42-frame-attest" || chain_id BE ||
  frame_root)`; otherwise it batch-verifies every Ed25519 signature. Unknown gateways are
  not an error, they only lose the shortcut.
- **Senders go into `AltSigSenderCache` at admission** so the payload builder and later
  lookups on this node do not verify again.
- **Profiling without perf.** The benchmark host has `perf_event_paranoid=4` and no sudo,
  so `perf` and `samply` are unavailable. The node gets an optional in-process pprof
  sampler (setitimer), armed by env and triggered by a file, so a capture can be taken
  mid-run without restarting. Rejected: `cargo flamegraph` (needs perf), tokio-console
  (no CPU attribution).
- **asm-keccak on by default** in `n42-node-bin`, matching n42-rs. The portable Keccak was
  the single hottest leaf (13-17% self on every node).

## 2. Implementation

### `crates/n42-node/src/ingest.rs`
- `IngestFrameHeader::Native(count)`, classified after the credit sentinel; a count of 0
  or above `MAX_INGEST_BATCH_TXS` is `Oversized`.
- Body: `count × (u32 LE len, EIP-2718 bytes)`, then `u8 n ≤ 16` and `n × (32-byte
  gateway key || 64-byte signature)`. Reply: the usual batch ack (accepted, pool pending).
- Room check before decoding: in credit-probe mode the leased credit must cover the
  frame; otherwise pending must be under the dynamic high-water mark. No room → ack 0,
  counted as deferred; the client retries.
- `admit_native_frame` runs authentication in `spawn_blocking` (bounded by
  `MAX_INGEST_BATCH_TXS/BYTES`), caches senders, wraps each tx as
  `N42PooledTransaction` from `Recovered::new_unchecked(AltSig(tx), sender)` and calls
  `add_prevalidated`.
- Metric `n42_ingest_native_frames_total{outcome=attested|verified|rejected|disabled}`.
- `run_ingest_server(pool, chain_id)`: the chain id now comes from the node's chainspec
  (it is part of the attestation message). Frames are refused when genesis has no
  `altSigTx`.
- Five new tests (13 ingest tests in total): header classification, attested frame pooled
  with pubkey-derived senders and cached, a bad signature rejects the whole frame, a
  foreign gateway falls back to verification, disabled genesis refuses.

### `bin/n42-node`
- `pprof_capture.rs` (feature `pprof`): `N42_PPROF_SECS=<n>` arms a watcher on
  `<datadir>/pprof.start`; touching it captures `n` seconds at `N42_PPROF_HZ` (499) and
  writes `pprof-<unix>.txt` (threads, top self, top inclusive) and `pprof-<unix>.folded`
  (`thread;root;...;leaf count`). Build with `--profile profiling` (symbols kept).
- Feature `asm-keccak = ["alloy-primitives/asm-keccak"]`, in `default`.

### `bin/n42-stress/src/bin/native-flood.rs`
- Deterministic sender keys `keccak256("n42-native-flood" || i BE)`; presigns all 0x50
  txs in parallel; a frame is (nonce, group of `--frame-size` senders).
- Group g is pinned to connection g mod C and walked in nonce order, so one sender's
  nonce chain never spreads over several nodes (the first round without this reached
  only 4.6k TPS: nonce gaps parked txs in the queued sub-pool on every node).
- `--gateway-seed` signs each frame root; without it frames go the verification path.
- Window pipelining per connection; accepted=0 → retry after 20 ms (up to 500 times).
- `--print-addresses` dumps the sender addresses for genesis funding.

### `scripts/testnet.sh`
- `N42_ALT_SIG_FUND_FILE`: fund every address listed in a file (20,000 flood senders).

## 3. Measurements

All on one host, 7 validators, 2 G gas limit (95,238 0x50 transfers per block at most),
20,000 senders × 80 nonces = 1.6 M attested transactions, 1,000 txs per frame, window 8.
TPS = committed txs / (timestamp of last busy block − timestamp of the block before the
first busy one); timestamps have 1 s resolution, so runs differ by a few thousand TPS.

| Run | Binary | Ingest | Committed | Notes |
|---|---|---|---|---|
| run1 | release | — | 4,651 TPS | frames round-robined across nodes; fixed by sticky groups |
| run2 | release | — | 36,364 TPS | 400k txs, 60k-tx blocks |
| run3 | release | — | 40,000 TPS | 800k txs, ~57k peak |
| run5 | profiling, soft keccak | 41.0k tx/s, 9,095 deferred frames | 1.6 M in blocks 33..53, 27 s → **59,259 TPS** | hash agreement OK |
| run6 | profiling, asm keccak | 43.2k tx/s, 8,492 deferred frames | 1.6 M in blocks 26..46, 31 s → 51,613 TPS | hash agreement OK |

In both run5 and run6 the full blocks (95,238 txs) arrive every 1.3-1.5 s, i.e. a steady
state of about 65-70k TPS. The tail blocks fall to 49,524 txs while the pool drains. The
run5/run6 difference comes from timestamp rounding and the length of that tail, not from
keccak. Raw outputs: `.artifacts/native-port-p0/run{5,6}-{flood.log,tps.txt}`.

Leader timing (run2, 60k-tx block): build 976 ms (~16 µs/tx); proposal 1,453 ms after the
slot start; about 1.65 s between blocks. Follower `new_payload` takes 560-600 ms (state
root about 190 ms) and stays off the critical path because followers vote optimistically.

### Profile (10 s at 499 Hz, run5 v0/v3, run6 v0/v3; about 15 cores busy per node)

| Subsystem | Share | What it is |
|---|---|---|
| reth `spawn_tx_iterator` (import) | 15-25% | Followers convert the block's txs in a rayon `par_iter`; a 0x50 tx misses `AltSigSenderCache` on a follower and verifies Ed25519 one at a time. Most samples are crossbeam epoch/steal spinning (atomics) around tiny tasks, the rest curve25519. |
| `mobile_packet_loop` / `generate_and_broadcast_v2` | 16-21% | Mobile verification packets, built on every node each block: rocksdb reads, `AccountBeforeTx` decoding, zstd. |
| network / consensus | 18-20% | 6-7% is `serde_json`: `exec_cache::inject_compact_block` (~6%) and `extract_state_diff_from_execution_output` (~3%) still JSON-encode the execution output. Gossipsub `message_id_parts` hashes whole messages (~2%). |
| idle spin in `cpu`/rayon/tokio pools | 10-17% | Mostly atomics: workers spinning for work. |
| state root (reth trie + QMDB) | 6-10% | `RlpNode::from_rlp` keccak is the hottest part (~7% after asm). |
| persistence | 5-7% | rocksdb writes. |
| payload build (leader only) | ~5% on the leader | |
| block execution | ~3% | |

Keccak self time: soft backend 13.4% (v0) / 17.2% (v3); asm 11.7% / 14.4%. With asm
almost all of it is in `KECCAK_ASM_SHA3_absorb`; callers: trie node RLP 6.8%, 0x50
`decode_payload` tx hash 2.4%, gossipsub message id 1.9%, block-announcement dedup 0.8%,
overlay hashed-address lookups 0.9%. The remaining cost is real hashing volume, not the
backend.

## 4. Problems and fixes

- `perf` blocked (paranoid 4, no sudo), `samply` absent and also needs perf events → built
  the in-process pprof capture. The release profile strips symbols → `--profile profiling`.
- First flood (4.6k TPS): nonce chains scattered across nodes → sticky sender groups.
- Test imports: `reth_transaction_pool::pool::AddedTransactionState`, `alloy_primitives::B256`.
- `cargo fmt` reordered imports in some phase-1 files (`el/reth_engine.rs`, `eof_guard.rs`,
  `native_payload.rs`, `parallel_payload.rs`, `pool.rs`, `tx_bridge.rs`); kept.
- The "Receipt root task dropped sender" error log on payload-cache hits is pre-existing
  (the N42 reth patch drops the receipt-root sender on that branch); unrelated to the port.

## 5. Status

- [x] Native frame wire + admission (attested and verified), genesis gating, metrics, tests
- [x] `native-flood` load generator, file-based genesis funding
- [x] 7-node 0x50 run: 1.6 M attested txs committed, hashes agree; ~59k TPS overall, ~65-70k steady
- [x] In-process profiler and first attribution
- [x] asm-keccak default on
- cargo check / clippy `-D warnings` / fmt clean for the touched crates

## 6. Next (phase 3 candidates, by expected gain)

1. **Followers should not verify 0x50 one by one.** Carry the frame attestation (or a
   block-level sender commitment) with the block so `spawn_tx_iterator` gets senders from
   the cache/attestation, and give the iterator coarser chunks to stop the rayon spinning.
   About 20% of each follower.
2. **Mobile packets off the hot path:** build once per block on one node (or lazily per
   subscriber), and skip when no phones are attached. 16-21% of every node.
3. **Drop JSON from compact-block injection and state-diff extraction** (binary codec). ~9%.
4. **Leader build at ~16 µs/tx** to bring a 95k block well under 1 s; then raise the gas
   limit, since blocks are already full.
5. Hash gossipsub message ids over a header, not the whole payload.
