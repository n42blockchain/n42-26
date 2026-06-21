# Caplin Stage 6 — clean extraction of `n42-consensus-service` (ZERO reth dep)

Branch: `feat/caplin-cl-stage3-6`. Stages 3-5 done (sink ports, network port, rename
`ConsensusOrchestrator`→`ConsensusService` + alias), all committed + unit-test green
(213 unit + 6 integ). Reth baseline `../reth` @ 449ecfdce.

User decision: **完全纯净抽取** — the new `n42-consensus-service` crate must have **zero
reth dependency** (no reth-node-builder, no reth_evm/reth_payload_*/reth_transaction_pool/
reth_execution_types/reth_ethereum_primitives). All reth concrete types stay in `n42-node`
behind ports/adapters. Real-machine E2E deferred (datc occupies the box) — correctness
rests on behavior-identical refactor + compile + unit tests.

## The full reth coupling surface in `crates/n42-node/src/orchestrator/` (target = ZERO)

Excludes `observer.rs` (ObserverOrchestrator is a separate type, stage 7 — it must NOT
move into the service crate; the `orchestrator` module gets split so observer stays in
n42-node).

| Coupling | Sites | Neutralization |
|----------|-------|----------------|
| `el/` trait returns `EthBuiltPayload` + `PayloadKind`/`PayloadId` | el/mod.rs, execution_bridge `handle_built_payload`/`broadcast_blob_sidecars`/`resolve_payload` call | **BuiltBlock** neutral struct + neutral resolve-kind; adapter converts |
| `DiskFileBlobStore` + `BlobStore` + `BlobTransactionSidecarVariant` RLP | execution_bridge `handle_blob_sidecar`/`broadcast_blob_sidecars`, mod.rs field+builder | **BlobStorePort** (byte-oriented: `insert_rlp`/`get_all_encoded`); adapter does RLP + DiskFileBlobStore |
| `reth_evm::payload_cache` + `BlockExecutionOutput`/`BlockExecutionResult` + `CachedPayloadData` (compact-block execution output cache) | execution_bridge `take_and_serialize_execution_output` (~36/87/158/1095) | **ExecutionOutputCache port**: `take_serialized(hash)->Option<Vec<u8>>` + `store(...)`; all reth_evm/BlockExecutionOutput in node adapter |
| `reth_evm::n42_defer_state_root()` | consensus_loop.rs:736 | read in node, pass as `bool` (config field) |
| `reth_ethereum_primitives::Receipt` in `CommittedBlock.receipts` + CachedPayloadData | mod.rs:105 | investigate: if orchestrator never reads receipts at runtime, drop from the service-side struct or carry as opaque; keep reth Receipt node-side |
| `with_engine_api(ConsensusEngineHandle, PayloadBuilderHandle, …)` reth-handle constructor | mod.rs:826-… | **remove from service**; node (main.rs) builds `RethExecutionLayer` + adapters and calls a new `ConsensusService` ctor taking `Arc<dyn ExecutionLayer>` + the other port objects |
| `EthEngineTypes` / `PayloadTypes::block_to_payload` | execution_bridge handle_built_payload | moves into the el adapter (BuiltBlock.execution_data is `alloy_rpc_types_engine::ExecutionData`, already neutral) |

`ExecutionData`, `B256`, `PayloadId` (alloy_rpc_types_engine), `ForkchoiceState`,
`PayloadStatus`, `PayloadAttributes` are **alloy**, not reth — they stay in the trait.

## Port designs

```rust
// el/mod.rs (trait → service crate; adapter reth_engine.rs → node)
pub struct BuiltBlock {
    pub hash: B256, pub number: u64, pub timestamp: u64, pub tx_count: usize,
    pub execution_data: alloy_rpc_types_engine::ExecutionData, // neutral, for re-import
    pub blob_tx_hashes: Vec<B256>,                              // eip4844 tx hashes
}
// ExecutionLayer::resolve_payload(..) -> Option<Result<BuiltBlock, ElError>>
// (adapter: block_to_payload + extract blob hashes/hash/number/timestamp/tx_count)

pub trait BlobStorePort: Send + Sync {
    fn insert_rlp(&self, tx_hash: B256, sidecar_rlp: &[u8]) -> Result<(), String>;
    fn get_all_encoded(&self, tx_hashes: Vec<B256>) -> Vec<(B256, Vec<u8>)>;
}

pub trait ExecutionOutputCache: Send + Sync {
    /// Take + serialize the cached compact-block execution output for `hash`.
    fn take_serialized(&self, hash: B256) -> Option<Vec<u8>>;
    // store(...) side stays where leader execution happens; investigate the store path.
}
```

## Sub-step sequence (each: behavior-identical, `cargo check -p n42-node -p n42-node-bin`
+ `clippy -p n42-node -D warnings` + `cargo test -p n42-node` green, then one commit)

- **6a-1**: el-trait `BuiltBlock` neutralization + remove `with_engine_api` reth ctor →
  add `ConsensusService::with_execution_layer(consensus_state, el, …)` taking
  `Arc<dyn ExecutionLayer>`; node (main.rs) builds `RethExecutionLayer`. handle_built_payload
  consumes BuiltBlock.
- **6a-2**: `BlobStorePort` + `ExecutionOutputCache` port + `defer_state_root` bool +
  `Receipt` neutralization + `TxImportBatch`→local `Vec<Vec<u8>>` alias. **Acceptance:
  `git grep -nE 'reth_|EthEngineTypes|EthBuiltPayload|DiskFileBlobStore' crates/n42-node/src/orchestrator/{mod,consensus_loop,execution_bridge,state_mgmt,view_jump_throttle}.rs` returns ZERO.**
- **6b**: split the module — move observer.rs OUT to `n42-node/src/observer/` (stays in
  node); the ConsensusService files become the body of the new crate.
- **6c**: create `crates/n42-consensus-service`; move ConsensusService files + the port
  traits (el/mod.rs trait part, net_port.rs trait, sinks/mod.rs traits) + clean supporting
  types (`SharedConsensusState`, `PoolDepthSnapshot`, `EpochSchedule`) into it. Crate deps:
  n42-primitives/chainspec/consensus/jmt + alloy + libp2p(PeerId only) + tokio + metrics —
  **NO reth-\***. Assert via `cargo tree -p n42-consensus-service | grep reth` = empty.
- **6d**: n42-node keeps adapters (`RethExecutionLayer`, `impl ConsensusNetwork for
  NetworkHandle`, sink adapters, blob/exec-output adapters) + wiring; re-export
  `ConsensusService` + `ConsensusOrchestrator` alias from n42-node. Update main.rs wiring.
  Full workspace `cargo check --all-targets` + `cargo test -p n42-node -p n42-consensus-service`.

## Guardrails (every sub-step)
No Cargo.lock drift beyond intended additions; no dep downgrades; do NOT touch
`crates/n42-consensus`, wire formats (BlobSidecarBroadcast bincode, CompactBlock json,
BlockDataBroadcast), the biased `select!` ordering, or observer runtime behavior. Commit
template: `GIT_COMMITTER_NAME="Nyxen" GIT_COMMITTER_EMAIL="40690755+MiraWells@users.noreply.github.com" git commit --author="Nyxen <40690755+MiraWells@users.noreply.github.com>" -m "…"` — NO "Claude" strings. Do NOT push.

## Risk
Hottest paths touched (payload resolve/build, eager import, compact-block cache, blob
broadcast). No E2E possible now → after datc frees the box, run scenario 1/3/4 (single +
ERC-20 + multi-node consensus) and assert `n42_eager_import_hits_total` /
`n42_fcu_latency_ms` / blob propagation unchanged before merging to main.
