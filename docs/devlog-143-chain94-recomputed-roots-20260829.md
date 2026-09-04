# Devlog 143 — chain 94: recomputed state roots, rebuilt committee evidence, EOF guard, range-pull recovery

Date: 2026-08-29. Follow-up to devlog-142. Offline work only: the operator paused
every fleet on the box for a stress test while this was done, so nothing here was
run against the live chain. The live participant test with recomputed roots is
written up at the end for the operator to run afterwards; everything before it was
verified against recorded chain-94 data.

## Outcome in one paragraph

The four "can be done" items of devlog-142 are implemented and verified offline.
(1) The state root is recomputed instead of trusted: the QMDB commitment of chain 94
is rebuilt from a leaf-form export of the snapshot head in 9 s and 700 MB, kept
incrementally per block by an in-place tree with undo records instead of a clone per
candidate, and **4,968 recorded blocks (13,560,376 → 13,565,343, 96,522 transactions)
replay to the header's state root every time, ending on the fleet head's root
`0x05dc2c52…3028` with gov5's exact slot cursor (63,430,744)** — 5 s for the whole
range, 35 µs per root. Two gov5 rules had to be learned from the chain on the way:
gov5 runs Prague (its `pectraTime`, 2025-05-07), and every Prague block writes the
system caller `0xffff…fffe` as an empty live leaf. (2) The committee evidence is
rebuilt from the genesis pool and every imported header's `parentBeaconRoot` is
checked against it — 4,973 consecutive real links verify, gov5's own byte-for-byte
vectors pass, and the payload builder stamps the same root. (3) EOF code is detected
and refused with an unmistakable error and a metric; chain 94 holds none (29 coded
accounts in the snapshot, 20 code rows at the head, 0 create transactions in the
4,968 recorded blocks). (4) When execution falls behind while consensus follows, the
orchestrator now pulls gov5 blocks by hash instead of asking gov5 peers for the N42
state-sync protocol they do not speak.

## Data recorded before the fleet stopped

The fleet went down minutes into the session; what could be captured (all under
`target/chain94-record/`, never committed):

- `qs-node5-chaindata/` — a file copy of the stopped Go node5's chaindata (13 GB,
  head 13,565,343 `0xffda999a…`, QMDB applied). Node5 is the slot this member
  borrows; the copy is read-only and the original was never opened.
- `post-snapshot-13560370-13565343.n42frng` — gov5's `export-range-linux.go` over
  that copy: 4,974 blocks (27 MB, 96,620 transactions) with native headers, bodies
  and receipts, Blake3-sealed.
- `snapshot-13560375.leafform.qmdb` — gov5's `n42-qmdb-export --leaf-form` of the
  qs-node6 snapshot copy (2.35 GB; 30,933 twigs all with leaf hashes, 6,157,495 live
  of 63,349,357 slots; 40 s, 3.5 GB RSS).
- `reth-replay/` — a copy of the participant's `snapshot-reth-template` datadir
  (2.8 GB, state at 13,560,375).

## 1. State roots recomputed (`N42_GOV5_STATE_ROOT_TRUST` no longer needed)

### The split commitment (`crates/n42-twig-core/src/qmdb_leaf_tree.rs`, new)

A QMDB root is `upper(twig roots)` and a twig root is `hash(leafRoot,
hash(activeBits))`. Once a twig's 2,048 slots are appended its leaves never change —
slots are never reused — so the only part of a sealed twig a later block can touch
is its bits. `QmdbLeafTree` keeps, per sealed twig, exactly its frozen leaf root and
256 bytes of bits; only the open twig carries a leaf heap; the live entries are a
key-indexed map. Chain 94 is 30,933 sealed twigs (≈ 9 MB) plus 6.16 M live entries,
instead of the 63 M entries and 4 GB of heaps the full `QmdbCompatTree` materialises
(n42-rs reported 9.2 GB RSS for the same forest).

- `read_leaf_form_v2` streams the portable v2 file: every twig's leaves are folded
  against its leaf root in parallel batches, every live entry is checked against its
  leaf, the Blake3 trailer is checked, and the leaf hashes of twig 0's first 64 slots
  are reported so the node can match the genesis allocation (the v1 positional
  prefix no longer exists). Memory while reading: the tree plus the live slots' leaf
  hashes; sealed twigs' heaps are never held whole.
- `write_leaf_form_v2` writes the tree back in a hollow form (sealed twigs as leaf
  root + bits, mode 0; the open twig with leaves), digest included. This is the
  node's own base file; it restores into this tree but not into the full tree, which
  insists on leaves for any twig with live slots (proofs). n42-rs's checked v2
  reader/writer, undo records and leaf-form types were ported into `qmdb_compat.rs`
  unchanged (their diff applies cleanly on the shared base), so the full tree remains
  the reference implementation and every leaf-tree test compares against it.
- Undo: `apply_sorted_ops_recorded` returns gov5's `BlockUndo`; `apply_undo` truncates
  the block's appends (reopening twigs it sealed, whose heaps are retained for the
  last 256 seals), revives the deactivated slots, and restores the root byte for byte
  — the property that lets a candidate be priced by apply / root / revert without
  copying anything.
- The upper tree is cached and repaired along dirty paths; its depth follows the
  twig count exactly as the full tree computes it (a count crossing a power of two
  rebuilds it — the first version got that wrong and the test caught it).
- Proofs: only for keys in the open twig (`prove`); sealed twigs have no siblings.
  The archive RPC returns `None` for the rest, which is documented in the store.

Measured on the real export (release, `cargo test --release -p n42-twig-core --
--ignored a_real_leaf_form_export_rebuilds_its_root` with `N42_QMDB_LEAF_FORM`):
loaded and verified in **9.4 s**, root recomputed in 64 µs and equal to the
export's `0xa697c095…495d`, **RSS 681 MB**, a candidate of 300 appends priced and
reverted in 610 µs.

### The store moves one tree in place (`crates/n42-node/src/qmdb_state_root.rs`)

`Gov5QmdbStateRootStore` used to keep a positional base snapshot, reconstruct a full
tree from it and clone the cached tip per candidate. It now holds one `QmdbLeafTree`
and a position (`tree_at`, depth, and the undo records of the blocks applied to reach
it, up to 8,192). `move_tree_to(target)` walks the target's ancestry back to the
nearest block the tree can reach by reverting, reverts to it, then replays the
retained operations forward, root-checking each block against what was stored when
it was committed. A candidate is priced on its exact parent (revert/replay as needed,
apply under an undo record, read the root, revert); a commit leaves the tree at the
new block. A WAL append failure steps the tree back off the failed block. Persisted
checkpoint version 2 carries only the base identity and the retained blocks; the base
tree lives next to it as `<checkpoint>.base.qmdb`, rewritten in a background thread
from a copy of the tree every `N42_QMDB_REBASE_BLOCKS` (default 20,000) commits, so a
restart reads a ~320 MB base (chain 94, hollow form) and replays only the blocks above it; blocks below a
moved base are dropped at load. The constructors that take a positional snapshot
still exist (tests, small chains) and convert through the full tree.
`validate_persisted_blocks` is a depth-first replay with undo instead of one full
rebuild per stored block. Tests: the 15 existing store tests adapted (the tip-cache
metric now counts "tree already at the parent"), plus a graph walk (siblings, deeper
forks, back to the base and forward again) and a base-file round trip.

### The executor's rules that gov5's root depends on

Three things the bundle does not show, each found by a root mismatch:

1. **Slots changed and restored within a block** — n42-rs's finding (chain 94 block
   13,561,251). Ported: `crates/n42-execution/src/restored_slots.rs` wraps every block
   executor built from `N42EvmConfig` (engine tree, payload builder, direct callers)
   with a tracker that records, per slot a transaction changed, its value at block
   start, filed under keccak(parent hash ‖ tx hashes) in a bounded registry;
   `gov5_qmdb_operations_with_restored` adds a rewrite for every slot the bundle
   lost. Three real-execution tests (`crates/n42-node/tests/restored_slots.rs`) and
   six unit tests. Not covered: a change and restore inside one transaction (not seen
   on chain 94).
2. **Prague is active on chain 94.** gov5's chainspec has `pectraTime: 1746612311`
   (mainnet's Prague timestamp); the chain-94 genesis file the Rust side uses carries
   it under a name reth does not read, and devlog-142 kept Prague off to match gov5's
   `IsPrague == false`. The QMDB entry log says otherwise: every block writes the
   EIP-2935 parent-hash slot (`0x…2935` storage, e.g. slot 63,349,367 of block
   13,560,376 holds the snapshot head's hash) and the EIP-4788 pair. Block 13,560,376
   replayed 26 leaf operations without Prague and 28 with it; gov5 appended 29.
   `N42_GOV5_PRAGUE_TIME=<seconds>` now activates Prague in the node's chain spec
   (`apply_gov5_prague_time`, all three gov5 execution modes), and the consensus
   adapter fills the empty requests hash into the temporary Ethereum-normalised copy
   of a live gov5 header when Prague is active (`normalize_header`), so reth's header
   rule passes and its post-execution rule compares that hash with the hash of the
   requests execution produced — chain 94 produces none; a produced request is still
   rejected (test). The original sealed header stays hash-authenticated.
3. **The system caller leaf.** gov5's 29th leaf for block 13,560,376 was
   `47ba50c3…`, at the last slot of the block in key order. It is the account
   `0xffff…fffe`, written as nonce 0 / balance 0 / no code (`[0x00]`): gov5 runs the
   EIP-7002/7251 calls as messages from `SYSTEM_ADDRESS` (`SysCallContract`), Erigon
   loads the caller as a state object, the journal marks it dirty, and gov5's root
   computer writes every dirty account; reth's `SystemCaller` removes the address
   after each call. n42-rs found the same on the devnet.
   `with_gov5_prague_system_caller` adds it for Prague blocks; the state-root job
   decides Prague from the chain spec it is now given. (The two leaves I first took
   for "extra" were the next block's first two entries — the block boundary in the
   entry log is only visible from the sorted key order; `cmd/qmdb-slot-dump`,
   `cmd/qmdb-key-probe` and `cmd/qmdb-block-changes`, scratch tools in the gov5
   checkout, print slots with leaf hashes and live rows, key lookups, and a block's
   Erigon changesets with gov5 keys.)

### Offline replay (`n42-qmdb-replay`, new binary in `bin/n42-node`)

Opens the snapshot datadir read-only, loads the leaf form, and for each recorded
block executes it through `N42EvmConfig` over an in-memory bundle overlay (with the
replayed headers' hashes for `BLOCKHASH`), converts the bundle with the three rules
above, applies to the tree and compares with the header's `stateRoot`; gas used is
checked against the header as well.

```
n42-qmdb-replay --chain $A/genesis.json --datadir target/chain94-record/reth-replay \
  --leaf-form target/chain94-record/snapshot-13560375.leafform.qmdb \
  --range target/chain94-record/post-snapshot-13560370-13565343.n42frng \
  --genesis-range $A/genesis-range.n42frng --genesis-hash 0xa2d2ff5d… \
  --prague-time 1746612311 --report replay-report.jsonl --write-base replay-base-at-head.qmdb
```

Result (release build):

```
leaf form: block 13560375 0x0e37dae9… root 0xa697c095… twigs 30933 live 6157495 next_slot 63349357
  loaded and verified in 9.2s, RSS 700 MB
range: 4974 recorded blocks, replaying 4968 (13560376..=13565343)
replayed 4968 blocks (96522 txs, 81387 leaf ops, 4 restored-slot rewrites) in 5.0s:
  0 root mismatches; exec p50 0.06 ms p90 0.30 ms max 7.77 ms; root p50 35 us p90 69 us max 251 us;
  RSS now 783 MB peak 783 MB; tree live 6157495 next_slot 63430744 twigs 30973
base file written: … at block 13565343 0xffda999a… root 0x05dc2c52…
```

The final root and cursor equal what `qs-probe-linux.go` reads from the Go node's
chaindata at the same head (`root=05dc2c52…3028`, `next_slot=63430744`). The
per-block report is `target/chain94-record/replay-report.jsonl`.

**N achieved: 4,968 of 4,968 recorded blocks.** Per block: execution 0.06 ms p50
(the faucet txgen's blocks of up to 312 transactions reach 7.8 ms), root 35 µs p50,
69 µs p90; memory 783 MB peak for the whole run (tree + overlay). Four blocks needed a
restored-slot rewrite (13,561,251 among them). The base file the run wrote at the head
is 318 MB in the hollow form (2.35 GB as exported with every leaf); it reloads in
3.8 s at 676 MB RSS and recomputes the same root.

### Node wiring

`N42_GOV5_QMDB_EXECUTION=1` with `N42_GOV5_QMDB_LEAF_FORM=<export>` (and, as before,
`N42_GOV5_GENESIS_BOOTSTRAP`; optionally `N42_QMDB_BOOTSTRAP_BLOCK/_BLOCK_HASH/_ROOT`
to pin the expected head) takes the new path `load_gov5_leaf_form_execution_bootstrap`
in `bin/n42-node/src/main.rs`: base file present in the datadir → read it; else read
the export, check chain id, genesis, the expected head, the genesis prefix through
twig 0's leaf hashes (native vs replay-v2 profile), and write the base file so the
next start needs no export. The store gets the chain identity for its base files.
`Gov5QmdbStateRootStrategy` is installed as before; `N42_GOV5_STATE_ROOT_TRUST`
remains available but is no longer the only option.

## 2. Committee evidence (`crates/n42-consensus/src/committee_pool.rs`, new)

gov5's `blspool`, ported from n42-rs's checked port: keys from
`keygen(sha256(seed ‖ i))`, the linear ramp, the partial Fisher–Yates committee
seeded by `(number, hash)`, one signature of the summed secret scalars over
`number ‖ hash` under the proof-of-possession ciphersuite (`sign_h2_v4`), gov5's
`ConsensusEvidence` marshalling, `Blake3(marshal)` as the child's `parentBeaconRoot`;
`verify_parent_link` has `VerifyHeader`'s semantics (genesis parent → zero/none).
Configuration is read from `config.hotstuff.committeePool` of the genesis (chain 94:
200,000 keys, committee 512, ramp 1,000,000, seed `0x03c75de6…`); the pool is derived
once per process (24–25 ms on 16 threads).

- Verification hook: `HeaderValidator::validate_header_against_parent` under the
  Gov5 profile (`adapter.rs`), which reth's engine tree runs for every `newPayload`,
  catch-up and live alike: `pool.verify_parent_link(header.number,
  header.parent_beacon_block_root, parent.number, parent.hash(), parent.receipts_root)`.
  Failure: `ConsensusError` whose text contains `committee-evidence link broken`, an
  error log, counter `n42_gov5_committee_evidence_link_broken_total`.
- Proposer: `execution_bridge.rs` rebuilds the root from the parent's remembered
  native header and stamps it into the payload attributes; a build whose parent header
  is unknown to the wire registry is refused (`n42_gov5_committee_evidence_stamp_failed_total`)
  rather than stamped with zero. The leader stays disabled for trusted-root members.
- Tests: gov5's vectors (`testdata/gov5_committee_evidence.txt`: small pool and the
  chain-94 200,000-key pool byte for byte at block 13,013,133; scalar sum ==
  aggregate), a checked-in fixture of six raw chain-94 headers
  (13,560,375..13,560,380), and `tests/chain94_committee_evidence.rs` over the
  recorded range: **4,973 consecutive links (13,560,370..13,565,343) all verify**,
  each also shown to break under a one-bit receipts-root tamper; 1.28 ms per link in
  a debug build (two evidence builds, one BLS signature each).

## 3. EOF guard (`crates/n42-node/src/eof_guard.rs`, new)

Chain 94's gov5 chainspec has `eofTime: 1765238400` (active). revm has no EOF, so
under the Gov5 profile the state-root job is wrapped by `EofGuardedStateRootStrategy`:
before the root is computed it scans (a) create transactions whose initcode starts
with `0xEF00`, (b) bytecode the block deployed (`output.state.contracts` and post-state
code hashes), (c) code loaded for execution — touched accounts' pre-state code and
every call target's code resolved through the state provider; a lookup failure fails
closed. A hit makes `newPayload` fail with `EOF code is not supported by this node
(revm has no EOF): block N (hash): <sighting>; refusing to vote or propose` and counts
`n42_gov5_eof_blocks_rejected_total{stage=import}`; the consensus side then never sees
`execution validated` and does not vote. The payload builder filters `EF00` initcode
out of its transaction stream (`n42_gov5_eof_transactions_skipped_total`) and refuses
a built block that still carries one (`stage=build`). Inert on the Ethereum profile.
Eleven unit tests.

Scan of chain 94: state dump at 13,560,375 — 29 accounts with code, none `EF00`/`EF01`;
Code table of the node5 copy at 13,565,343 — 20 rows, none; recorded blocks
13,560,376–13,565,343 — 96,522 transactions, **0 create transactions**, 0 EOF initcode
(`target/chain94-record/eof-scan.log`). Not covered: code loaded transitively by a
contract that is neither a call target nor touched; with no EOF code in the state and
every deployment screened, such code cannot exist on chain 94.

## 4. Range pull when execution falls behind (`orchestrator/state_mgmt.rs`)

There is no outbound `bodies_by_range` client in N42-26 (the network layer only serves
it); the start-up catch-up that imported 1,639 blocks in 2.8 s is the hash-bound
`RequestGov5BlockByHash` whose staging walks parent hashes down to the durable
execution head. The stall in devlog-142 went through the other path:
`handle_commit_execution_timeout` → `initiate_execution_catchup_sync` → N42 state-sync
→ "peer does not speak N42 state-sync". Under the gov5 profiles every
`initiate_sync` / `initiate_execution_catchup_sync` caller now goes to
`initiate_gov5_execution_pull`: target = the newest authenticated block above
`execution_validated_head_view`, one outstanding pull per hash, retry and peer rotation
through the existing fetch machinery, re-armed on every view change (throttled to one
per 2 s) while the target exists. Log line `gov5 range pull started: execution behind
consensus reason=… target_view=… execution_head_number=…`, counter
`n42_gov5_execution_pull_started_total{reason}`. Five tests (real trigger through the
commit timeout, standard profile unchanged, no duplicate while in flight, target
selection, re-arm and rotation); the crate's 219 tests pass.

## Tests

`cargo test -p n42-twig-core -p n42-execution -p n42-consensus -p n42-node
-p n42-consensus-service` (`-j 16`, `--test-threads 16`): all green — twig-core 96
(9 new for the leaf tree, plus the ported undo and leaf-form suites), execution 60
(6 new: restored slots, tracking executor), consensus 240 lib + 2 + 12 + 67 (committee
pool vectors, chain-94 links, Prague fill-in, committee link semantics), node 196 lib
+ 3 integration (store graph walk and base file, system caller leaf, EOF guard,
restored slots), consensus-service 219 (5 new for the range pull). Ignored
measurement tests: the real leaf-form export (`N42_QMDB_LEAF_FORM`) and the recorded
range (`N42_CHAIN94_RANGE_FILE`), both run and reported above.
`cargo check -p n42-node-bin` clean.

## What remains, and why

- **The live participant test with recomputed roots is not done** — the fleets were
  paused. Procedure below.
- **Leader duty.** The member still declines to build (`with_leader_disabled`): with
  roots recomputed the payload builder could seal, but under Prague it would stamp a
  `requestsHash` that gov5's miner leaves out, and the H2 wire normalisation of a
  self-built header has not been exercised. The committee root stamping is in place.
- **`N42_GOV5_PRAGUE_TIME` is an operator setting**, not read from the genesis file
  (`pectraTime` is gov5's name). Wrong or missing, the first block's root mismatches
  and the node refuses it — loud, not silent.
- **Change-and-restore inside one transaction** and **an account revm drops entirely**
  (info unchanged, storage nets to zero) are not modelled; neither occurred in the
  4,968 blocks.
- **Base-file rebase is on a commit counter** (20,000 blocks); the WAL is not trimmed
  at rebase (the loader drops blocks below the new base instead). A `persist_now` at
  shutdown would make restarts instant; today a restart replays the retained blocks
  from the last base.
- **Proofs in sealed twigs** are unavailable from the split tree; the archive RPC
  answers `None` for them.
- The gov5-side scratch tools and the `--leaf-form` exporter live in the gov5
  checkout, not in this repository.

## Live test procedure (for the operator, after the stress test)

Fleet margin is 1: take slot 5 for at most 15 minutes, never while another holder of
key 5 runs, never touch nodes 0–4, node6 or the n42-rs processes, never `kill -9`.

```bash
R=/data/blockchain/mixed-fleet/n42-26-qs; source $R/env.sh
cd $WT && cargo build --release -p n42-node-bin -j 32       # this branch
# the leaf-form export is at $WT/target/chain94-record/snapshot-13560375.leafform.qmdb
cd /data/blockchain/scripts-qs && source ./qs-env.sh && qs_stop_node 5   # "stopped clean"
$R/run-node.sh participant-qmdb            # new mode: fresh datadir $R/rust-qmdb/reth, roots recomputed
tail -f $R/logs/participant-qmdb.log
```

Expect, in order: `QMDB split commitment rebuilt from the leaf form and verified
against its root` (≈ 10 s, `live=6157495`), `QMDB base file written`, `Prague activated
for gov5 execution`, the gov5 catch-up (`authenticated Gov5 catch-up parent reached
new_payload(Valid)` up to the fleet head — every one of those blocks now had its root
recomputed and compared; a mismatch shows as `QMDB block … root mismatch` and the
payload is Invalid), then `import-gated vote: execution validated, sending vote`.
Success criteria for the ≤ 15 min window: `grep -c "root mismatch" log` = 0,
`h2_shadow_rejected=0`, `n42_qmdb_commit_outcomes_total{outcome="committed"}` growing
with `n42_qmdb_trusted_state_roots_total` absent, heads and state roots equal on 20117
vs 20012 at three sampled heights, validator 5's bit in the committed QCs
(`tools/qc-bitmaps.py 20013 100`), `n42_gov5_committee_evidence_link_broken_total` = 0,
`n42_gov5_eof_blocks_rejected_total` = 0. For the range-pull change: `kill -STOP` the
member's process for ~30 s and `kill -CONT` it; expect `gov5 range pull started:
execution behind consensus` and a run of `new_payload(Valid)` back to the head with
no `does not speak N42 state-sync`.

Then hand the slot back:

```bash
$R/stop-node.sh participant-qmdb
cd /data/blockchain/scripts-qs && set -a && source /data/blockchain/faucet.env && set +a && \
  QS_ROOT=/data/blockchain QS_VALIDATORS=$HOME/qs-validators.md QS_SEED=/data/blockchain/qs-era-linux \
  QS_BASE=/data/blockchain/qs-replay-linux QS_UDP_BASE=31000 \
  ./roll-one-node.sh --node 5 --bin /data/blockchain/bin/n42 --txgen-max 0
```
