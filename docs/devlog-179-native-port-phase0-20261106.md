# Devlog 179: native-path port, phase 0 (difference list)

Date: 2026-11-06. Scope: source comparison only, with no build, test or fleet run.

## Decisions (user, 2026-11-06)

- **Port base:** n42-rs tag `fleet7-x22-5.2M-20261024` (`47b8180ab`).
  - It uses reth v2.8.1 and revm 43.0.3, the same versions as n42-26 main.
  - It is the code of the X22 5.2M rounds (BREAKTHROUGH_DESIGN 10.227-10.230).
  - n42-rs main is not the base. It is on reth v2.10.0 and moved from revm to evm2 in its 2.9 upgrade; `fast_transfer.rs` and `parallel_transfer.rs` are gone there.
  - The reth 2.10 upgrade waits until the native path runs.
- **Direction:** port into n42-26 in stages and keep n42-26's own structure (mobile, zk, jmt, its QMDB store). Do not rebase onto n42-rs.

Phases:
- 0: this list.
- 1: native types end to end.
- 2: tx ingest, gateway attestation, the Ed25519 flood and the first 7-node profile.
- 3+: queue and frame blocks, direct transfer, one execution per machine, output shards, X22 tuning. Ordered by the profile.

## 1. reth: n42-rs forked crates vs n42-26 patches

n42-rs keeps modified copies of 18 reth crates in its tree, plus two vendored ones. n42-26 uses the adapted checkout with six patches (`reth-source.lock`).

Diff of the tag's copies against upstream v2.8.1 (`d763342f`), in changed `+/-` lines:

| Crate | Lines | What changed | Needed by phase |
| --- | ---: | --- | --- |
| chainspec | 1942 | genesis files; `qmdb.rs`: `alt_sig_tx_enabled`, `frame_blocks_enabled`, deferred-execution keys, QMDB genesis root | 1-2: only the genesis-flag helpers. They are pure functions of `Genesis` extra fields, so they go into `n42-chainspec` with no reth change |
| storage/provider | 1724 | n42 persistence, static-file cache, account history, overlay filter, metrics | 3+ (persistence tuning) |
| storage/storage-overlay | 410 | overlay builder/manager | 3+ |
| node/builder | 378 | `executed_inserts.rs` (hand an already-executed block to the engine); sender-cache sizing; tree config | 3 (own-build reuse). n42-26 already has `sender_recovery_cache` (`crates/node/builder/src/rpc.rs:1360` in the adapted checkout) |
| chain-state | 141 | `canon_lag.rs`, lean canonical notifications | 3+ |
| node/core, ethereum/node, consensus, db, db-api, storage-api, network*, peers, hardforks, cli, revm, bin/reth | 4-149 each | args, n42 state read hook, small API exposure | checked when a phase needs one |
| transaction-pool (vendored) | 117 | Cargo packaging only; no source change besides license files | none |
| nippy-jar (vendored) | 154 | packaging and small fixes | none |

Result: phases 1 and 2 need **no new reth fork**. The pool's custom 0x50 type (`with_custom_tx_type`) is upstream API, and the sender-recovery cache already exists in n42-26's patched builder. `executed_inserts` and the provider/overlay changes come with phase 3.

## 2. Header

- At the tag, `N42Primitives::BlockHeader = alloy_consensus::Header` (`crates/n42/tx-types/src/primitives.rs`). This matches n42-26's port.
- `N42Header` (Ethereum header plus `mobileRegistryRoot`) only appears later on n42-rs main (`crates/n42/header`). It is not needed at this base.
- n42-26 carries `mobile_registry_root` in its gov5 wire type (`crates/n42-network/src/gov5_block.rs:23`). That stays as it is.

## 3. State root

- **n42-26:** QMDB bin root from its own store: `Gov5QmdbStateRootStore` → `QmdbLeafTree` (`crates/n42-twig-core/src/qmdb_leaf_tree.rs:239`), installed as engine-tree root strategies (`crates/n42-node/src/qmdb_state_root.rs`).
- **n42-rs:** the same commitment through `qmdb-state` / `qmdb-reth`.
- The two implementations already agree on chain: the 7-node mixed 1 h soak passed on v2.7.0 binaries (docs/04 §2).
- Consequence: n42-rs pieces wired to its QMDB crates (`built_executions`, `chain_alias`, output shards feeding the root) must be adapted to n42-26's store in phase 3. Phases 1 and 2 only need n42-26's root strategy made generic over the node primitives. It is typed to `EthPrimitives` today.

## 4. tx-types sync

- `crates/n42-tx-types` was ported from n42-rs `52a1df393`, which is 421 commits before the tag.
- Against the tag's `crates/n42/tx-types`: 442 changed lines.
  - `alt_sig`, `compact`, `envelope`, `frame` and `lib` differ.
  - The tag has `evm.rs`, `rpc.rs` and `sender_cache.rs`.
  - n42-26 has its own `admission.rs`, `execution.rs` and `key_cache.rs`.
- Phase 1 starts by bringing these files to the tag and keeping n42-26's additions where they are still used.

## 5. n42-26 code typed to Ethereum primitives (phase 1 work list)

From the survey of the node (file:line in the current tree):

- **Node types:** `crates/n42-node/src/node.rs:92-97, 106-145` and `components.rs:24, 96`.
- **Pool:**
  - `pool.rs:47-72`.
  - The trusted ingest (`ingest.rs:121-163, 842-852`) and `tx_bridge.rs` are typed to `EthPooledTransaction` / `TransactionSigned`.
- **Payload:**
  - `payload.rs:404-415, 486-498, 624`.
  - `parallel_payload.rs`: `BasicBlockBuilder<.., EthPrimitives>`, `default_ethereum_payload`.
- **Execution:**
  - `n42-execution/src/evm_config.rs:19-34, 92-131, 184-202`.
  - `parallel_block.rs:75-226`: `commit_transfer_batch` needs `TransactionSigned` and the Ethereum `TxType`.
  - `executor.rs`.
- **Engine:** `engine_validator.rs:65-140` (`try_into_block::<TransactionSigned>`), `el/reth_engine.rs`, and `n42-consensus-service/src/el.rs:183`.
- **State root:** `qmdb_state_root.rs:2366-2640` (`StateRootStrategy<EthPrimitives, ..>`).
- **Wire and caches:**
  - `exec_cache.rs`.
  - The compact-block receipts in the orchestrator (`mod.rs:524`).
  - `n42-network` `compact_receipts.rs`, `gov5_block.rs`, `finalized_range.rs`.
  - `n42-consensus` `receipt.rs`, `adapter.rs:331-390`.
- **Tools:** `n42-stress`, the N42T presign format and `native_fleet_main` only sign secp256k1. Phase 2 needs an Ed25519 / 0x50 load source; the reference is n42-rs `crates/n42/h2-node/examples/tx_flood.rs` `--alg ed25519 --ingest --gateway-key`.

The direct-transfer executor already takes a revm `TxEnv`. `AdmittedTransaction::execution_env()` passes its shape gate, so execution itself needs no change for 0x50 (it runs as type 2).

## 6. Phase 1 acceptance

- 7 validators commit blocks that hold 0x50 transactions submitted over RPC.
- Receipts are readable with type 0x50.
- QMDB roots agree on every node.
- The workspace passes `cargo clippy --all-targets -- -D warnings` and the existing tests.
- Ethereum-profile behaviour is unchanged when the genesis does not enable `altSigTx`.

## Status

Phase 0 done (source comparison). No build or run. Scratch exports are in `.artifacts/native-port-p0/` (ignored). Next: phase 1, starting with the tx-types sync and the node-type switch, on a branch off main. Builds and tests wait for the box queue.
