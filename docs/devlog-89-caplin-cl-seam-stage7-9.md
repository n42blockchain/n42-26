# Devlog 89: Caplin EL-seam refactor — stages 7-9 (observer fold, async finalize, standalone)

Date: 2026-06-21
Branch: `feat/caplin-cl-stage3-6` (continues devlog-88)
Status: code + unit tests complete; **real-machine E2E + the stage-8 A/B deferred** (datc
occupied the box).

Builds on devlog-88 (stages 3-6: ports-and-adapters + `n42-consensus-service` crate).

## Stage 7 — fold `ObserverOrchestrator` onto the ports, move into the crate (`73d4f5f`)

The observer (a second EL consumer that pre-executes blocks via `new_payload` without
promoting them to canonical head) was already on `el: Arc<dyn ExecutionLayer>` for
new_payload/fork_choice_updated. Routed its last 3 reth couplings through the existing
ports and moved it into the crate:
- constructor `beacon_engine: ConsensusEngineHandle` → `el: Arc<dyn ExecutionLayer>` (node
  builds `RethExecutionLayer::engine_only`);
- blob `DiskFileBlobStore` → `Arc<dyn BlobStorePort>` (`insert_rlp`);
- compact inject `crate::exec_cache::inject_compact_block` → `Arc<dyn ExecutionOutputCache>`.

`git mv` observer.rs → `crates/n42-consensus-service/src/observer.rs`; n42-node re-exports
`ObserverOrchestrator`. observer is now hard-reth-free. Verified: workspace
`cargo check --all-targets` EXIT 0, `n42-node 84 + n42-consensus-service 128 + 6 integ`,
crate `cargo tree` still no hard-reth.

## Stage 8 — async finalize-FCU off the hot path: ALREADY IMPLEMENTED (`f336fd7` doc)

No new code: `N42_ASYNC_FINALIZE_FCU=1` (default off) already `tokio::spawn`s the finalize
`forkChoiceUpdated` off the consensus select-loop, reporting via `finalize_done_tx` →
`select!` arm on `finalize_done_rx` → `handle_finalize_done`, recording `n42_fcu_latency_ms`.
The port refactor (FCU via `ExecutionLayer::fork_choice_updated`) is exactly what makes the
spawn clean. The remaining work is the **A/B measurement** (proving it beats the devlog-69
in-loop-FCU stall) — deferred to real-machine; task recorded in
`docs/task-async-finalize-fcu-ab.md`.

## Stage 9 — `EngineApiRpcExecutionLayer` + standalone consensus binary (`a7b34c0`, `6eae251`)

Proves the `ExecutionLayer` port enables Caplin's dual-mode: consensus driving ANY Engine-API
EL over RPC, in a separate process.

- **`crates/n42-el-rpc`** (new lib, hard-reth-free — zero reth/revm direct deps):
  `EngineApiRpcExecutionLayer` implements `ExecutionLayer` over HTTP JSON-RPC + JWT
  (`reqwest` + `alloy_rpc_types_engine`). `new_payload`→`engine_newPayloadV4`,
  `fork_choice_updated[_with_attrs]`→`engine_forkchoiceUpdatedV3`,
  `resolve_payload`→`engine_getPayloadV4`→`BuiltBlock`. `parent_beacon_block_root` (absent
  from the getPayload envelope) is cached keyed by `payload_id` at
  `fork_choice_updated_with_attrs` and consumed at resolve. `blob_tx_hashes` stubbed empty
  (TODO; the envelope carries versioned hashes, not per-tx hashes — blob rebroadcast is a
  leader-only optimization, empty is correct in standalone mode). Pure request-build /
  response-parse helpers with unit tests.
- **`bin/n42-consensus-standalone`** (new bin): wires
  `ConsensusService::with_execution_layer(remote_el, …)` with **all sinks = None**
  (standalone consensus mode — no in-process state-tree/ZK/blob/exec-cache). CLI
  `--engine-url`/`--jwt-secret`; smoke test constructs the dual-mode service. Full swarm
  bring-up + `run()` against a live `n42-node` Engine API is the post-datc cross-process E2E.

### Problem solved — JWT provider-feature unification (the key lesson)
The first cut used `alloy_rpc_types_engine::JwtSecret::encode` (→ `jsonwebtoken`). It passed
when testing `n42-el-rpc` alone but **panicked deterministically** in the four-crate build
(`cargo test -p n42-el-rpc -p n42-consensus-standalone -p n42-consensus-service -p n42-node`):

```
jsonwebtoken: Could not automatically determine the process-level CryptoProvider ...
make sure exactly one of the 'rust_crypto' and 'aws_lc_rs' features is enabled.
```

Cause: feature unification — the reth/node crates pull `jsonwebtoken/aws_lc_rs`, el-rpc added
`rust_crypto`, so in the combined graph BOTH providers are on and `encode` can't choose. (It
looked "flaky" only because it depends on which crates share the build invocation, not on
thread timing.) **Fix:** mint the HS256 JWT directly with `hmac` + `sha2` + `base64`
(`bearer_header`), never calling `JwtSecret::encode`. Pure HMAC has no provider ambiguity;
`jsonwebtoken` remains in the tree (via alloy's `jwt` feature, for `JwtSecret`) but its
panicking path is never hit. Now deterministic: four-crate run green across repeated runs.

## Verification (whole branch)
- `cargo check --all-targets` workspace EXIT 0; `cargo clippy -- -D warnings` clean per crate.
- Tests: `n42-node 84 + n42-consensus-service 128 + n42-el-rpc 7 + n42-consensus-standalone 1
  + 6 integration`, 0 failed (deterministic across repeated four-crate runs).
- `cargo tree -p n42-el-rpc`/`-p n42-consensus-service` direct deps: no
  reth-node-builder/reth-evm/reth-payload/reth-transaction-pool/reth-execution-types.
- Cargo.lock: new crate entries + edges only; no revm/alloy/reth-* version downgrades.

## Follow-up (deferred to post-datc real-machine)
1. **Pre-merge E2E** (required for the whole branch): scenarios 1/3/4 — assert eager-import /
   `n42_fcu_latency_ms` / blob propagation / mobile-reward + staking withdrawals unchanged vs
   `main`.
2. **Stage 8 A/B**: `N42_ASYNC_FINALIZE_FCU` off-vs-on under sustained load (see
   `docs/task-async-finalize-fcu-ab.md`); flip default if it's a clear win.
3. **Stage 9 cross-process E2E**: run `n42-consensus-standalone` against a live `n42-node`
   reth Engine API (real JWT); fill `blob_tx_hashes` if standalone blob rebroadcast is wanted;
   complete the swarm bring-up in `main`.
