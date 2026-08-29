# Devlog 142 — chain 94: a Rust member votes in the live qs fleet

Date: 2026-08-29 (handover executed 06:08:45Z; the node is left running).

Host: the Linux box of devlog-141. Target: the *real* qs fleet — chain 94
`mainnet_qmdb_staggered`, seven gov5 HotStuff validators (`/data/blockchain/qs-node0..6`,
binary `n42-fix-receiptroot` `bc071302…`), replayed-mainnet state, period 3 s,
epochLength 200 with a static 7-validator set, committeePool 200,000/512,
devBlockReward 1 ETH to coinbase and faucet, twoPhaseVoteGate, mobileAnchor active,
EOF active, `interopV4` **not** set. Genesis `0xa2d2ff5d…be99`.

Everything runs under `/data/blockchain/mixed-fleet/n42-26-qs` (`R` below). The other
fleets, nodes 0–4 and node6 were never touched; node6's slot was taken by the n42-rs
agent in parallel (`h2_validator` on tcp/32006), so the fleet is 5 Go + 2 Rust members.

## Outcome

The N42-26 node built from this branch replaced gov5 **node5** (validator index 5,
`0x580339c3…f70a`, PeerId `16Uiu2HAmJm4…qsDH`) as an H2-v4 participant on the live
chain, without a bundle and without the chain's 13.5 million blocks of history:

- it started at the qs-node6 snapshot head (block 13,560,375, `0x0e37dae9…`), fetched
  the 1,639 blocks the fleet had produced in between over gov5's `bodies_by_range`,
  executed them (`new_payload(Valid)`, 13,560,376 → 13,562,014 in 2.8 s), and was
  voting six seconds after start;
- every proposal it votes on is executed first (`import-gated vote: execution
  validated, sending vote`), including the blocks with 248 and 312 transactions the
  faucet txgen produces, with gov5's rewards credited and its native receipts root and
  rewards root checked;
- `received Decide, committing block` follows each vote; `last_voted_view` advances
  (11,030 at start → 12,765 six minutes later; `consensus_state.json`
  `execution_validated_head_view` tracks it);
- the gov5 side counts its vote: **82 of the last 100 embedded commit QCs carry bit 5**
  (`tools/qc-bitmaps.py`, decoding the QC in every header's extra-data) while Go node5
  is stopped — the gov5 leaders stop collecting at the quorum of 5, so the misses are
  views where five Go votes arrived first, not rejections;
- heads are identical on all five gov5 RPCs and the Rust RPC at every sampled height
  (e.g. 13,562,086: `0x7c9de926…955a`, state root `0x68efb7ec…`, receipts root
  `0xc5d24601…`);
- RSS 0.68 GB.

Leader duty is the one thing the member declines: every seventh view (its slot) times
out after gov5's 6 s base timeout, exactly as when node5 was down. See "What is not
done".

## What had to be built

n42-rs's audit (`docs/N42_26_PORT.md`, "what is still missing") listed rewards,
committee evidence, mobileAnchor, EOF and a snapshot bootstrap. Taking the chain
block by block, the list that actually bit, in the order it bit, was:

### 1. The header cannot be decoded by alloy (`crates/n42-consensus/src/gov5_native_header.rs`)

Live chain-94 headers have 23 RLP fields. gov5 encodes with Go's `rlp:"optional,nil"`
semantics: a nil optional is *omitted* when nothing follows it and written as the
empty string `0x80` when a later optional is set. A header sampled from the snapshot
(`qs-probe-linux.go -scan 3000`: 2,572 blocks of one shape, 428 of another from the
newer node6 binary) is

```
… baseFee, withdrawalsRoot, 0x80, 0x80, parentBeaconRoot, 0x80, 0x80, mobileRegistryRoot(=0)
```

— reward commitment, two blob placeholders, committee-evidence root, `0x80` for the
Pectra requests hash (gov5 runs "Pectra" = EIP-7702 only, `IsPrague` is false),
`0x80` for the EIP-7928 hash, and the `mobileAnchor` root (zero, but present). Alloy's
`Header::decode` fails at the requests-hash placeholder and has no 23rd field, and its
encoder cannot emit placeholders, so `hash_slow()` of any alloy view is the wrong block
hash.

`Gov5NativeHeader` decodes that shape losslessly (integer placeholders become
`Some(0)`, which re-encodes to the same byte; hash placeholders become `None`; the
mobile-registry root rides alongside), re-encodes byte for byte and hashes; a bounded
registry remembers the raw encoding of every header seen on the wire. The engine
validator (`convert_payload_to_block`) consults the registry when alloy's
reconstruction cannot reproduce the expected hash, binds the payload to the remembered
header field by field (rewards root via the withdrawals it carries) and seals with
gov5's hash. The consensus adapter's normalisation is unchanged: it already re-hashes
normalised headers on both sides of every parent check.

Test: the live header of block 13,560,375 round-trips to `0x0e37dae9…`.

### 2. Rewards (`crates/n42-consensus/src/gov5_rewards.rs`)

gov5 credits 1 ETH to the coinbase and 1 ETH to `devFaucetAddress` in `Finalize` and
commits the list as `hash.DeriveSha(Rewards)` — keccak of the concatenated
`RLP([address, amount])` items — in the withdrawals-root slot. Inside reth a reward is
an EIP-4895 withdrawal (gwei; 1 ETH is exact), credited after the transactions where
gov5 credits it. The gov5 block wire's reward list is now decoded
(`Gov5GossipBlock::rewards`), turned into withdrawals for the Engine payload, checked
against the header's rewards root (`build_gov5_gossip_execution_data`), and the
consensus adapter hands reth the Ethereum withdrawals root of the same list so reth's
body-vs-header rule keeps guarding the body. Test: block 13,540,000's two rewards hash
to its withdrawals root `0x29c0690c…`.

### 3. The fleet signs with the pre-interopV4 domains (`ConsensusSigningProfile::Gov5Legacy`)

`hotstuff.interopV4` is off on chain 94. gov5 then signs Proposal and Vote over
`view || hash`, CommitVote over `"commit" || view || hash`, Timeout/NewView as the
native profile — under the proof-of-possession ciphersuite (`crypto/bls/blst`), which
is the H2-v4 DST here. The participant's H2-v4 profile signed chain-bound 56-byte
prefixed messages, so nothing it signed would have counted and nothing gov5 signed
would have verified. The new profile keeps the H2 transport (`/n42/a2d2ff5d/
hotstuff_consensus/ssz_snappy` plus direct pushes) and swaps the domains
(`N42_GOV5_LEGACY_SIGNING=1`). Test: gov5's persisted commit QC for view 11,030
verifies under the legacy profile only.

### 4. Starting at a checkpoint (`n42-init-snapshot`, `N42_GOV5_STATE_ROOT_TRUST`)

Chain 94's blocks 1–13,536,950 are replay-v2: empty bodies whose state roots change
anyway (the folded mainnet state), so no client can re-execute the history; and the
snapshot's QMDB slot log has 63.3 M slots of which only the 6.16 M live rows still
exist (`n42-qmdb-export` refuses: "entry log is not contiguous at slot 0"), so the
portable checkpoint the bundle path needs cannot be produced.

`n42-init-snapshot` (new binary) initialises an empty Reth datadir at gov5's applied
head from `n42-reth-state-dump` output: 13,560,374 dummy headers, the head header
sealed with gov5's hash (decoded by the native codec, checked against the snapshot
hash), empty changeset segments up to the head (reth's history invariants otherwise
demand an unwind to zero), and the 6,120,111 accounts / 37,383 slots written straight
into the state tables of the datadir's layout (v2: hashed only). Block zero is bound to
gov5's authenticated genesis header, and the configured alloc is checked to reproduce
its QMDB root `0x64102f3c…` (the native profile does). 17 s.

Without a QMDB forest the node cannot recompute state roots. `N42_GOV5_STATE_ROOT_TRUST=1`
(observer or participant, Gov5 header profile only, logged as a warning at start) takes
the proposer's state root and executes everything else; `Gov5TrustedStateRootStrategy`
replaces the QMDB strategy in the engine tree. `n42-bootstrap --consensus-state-out`
writes `consensus_state.json` from gov5's persisted HotStuff state after verifying the
commit QC against the validator set, so no bundle is needed.

### 5. The rest

`N42_GOV5_H2_PARTICIPANT` accepts `epoch_length > 0` with a static schedule already (it
logs "static-schedule epoch profile"); the fleet's `validator set for epoch not in
history … falling back to current set` warnings are the same static set (the log line is
noisy but correct). The fork digest is the first four bytes of the genesis hash on both
sides; the topics match without change.

## Runtime

```
R=/data/blockchain/mixed-fleet/n42-26-qs
├── env.sh                       # paths, chain identity, base block, gov PeerIds
├── init-snapshot.sh             # n42-init-snapshot from artifacts/state.jsonl
├── run-node.sh observer|participant, stop-node.sh
├── artifacts/
│   ├── genesis.json             # n42-rs's chain-94 genesis + shanghaiTime/cancunTime, Prague off
│   ├── genesis-range.n42frng    # block 0 (export-range-linux.go)
│   ├── state.jsonl(+.header.rlp,+.manifest.json)   # n42-reth-state-dump of qs-snapshot/qs-node6
│   ├── hotstuff-state-hex.txt   # hotstuff-state-v1-linux.go
│   ├── consensus_state.json     # n42-bootstrap --consensus-state-out --legacy-signing
│   └── consensus-peer-bound.json  # 7 validators, f=2, epoch_length 200, slot 3000 ms
├── keys/node5/{keystore/bls_0x580339….key,network-keys}
├── snapshot-reth-template/      # initialised datadir, copied per run
├── rust/{reth,consensus}        # the participant
├── logs/participant.log, tools/qc-bitmaps.py
```

Ports: consensus QUIC+TCP 32005 (node5's slot), reth p2p 31305, HTTP 20117, authrpc
20217, starhub 9445.

Exact commands (gov5 tools run from the gov5 checkout; `GOTMPDIR=/home/n42/.gotmp`):

```bash
go run ./qs-probe-linux.go -db $QS_SNAPSHOT/chaindata -scan 3000        # head, header shapes, QMDB tables
go run ./export-range-linux.go -db $QS_SNAPSHOT/chaindata -from 0 -to 0 -out $A/genesis-range.n42frng
go run ./hotstuff-state-v1-linux.go $QS_SNAPSHOT > $A/hotstuff-state-hex.txt
go run ./cmd/n42-reth-state-dump --datadir $QS_SNAPSHOT/chaindata --out $A/state.jsonl
n42-bootstrap --consensus-config $A/consensus-peer-bound.json --gov5-hotstuff-state-hex $(cat $A/hotstuff-state-hex.txt) \
  --chain-id 94 --genesis-hash $GENESIS_HASH --consensus-state-out $A/consensus_state.json \
  --checkpoint-block 13560375 --checkpoint-hash 0x0e37dae9… --legacy-signing
n42-init-snapshot --chain $A/genesis.json --datadir $R/snapshot-reth-template --header $A/state.jsonl.header.rlp \
  --state $A/state.jsonl --expected-hash 0x0e37dae9… --genesis-range $A/genesis-range.n42frng --genesis-hash $GENESIS_HASH
cp -a $R/snapshot-reth-template $R/rust/reth
cd /data/blockchain/scripts-qs && source ./qs-env.sh && qs_stop_node 5     # 06:08:4xZ, "stopped clean"
$R/run-node.sh participant                                                   # 06:08:45Z
```

`run-node.sh participant` refuses to start while tcp/32005 is still held. The
participant environment is the devlog-141 one plus `N42_GOV5_STATE_ROOT_TRUST=1`,
`N42_GOV5_TRUSTED_BASE_{BLOCK,HASH,ROOT}`, `N42_GOV5_GENESIS_BOOTSTRAP`,
`N42_GOV5_LEGACY_SIGNING=1`, `N42_GOV5_CATCHUP_BUFFER_BLOCKS=65536`.

## Evidence

Rust log (`logs/participant.log`, first six seconds):

```
06:08:45.968 gov5 legacy signing profile enabled: the fleet runs without hotstuff.interopV4
06:08:45.968 H2-v4 participant bridge enabled chain_id=94 genesis_hash=0xa2d2ff5d… validator_index=5
06:08:45.971 peer connected … validator_index=0  (… 1, 2, 3, 4, 6)
06:08:47.820 authenticated Gov5 catch-up parent reached new_payload(Valid) block_number=13560376
06:08:50.596 authenticated Gov5 catch-up parent reached new_payload(Valid) block_number=13562014
06:08:50.603 received Decide, committing block view=12673
06:08:51.560 import-gated vote: execution validated, sending vote view=12674 … voter=5 target_leader=…
06:08:51.573 view committed view=12674 … consensus_timing=follower proposal=@953ms vote_delay=3ms commit_vote=@958ms
```

Counters at 06:14:55Z (six minutes): 71 `sending vote to leader`, 72 `received Decide`,
72 `view committed`, 16 `view timed out` (all our own leader views), 0 errors outside
`leader_emit`. `consensus_state.json`: `current_view 12766, last_voted_view 12765,
last_commit_voted_view 12765, committed_block_count 13562086,
execution_validated_head_view 12765`.

gov5 side (`qs-node0/log/n42.log`): `received Decide, committing block` / `hotstuff:
block committed` for the same views and hashes (e.g. view 12716 `d82693…67f3dd`, which
our log voted on at 06:10:13.991 and gov5 committed at 06:10:14); leaders report
`votes=5/5`.

QC bitmaps (`tools/qc-bitmaps.py 20013 100`, bit index 5 = validator 5, Go node5
stopped throughout):

```
13562060 view 12728 … embedded_qc_view 12727 signers 1110110
13562061 view 12729 … embedded_qc_view 12728 signers 0111110
13562062 view 12733 … embedded_qc_view 12730 signers 1101110
port 20013: 82 of 100 embedded commit QCs carry validator 5's signature (bit index 5)
```

Heads at 13,562,086 on 20012–20016 and 20117: hash `0x7c9de9264416c9f5…955a`, state
root `0x68efb7ecdea7f258…`, receipts root `0xc5d2460186f7233c…` on all six.

## What is not done, precisely

- **State root is trusted, not recomputed.** The QMDB forest is not rebuilt locally.
  The portable snapshot format carries every slot ever appended; chain 94's log has
  63,349,357 slots and the gov5 datadir keeps rows only for the 6,159,032 live ones
  (dead rows are reclaimed once a leaf store persists the frozen twig leaves), and the
  Rust `QmdbCompatTree` materialises every slot and every twig heap (≈ 4 GB of node
  heaps alone) and clones the tree per candidate. A twig-level format (30,933 sealed
  twigs as `leafRoot || activeBits`, the open twig's leaves, the live index) with a
  copy-on-write tree is the follow-up; until then the node verifies transactions,
  receipts, gas, rewards and the block hash, and takes `stateRoot` from the proposer.
- **No leader.** Sealing a gov5 block needs the QMDB root and the committee evidence;
  the member's leader views time out (gov5 base timeout 6 s), which the fleet handles
  as it did while node5 was down. The refusal is logged at `leader_emit`.
- **Committee evidence (`parentBeaconRoot`) is carried, not verified.** The root is
  executed as the EIP-4788 beacon root (gov5 writes the same two slots) but its
  derivation from the parent's committee evidence is not rebuilt; n42-rs has a checked
  port of `blspool` to consult.
- **EOF, `mobileAnchor` semantics.** The mobile-registry root is decoded and re-encoded
  (it is zero on every block seen) but not computed; EOF contracts would execute
  differently under revm. Prague/EIP-2935 is deliberately inactive on the Rust side to
  match gov5's `IsPrague == false`; an EIP-7702 transaction would be rejected here.
- **Serving ranges to gov5.** `Gov5CanonicalBlockReader` re-encodes stored blocks
  through alloy and refuses those whose hash it cannot reproduce, so gov5 peers cannot
  catch up *from* this member for native-shape headers.
- **Observer mode against the fleet did not connect**: every gov5 node resets the TCP
  connection of an unlisted identity right after identify (`Connection reset by peer`
  ~0.5 ms after our Status request; nothing in gov5's info log, no goodbye). The
  trusted identity of node5 is accepted, so the participant is unaffected; the observer
  path on this fleet is unexplained.
- The persisted alloy header of a native-shape block loses the `0x80` placeholders and
  the mobile-registry root; the stored hash is gov5's, and every parent check
  normalises both sides, but `eth_getBlockByHash` on this node shows the alloy view.

## Stall at 06:27 UTC and its fix

After 19 minutes the member stopped executing: its RPC stayed at 13,562,197
while the fleet went on, and every view committed with `execution_ready=false`
(`Decide` arrived, no body to execute, no vote). The consensus view kept following
the fleet; execution did not. The chain of events, from `logs/participant-run1-stalled.log`:

1. Its own leader view 12,913 timed out three times (06:27:06, :18, :30 — the
   repeat re-broadcast is every 12 s). At the third repeat a **republish storm**
   started: the same H2 message (gossip id `7e78639e…`) was published ~700 times a
   second for 6 s (`failed to publish … error=Duplicate`), and a second storm ran
   06:28:06–12 for view 12,920. On the H2 transport every publish is a gossip
   message plus one direct `hotstuff_direct` stream to each of six peers, so the
   peers' answers came back as thousands of inbound streams: **3,151 `Dropping
   inbound stream because we are at capacity`** in 40 s. The request-response
   behaviour's queues on both sides were then jammed for the rest of the run:
   gov5's block pushes never reached the orchestrator again (`ExecuteBlock
   requested … pending_data=false`), `block_by_hash` fetches timed out 19 times,
   and the execution head never moved.
2. The trigger is the timeout relay: on every newly observed Timeout the engine
   forwards it to the next leader (`SendToValidator`), which in H2 mode is a full
   fan-out; gov5's rotor re-forwards, the copies come back, and each side answers
   the other. The code carried a comment describing exactly this loop and
   deduplicated the relay per sender; that was not enough.
3. On top of that, the leader view at 06:28:12 (12,927) found reth behind and the
   build path retried `forkchoice_updated` every 2 s (`did not return payload_id,
   scheduling retry`), noisy but not the cause; and the execution catch-up chose the
   N42 state-sync protocol, which no gov5 peer speaks, instead of the gov5 range
   fetch that works at start-up.

Fix (commit below, binary rebuilt and the member restarted at 07:00:18Z):

- `broadcast_engine_consensus` suppresses a byte-identical H2 publish within
  750 ms of the previous one (`H2_REBROADCAST_MIN_INTERVAL`, blake3 of the encoded
  message, bounded map, metric `n42_h2_v4_rebroadcasts_suppressed_total`). The
  intentional resends (vote every 2 s, timeout every 12 s) are untouched; any echo
  loop is capped at one publish per message per 750 ms.
- The timeout relay is skipped under the gov5 profiles (`process_timeout`): gov5
  members gossip their own timeouts and their next leader forms the TC from those.
- A trusted-root member never starts a payload build (`with_leader_disabled`,
  `leader build skipped: this member cannot seal a gov5 block; the view will time
  out`): no FCU-with-attributes retry loop, no leader recovery timer; its leader
  views time out once, as an absent validator's do.

After the restart the member caught up 601 blocks (13,562,198 → 13,562,798) from
gov5 in 20 s, was voting at 07:00:44Z (view 13,654), lag 0 on both RPCs at
13,562,810; the first 40 s show `dup=0 capacity=0 errors=0`, five leader views
skipped. Still open: when execution falls behind while the consensus view keeps
following the fleet, the orchestrator should start the gov5 range fetch rather
than the state-sync request; today a restart is the recovery.

## Handing the slot back

```
$R/stop-node.sh participant
cd /data/blockchain/scripts-qs && set -a && source /data/blockchain/faucet.env && set +a && \
  ./roll-one-node.sh --node 5 --bin /data/blockchain/bin/n42 --txgen-max 0
```

The node is left running as validator 5.
