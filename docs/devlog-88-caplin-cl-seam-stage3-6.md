# Devlog 88: Caplin EL-seam refactor — stages 3-6 (consensus service crate)

Date: 2026-06-21
Branch: `feat/caplin-cl-stage3-6` (off `origin/main` @ 0acb485)
Plan: `~/.claude/plans/enumerated-wobbling-fox.md` + `docs/task-caplin-cl-module.md` +
`docs/task-caplin-stage6-clean-extraction.md`
Status: code + unit tests complete; **real-machine E2E deferred** (datc occupied the box).

## Goal

Restructure n42's consensus layer like Erigon **Caplin** — a highly decoupled CL that runs
in the same single binary as the EL — by driving every side effect through *ports* (traits)
and finally lifting the consensus driver into its own crate `n42-consensus-service` that
does **not** depend on reth's node assembly. Stages 1-2 (the `ExecutionLayer` seam +
routing the orchestrator's EL calls through it) were already on `main`; this devlog covers
**stages 3-6**.

## Design decisions

- **Ports-and-adapters, behavior-identical, one stage per commit.** Each stage compiles,
  passes unit tests, and is a behaviour-preserving refactor — the call sites' control flow,
  logs, metrics, lock order, and wire formats are byte-identical. Adapters wrap the concrete
  node/reth types and lock internally; the trait is the port.
- **Pragmatic purity boundary (user decision).** Mid-stage-6 we found the orchestrator
  genuinely processes *execution state* — `StateDiff` (→ `revm::BundleState`), `Receipt`,
  and the compact-block `BlockExecutionOutput` feed the state-tree / ZK / compact-block
  paths. Literal "zero reth/revm" would have required byte/opaque-izing those ports (with a
  double-extraction perf regression or an opaque decoded-block handle). We chose the
  pragmatic line: the service crate must not depend on the **hard red-line**
  (`reth-node-builder`, `reth-evm`, `reth-payload-builder`, `reth-payload-primitives`,
  `reth-transaction-pool`, `reth-execution-types`, node-assembly) but **may** use `revm`,
  `reth-ethereum-primitives::Receipt`, and `n42-execution` (`StateDiff`) as plain
  execution-domain type deps.
- **Builders keep node-friendly wiring.** Stages 3-5 kept builder signatures concrete
  (wrapping into adapters internally) so `bin/n42-node` wiring didn't churn; stage 6
  inverted them to take `Arc<dyn …>` once the driver moved to the crate (node now builds the
  adapters and passes trait objects).
- **`ConsensusOrchestrator` → `ConsensusService` + compat alias** so external callers keep
  compiling through one release.

## Stages (each a commit on the branch)

| Stage | Commit | What |
|------|--------|------|
| 3 | `711a92e` | Sink ports: `StateSink` (jmt/twig), `ZkSink`, `StakingSink` (scan/save), `WithdrawalSource` (reward+staking withdrawal computation moved verbatim into `NodeWithdrawalSource`). `crates/n42-node/src/sinks/`. |
| 4 | `e90037e` | `ConsensusNetwork` port over `NetworkHandle` (13 methods — discovered via compiler, not grep: multiline + spawned-task + free-fn call sites). `crates/n42-node/src/net_port.rs`. |
| 5 | `5a28425` | Rename `ConsensusOrchestrator` → `ConsensusService` + `pub use … as ConsensusOrchestrator` alias. |
| 6a-1 | `e58187f` | Neutralize `ExecutionLayer` to a node-neutral `BuiltBlock` (drops `EthBuiltPayload`/`EthEngineTypes`/`PayloadTypes`/`PayloadKind`); adapter `to_built_block` does the reth introspection. Remove the reth-handle `with_engine_api` ctor → `with_execution_layer(Arc<dyn ExecutionLayer>)`; node builds `RethExecutionLayer`. |
| 6a-2 | `a004a8f` | Move the compact-block exec-output cache (`reth_evm::payload_cache`, `BlockExecutionOutput`) behind `ExecutionOutputCache` (`take_serialized`/`inject`, bytes only). `crates/n42-node/src/exec_cache.rs`. |
| 6a-3 | `85e17b9` | `BlobStorePort` (byte-oriented `insert_rlp`/`get_all_encoded`, RLP coding + `DiskFileBlobStore` in the adapter); `defer_state_root` → bool config; local `TxImportBatch = Vec<Vec<u8>>`. **Orchestrator module now hard-reth-free.** |
| 6b-6d | `8c8ccf4` | Extract `crates/n42-consensus-service`: the `ConsensusService` driver + the 5 port traits + reth-free supporting types (`consensus_state`, `epoch_schedule`, `persistence`, `validator_peers`, `now_unix_ms`, the ingest credit hook). Adapters stay in n42-node; `observer.rs` moved out of the `orchestrator/` dir (reth-coupled, stays node); builder inversion; main.rs wiring. |

## Implementation notes / problems solved

- **Network port surface was 13, not 6.** A same-line grep missed `self.network\n.method()`
  multiline calls + uses in spawned tasks / free fns. The field-type change to
  `Arc<dyn ConsensusNetwork>` made the compiler enumerate the true set. Trait is all-sync +
  6 `async` (reliable) methods via `async_trait`.
- **el trait neutralization (`BuiltBlock`).** `ExecutionData` / `PayloadId` /
  `ForkchoiceState` are alloy (neutral) — they stay in the trait. Only `EthBuiltPayload`
  (resolve result) + `PayloadKind` were reth; replaced by `BuiltBlock` + `ResolveKind`, with
  `block_to_payload` + blob-hash extraction relocated into the adapter `to_built_block`.
- **Compact-block cache** exchanges only compressed bytes at its boundary
  (`Option<Vec<u8>>` out, `&[u8]` in) → cleanly hidden behind `ExecutionOutputCache`; all
  `reth_evm::payload_cache` + `CompactBlockExecution` serde stay node-side.
- **`note_virtual_block_credit`** lived in the reth-coupled `ingest.rs`. Rather than pull
  ingest into the crate, the crate exposes a fn-pointer hook
  (`ingest::set_block_credit_hook`); `n42_node::register_consensus_service_hooks()` registers
  the node's real credit fn at startup (`bin/n42-node/src/main.rs`). No-op until registered →
  behaviour-identical in production. **This is the one runtime indirection — verify it stays
  wired.**
- **`impl ConsensusNetwork for NetworkHandle` lives in the crate** (net_port.rs) because it's
  reth-free (n42-network/libp2p only); this kept the 15 moved orchestrator tests green. A
  follow-up could move it node-side like the other adapters if a future standalone build
  wants the crate free of n42-network.

## Status / verification

- `cargo check --all-targets` (workspace) EXIT 0; `cargo clippy -p n42-consensus-service -p
  n42-node -- -D warnings` clean.
- Tests: **n42-node 77 + n42-consensus-service 135 unit + 6 integration, 0 failed**
  (the orchestrator/sink/net/blob unit tests moved into the crate).
- `cargo tree -p n42-consensus-service -e normal --depth 1`: direct reth/revm deps are only
  `reth-ethereum-primitives` + `revm` (both allowed). No hard-reth direct dep. (Transitive
  reth-evm/reth-execution-types arrive only via the allowed `n42-execution`/`n42-jmt`.)
- `Cargo.lock`: new crate entry + edges only; **no revm/alloy/reth-* version downgrade**.

## Follow-up

1. **E2E before merge (required).** No real-machine validation was possible (datc occupied
   the box). After datc frees it, run E2E scenarios 1 / 3 / 4 (single-node, ERC-20,
   multi-node consensus) and assert `n42_eager_import_hits_total` / `n42_fcu_latency_ms` /
   blob propagation / mobile-reward + staking withdrawals unchanged vs `main`. Only then
   merge `feat/caplin-cl-stage3-6`.
2. Stage 7 (optional): fold `ObserverOrchestrator` onto the same `ExecutionLayer` port and
   move it into the crate (delete its duplicated FCU/new_payload).
3. Stage 8 (optional, perf): move finalize-FCU off the consensus hot path behind the trait
   (env-gated A/B) — directly addresses the devlog-69 in-loop-FCU stall.
4. Stage 9 (future): a remote `EngineApiRpcExecutionLayer` + a network port impl decoupled
   from `NetworkHandle` → a standalone consensus binary (Caplin's dual-mode).
