# Devlog 183 — Native-path port phase 3b: 0x50 fleet on H2-v4/QMDB and the leader critical path (2026-11-08)

Branch `feat/native-port`, on top of phase 3a (`515dd9b`,
[devlog-182](devlog-182-native-port-phase3a-20261107.md)). This is step 1 of the plan in
devlog-182 section 6: run the 0x50 load on the QMDB route instead of the MPT testnet, then
cut the cycle there.

Bench: 7 validators on one host, chain 941050, `fresh-native-h2-qmdb-cancun` profile
(`n42-native-fleet --alt-sig`), 2 G gas limit, 95,238 transfers per full block, 20,000
Ed25519 senders × 80 attested frames = 1.6 M 0x50 transfers, frames to one node per sender
(`native-flood` without `--broadcast`). Fleet env: `--fast-transfers --parallel-build
--parallel-import`, `N42_FRAME_GATEWAYS`, `N42_FRAME_ATTEST_MIN=1`. Scratch: 
`.artifacts/native-port-p0/` (`qf7*`, `run1{2..6}-*`, `run-qround.sh`, `wall-tps.py`).

## 1. Design decisions

- **0x50 on the QMDB fleet, not on `testnet.sh`.** The performance route is specified on
  H2-v4 participant mode with QMDB execution (docs/04 section 3). `n42-native-fleet` gets
  `--alt-sig` (genesis `altSigTx`) and `--alt-sig-fund <file>`; `chain94-fleet.py start`
  gets `--env N42_*=…` for per-run node settings.
- **Gov5 block codec over `N42TxEnvelope`.** H2 normalization and the Gov5 wire codec were
  typed to `TxEnvelope`; the first 0x50 transaction stalled the chain ("refusing to propose
  a payload that cannot be normalized ... unexpected tx type"). The codec, finalized-range
  sync and replay import now carry `N42TxEnvelope`.
- **No MPT state root in the H2 builder.** In H2-v4 participant mode every built payload is
  normalized to the Gov5 header profile, which writes the QMDB root over the builder's
  state root. The builder computed a synchronous MPT root (~0.5-0.7 s per full block) that
  was always thrown away. `n42_node::payload::set_builder_state_root_replaced` is set from
  `N42_GOV5_H2_PARTICIPANT` before launch and turns on `skip_state_root`. The original
  built block is still inserted into reth's tree with the parent's root and no trie
  updates; it is never made canonical, because only the normalized hash is proposed.
- **Header-only native normalization.** Normalization changes header fields only (roots,
  extra data, withdrawals root, requests hash). It decoded and re-encoded all 95k
  transactions and recomputed the transaction root. The native path now builds a raw
  `Block<Bytes>` with the builder's cached transaction root and patches the payload in
  place. The early devnet profile is unchanged.
- **Transaction root at most once per validation.** `N42EngineValidator` computed the root
  to get the standard header hash, then reth's `ensure_well_formed_payload` computed it
  again. The root is now computed once (or taken from the cache for the native hash) and
  lent to the upstream validator under the standard hash. The cache has a single slot, so
  the native hash's entry is put back afterwards; a concurrent overwrite only costs a
  recomputation, never a wrong root (lookups are by hash, and both hashes are still
  checked against rebuilt headers).
- **Native block data before the Gov5 copy.** The leader encoded the Gov5 block (~130 ms)
  before sending native block data. The order is swapped; the proposal is still released
  only after the Gov5 broadcast succeeded.
- **Leader validation overlapped with the transfer.** The leader released the proposal only
  after `new_payload(Valid)` of its normalized block (~0.5 s), and sent the block after
  that. Validation now runs in a task started before serialization; the block goes out at
  once and the proposal waits for the task. Followers validate before they vote
  (import-gated), so an invalid block still cannot collect votes; an invalid leader block
  costs followers wasted work, as it costs a timeout today.
- **Duplicate Gov5 copy dropped before decoding.** A follower already holding native block
  data for a hash checks the header hash (`gov5_attributable_hash`) before decoding the
  Gov5 copy. A requested body keeps the full path.

## 2. Implementation

- `bin/n42-node/src/native_fleet_main.rs`: `--alt-sig`, `--alt-sig-fund`; manifest
  `alt_sig_tx`, `alt_sig_funded`; test `alt_sig_fleet_sets_the_flag_and_funds_listed_senders`.
- `scripts/chain94-fleet.py`: `start --env N42_NAME=VALUE` (repeatable, `N42_` prefix,
  recorded as `extra_env`).
- `n42-network`: `gov5_block.rs`, `finalized_range.rs` retyped to `N42TxEnvelope`;
  `normalize_execution_payload_for_gov5_h2_with_transactions_root`; re-export
  `gov5_attributable_hash`. Tests: `alt_sig_transaction_round_trips_through_gov5_block_wire`,
  `native_normalization_edits_only_the_header_and_matches_a_full_rebuild` (new path equals
  the old decode-and-rebuild result, with and without a cached root).
- `n42-consensus-service`: `ExecutionOutputCache::built_transactions_root` (default
  `None`); `execution_bridge.rs` `handle_built_payload` reordered (spawned leader
  validation, native broadcast, Gov5 encode, await validation, `BlockReady`);
  `orchestrator/mod.rs` early duplicate check in `handle_h2_v4_gov5_block` plus the later
  one in `process_h2_v4_gov5_block`; `replay_import.rs` retyped.
- `n42-node`: `payload.rs` builder flag; `exec_cache.rs` `built_transactions_root`;
  `engine_validator.rs` single transaction-root computation.
- `bin/n42-node/src/main.rs`: sets the builder flag from `N42_GOV5_H2_PARTICIPANT`.

Workspace clippy `-D warnings` clean; n42-network (224), n42-consensus-service (249),
n42-node (278 + integration) tests pass.

## 3. Measurements

TPS by wall clock: committed transactions over the time from the commit of the block before
the first loaded block to the commit of the last loaded block, in node0's log
(`wall-tps.py`). Block timestamps run ahead of the wall clock (each block gets parent+1 s),
so the timestamp window overstates TPS once the cycle exceeds 1 s; run16's timestamp figure
was 69.6k.

| Run | Change | Committed | Full-block cycle |
|---|---|---|---|
| run12/13 | QMDB fleet, 0x50, codec retyped | 30.8k / 30.2k (timestamps) | ~3.2 s |
| run14 | + early duplicate Gov5 skip (after decode) | 28.1k (timestamps) | ~3 s |
| run15 | + no builder MPT root | 31.9k (timestamps) | ~2.87 s |
| run16 | + header-only normalization, one tx root, native first, overlapped validation, dup skip before decode | **46.4k wall clock** (49.2k over full blocks 25..33) | ~1.94 s |

All runs: identical hashes and roots on all 7 nodes (`chain94-fleet.py verify`).

### Leader of view 30, run16

| Step | Time |
|---|---|
| slot wait after commit | ~150 ms |
| build (packing + assembly, no MPT root) | 437 ms (was ~1.1 s with the MPT root) |
| normalization + serialization | 195 ms (gap was ~350 ms) |
| block data reaches followers | ~190 ms after ready (was 430-480 ms) |
| leader validation (parallel) | 507 ms |
| follower `new_payload` (full re-execution, Ed25519 verification, QMDB root) | 950-980 ms |
| votes + commit | ~50 ms |

## 4. Problems and fixes

- First 0x50 QMDB fleet stalled: Gov5 codec typed to `TxEnvelope` → retyped.
- `run-qround.sh` measured before the load committed (flooding takes 2 s) → waits for
  empty pending pools.
- The timestamp-window TPS overstated run16 → `wall-tps.py`, appended to `run-qround.sh`.
- `ExecutionPayloadSidecar` has no `PartialEq`; the equivalence test compares its Cancun
  and Prague parts.

## 5. Status

- [x] 0x50 load on the H2-v4/QMDB fleet with hash agreement
- [x] Builder MPT root skipped in H2 mode
- [x] Normalization header-only; transaction root once per validation
- [x] Native block data before Gov5 encoding; leader validation overlapped
- [x] Duplicate Gov5 copy dropped before decoding
- Cycle 2.9-3.2 s → 1.94 s; 46.4k TPS by wall clock

## 6. Next

1. **Follower verification (decision needed).** Follower `new_payload` is now ~half of the
   cycle: full re-execution of 95k transfers with cold Ed25519 sender recovery, because
   each frame reaches one node. Options: frames to every node (gateway fan-out, sender
   cache warm everywhere, no consensus change, ~2× ingress bandwidth) or n42-rs
   frame-aligned blocks (a block names attested frames; followers assemble by reference;
   a consensus rule).
2. **Leader pipeline and timestamps.** The next leader waits for commit, then the slot
   boundary (~150 ms), then builds. Building on the parent's execution output before the
   commit, and fixing timestamps that run ahead of the wall clock, are the next cycle cuts.
3. Remaining duplicate decodes: the network service decodes every Gov5 copy for its served
   cache, and the leader's Gov5 encoder decodes every transaction to check canonical form.
