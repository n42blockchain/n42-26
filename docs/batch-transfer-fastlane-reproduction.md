# Reproducing the 3.24M TPS / 13.33M Peak Batch-Transfer Fast-Lane Result

This guide is for human testers who need to objectively reproduce the
benchmark-only batch-transfer fast-lane record:

- average commit TPS: **3,237,965 transfers/s**;
- p50 commit TPS: **3,038,576 transfers/s**;
- p95 commit TPS: **6,416,040 transfers/s**;
- max single-interval TPS: **13,333,333 transfers/s**.

The record comes from the `512 x 10k` release run documented in
`docs/devlog-83-batch-transfer-profile-optimize.md`.

## What This Test Measures

This is **not** production Ethereum transaction TPS.

The test enables `N42_BATCH_TRANSFER_FASTLANE=1`, a benchmark-only path that
builds synthetic compressed transfer sidecars:

- one sender batch per block;
- one ECDSA signature per sender batch;
- monotonically increasing nonces inside each sender batch;
- each transfer record is `recipient_index u32 + amount u64` = 12 bytes;
- block hash is `blake3(encoded_batch)`;
- consensus commits the synthetic batch-transfer block hash;
- followers verify the block hash, recover one signature per sender, and apply
  balances/nonces in memory.

The path intentionally skips reth/EVM execution, state roots, receipts, MDBX
persistence, and production replay semantics. Quote this result only as a
benchmark upper bound for this narrow compressed-transfer lane.

## Reference Run

| Field | Value |
| --- | ---: |
| Nodes | 7 local validators |
| Binary profile | `release` |
| Senders | 512 |
| Transfers per sender | 10,000 |
| Transfers per block | 5,120,000 |
| Encoded sidecar size | 60,038 KiB |
| Bytes per transfer | 12.008 |
| Direct-push rejections | 0 |
| Gossip fallback | skipped |
| Commit samples | 49 |
| Average TPS | 3,237,965 |
| p50 TPS | 3,038,576 |
| p95 TPS | 6,416,040 |
| Max TPS | 13,333,333 |

## Hardware And Environment

Use a quiet local machine. The reference run was on Apple Silicon with all 7
validators on the same host.

Recommended minimum:

- Apple Silicon Mac or comparable high-end Linux workstation;
- 32 GB RAM or more;
- enough file descriptor limit for 7 local nodes;
- no other CPU/network-heavy workload during the run;
- repo checked out with the paired `../reth` path dependency available.

Record these before running:

```bash
git rev-parse HEAD
git -C ../reth rev-parse HEAD
rustc --version
cargo --version
uname -a
sysctl -n machdep.cpu.brand_string 2>/dev/null || lscpu | sed -n '1,20p'
ulimit -n
```

For the original record, the implementation branch was
`feat/continuous-ingest-cadence`; the result is indexed in
`docs/performance-records.md`.

If this guide is being read from `main`, first check whether the benchmark
implementation is present:

```bash
test -f crates/n42-node/src/batch_transfer.rs
rg -n "N42_BATCH_TRANSFER_FASTLANE|N42_BATCH_TRANSFER_COMMIT" crates/n42-node/src
```

If either command fails, checkout a branch or commit that contains the
batch-transfer fast-lane implementation before running the benchmark:

```bash
git fetch origin
git switch --track origin/feat/continuous-ingest-cadence
```

Keep the same paired `../reth` checkout used by that branch.

## Build

From the `n42-26` repository root:

```bash
cargo build --release --bin n42-node --bin n42-mobile-sim
```

The run does not need `n42-stress`. Do not use
`n42-stress --batch-transfer-bench` for this record; that command is an offline
CPU-only microbenchmark and reports different numbers.

## Start The 7-Node Testnet

Use a fresh data directory for every attempt:

```bash
BASE=/tmp/n42-batch-fastlane-repro-512x10k
rm -rf "$BASE"
mkdir -p "$BASE"

export N42_BATCH_TRANSFER_FASTLANE=1
export N42_BATCH_TRANSFER_SENDERS=512
export N42_BATCH_TRANSFER_PER_SENDER=10000
export N42_BATCH_TRANSFER_ACCOUNT_CACHE=1024

export N42_BLOCK_DIRECT_ACCEPT_EXPECTED=1
export N42_BATCH_TRANSFER_GOSSIP_FALLBACK=0
export N42_BLOCK_DIRECT_MAX_MB=128
export N42_GOSSIP_BLOCK_MAX_MB=128

export N42_SKIP_WAIT_FOR_BLOCKS=1
export N42_FAST_PROPOSE=1
export N42_MIN_PROPOSE_DELAY_MS=0
export N42_STARTUP_DELAY_MS=3000

export RUST_LOG=n42::batch_transfer=info,n42::cl::consensus_loop=info,n42::cl::exec_bridge=info,n42::cl::orchestrator=info,n42::net=warn

./scripts/testnet.sh \
  --nodes 7 \
  --clean \
  --no-explorer \
  --no-tx-gen \
  --no-mobile-sim \
  --no-monitor \
  --block-interval 2000 \
  --data-dir "$BASE/data" 2>&1 | tee "$BASE/testnet.log"
```

Let the network run for at least 90 seconds after all validators start. The
reference record used 49 commit samples. Stop the run with `Ctrl-C` after enough
samples are present.

## Required Log Signals

Before calculating TPS, confirm the run used the intended path:

```bash
rg -n "N42_BATCH_TRANSFER_BUILD|N42_BATCH_TRANSFER_COMMIT|N42_BATCH_TRANSFER_GOSSIP_SKIPPED|N42_BATCH_TRANSFER_DIRECT_PUSH|rejected block direct push|failed to verify|invalid batch-transfer" "$BASE"
```

Expected:

- `N42_BATCH_TRANSFER_BUILD` contains `senders=512`,
  `transfers_per_sender=10000`, `transfers=5120000`, and
  `encoded_kb` near `60038`.
- `N42_BATCH_TRANSFER_COMMIT` appears on all validators.
- `N42_BATCH_TRANSFER_GOSSIP_SKIPPED` appears, confirming full GossipSub
  fallback was disabled.
- `rejected block direct push` count is 0.
- `failed to verify` and `invalid batch-transfer` count is 0.

Quick objective checks:

```bash
rg -c "N42_BATCH_TRANSFER_COMMIT" "$BASE"
rg -c "rejected block direct push" "$BASE"
rg -c "invalid batch-transfer|failed to verify" "$BASE"
```

The first number should be at least 40 for a comparable run. The latter two
numbers must be 0.

## Calculate TPS

The committed TPS is logged directly as `transfer_tps` in
`N42_BATCH_TRANSFER_COMMIT`. Use this parser to compute the same summary fields
as the reference run:

```bash
python3 - <<'PY'
import math
import re
import statistics
from pathlib import Path

base = Path("/tmp/n42-batch-fastlane-repro-512x10k")
ansi = re.compile(r"\x1b\[[0-9;]*m")
values = []
encoded = []
transfers = []

for path in base.rglob("*.log"):
    try:
        text = path.read_text(errors="replace")
    except OSError:
        continue
    for raw in text.splitlines():
        line = ansi.sub("", raw)
        if "N42_BATCH_TRANSFER_COMMIT" not in line:
            continue
        m = re.search(r"transfer_tps=([0-9]+)", line)
        if m:
            values.append(int(m.group(1)))
        m = re.search(r"encoded_kb=([0-9]+)", line)
        if m:
            encoded.append(int(m.group(1)))
        m = re.search(r"transfers=([0-9]+)", line)
        if m:
            transfers.append(int(m.group(1)))

if not values:
    raise SystemExit("no N42_BATCH_TRANSFER_COMMIT transfer_tps samples found")

def percentile_nearest_rank(xs, pct):
    xs = sorted(xs)
    rank = math.ceil((pct / 100.0) * len(xs))
    return xs[max(0, min(len(xs) - 1, rank - 1))]

print(f"samples={len(values)}")
print(f"avg={statistics.mean(values):.0f}")
print(f"p50={percentile_nearest_rank(values, 50)}")
print(f"p95={percentile_nearest_rank(values, 95)}")
print(f"max={max(values)}")
print(f"encoded_kb_minmax={min(encoded) if encoded else 'n/a'}..{max(encoded) if encoded else 'n/a'}")
print(f"transfers_minmax={min(transfers) if transfers else 'n/a'}..{max(transfers) if transfers else 'n/a'}")
PY
```

## Acceptance Criteria

Use two levels of acceptance.

Strict record reproduction on comparable hardware:

- commit samples: at least 40;
- `transfers_minmax`: `5120000..5120000`;
- `encoded_kb_minmax`: near `60038..60038`;
- average TPS: 2.9M to 3.6M;
- p50 TPS: at least 2.7M;
- p95 TPS: at least 5.5M;
- max TPS: at least 12.0M;
- direct-push rejections: 0;
- invalid batch-transfer / verify failures: 0.

Functional reproduction on slower hardware:

- commit samples: at least 20;
- `transfers_minmax`: `5120000..5120000`;
- `encoded_kb_minmax`: near `60038..60038`;
- average TPS above 1.0M;
- max TPS above 3.0M;
- no direct-push rejections or verification failures.

If the strict record is missed but the functional criteria pass, preserve the
logs and report CPU model, memory pressure, commit sample count, and the parser
output. The most common reasons for a lower peak are background CPU load, memory
pressure from 60 MB sidecars, and startup/cadence jitter.

## Optional Control Run

If the 512 x 10k run is unstable, first validate the cleaner 256 x 10k setting:

```bash
export N42_BATCH_TRANSFER_SENDERS=256
export N42_BATCH_TRANSFER_PER_SENDER=10000
BASE=/tmp/n42-batch-fastlane-repro-256x10k
```

Expected reference values:

| Shape | Avg TPS | p50 TPS | p95 TPS | Max TPS |
| --- | ---: | ---: | ---: | ---: |
| 256 x 10k | 3,267,444 | 2,825,607 | 6,918,919 | 11,277,533 |

The encoded size should be near `30019 KiB`.

## What To Submit

Attach or archive:

- `git rev-parse HEAD`;
- `git -C ../reth rev-parse HEAD`;
- build command output;
- complete `$BASE/testnet.log`;
- all `$BASE/data/validator-*.log` files;
- the parser output;
- counts for direct-push rejections and invalid/failed verification logs;
- hardware and OS information.

Do not compare this benchmark-only result against production Ethereum
transaction TPS without repeating the caveat that this path skips reth/EVM,
state roots, receipts, persistence, and production replay semantics.
