
<!-- docs/NATIVE_FLEET7.md lines 1-280 at 466c1839791afadba7f1dd4d48c5518440dcafba -->
# The all-Rust seven-node fleet on the native chain

gov5 runs the flagship native chain, `mainnet_qmdb_staggered` (chain 94), on
seven HotStuff nodes. This is the same fleet shape built entirely from this
repo: seven independent members, each an `n42` execution layer driven by its own
`h2_validator` over the Engine API, wired into a static full mesh with no
discovery and no devp2p.

`scripts/fleet7.sh` runs it; `scripts/fleet7-env.sh` holds every launch argument
and is the only place any of them is written down. That separation is gov5's
lesson, already paid for once in `scripts/qs/qs-env.sh`: their Windows fleet
declared its environment inside the deploy script, a rolling restart driven by a
*different* script silently dropped one lever, and the whole transaction index
went with it.

```bash
cargo build --release -p n42 -p n42-h2-node --bins --examples
scripts/fleet7.sh up --fresh     # seven nodes from genesis
scripts/fleet7.sh status         # heights and head hashes, and whether they agree
scripts/fleet7.sh watch 300      # a measured window: blocks, memory, disk, loopback
scripts/fleet7.sh roll 3         # stop and restart one node; check it rejoins
scripts/fleet7.sh down           # SIGTERM, and wait
```

Data lives under `/data/blockchain/rust-fleet7` — deliberately not `/tmp`, which
on this host is a 69 GB tmpfs, where a datadir *is* resident memory.

## Where it stands today: 365,399 TPS (2026-09-05)

The record on this fleet (round 39, `loop53Q300a`, pacing 300; pacing 350 read
357,292 and 357,651 on window 1 in the legs around it, pacing 400 349-353k):

| window | TPS | blocks | cycle | occupancy |
| --- | ---: | ---: | ---: | ---: |
| win1 | **365,399** | 71 | 0.423 s | 94.7% |
| win2 | **343,885** | 64 | 0.469 s | 98.9% |
| win3 | 271,655 | 69 | 0.435 s | 72.5% (the 48M flood running out) |

Seven nodes, every follower executing every transaction and computing the
QMDB root, senders recovered on every node, bodies over direct push with
GossipSub as the fallback, 163,000 transactions a block at the 480M gas
ceiling. Same-binary spread on window 1 is 1-4%, so nothing under ~5% is a
result. The configuration, all in the environment of `fleet7-bench.sh`:

```bash
F7_LEADER_TENURE=16 F7_INGEST=1 F7_INGEST_ALL=1 F7_NO_TX_GOSSIP=1 N42_TX_INGEST_ASYNC=1 \
F7_DIRECT_PUSH=1 F7_BLOCK_INTERVAL_MS=250 F7_SKIP_STALE_CHECK=1 N42_TX_QUEUE=1 \
N42_TX_INGEST_RECOVER_NICE=10 N42_TX_INGEST_RECOVER_PARALLEL=16 N42_TX_INGEST_DIRECT=1 \
N42_FAST_TRANSFER=1 N42_FOLLOWER_DIRECT_IMPORT=1 F7_SENDER_CACHE_MULT=4 \
N42_TX_QUEUE_BATCH=1024 N42_TX_QUEUE_DRAINER=1 N42_BUILDER_PULLER=1024 \
N42_TX_INGEST_RECOVER_PARALLEL=20 N42_TX_QUEUE_RUN=64 MALLOC_CONF=thp:always N42_FOLLOWER_PARALLEL=1 \
TOKIO_WORKER_THREADS=8 F7_EL_EXTRA="--builder.interval 60 --builder.deadline 3" F7_FLOOD_WINDOW=6 F7_BLOCK_INTERVAL_MS=300 \
scripts/fleet7-bench.sh --tag <tag> --gasceil 3423000000 --senders 6000 --pertx 8000 --conc 64 --rpcbatch 500
```

How it got here, in the order the rounds found it (each has its section
below): the RPC block cache evicting the page cache (round 34, 4 blocks);
the builder's per-transaction state read and clone (round 36); the full
block's refusal tail and the own-block conversion (round 37: 190k -> 233-239k);
the plain-transfer path without the interpreter, which only reached the
builder once the payload service used the node's EVM factory (round 37:
248-255k); the follower's direct import (round 38: 262-269k); batch 500 on
the supply (supply session: 277k); the queue's prune, the pool puller and
the re-ask fraction (round 39: 288k, then 302-306k). Measured and rejected on the way: 24 recovery slots (slower and
sloped), a parallel account prefetch (cold reads are 17 ms a block), a
tighter view timeout, jemalloc knobs, RocksDB memtable size.

The cycle is now set by the followers -- publish -> receive 74 ms,
receive -> vote 447 ms, vote -> decide 120 ms -- not by the leader's build,
which the build-ahead hides. The next 300k is a follower-side job.

## Round 40: the 0x50 (Ed25519) transaction on the fleet (2026-09-07)

`docs/ROADMAP_ED25519_TX.md` phase 1. The node runs on `N42Primitives`
(`crates/n42/tx-types`), the fleet7 genesis files carry `altSigTx: true`,
and `F7_FLOOD_ALG=ed25519` makes the flood sign 0x50 transfers. loop64,
the record configuration (pacing 300, 20 slots, run 64, huge pages for the
heap, parallel follower, 8 tokio workers, pertx 8000), A-B-A-B with a
warm-up leg:

    leg  algorithm  win1     cycle    occ    win2     cycle    win3     cycle
    W    secp       269,666  0.600 s  99%    222,718  0.732 s  265,995  0.613 s   (warm-up, void)
    A1   secp       355,559  0.349 s  76%    334,650  0.330 s  325,169  0.395 s
    B1   ed25519    345,726  0.441 s  94%    --  chain halted at block 222 (below)  --
    A2   secp       364,718  0.353 s  79%    339,397  0.330 s  323,338  0.380 s
    B2   ed25519    350,713  0.423 s  91%    295,593  0.492 s  249,305  0.640 s
    B3   ed25519    347,718  0.469 s  100%   294,841  0.526 s  221,277  0.625 s

The secp256k1 legs reproduce the record with the new node (A2 364,718
against 365,399): the type swap costs nothing. The Ed25519 legs read the
same window 1 (345-351k, inside the spread) with the bound moved: the
blocks are full (91-100% occupancy, 61/71 and 64/64 full) and the cycle
is 0.42-0.47 s, where the secp legs run 76-79% full at 0.35 s. The supply
side did what it was built to do -- node0's ingest at 20 slots:

    algorithm  rate       busy us/tx  slots busy  altsig batches
    secp       333-345k   54-56       93%         --
    ed25519    336-345k   28-31       48-53%      64 a batch

-- half the CPU a transaction, half the slots idle, the flood throttled by
the ingest gate (pool at its 410k high-water mark) rather than by
recovery. What binds now is the follower. Full-block direct import, node0
medians:

    algorithm  senders  exec  root  total   senders cached
    secp       58 ms    79    11    251 ms  153k / 163k (94%)
    ed25519    107 ms   92    23    336 ms  102k / 163k (63%)

The 0x50 sender cache (`N42_ALTSIG_SENDER_CACHE`, 2^20 entries) is a
direct-mapped fixed cache and a million in-flight transactions collide in
it: 37% of a block's senders miss and are verified again on the follower,
in batches, 50 ms of the 107. A 0x50 block is also 23 MB against 19
(141 B a transaction against 110), which shows in the exec/root numbers
and in the second and third windows, which fall to 0.63 s cycles where
the secp legs hold 0.38-0.40. loop65 runs the cache at 2^22 and the batch
at 128.

**B1 halted at block 222.** Six followers imported it; node0, the leader
of the next view, logged `parallel import phases number=222` and then
nothing -- no error, no `direct import` line -- while its build-ahead for
view 224 had just started on the same execution layer. Every later view
timed out waiting for it. It did not recur in B2 or B3; a watchdog now
dumps every thread's state and a perf sample if a chain stalls for 20 s
during a round.

## Round 41: the sender cache at 2^22 -- 396,601 and 385,742 (2026-09-07)

loop65, the loop64 configuration with `N42_ALTSIG_SENDER_CACHE=4194304`
(C legs) and, in C2, `N42_ED25519_BATCH=128`; A3 the secp256k1 bookend,
B4 the Ed25519 baseline with the 2^20 cache:

    leg  configuration              win1     cycle    occ   win2     win3
    C1   ed25519, cache 2^22        265,085  0.612 s  100%  233,513  331,350   (first leg after the rebuild: warm-up)
    A3   secp                       354,604  0.345 s  75%   338,484  321,031
    C2   ed25519, cache 2^22, b128  396,601  0.411 s  100%  342,288  244,492 (52%: the 48M flood ran out)
    C3   ed25519, cache 2^22        385,742  0.423 s  100%  -- halted at block 222, as B1 --
    B4   ed25519, cache 2^20        353,153  0.462 s  100%  298,568  226,739

**396,601 and 385,742 on window 1, every block full**, against the
365,399 record: the larger cache takes the follower's senders phase from
107 ms to 33-34 ms (141k of 163k senders cached, 87%) and the full-block
direct import to 220-222 ms, under the secp256k1 legs' 251. The batch of
128 is not distinguishable from 64 at this spread. The flood delivered
its 48M transactions in 148 s, so window 3 of a 400k round is starved;
loop66 runs pertx 10000.

Where it binds now is the chain's cycle at 0.41 s with 163k a block
(~397k/s), the phase-2 question. C3 halted the way B1 did -- node0, the
next leader, stuck after executing block 222 with every thread of its
execution layer in futex wait and no log line -- so the halt is real and
recurring (2 of 7 Ed25519 legs), and the node now publishes the stage of
its import and its build for a watchdog to log when it happens again.

## Round 42: phase-2 levers at full 0x50 blocks (2026-09-07)

loop66, the round-41 configuration (Ed25519, cache 2^22, batch 128) with
pertx 10000 so the flood outlasts the windows, and the phase-2 levers one
at a time. Every block full in every leg:

    leg  change                         win1     cycle    win2     cycle    win3     cycle
    D1   (first leg after the rebuild)  358,253  0.455 s  282,430  0.577 s  304,252  0.400 s
    D2   gas ceiling 5.13B (245k/block) 407,485  0.600 s  325,844  0.750 s  309,342  0.790 s
    D3   pacing 250                     402,028  0.405 s  336,041  0.484 s  233,624  0.370 s (53%)
    D4   baseline (163k, pacing 300)    396,619  0.411 s  342,283  0.476 s  249,924  0.345 s (53%)
    D5   gas ceiling 5.13B              423,775  0.577 s  334,133  0.732 s  228,067  1.072 s

- The baseline reproduces round 41 to four digits (D4 396,619 / 342,283
  against C2 396,601 / 342,288).
- **Pacing 250 is null** (402k against 397k, inside the spread): the gate
  is not what holds the cycle at 0.41 s; the work per full block is.
- **Bigger blocks buy 3-7% on window 1 and lose the round**: 245k a block
  reads 407k and 424k on window 1 at a 0.58-0.60 s cycle, then 0.73-0.79 s
  on window 2 and 1.07 s on window 3 in D5 -- 34 MB bodies and 382 ms
  follower imports (convert 90, senders 75, exec 117, root 21) degrade
  faster than they gain. 163k stays.
- Window 3 at 53% occupancy in D3/D4 is the flood: its closed loop slows
  from 390k/s to 250k/s as the pool sits at its high-water mark, not the
  chain (cycle 0.35-0.37 s there).
- No halt in five legs; the watchdog stayed silent.

Where a full 163k block's 0.41 s goes, from the C2/D4 logs: the leader's
build is 292-313 ms (execution 202-244 ms serial through the transfer
path, finish 55, assemble 8) and its own import by header ~50 ms; a
follower's direct import is 220-222 ms (convert 50-56, exec 72 in 44
groups with a 38 ms merge, senders 33, root 15, header/checks/hashed
~15) plus 17 ms to decode the body; publish to receive is 30 ms and vote
to decide 10-13 ms. The leader path is the long pole -- the same as the
roadmap's 3A: the builder executes serially where the follower already
runs 44 groups in parallel.

## Round 43: the parallel builder, and the flood that paid 13,000 recipients (2026-09-07)

Roadmap 3A: the leader executes a block's transfers in parallel instead of
serially. Four versions were needed, each measured on the fleet, and the
measurement found a defect in the flood that every earlier round ran on.

**The builder.** `N42_PARALLEL_BUILD=1` (`payload.rs`, `parallel_transfer::
execute_for_build`, `graft_bundles`, `append_reverts`):

- v1 (98061d2d1) grouped by connected component, as the follower does, and
  committed each transfer's state into the builder's `State`. Slower than
  serial: 292-342 ms against 192 for a full block (loop67W). A block of
  random transfers is ~9 giant components; and the microbenchmark
  `bench_build_run` (163,000 transfers, 6,000 senders) split the serial
  transfer path into 112 ms of execution, 110 ms of per-transaction commits
  and 55 ms of transition merge -- half the cost is revm's `State`.
- v2 (292f282b8) groups by sender only (a transfer only adds to its
  recipient; additions commute) and grafts each batch's bundle straight
  into the builder's cache and bundle, adding accounts two batches touched
  and taking the few the block already holds through a commit; reverts
  join the block's set once the bundle is taken. Microbenchmark: 105 ms
  parallel + 71 graft + 8 merge against 245 serial.
- v3 (0a0840775) converts the pool transactions on the batch threads
  (55-100 ms of the builder's thread otherwise) and runs the batches on
  their own 16-thread pool (`N42_PARALLEL_BUILD_THREADS`): on the fleet the
  batches had queued behind the global pool (execution 25-466 ms a block).
- Fixes from the legs (db5696983, 04a3eb31c): an account the block touched
  again after the graft (a later sender, a withdrawal) got a second revert
  from the merge, and two changeset entries for one account in one block
  fail persistence's history index (`UnsortedInput`) -- a leader died
  mid-round; the merge's revert is dropped for the graft's. When the base
  fee had run past every candidate's fee the batches skipped all 163,000
  and the serial loop then refused each with a full validation (5.2 s
  builds of empty blocks); a sender's group now stops at its first refusal
  and skipped candidates go back to the queue. The block is laid out in
  candidate order.

The equivalence test `build_run_matches_serial` compares the grafted
bundle with the serial executor's account by account, originals, statuses
and reverts included.

**On the fleet (2,000,000-recipient flood, round-41 configuration), the
builder's full-block build went from a 285 ms median (loop65C2) to 220-243
ms, and the chain's cycle from 0.405-0.417 s to 0.375-0.400 s -- and TPS did
not move:**

    loop70  P1 398,821 / 293,618 / 215,970   S1 401,905 / 347,722 / 232,312
            P2 393,518 / 323,736 / 247,261   S2 402,040 / 353,151 / 233,623
            P3 403,370 / 314,040 / 271,494
    loop71  P1 397,620 / 275,916 / 255,838   S1 396,616 / 331,421 / 238,113   (candidate order)
            P2 398,573 / 344,517 / 190,457   S2 391,162 / 274,544 / 282,522
            P3 405,593 / 307,646 / 266,222

Window 1 equal within the spread (P 394-406k, S 391-402k) at a shorter
cycle but 92-98% occupancy; window 2 lower by up to 10%. Two reasons, both
measured: every execution layer is at 27 of its 32 cores under the flood
(19 of them tokio threads doing ingest and Ed25519 verification), so the
batches get what is left; and the followers imported a parallel-built
block in 332 ms against 244 (execution 129 against 85, merge 59 against
28, senders 52 against 38). The second was traced to the blocks
themselves: a parallel-built block touched 24,000 distinct recipients, a
serial-built one 13,000, with the same 376-392 senders in runs of 64.

**The flood.** 163,000 transfers to 13,000 recipients is not the shape the
harness claims (`recipient()` is documented as writing 163,000 accounts a
block). The ingest path derived a transfer's recipient from the sender's
index within the worker's own part, not the global index the JSON-RPC
path uses, so the 64 workers' senders at one local index paid the same
recipients at the same nonces. Fixed in 9a40a002a; `blockmix.py` on a
live block reads 141,000 distinct recipients now. **Every number through
round 42 -- the 365k and 396k records included -- was measured on blocks
that touched ~13,000 recipients**, and the follower's import, the roots
and persistence all scale with the accounts a block touches. (A prime
recipient spread, loop72, changed nothing: the multiplicative hash is a
bijection either way.)

With the flood fixed, the serial builder's first legs (loop73 W/S1) read
114-168k TPS at a 0.97-1.43 s cycle, every block full. A full block is now
`created=14,196 updated=132,803`; the leader's build is 1,054 ms (serial
execution 336, finish 338, assemble 35 with a 191 ms QMDB root) and a
follower's import 736 ms (execution 349 in 212 groups of which the merge
into the block's state is 265, root 170, hashed 73, convert 46, senders
33). That is the chain's honest cycle at 163,000 scattered transfers, and
the parallel builder and the follower's fold are now measured against it
(loop73, and `N42_FOLLOWER_GRAFT=1`, 7b99e17ae, which grafts the
follower's groups the way the builder does).

**loop73, the fixed flood, serial against parallel builder (S-P-S-P-S after a
warm-up):**


<!-- docs/NATIVE_FLEET7.md lines 2079-2099 at 466c1839791afadba7f1dd4d48c5518440dcafba -->
**loop141 (2026-09-12 04:20-04:42 EDT): the pacing under seal-first -- 350 ms holds, 333,416 on
window 1 and the best round, 22,518,476.** The loop140 binary (seal-first on, gated bench
genesis, record configuration) with the block interval swept, 450 bookending 350 and 300:

    leg     pacing  win1          win2     win3     blocks/window   round total   cycle median (win1)
    W       450     312,199 (58)  235,623  167,514  58 / 45 / 33    21,472,500
    P350a   350     333,416 (62)  226,815  132,927  62 / 44 / 25    20,799,984    420 ms (p25 376, p75 495)
    P300    300     216,235 (40)  247,549  209,062  40 / 48 / 40    20,194,184
    P350b   350     328,018 (61)  220,073  202,273  61 / 42 / 40    22,518,476
    P450    450     307,401 (58)  219,253  182,784  58 / 41 / 36    21,289,172

With the follower's import beside the loop and the leader sealing ~300 ms after the previous
block, 450 ms of pacing was the cycle (0.517 s on both 450 legs); at 350 the chain runs at a
0.484-0.492 s window-1 cycle (a 420 ms median block) and reads 328-333k, +6-7% over 450. At 300
it collapses as it did before deferred execution (loop92, round 43): a pacing tighter than the
block's actual cycle makes the leader run past the stragglers and every handover stall. P350a's
third window (25 blocks at a 1.2 s cycle with the flood still delivering 190-265k/s) is the
late-window memory phase, not the pacing: P350b held 40 blocks there. Adopted: the bench's
default pacing is 350 ms (`fleet7-bench.sh`; the launchers' `PACE`). Records: window 1 333,416
(P350a), the round 22,518,476 (P350b).

