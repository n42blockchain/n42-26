# Devlog 82: Batch Transfer Fast-Lane 7-Node Probe

Date: 2026-06-20
Branch: `feat/continuous-ingest-cadence`
Base: `1ccaf7c`

## Question

Measure the 7-node ceiling for the proposed special-case transfer lane:

- compress each transfer record to `recipient_index u32 + amount u64` = 12 bytes;
- group all transfers from one sender in the block under one ECDSA signature;
- enforce sequential nonces inside the batch;
- use the path only as a benchmark fast lane to explore the surrounding consensus/network ceiling.

This is deliberately not a production protocol yet. It bypasses reth execution, state root, MDBX persistence, and receipt generation, and measures the 7-node consensus + block dissemination + decode/hash/recover/apply ceiling for the compressed format.

## Implementation

Added `N42_BATCH_TRANSFER_FASTLANE=1`:

- leader builds a synthetic `BatchTransferBlock` at the normal payload-build point;
- block hash is `blake3(encoded_batch)`;
- block data is sent through the real 7-node block-data path;
- followers verify the block hash, recover one ECDSA signature per sender batch, and apply transfers into in-memory balances/nonces;
- consensus commits the synthetic block hash and logs `N42_BATCH_TRANSFER_COMMIT`;
- normal behavior remains unchanged when the flag is off.

Encoding:

- block header: `block_number u64 + batch_count u32`;
- per sender: `start_nonce u64 + count u32 + signature[65]`;
- per transfer: `recipient_index u32 + amount u64`.

Added env-tunable large block limits for this benchmark:

- `N42_BLOCK_DIRECT_MAX_MB`, default still 16 MB;
- `N42_GOSSIP_BLOCK_MAX_MB`, default still 8 MB.

## Build

```bash
cargo check -q -p n42-node
cargo build --release --bin n42-node --bin n42-mobile-sim
```

`cargo check` passed. Release build passed. Existing reth warnings remained unrelated.

## Runs

Common environment:

```bash
N42_BATCH_TRANSFER_FASTLANE=1
N42_BATCH_TRANSFER_ACCOUNT_CACHE=1024
N42_BLOCK_DIRECT_MAX_MB=128
N42_GOSSIP_BLOCK_MAX_MB=128
N42_SKIP_WAIT_FOR_BLOCKS=1
N42_FAST_PROPOSE=1
N42_MIN_PROPOSE_DELAY_MS=0
N42_STARTUP_DELAY_MS=3000
RUST_LOG=n42::batch_transfer=info,n42::cl::consensus_loop=info,n42::cl::exec_bridge=info,n42::cl::orchestrator=info,n42::net=warn
./scripts/testnet.sh --nodes 7 --clean --no-explorer --no-tx-gen --no-mobile-sim --no-monitor --block-interval 2000 --data-dir <dir>
```

### 128 x 10k

- Transfers/block: 1,280,000
- Encoded size: 15,009 KiB
- Bytes/transfer: 12.008
- Build avg/max: 30/106 ms
- Broadcast avg/max: 9/13 ms
- Verify/apply avg/max: 192/971 ms
- Single-node sustained samples:
  - validator-2 avg 457,331 TPS, max 831,169 TPS
  - validator-5 avg 441,656 TPS, max 860,794 TPS
  - validator-6 avg 416,957 TPS, max 609,234 TPS

This was the cleanest stable run. It consistently exceeds the old 43-45K single-block record by roughly 9-10x on sustained samples.

### 256 x 10k

- Transfers/block: 2,560,000
- Encoded size: 30,019 KiB
- Bytes/transfer: 12.008
- Build avg/max: 68/281 ms
- Broadcast avg/max: 18/38 ms
- Verify/apply avg/max: 402/1,198 ms
- Single-node samples:
  - validator-6 avg 460,174 TPS over views 3-6, max 591,634 TPS
  - validator-5 avg 487,862 TPS over 3 samples, max 627,605 TPS
  - validator-4 max 1,114,012 TPS on one short interval
  - validator-2 max 708,945 TPS

This is the best observed practical range: roughly 0.45-0.70M TPS sustained-ish, with >1M TPS single-interval peaks. Some direct pushes were rejected as unauthenticated and recovered via fallback, which adds jitter.

### 512 x 10k

- Transfers/block: 5,120,000
- Encoded size: 60,038 KiB
- Bytes/transfer: 12.008
- Build avg/max: 158/372 ms
- Broadcast avg/max: 36/57 ms
- Verify/apply avg/max: 831/2,136 ms
- Best clean interval: validator-3 view 3 committed 5.12M transfers in 4,112 ms = 1,245,136 TPS.
- Later intervals degraded to roughly 0.16-0.25M TPS because 61 MB blocks triggered repeated pacemaker timeouts and unauthenticated direct-push rejections.

This proves the million-TPS regime is reachable for a single interval on the compressed lane, but the current harness/peer-auth/pacemaker setup does not keep 61 MB blocks stable.

## Answer

Yes, this special-case batch transfer lane can exceed the historical Mac records by a large margin in 7-node measurement:

- historical reference: ~43-45K TPS single-block peak, ~14K sustained;
- stable 128 x 10k run: ~0.4-0.46M TPS on repeated blocks;
- best practical 256 x 10k range: ~0.45-0.70M TPS, with >1M single-interval peak;
- largest 512 x 10k block: 1.245M TPS peak, but not stable in the current harness.

Update: `devlog-83` fixed the benchmark network artifacts called out below
(`rejected block direct push from unauthenticated peer` and duplicate full
GossipSub fallback). The cleaned 7-node results are:

| Run | TPS avg | TPS p50 | TPS p95 | TPS max |
| --- | ---: | ---: | ---: | ---: |
| Optimized 256 x 10k | 3.27M | 2.83M | 6.92M | 11.28M |
| Optimized release 512 x 10k | 3.24M | 3.04M | 6.42M | 13.33M |

See `docs/devlog-83-batch-transfer-profile-optimize.md` and
`docs/performance-records.md`. The same benchmark-only caveat still applies:
this path skips reth/EVM execution, state root, receipts, persistence, and
production replay semantics.

The bottleneck is no longer ECDSA verification or transfer encoding. With one ECDSA recovery per sender, verification/apply remains sub-second for 30 MB blocks and around 0.8-2.1s for 60 MB blocks. The visible instability comes from consensus/network cadence around very large sidecars:

- pacemaker timeouts under 30-60 MB blocks;
- direct block pushes rejected as unauthenticated for some peers, forcing fallback paths;
- repeated proposals/duplicate votes during startup and large-block retries.

## Next Steps

1. Fix validator peer binding for direct block push in benchmark testnet so the large sidecars do not hit `rejected block direct push from unauthenticated peer`.
2. Run 256 x 10k again with clean direct push and a larger pacemaker timeout to get a cleaner sustained number.
3. If the fast lane is promoted beyond benchmark mode, add real state-root/persistence semantics for the batch lane; the current result is an upper-bound measurement, not a production execution result.
