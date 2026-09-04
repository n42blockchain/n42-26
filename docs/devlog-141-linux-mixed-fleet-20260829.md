# Devlog 141 — Linux mixed fleet: one Rust validator inside a gov5 HotStuff committee

Date: 2026-08-29 (run executed 2026-08-28 23:13–23:24 UTC)

Host: Linux x86_64 (256 cores), everything under `/data/blockchain/mixed-fleet/n42-26`.
The qs fleet (`/data/blockchain/qs-node*`, ports 20012–20018 / 32000–32006) and
the n42-rs devnet (18545/28545/30313/30393, `/data/blockchain/mixed-fleet/n42-rs`)
kept running untouched.

## Outcome

`scripts/gov5-interop-qualification.sh` in H2-v4 participant mode (`start-rust2`:
the Rust node replaces exactly one Go validator, reusing its BLS key and secp256k1
libp2p PeerId) works on Linux against gov5 built from `fix/qmdb-receipt-root`.
On a freshly generated six-validator static chain (chain id 1143, genesis
`0x0f8ea34c…d4b6ad`):

- the Rust node cold-started from an authenticated bootstrap bundle
  (checkpoint block 52), replayed blocks 1–52, caught up live to the gov5 head
  and entered the committee as validator index 5;
- it votes on every gov5 proposal after `new_payload=Valid` and commits on the
  gov5 Decide;
- it leads every sixth view: 17 consecutive Rust-authored blocks (heights
  67…163, stride 6) were committed with `votes=5+5` and are byte-identical on
  all five gov5 RPC endpoints;
- 182 s head monitor (`monitor-heads 180 10`): 19 samples, common height
  98 → 301, maximum lag 0, identical `hash:stateRoot:receiptsRoot` at the common
  height on all six endpoints every sample; `audit-soak … 150 120 6 0` PASS;
- gov5 restart/rejoin drill: with gov5 (validator 5) stopped for 10 s the
  remaining four gov5 nodes plus the Rust node (exactly the quorum of 5)
  committed views 403–411, including Rust-led view 407 — the Rust votes were
  necessary for every QC in that window; gov5 rejoined, fetched the missed
  bodies from the Rust node, and all six endpoints were equal again at 0x1d4.

No soak was run (the brief asked for a 2–5 minute run); the processes are left
running for inspection.

## Why the pinned runtime could not be reused

The script pins the macOS `runtime-11` committee: genesis artifact SHA-256
`5618…dca687`, genesis hash `0xb71c…92ec`, six gov5 PeerIds/addresses and the
frozen validator keys under `artifacts/validator-keys/`. None of those files
exist on this host (they were generated on the Mac by `cmd/hotstuff-testnet`
and never committed). A chain with the same genesis hash cannot be
regenerated because the validator BLS keys are random. So the *procedure* was
kept and the *identity* regenerated:

1. the script's constants became `N42_QUAL_*` overrides (commit in this
   branch; defaults unchanged, so the macOS runtime still works verbatim);
2. an equivalent six-validator committee was generated here.

## Binaries

| Role | Path | Source | SHA-256 |
|---|---|---|---|
| gov5 (`geth-live`) | `/data/blockchain/bin/n42-fix-receiptroot` → `runtime/geth-live` | gov5 `fix/qmdb-receipt-root` (`325d88ef` on top of main `5b0be916`), 5.7.960 | `bc071302…9f882` |
| Rust node | `/home/n42/src/n42/N42-26/target/release/n42-node` (built 2026-08-24 from `main`) | N42-26 main (`4e7ff0b` at time of run) | `7382acb9…1a34f` |
| bundle tool | `/home/n42/src/n42/N42-26/target/release/n42-bootstrap` | same | `0e18408f…9027` |

**Do not use `/data/blockchain/bin/n42-main-5b0be916` for this**: on QMDB chains
its validator recomputes an Ethereum receipt-trie root while the producer seals
the native keccak-concat root, so every sealed block (and a Rust leader's empty
block) is rejected with `invalid receipt root hash`. The fix branch was supplied
mid-task; the gov datadirs here were initialised and run only with the fixed
binary.

## Runtime layout

```
/data/blockchain/mixed-fleet/n42-26/runtime          # N42_QUAL_RUNTIME
├── env.sh                     # all N42_QUAL_* / N42_GOV_BINARY / N42_NODE_BINARY overrides
├── build-bundle.sh            # gov6 datadir -> bootstrap-bundle.json (see below)
├── geth-live                  # copy of n42-fix-receiptroot
├── artifacts/
│   ├── genesis.json           # sha256 8d7ddb7f…26ce (pinned via N42_QUAL_GENESIS_SHA256)
│   ├── validators.json        # addresses, BLS pubkeys, PeerIds (generator manifest)
│   ├── consensus-peer-bound.json   # Rust ConsensusConfig, 6 validators with p2p_peer_id
│   ├── genesis-range.n42frng  # block 0
│   ├── finalized-range.n42frng     # blocks 1..52 (from gov6)
│   ├── qmdb-checkpoint.n42qmdb     # block 52 portable QMDB snapshot (from gov6)
│   ├── hotstuff-state.txt / qmdb-export.txt   # tool outputs
│   ├── bootstrap-bundle.json  # sequence 1, digest 0x7418f455…e19d
│   └── validator-keys/node6/{keystore/bls_b42162….key,network-keys}  # copied from gov/node6
├── gov/node1..node6           # gov5 datadirs (n42 init), keystore, network-keys, epoch_schedule.json, network.json
├── rust2/{consensus,reth}     # Rust validator 6 data
├── logs/gov1..gov6.log, rust2.log
├── pids/
├── evidence/head-monitor.jsonl, rust-leaders.jsonl
└── tools/rg                   # `rg` shim (grep -a) for the audit helpers; ripgrep is not installed here
```

Ports (script defaults, no clash on this host): gov1–gov5 P2P TCP 30301–30305,
HTTP 28501–28505; gov6's slot 30306 is taken over by the Rust node (QUIC
udp/30306, Gov5 TCP profile on 30306), Rust reth p2p 31306, HTTP 29546, authrpc
29552, starhub udp/9444. Ports 19780/29545/9443 (the second Rust node `rust`)
are unused in this topology.

## Committee

| # | address (etherbase) | libp2p PeerId | runs as |
|---|---|---|---|
| 1 | `0xaae49f0c7c9d7573f81decb2e4a3156daae9b7ce` | `16Uiu2HAmSZhq9w8dri8oDMoUT39NuGa8fvZMW5ibmFh315Vn6foz` | gov1 |
| 2 | `0x87061e789aadc55d1928a3547b8c07cd5d50d8dc` | `16Uiu2HAm1jqEr3U62i7xG5vsLLvqerPz2E8prXXhSUPLoVacdvUq` | gov2 |
| 3 | `0xb5f455809bfb0bf78cbe54e7eb00aec2ed89d456` | `16Uiu2HAmLmiiZtxvjR1AsH8X1CFQ518R2AvyN3QHGetFdwsvSiuG` | gov3 |
| 4 | `0xa4f6298d6367737d0753c329aeeff5e4036c709d` | `16Uiu2HAmN9MDjBwxWzKvvycAyFS1YSte3aHHasBJJtpppamMH1ku` | gov4 |
| 5 | `0xb595a118950d6521d3b7790cf64a04c7cff8de14` | `16Uiu2HAmQaD6n87WBYAxMm8E9uk4YFQ8rEv73mCBN44J82qBiYJa` | gov5 |
| 6 | `0xb42162b97a36b51dda512ca86a211243ada8fe0d` | `16Uiu2HAkyi7mtbMAP2Az9B1JAaWxBqzW5YPJFRSQogcEiCgdL4Ah` | gov6 until block 52, then **Rust `rust2`** (validator_index=5) |

n=6, f=1, quorum n−f=5 on both clients (`ValidatorSet::quorum_size` in Rust,
`QuorumSize()` in gov5). Genesis `hotstuff`: `period 1`, `baseTimeout 6000`,
`maxTimeout 30000`, `epochLength 0`, `fastPropose false`,
`minProposeDelayMs 200`, `interopV4 true`; `stateScheme qmdb`; London at 0;
difficulty 0, extraData 32 zero bytes; one funded dev account
(`0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266`, hardhat #0). No
`devBlockReward`/`committeePool` (the Rust profile assumes matching reward
semantics and no committee-pool header commitments).

## Exact commands

Helper Go tools live in the gov5 checkout used for `go run` (a gov5 worktree on
`fix/qmdb-receipt-root`/main; they only read MDBX):
`interop-gen-linux.go`, `hotstuff-state-v1-linux.go`, `export-range-linux.go`
(copies are kept next to the runtime in `/data/blockchain/mixed-fleet/n42-26/tools/`).

```bash
GOV5=<gov5 checkout>            # go run needs the module
R=/data/blockchain/mixed-fleet/n42-26/runtime
export GOTMPDIR=/home/n42/.gotmp TMPDIR=/home/n42/.gotmp

# 1. committee, genesis, keys, Rust consensus config
cd $GOV5 && go run -tags nosqlite,noboltdb ./interop-gen-linux.go -out $R -n 6 -chain-id 1143
cp /data/blockchain/bin/n42-fix-receiptroot $R/geth-live
for i in 1 2 3 4 5 6; do
  $R/geth-live init --profile n42 --chain private --data.dir $R/gov/node$i $R/artifacts/genesis.json
done   # -> genesis 0x0f8ea34cad3a431102f544f5c92e749cbd2bb07ff178ec9861e77da6d9d4b6ad, network.json written
go run -tags nosqlite,noboltdb ./export-range-linux.go -db $R/gov/node6/chaindata -from 0 -to 0 -out $R/artifacts/genesis-range.n42frng

# 2. overrides for the qualification script (runtime/env.sh)
export N42_QUAL_RUNTIME=$R N42_GOV_BINARY=$R/geth-live \
  N42_NODE_BINARY=/home/n42/src/n42/N42-26/target/release/n42-node \
  N42_QUAL_GENESIS_HASH=0x0f8ea34cad3a431102f544f5c92e749cbd2bb07ff178ec9861e77da6d9d4b6ad \
  N42_QUAL_GENESIS_SHA256=8d7ddb7f37edcd879b7b7f9b80e4014a85bcba39bee698285d66db3c5beb26ce \
  N42_QUAL_GOV_PEERS="<six PeerIds in node order>" N42_QUAL_GOV_ADDRESSES="<six addresses>" \
  N42_QUAL_PORTS="28501 28502 28503 28504 28505 29546" N42_QUAL_RUST_PORT=29546 \
  N42_QUAL_RUST_MINER=0xb42162b97a36b51dda512ca86a211243ada8fe0d N42_GOV_COUNT=6

# 3. six gov5 validators (block interval 1000 ms)
scripts/gov5-interop-qualification.sh start-gov
scripts/gov5-interop-qualification.sh status        # all six at the same height/hash

# 4. replace validator 6: stop gov6, build the bundle from its datadir
scripts/gov5-interop-qualification.sh stop-gov-node 6
$R/build-bundle.sh 1                                 # sequence 1
#   hotstuff-state-v1-linux  -> view=55 lockedQC=54/753b8171… committedQC=54/753b8171…
#   n42-qmdb-export          -> block=52 hash=753b8171… root=264d4bdb… slots=1 live=1
#   export-range-linux 1..52 -> last_hash=753b8171… last_state_root=264d4bdb…
#   n42-bootstrap            -> bootstrap-bundle.json (sequence 1, digest 0x7418f455…)
mkdir -p $R/artifacts/validator-keys/node6/keystore
cp $R/gov/node6/keystore/bls_*.key $R/artifacts/validator-keys/node6/keystore/
cp $R/gov/node6/network-keys      $R/artifacts/validator-keys/node6/

# 5. Rust participant as validator 6
scripts/gov5-interop-qualification.sh start-rust2

# 6. evidence
scripts/gov5-interop-qualification.sh monitor-heads 180 10 $R/evidence/head-monitor.jsonl
scripts/gov5-interop-qualification.sh audit-soak $R/evidence/head-monitor.jsonl 150 120 6 0
PATH=$R/tools:$PATH N42_QUAL_RUST_LOG=$R/logs/rust2.log N42_QUAL_RUST_VIEW_STRIDE=6 N42_QUAL_RUST_LEADER_STRIDE=6 \
  scripts/gov5-interop-qualification.sh audit-rust-leaders 67 "" $R/evidence/rust-leaders.jsonl
```

`build-bundle.sh` refuses to continue if gov6's persisted `committedQC` block
and its QMDB applied head differ (`n42-bootstrap` requires the finalized range,
the checkpoint and the commit QC to name the same block); restart gov6, let it
catch up, stop it again and retry. On this run they matched on the first stop
(the gov5 node persists between views most of the time).

### Bridging gaps the macOS runbook never had to cross

1. **gov5 main persists HotStuff state in the v2 layout**
   (`N42HSSv2 | view | timeouts | lastVoted(8+32) | lastCommitVoted(8+32) |
   len+lockedQC | len+committedQC`), while `n42-bootstrap
   --gov5-hotstuff-state-hex` parses the v1 shape
   (`view | timeouts | len+lockedQC | committedQC`). `hotstuff-state-v1-linux.go`
   re-lays the record; the SSZ QC bytes are identical.
2. **No finalized-range exporter for blocks ≥ 1 exists in either repo** —
   `scripts/gov5-export-genesis-range.go` only writes block 0.
   `export-range-linux.go` is its `-from/-to` generalisation (same framing,
   gov5's own RLP, compact receipts via `Receipts.MarshalCompact`, blake3 trailer).
3. `ripgrep` is absent on this host; `audit-rust-leaders`/`audit-runtime-logs`
   call `rg`. `runtime/tools/rg` is a `grep -a` shim.
4. gov5 `--verbosity 3` prints only the console banner/status line to stdout;
   consensus detail needs `--log.file` (goes to JSON under `<datadir>/log/`). The
   script now accepts `N42_GOV_EXTRA_ARGS` for such diagnostic restarts.

## Evidence

### Heads equal across five gov5 endpoints and the Rust endpoint

`status` immediately after the Rust node joined (23:16:40Z):

```
rpc:28501..28505  0x56  0xfa5976a68e7a9e36a8b52494f42c074625977f3632bf709242e8ff2f5a8437cb  0x264d4bdb…3e5b
rpc:29546         0x56  0xfa5976a68e7a9e36a8b52494f42c074625977f3632bf709242e8ff2f5a8437cb  0x264d4bdb…3e5b
```

Heights 87–111 sampled on 28501 and 29546: identical `hash`/`stateRoot`
per height, `difficulty 0x0`, zero transactions; the Rust address
`0xb42162…` is the miner of 91, 97, 103, 109 on both endpoints.

`evidence/head-monitor.jsonl` (19 samples, 23:16:51Z–23:19:53Z) and
`evidence/soak-audit.json`:

```
{"event":"mixed_client_soak_audit","status":"PASS","samples":19,"elapsedSeconds":182,"maximumSampleGapSeconds":11,
 "startHeight":98,"endHeight":301,"blockGrowth":203,"maximumLag":0,
 "evidenceSha256":"afc02c0ab6ee50bb7465bdb5092f1dc3f24a46680a820bde992d2768eed47800"}
```

Every sample has `ok:true`, `lag:0` and one identity string for the common
height across 28501–28505 and 29546 (e.g. height 166:
`0x18b34fbe…8bab:0x264d4bdb…3e5b:0xc5d24601…a470`; the receipts root is the
native keccak of an empty receipt list, which is exactly the value the
`n42-main-5b0be916` validator would have rejected).

### Rust side (`logs/rust2.log`, `rust2/consensus/consensus_state.json`)

```
authenticated Gov5 participant bootstrap bundle materialized sequence=1 checkpoint_block=52 checkpoint_hash=0x753b8171…
prepared Gov5 block-zero QMDB Engine Tree execution base base_block=0 base_hash=0x0f8ea34c…
recovered consensus state from snapshot view=55 locked_qc_view=54 last_committed_view=54 committed_block_count=52
H2-v4 participant bridge enabled chain_id=1143 genesis_hash=0x0f8ea34c… validator_index=5
imported authenticated gov5 replay-v2 block block=1 … block=52         (bundle replay, ~10 ms)
peer connected ×5 (all gov PeerIds)
import-gated vote: waiting for execution validation view=106 …
import-gated vote: execution validated, sending vote view=106 block_hash=0x58b9edbf…
sending vote to leader view=106 … voter=5 target_leader=4
received Decide, committing block view=106
leader_build_start view=107 leader_idx=5 my_index=5
validated normalized Gov5 leader payload before proposal release hash=0x39dbf431… block_number=103
direct-pushed gov5 leader block peers=5
block committed! view=107 block_hash=0x39dbf431… consensus_timing=leader proposal=@12ms R1_collect=7ms R2_collect=5ms total=25ms votes=5+5
```

Counters at 23:17Z (≈100 s after start): 70 `sending vote to leader`, 70
`received Decide`, 14 `block committed!` as leader, 0 `ERROR`; the 24 `WARN`
lines are one `view timed out view=55` (the view gov6 was stopped in, before
the Rust node was up), five `peer does not speak N42 state-sync` (gov5 peers,
expected), timestamp bumps on leader builds, and the compact-output eviction
notices that are normal on the mixed path (`evicted rejected compact execution
output` — followers use full execution). No `h2_shadow_rejected`/H2-v4
rejection lines exist; the only H2-v4 warning is a GossipSub `Duplicate`
publish.

`consensus_state.json` at 23:17:25Z:
`{"version":5,"current_view":112,"last_voted_view":111,"last_commit_voted_view":111,"committed_block_count":107,"execution_validated_head_view":111,"last_committed_qc.view":111}`
— `last_voted_view` advances with the chain.

### Rust leader audit (`evidence/rust-leaders.jsonl`)

```
{"event":"rust_leader_canonical_audit","status":"PASS","miner":"0xb42162…","startHeight":67,"endHeight":167,
 "blocksScanned":101,"leaderStride":6,"rustAuthoredBlocks":17,"firstRustHash":"0x17d023ff…","lastRustHash":"0xf4a7f66a…",
 "ports":[28501,28502,28503,28504,28505,29546],"parentChainContinuous":true,"expectedLeaderSlotsExact":true,
 "allConfiguredEndpointsExact":true,"leaderCommitLog":{"matchedCommits":17,"allVotesFivePlusFive":true,
 "expectedViewStride":6,"viewStrideExact":true,"hashOrderExact":true,"firstView":71,"lastView":167,
 "latencyMs":{"proposalMinimum":12,"proposalMaximum":19,"commitMinimum":24,"commitMaximum":32,"commitAverage":25.4}}}
```

Every sixth height from 67 is Rust-authored, every other height is not; the
17 Rust blocks are identical on all six endpoints and match the leader commit
log hash order; each was committed with five prepare and five commit votes
from the gov5 members.

### gov5 side

gov5 stdout at `--verbosity 3` is only the console status line, so validator 5
was restarted at 23:21:21Z with `N42_GOV_VERBOSITY=debug
N42_GOV_EXTRA_ARGS="--log.file gov5-debug"` (`restart-gov-node 5`); its log is
`gov/node5/log/gov5-debug`. Relevant lines (ANSI stripped):

```
p2p  Dialing peer addrs=[/ip4/127.0.0.1/tcp/30306] id=16Uiu2HAkyi7…      # gov6's slot, now the Rust node
p2p  Peer dial failed … dial tcp4 127.0.0.1:30306: connect: connection refused   # Rust listens on QUIC only
p2p  Peer connected activePeers=5 direction=Inbound multiAddr=/ip4/127.0.0.1/tcp/53312/p2p/16Uiu2HAkyi7…   # Rust dialled in
sync First chunk decoded successfully blockNumber=407 peer=16Uiu2HAkyi7…   # gov5 ← Rust bodies_by_range catch-up
sync First chunk decoded successfully blockNumber=413 peer=16Uiu2HAkyi7…
hotstuff view changed hasProducer=true isLeader=true view=502
hotstuff block committed! blockHash=222acb…3db874 view=502
hotstuff hotstuff view timing: view=502 role=leader propose=1002ms r1=58ms r2=9ms total=1069ms votes=5/5
hotstuff import-gated vote: block already imported, voting now blockHash=d555eb…85f090 view=503   # Rust-led view
hotstuff received Decide, committing block blockHash=d555eb…85f090 view=503
hotstuff hotstuff view timing: view=503 role=follower recv=18ms r1=6ms r2=4ms total=28ms
hotstuff hotstuff: block committed hash=d555eb…85f090 view=503
runtime hotstuff: dropped stale rotor peer mapping after failed direct send peer=16Uiu2HAkyi7… validator=0xB42162…
```

Block 497 (`0xd555eb63…`, miner `0xb42162…`, view 503) is the Rust-authored
block gov5 voted for and committed on the Rust-formed Decide; block 496
(`0x222acbe3…`, view 502) is gov5's own proposal, committed with `votes=5/5`,
for which the Rust log shows `sending vote to leader view=502 … voter=5
target_leader=4` 12 ms before gov5's commit.

The restart window makes the Rust vote's inclusion unambiguous (quorum is 5 of
6): the Rust log shows gov5's peer disconnecting at 23:21:20.53Z and
reconnecting at 23:21:30.44Z, and in between views 403–405 and 407–411 were
committed with `peers=4` (four gov5 nodes + the Rust node), view 407 as Rust
leader; the two skipped views 406 and 412 are gov5's leader slots
(`view % 6 == 4`) that timed out while it was down. After the reconnect gov5
pulled blocks 407… and 413… from the Rust node's range server and committed
view 413 onward with all six.

Rotor direct sends from gov5 to the Rust member fail (`dropped stale rotor
peer mapping after failed direct send`); the messages still arrive through
GossipSub (`/n42/0f8ea34c/hotstuff_consensus/ssz_snappy` and
`/n42/h2/4/ssz_snappy`), which is why view timing stays at ~25 ms for Rust-led
views and ~1.07 s (the 1000 ms block interval) for gov5-led ones.

## What did not work / caveats

- `/data/blockchain/bin/n42-main-5b0be916` cannot be used (receipt-root
  regression, see Binaries). Not observed here because the fixed binary was
  used from `n42 init` onward.
- The pinned macOS artifacts (`genesis.json` `5618…`, validator keys,
  bundle) are not on this host; participation needed a regenerated committee,
  hence the `N42_QUAL_*` overrides. The macOS defaults remain the script's
  defaults.
- Rust `N42_GOV5_TCP_PORT` listener: the node advertises only QUIC on 30306
  in this run (`ss` shows udp/30306, no tcp/30306 listener); the gov5 peers
  were reached because the Rust node dials them as trusted peers. A gov5
  node that restarts therefore has to be dialled by the Rust node
  (`N42_TRUSTED_PEERS` covers all five), which is what happened in the
  restart drill.
- `audit_rust_leaders` hard-codes `votes == "5+5"`; it happens to be right
  for n=6 as well as for the macOS n=7 committee with one absent validator.
  `N42_QUAL_RUST_VIEW_STRIDE` must be set to 6 here (default 7).
- `audit-runtime-logs` was not run; its warning allow-list is tuned to the
  macOS runtime and `rg`.
- No transaction burst, no restart of the Rust node, no timeout/TC drill —
  outside the 2–5 minute brief.

## Leaving it running

```
gov1..gov5  pids in runtime/pids/gov{1..5}.pid   HTTP 28501..28505
rust2       pid  in runtime/pids/rust2.pid       HTTP 29546
gov6        stopped (datadir intact for rollback: `scripts/gov5-interop-qualification.sh stop-rust2 && start-gov-node 6`)
```

Stop everything with `source runtime/env.sh && scripts/gov5-interop-qualification.sh stop`
(SIGTERM, 30 s grace, per-pid-file; never touches the qs fleet).
