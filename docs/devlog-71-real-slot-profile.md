# Devlog 71: Mac Real Slot Profiling Pilot

Date: 2026-06-13
Branch: `chore/merge-reth-main-deps-upgrade`

## Scope

This records a bounded real-node slot profiling run on macOS. It used real local
validators, real reth execution, TCP ingest, Twig, and mobile simulation. It is not a
hundreds-of-block steady-state capacity run.

The run also identified one harness fix: `scripts/testnet.sh` now predeploys the
`n42-stress --erc20-ratio` storage-burner contract at
`0x000000000000000000000000000000000000C042`. Without this, contract-heavy presigned
transactions silently target an account with no code and degenerate into simple value
transfers.

## Environment

- Base before local changes: `6aeaa68` (`docs(task): retarget real-slot-measurement to macOS`).
- macOS sample report: macOS 26.5.1, arm64.
- Build: `cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim`.
- Testnet: 4 validators, 8s slot, standard cache mode, 2G block gas limit.
- Runtime flags: `N42_TWIG=1`, `N42_ENABLE_MDNS=0`, `N42_INJECT_PORT=19900`,
  `N42_MAX_TXS_PER_BLOCK=48000`.
- Mobile: `n42-mobile-sim`, 4 phones.
- Artifacts:
  - transfer: `/tmp/n42-real-slot-profile-r2/artifacts`
  - contract-heavy: `/tmp/n42-real-slot-profile-r3-contract/artifacts`

## Workloads

Two runs were kept:

| Workload | Data dir | Stress mode |
|----------|----------|-------------|
| Transfer | `/tmp/n42-real-slot-profile-r2` | 240k presigned transfer file, `--wave 48000`, 192k submitted before duration stop |
| Contract-heavy | `/tmp/n42-real-slot-profile-r3-contract` | clean contract-only chain, fresh 120k presign after contract predeploy, `--wave 24000 --duration 180` |

The first attempted contract run on the transfer chain was discarded. It reused a
contract presign generated before the transfer run, so its nonces were stale after the
transfer workload advanced the same accounts.

## Throughput Result

The stress tool's immediate `BLOCK_ANALYSIS` undercounted tail blocks that landed just
after the stress process printed its summary. The post-drain node/mobile logs are the
authoritative tx totals below.

| Workload | Submitted | Errors | Post-drain chain tx | Tx-bearing blocks | Max tx/block | Max gas/block | Stress wall TPS |
|----------|-----------|--------|---------------------|-------------------|--------------|---------------|-----------------|
| Transfer | 192,000 | 0 | 192,000 | 11 | 24,000 | 504M gas, 25.2% | 2,251 |
| Contract-heavy | 120,000 | 0 | 120,000 | 14 | 12,000 | 780M gas, 39.0% | 1,460 |

Stress immediate summaries:

| Workload | Stress immediate chain count | Note |
|----------|------------------------------|------|
| Transfer | 180,000 tx / 10 blocks | missed final 12k-tx block after drain |
| Contract-heavy | 102,000 tx / 11 blocks | missed final three 6k-tx blocks after drain |

## Tx-Bearing Slot Timing

All values are milliseconds. Pipeline rows are restricted to views whose block had
transactions.

| Workload | Payload pack p50/p95 | EVM exec p50/p95 | Pool overhead p50/p95 | Leader build p50/p95 | Leader import p50/p95 | Leader commit p50/p95 |
|----------|----------------------|------------------|------------------------|----------------------|-----------------------|----------------------|
| Transfer | 40 / 64 | 27 / 45 | 12 / 19 | 88 / 132 | 116 / 193 | 279 / 376 |
| Contract-heavy | 33 / 47 | 27 / 39 | 6 / 11 | 58 / 93 | 80 / 120 | 176 / 243 |

| Workload | Follower import p50/p95 | Follower commit p50/p95 | Follower ready-to-accept p50/p95 |
|----------|-------------------------|-------------------------|---------------------------------|
| Transfer | 64 / 132 | 123 / 186 | 129 / 346 |
| Contract-heavy | 54 / 116 | 99 / 246 | 95 / 418 |

The measured critical path is well below the 8s slot. The slot wall time remains dominated
by the configured 8s boundary, not by EVM execution, state root, compact block decode, or
vote processing in this bounded run.

## Twig And Mobile Background Work

Twig update timing, tx-bearing windows:

| Workload | Accounts p50/p95 | Storage changes p50/p95 | Twig elapsed p50/p95/max |
|----------|------------------|--------------------------|--------------------------|
| Transfer | 299 / 549 | 0 / 0 | 10 / 28 / 36 |
| Contract-heavy | 251 / 501 | 0 / 0 | 10 / 64 / 170 |

`storage_changes=0` was observed even for the contract-heavy stress contract. Treat that
as an instrumentation or StateDiff follow-up item before drawing storage-specific
conclusions from this run.

### Follow-up: contract gas limit was the root cause

The `storage_changes=0` observation above was rechecked on 2026-06-14 after
`ef11287` (`fix(stress): raise CONTRACT_CALL_GAS 65k->100k so storage burner doesn't
OOG`). Root cause was the stress harness, not node StateDiff/Twig extraction:
the old contract call limit was 65,000 gas, below the storage-burner contract's
fresh-slot write cost, so every contract call reverted out-of-gas and rolled back
storage. The original 12k-tx contract-heavy blocks used exactly 780M gas
(`65,000 gas/tx`), which matches the old limit and explains the empty storage diffs.

Minimal confirmation run:

- tip: `ef11287`
- build: `cargo build --release --bin n42-node --bin n42-stress --bin n42-mobile-sim`
- testnet: 4 validators, `N42_TWIG=1`,
  `RUST_LOG=n42::cl::consensus_loop=debug`
- stress: `n42-stress --erc20-ratio 100`, 8,000 contract-heavy tx, 0 errors
- post-drain chain: 4 tx-bearing blocks, 2,000 tx/block, 8,000 tx total
- gas: each tx-bearing block used `102,970,000` gas, or `51,485 gas/tx`, below the
  new 100k limit and not pinned to the limit

Representative tx-bearing `STATE_DIFF_DIAG`:

```text
block_hash=0xed7e510fd06ebe282af51b7a689f5e197b9a0eb0b5ea4f604f93176d3a574397
bundle_accounts=1252 raw_bundle_storage_slots=2500
diff_accounts=1252 diff_storage_changes=2500
```

All four validators logged the same non-zero storage counts for the tx-bearing blocks.
Twig also applied the same storage delta, for example:

```text
Twig updated version=10 root=0x71895f638874019a3de278d8d9d8680b4810e79c0e79a14c326804d595a9ef67 accounts=1252 storage_changes=2500
```

Conclusion: the previous `storage_changes=0` result was an all-OOG stress workload.
With `CONTRACT_CALL_GAS=100_000`, contract storage commits, raw broadcast bundle
storage slots are non-zero, extracted StateDiff storage changes are non-zero, and Twig
receives the expected storage changes. No node StateDiff/Twig bug is indicated by this
follow-up.

Mobile verification is background relative to leader proposal:

| Workload | Mobile verify p50/p95/max | By block size |
|----------|---------------------------|---------------|
| Transfer | 254 / 449 / 547 | 12k tx: 230 / 259 ms; 24k tx: 358 / 505 ms |
| Contract-heavy | 97 / 229 / 233 | 6k tx: 93 / 106 ms; 12k tx: 215 / 230 ms |

## Sampling

`samply record -p <pid>` failed with macOS `task_for_pid` error code 5, so this run used
`sample <leader_pid> 150` as a fallback. The captured validator-0 sample reported:

| Item | Value |
|------|-------|
| Physical footprint | 514.1M |
| Peak physical footprint | 516.4M |
| Symbolication | not available from the release binary |

Because the sample is unsymbolicated, it is not used for function-level hotspot claims.
The runtime tracing metrics above are the actionable data from this run.

## Readout

- Current tx-bearing critical-path timings are sub-400ms p95 for leader commit in both
  workloads on this 4-node mac testnet.
- The visible pressure point is submission/drain behavior: several waves hit the 30s
  drain timeout with 6k or 12k tx pending, while payload build itself remained tens of ms.
- Mobile verification is background and stayed under 550ms max in the measured windows.
- The contract workload is gas-heavier than transfer, but block packing tops out at 12k
  tx/block under the current 2G gas limit and stress contract gas shape.

Next measurement work:

- rerun under `--profile profiling` or launch the process through `samply record --` to
  get symbolicated leader self-time;
- run a longer 4-node and 7-node steady-state pass;
- teach `n42-stress` to wait for post-drain finality before printing `BLOCK_ANALYSIS`.
