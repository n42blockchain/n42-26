# Devlog 60: SBMT RPC Proof E2E Verification

Date: 2026-06-10
Branch: `chore/merge-reth-main-deps-upgrade`

## Goal

Verify the SBMT production path end to end on macOS:

- run a local `n42-node` with `N42_JMT=1`;
- confirm `n42_jmtVersion`, `n42_jmtRoot`, and `n42_jmtProof` are reachable over HTTP RPC;
- decode `proofHex` as `bincode(ShardedBmtProof)`;
- verify inclusion and exclusion proofs with `n42_mobile::state_proof::verify_state_proof`;
- confirm tampering with `shard_root`, `shard_path`, or `value` is rejected.

## Environment Notes

`../reth` points at the n42 reth branch that has upstream main merged plus the N42
execution hooks. A plain `../reth/main` checkout failed the required release build
because it does not contain `reth_evm::payload_cache` or `n42_defer_state_root`.
The validated setup used:

```text
../reth: n42/chore/merge-upstream-main
HEAD: 04c7f29f9b feat(deps): bump alloy-evm 0.35 -> 0.36, adapt OnStateHook API
```

The n42 workspace manifest stayed aligned with the reth-main dependency line:

- `reth-primitives-traits = 0.4.0` in `Cargo.toml` (resolved as `0.4.2` in `Cargo.lock`);
- `alloy-evm = 0.36.0`;
- `revm = 40.0.3`.

## Fixes Made During Verification

### Leader Local Block Data Drain

Single-node leader eager import could commit Case A before the select loop processed
`leader_payload_rx`. SBMT update then ran before local `BlockDataBroadcast` was visible,
so logs repeated:

```text
no block data available for JMT update (will catch up)
```

and `n42_jmtRoot` stayed uninitialized. The commit path now drains pending local leader
payloads into `pending_block_data` immediately before the SBMT/JMT update.

### Genesis Alloc Seeding

`ShardedSbmt::new()` previously started as an empty tree and only consumed per-block
`StateDiff`s. That meant genesis alloc accounts existed in reth state but returned SBMT
exclusion proofs. The node now seeds `ChainSpec.genesis.alloc` into SBMT at version 0
when `N42_JMT=1`, then publishes the genesis SBMT root to RPC state.

Observed startup log:

```text
SBMT seeded from genesis alloc version=0 root=0x661cbcc1a53b67794b35a46a348788abc3eaa45d72a511d745e10698852f09dc accounts=5001
```

## Validator Added

Added `crates/n42-mobile/tests/sbmt_rpc_e2e.rs`.

The test is ignored by default because it requires a running local node:

```bash
N42_SBMT_RPC_URL=http://127.0.0.1:18000 \
  cargo test -p n42-mobile --test sbmt_rpc_e2e -- --ignored --nocapture
```

It checks:

- `n42_jmtRoot.version` does not lag `n42_jmtVersion`;
- SBMT version advances while blocks commit;
- genesis account proof is inclusion;
- missing account proof is exclusion;
- both proofs verify against the SBMT root returned by RPC;
- tampered `shard_root`, `value`, and `shard_path` fail verification;
- proof sizes stay in the expected compact range.

## Local E2E Run

Testnet command:

```bash
env N42_JMT=1 N42_ENABLE_HTTP_RPC=1 N42_LOW_MEMORY=1 \
  ./scripts/testnet.sh --nodes 1 --clean --no-explorer --no-monitor \
  --no-mobile-sim --data-dir /tmp/n42-sbmt-e2e --block-interval 1000
```

RPC smoke:

```text
eth_blockNumber -> 0x1c
n42_jmtVersion -> 28
n42_jmtRoot -> { version: 28, root: 0xb48623238c0cc0155172c8ce9692ac3da79dfcdfae463f3cf1269b9db5f1dec4 }
```

Representative SBMT logs:

```text
SBMT state tree enabled (16 shards)
SBMT seeded from genesis alloc version=0 root=0x661cbcc1a53b67794b35a46a348788abc3eaa45d72a511d745e10698852f09dc accounts=5001
SBMT updated version=28 root=0xb48623238c0cc0155172c8ce9692ac3da79dfcdfae463f3cf1269b9db5f1dec4 accounts=4 storage_changes=0
```

Ignored E2E result:

```text
running 1 test
inclusion proof size: 708 bytes
exclusion proof size: 692 bytes
test sbmt_rpc_proof_roundtrip ... ok
```

## Verification Commands

Passed:

```bash
cargo test -p n42-bmt-core -p n42-jmt
cargo build --release -p n42-node-bin
cargo test -p n42-mobile --test sbmt_rpc_e2e
N42_SBMT_RPC_URL=http://127.0.0.1:18000 \
  cargo test -p n42-mobile --test sbmt_rpc_e2e -- --ignored --nocapture
```

## Outcome

SBMT proof RPC is verified end to end on the reth-main merged fork for both inclusion
and exclusion. The proof format returned by `n42_jmtProof` is compatible with the
mobile verifier, and local negative tests confirm proof tampering is rejected.

---

## Reth Main Alignment Status

The earlier old-fork E2E note is superseded by this run. The actual RPC proof
roundtrip above was executed after switching `../reth` to `chore/merge-upstream-main`.
`Cargo.toml` kept the reth-main dependency line (`revm 40.0.3`, `alloy-evm 0.36.0`,
`reth-primitives-traits 0.4.0`), and final diff checks showed no dependency rollback
or reth-main API rollback in `crates/n42-parallel-evm`, `crates/n42-consensus`, or
`crates/n42-execution`.
