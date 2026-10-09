# QS 7-node fleet handover -- 2026-09-12 09:00 EDT

State of the self-developed N42 chain's seven-node TPS campaign when the user
paused the session. Detail and every prediction live in
`docs/QS_BLOCK_TIME_BUDGET.md` (sections 6ba-6bh cover this handover's span);
this file is the entry point. No keys in here.

## Paused 2026-09-14 04:40 EDT (read this first)

- **Box:** nothing of gov5 runs or is queued: no claim, no fleet, no runner, no chain
  script. BOX-NOTE-gov5.txt says so. The box is shared by turns with n42-rs (user's
  instruction, 2026-09-14).
- **Measured today.** 35zzo re-run (r74): B mean 90.6k (35zzl 83.7k), A1 62.3k, A2 lost
  to a foreign cargo check. 35zzp (r74 + BLAKE3 tx root), stopped at the pause: A1 68.2k,
  B1 win1 125.9k at 1.277 s. 35zzq (r75, deferred execution) has not run. Budget doc 6bi
  and 6bj have the tables.
- **The open problem is the second window of every B leg**: 113-126k at 1.3-1.4 s, then
  52-68k at 2.4-3.2 s. The heap reaches GOMEMLIMIT 10 GiB (GC every ~1.1 s) and the 20 s
  history fold holds the MDBX writer up to 8.5 s. Block cache and hint queue ruled out.
- **On main, in no fleet binary yet:** `a8ab0c61` write probe counts rows once;
  `f0603e82` fold prepared outside the write transaction; `15710ab9` tx lookup tail
  bounded by 1M transactions with a draining sealer. Harness bug: `bench-7node.sh`
  rebuilds `QS_EXTRA_ARGS`, so `--mobileverify.packet-window 8` never reached the nodes
  (default 256, ~1 GB); per-node flags go in `QS_NODE_EXTRA`.
- **To resume:** in `/data/blockchain/gov5-work`, re-run 35zzp then 35zzq:
  `setsid nohup bash ./chain-35zzp.sh >> wr-logs/chain-35zzp.out 2>&1 </dev/null &` and the
  same for `chain-35zzq.sh`. Both wait for any rbtc replay, yield to a waiting n42-rs
  runner (up to 30 / 20 min), claim before reseeding, reseed, install n42-r74 (q installs
  n42-r75) and launch. 35zzp's aborted logs are in `wr-logs/r35zzp-paused/`, so the re-run
  writes a fresh `r35zzp.log`. Then register a round per lever above (fold; heap headroom:
  packet window + tail) on top of whichever of 35zzp/35zzq wins; each needs a new binary
  (build before claiming, not while another driver measures).
- **Coordination lessons:** an rbtc cold replay and a CI cargo test both ran without a
  claim; the chain scripts now wait for rbtc by process. Do not compile while another
  driver's runner waits for load < 8.

## Update 2026-09-13 (read this before section 0)

- A second full audit (four read-only reviews plus n42-rs's last three
  days) led to **n42-r74 = `6445f1bf`**: duplicate work removed on the
  follower import, the leader's seal and push, the ingest and RPC paths;
  the r73 deferred check fixed (it would have failed every non-empty
  block: senders were never recovered); deferred commit retries made a
  set; checked evidence withdrawn on import failure. Details and
  predictions 70-72: `QS_BLOCK_TIME_BUDGET.md` 6bi-6bk.
- **35zzm and 35zzn are superseded** (never run on r73). Queued instead
  (launched 2026-09-13 17:41 EDT, waiting for the box): `chain-35zzo.sh` (r74, 35zzl config), then
  `chain-35zzp.sh` (+ tx root gate), then `chain-35zzq.sh` (+ deferred
  execution, on n42-r75); each waits for the previous ROUND line, the box claim
  protocol and three quiet checks, reseeds, installs n42-r74, launches.
  Start all three with `setsid nohup bash ./chain-35zzX.sh > wr-logs/chain-35zzX.out 2>&1 </dev/null &`.
- 2026-09-14 00:16 EDT: 35zzo attempt 1 was aborted because an unclaimed rbtc cold replay
  shared the disk (budget doc 6bi). The queue is now `chain-35zzo2.sh` (re-run), then
  `chain-35zzp.sh`, then `chain-35zzq.sh`; all three also wait for the rbtc supervisor
  (`cold-replay-20260914/run_replay.py`) to exit.
- 2026-09-14 02:15 EDT: the 35zzo re-run gave B mean **90.6k** (35zzl 83.7k, +8.3%), A1 62.3k;
  A2 was lost to the memory watchdog when a foreign cargo check took ~48 GB. First windows run
  113k at 1.43 s, second windows fall to 68k at 2.40 s: the heap reaches GOMEMLIMIT 10 GiB
  (GC every ~1.1 s) and the 20 s history fold holds the writer up to 8.5 s. That slide is the
  next lever (budget doc 6bi). The box is shared by turns with n42-rs now: 35zzp/35zzq wait
  for a waiting rust runner's claim to come and go.
- Still open from the audit (not in r74): QMDB eviction one block behind
  (the next block re-reads its hot set from MDBX; needs the dead-row
  reclaim in `deactivate` for resident-but-flushed slots); the miner tree
  sharing the live index (~0.8 GB a node); QMDBUndoWindow v3; the history
  fold (bench could run with `N42_NO_HISTORY_INDEX=1`); the leader's
  handover reload from disk (1 block in 4); the deferred path reusing the
  build's receipts root/bloom in the write; the 50 ms sleep before each
  Proposal once deferred execution is on; the mobileverify packet being
  built from a read log the parallel fill does not record; txflood
  sending hints to the node it RPC-submits to.

## 0. In one screen

- **Best measured**: round 35zzl (n42-r70 + history fold every 20 s): B mean
  **83.7k TPS**, best window **90.3k**, A mean 54.8k. 35zzm's void attempt 1
  (same effective config) repeated a 90.9k B window.
- **Code on origin/main** through `b8a8ae86`. The deployable binary is
  **n42-r73** (`b15cd2b7`): r70's levers + BLAKE3 binary transactions root +
  deferred execution, both behind timestamp forks that are OFF unless gated,
  plus the fixes from a four-way audit.
- **Queued but NOT run**: 35zzm (r73, BLAKE3 tx root, prediction 68) and
  35zzn (r73, tx root + deferred execution, prediction 69). Chain scripts are
  stopped; nothing starts on its own. gov5 holds no box claim.
- **Largest open lever**: deferred execution (header N carries N-1's execution;
  the vote no longer waits for the import). Implemented, unit-tested against
  the n42-rs cross-client vectors, never run on the fleet.
- **Read first before resuming**: sections 5 (how to run), 6 (what n42-rs hit
  with the same rule), 9 (rules).

## 1. Scope and terminology

- **Native N42 chain** (`mainnet_qmdb_staggered`, chain id 94): HotStuff-2,
  QMDB state = a BLAKE3 binary forest. The MPT has no place in its design;
  levers for it are binary-tree / BLAKE3 levers. Its transactions root was
  still the keccak MPT (a73a7258's `UseEthereumTxRoot`, a transition leftover)
  until the `txRootBlake3Time` gate.
- **eth-el**: the Ethereum-compatible execution layer (MPT, Engine API,
  eldevp2p). Separate code paths; nothing in this campaign changes it. Both
  bench env gates refuse to act on a non-native chain (`nativeQMDBChain`).
- **n42-rs**: the Rust client of the same native chain, driven by its own
  session on the same box (fleet7, ~300k TPS at its block shape).

## 2. Where things are

| what | where |
|---|---|
| worktree (work only here) | `/data/blockchain/gov5-work/wt-r27`, push with `git push origin HEAD:main` |
| main checkout (do not touch) | `/home/n42/src/n42/N42-gov5` (another session's uncommitted work) |
| binaries | `/data/blockchain/gov5-work/n42-r66 .. n42-r73`; `n42-r35` is the fleet slot the runners exec (currently r71; the chain scripts copy r73 in) |
| runners / chain scripts | `/data/blockchain/gov5-work/run-r35zz?.sh`, `chain-35zz?.sh`, `abort-round.sh <tag> <reason>` |
| harness env | `/data/blockchain/scripts-qs/qs-env.sh` (RPC 20012+i, pprof 6090+i), `seed-7node-nolaunch.sh` (reseed from `qs-era-linux`, reflink, 16 GB a node) |
| logs | runner `wr-logs/r35zz?.log`; nodes `qs-node*/log/n42.log` (+ rotated .gz), **deleted by every reseed** |
| saved per-minute summaries | `wr-logs/r35zz{i,j,k,l}-perminute.txt`, `r35zzm-attempt1-perminute.txt` |
| log analysis helpers | `scripts/qs-analysis/` (this commit): `perminute.py <since> [out]`, `import_breakdown.py <since> <until>`, `cycle.py <since>`, `leader_phases.py <since>`, `leg_compare.py <since> <until> <txmin>`, `view-timeline.py <since> [txmin]`; since = `'YYYY-MM-DD HH:MM'` |
| offline replay | `scripts/qs-replay/replay-run.sh` on a reflink copy of a FLEET node dir (the era seed has no QMDB applied marker) |
| box protocol | `wr-logs/BOX-CLAIM-PROTOCOL.md`; gov5's note `wr-logs/BOX-NOTE-gov5.txt` |
| cross-client | n42-rs `docs/PHASE_D_DEFERRED_EXECUTION.md` (sections 8-9 agreed, 14 gov5 tx-root proposal, 15 gov5 status, 16 their audit); `wr-logs/NOTE-deferred-execution-{for,from}-gov5.md` |

## 3. Results since the fresh-dirs baseline

All rounds: fresh reseed, 32 Block-STM workers, leader tenure 4, 163k-transfer
B blocks, 23k A blocks, windows in thousands of TPS (win1/win2).

| round | binary: the one change | warmup | A1 | B1 | B2 | A2 | B mean | verdict |
|---|---|---|---|---|---|---|---|---|
| 35zzg | r64 baseline | 79.4/70.6 | 45.3/42.3 | 70.6/64.9 | aborted (one undecodable QC in extra) | - | 67.8 | Seal self-check added (r66) |
| 35zzh | r66 FinalizeTx flag-only fast path, arena nil-fill | 76.1/65.2 | 27.8/41.9 | 73.4/65.2 | 65.2/cut | 44.6/44.2 | 68.0 | P63 falsified: aimed at the wrong costs |
| 35zzi | r67 QMDB root applies the block under the tree's leaf batch | 78.5/70.6 | 49.1/45.7 | 65.2/48.9 | 54.3/40.8 | 40.4 | 52.3 | P64 phase held (finalize 226->164 ms); B legs decayed on the MDBX writer lock |
| 35zzj | r68 arena free list keeps the largest arenas | 78.8/70.6 | 48.0/44.6 | 70.6/68.4 | 73.4/70.6 | 44.6/40.0 | 70.7 | P65 held (executorMs 58->0) |
| 35zzk | r69 delta-credited recipients prefetched across workers | 76.1/70.6 | 50.3/46.9 | 78.8/69.5 | 76.1/67.9 | 46.5/43.8 | 73.1 | P66 throughput held; phase -25 ms, not -50 |
| 35zzl | r70 + `N42_HISTORY_INDEX_INTERVAL=20s` | 81.5/70.6 | 56.4/53.0 | 86.9/76.1 | **90.3**/81.5 | 57.9/51.8 | **83.7** | P67 held: block-write begin-wait p90 2.2 s -> 0 |
| 35zzm #1 | r71 tx-root gate (never active) | 78.8/72.9 | 56.4/52.6 | 90.9/(cut) | aborted for the audit | - | - | void: gate set in wall-clock time (section 9) |

**Where a 163k block's time goes now (35zzl, per full block; ranges = first to
fifth flood minute):**

| side | phase | ms |
|---|---|---|
| follower import | total | 750 -> 1040 (leg median ~1000) |
| | sender recovery (hinted) | 50 -> 110 |
| | executor setup | 0 |
| | Block-STM exec | 200 -> 330 |
| | apply MVS | ~25 |
| | finalize (QMDB apply 26-31 + the balance-increase fold) | 120 -> 165 |
| | body (`ValidateBody`) | ~100 = the keccak MPT tx root (35zzm #1 profile: 2.36 s of 25 in `TxRoot`->`DeriveShaErigon`); ~6 ms under the r73 gate |
| | write (+ writer-lock wait, p90 0 with the 20 s fold) | 150 -> 240 |
| leader | parallel fill / assemble / finalize / push / write | ~480 / ~260 / ~245 / ~180 / ~250 |
| cycle | chained seal->seal / seal->QC / QC->next seal / handover (1 in 4) | ~1650 / ~1400 / ~240 / ~3200 |

Exec and recovery grow over a leg with the box's load (seven 8 GB heaps near
GOMEMLIMIT 10 GiB, eight generators); the write-lock decay that dominated
35zzi is gone with the 20 s fold.

## 4. Code landed in this span (origin/main)

| commit | binary | what |
|---|---|---|
| `9ecfa3be` | r67 | `QMDBRootComputer.ComputeRoot` -> `Tree.ApplyOps` (leaf batch: SIMD leaf hashes, one fold per touched twig level); 31k ops 92 -> 18 ms |
| `be70215c` | r68 | executor arena / result free lists replace their smallest entry instead of clogging with ramp-sized arenas |
| `1c365ed9` | r69 | `AccountPrefetch` reader layer under the recorder: pending balance-increase recipients read across 16 goroutines before the fold |
| `55d9212b` | r70 | `N42_HISTORY_INDEX_INTERVAL` (default 2 s); write probe `waitMs`/`heldMs` |
| `9564ffe6` | r71 | `hash.Blake3BinaryRoot` tx root behind `txRootBlake3Time` / `N42_TXROOT_BLAKE3_TIME`; leaf `blake3(0x00\|\|enc)`, node `blake3(0x01\|\|l\|\|r)`, odd node carried up, empty `blake3("")`; 163k: 70 -> 6 ms |
| `93e31b89`, `f2bb919e` | r72 | deferred execution behind `deferredExecutionTime` / `N42_DEFERRED_EXECUTION_TIME` (section 6) |
| `ba434d72` | - | n42-rs cross-client vectors (`internal/testdata/deferred_execution_vectors.json`) run against the header check |
| `b15cd2b7` | **r73** | the audit's 20 fixes (section 7 lists what was left) |

## 5. Resuming: 35zzm then 35zzn

Both runners export `N42_TXROOT_BLAKE3_TIME=1788393864` (35zzn also
`N42_DEFERRED_EXECUTION_TIME=1788393864`): the era seed head (block
13,652,362) is stamped 1788393863, so every block of a reseeded round is past
the gate and the round's first block is the fork block.

1. **Box free?** No `.box-claim-*` other than gov5's at both paths, load1 < 8,
   no n42-rs fleet processes; read the newest `BOX-NOTE-*`. The Rust session
   drives ~10-minute legs back to back: launch something that waits.
2. **Launch** (each chain script waits for three quiet checks, reseeds, copies
   n42-r73 into the n42-r35 slot, starts the runner, which claims the box):
   ```
   cd /data/blockchain/gov5-work
   setsid nohup bash ./chain-35zzm.sh >/data/blockchain/wr-logs/chain-35zzm-3.out 2>&1 </dev/null &
   setsid nohup bash ./chain-35zzn.sh >/data/blockchain/wr-logs/chain-35zzn-3.out 2>&1 </dev/null &
   ```
   `chain-35zzn.sh` line 14 waits for a ROUND line written after line 183 of
   `r35zzm.log` (attempt 1 ends at 183). If 35zzm is skipped, edit that line.
3. **Before each ROUND DONE + 90 s** (the next reseed deletes the node logs):
   `python3 scripts/qs-analysis/perminute.py '2026-09-DD HH:MM' /data/blockchain/wr-logs/r35zzX-perminute.txt`,
   plus `import_breakdown.py` per B leg and a CPU profile keyed on the first
   full B1 block (`curl -s "http://127.0.0.1:6091/debug/pprof/profile?seconds=25"`).
4. **35zzm (P68)**: every node logs "transactions root: BLAKE3 binary root from
   N42_TXROOT_BLAKE3_TIME"; no "transaction root hash mismatch"; follower
   `blockimport phases` body ~100 -> ~35 ms; the profile shows
   `Blake3BinaryRoot`, not `DeriveShaErigon`, under `ValidateBody`.
5. **35zzn (P69)**: "deferred check: block passes" and "deferred vote: block
   checked and parent imported, voting" on followers (against
   "import-gated vote:"); seal->QC (`cycle.py`) ~1.4 s -> ~0.5 s; chained cycle
   ~1.65 -> ~1.1 s. The runner's watchdog aborts on `BAD BLOCK`, "does not
   reproduce sealed root", "QMDB tree/marker discontinuity before execution",
   "deferred execution: header ... carries", "deferred check FAILED",
   "... not stored on this node", "transaction root hash mismatch".
6. **Record** results under 6bf / 6bg of `QS_BLOCK_TIME_BUDGET.md`, then
   memory. Abort with `bash abort-round.sh r35zzX "<reason>"` (kills by
   exact command, stops the fleet, drops the claim).

## 6. Deferred execution: the rule, gov5's implementation, and the known traps

**Rule (agreed with n42-rs, PHASE_D sections 8-9):** from the fork, a header's
Root, ReceiptHash, Bloom and GasUsed are the parent's executed values; the
first deferred header repeats its parent's own fields; a follower votes on N
once the parent is imported, N's header equals its own result for the parent,
and N's transactions are includable against that post-state. Pipeline depth 1.
Names: `executed_root_of(n)` = `header(n+1).Root` past the fork.

**gov5 pieces:** `rawdb.ExecutedResult` table (hash -> root, receipts root,
bloom, gas used; written with the applied marker); `ExecutedResultOfHeader`
(pre-fork and genesis headers are their own result); `checkDeferredHeader` in
`insertChain` before execution (an unstored parent result is retried as an
ancestor condition); builder stamps `parentExecutedResult` (own sealed record
`sealedExec`, else stored); `CheckDeferredBlock` on push and gossip arrival
(parent is the applied head and the state is read under the tree's readers
lock; includability = senders, contiguous nonces, worst-case cost with
overflow checks, fee cap vs base fee, tip vs cap, EIP-3607 sender code, no
blob txs, intrinsic gas with timestamp rules, block gas) -> `EventBlockChecked`
-> the engine votes when checked AND the JustifyQC block is imported.

**What n42-rs hit with the same rule** (their commits 7a469e2f5, 4e57e8823,
5e7e50669, 8e513160b, 0a8dfc2e2; PHASE_D section 16) -- check each on gov5
before or during 35zzn:

1. **Decide before body, repeatedly** (loop149, loop154): the commit for a
   block runs before its import; only the LAST committed block was retried,
   so the head stood still. gov5's `Service.pendingCommit` is a single slot
   retried in `NotifyBlockImported` -- the same shape. High risk.
2. **A checked block whose import then fails keeps its vote evidence**; a
   re-proposal of the hash is voted for unchecked. gov5's `checkedBlocks` has
   no withdrawal on import failure (their fix: `BlockRejected`).
3. **Sibling re-proposed after a TC at an own canonical height** executed
   against the wrong state (QMDB) -> every later header rejected. gov5 has the
   own-unverified mark and branch-switch undo, but siblings now share the
   parent's header Root; exercise a TC during a tenure.
4. **Pipeline depth**: theirs grew unbounded until capped; gov5's check
   requires the parent to be the applied head (depth 1 by construction).
5. Intrinsic gas missing from the check (theirs); gov5 checks it.

**Semantics to finish:** a genuine header mismatch still goes to
`reportBlock` + the sync layer's `setBadBlock` (design 8.3 says "refuse the
vote on N+1"); RPC shows parent fields in `eth_getBlock*` (no
`executed_root_of` presentation; `measure-tps.sh` gas sums are shifted one
block); mobile receipt binding under N+1; `ExecutedResult` is never pruned
(~328 B a block); Glamsterdam's pre-refund block gas vs receipts'
cumulative gas; `NoReceipts` stores gas 0. Stage 3 (leader seals right after
execution + tx root; PHASE_D section 13) is the Rust side's next step.

## 7. Open items left by the audit (not fixed in r73)

- **medium** build-path prefetch answers through a different reader stack
  than the fold (tree vs layered cache) -- values agree in `READ_QMDB=1`
  mode, but block-size dependent.
- **medium** the two bench gates live only in the process env on the private
  chain (its config is read from the DB, not the built-in chainspec); no
  cross-node consistency check. Persisting `txRootBlake3Time` /
  `deferredExecutionTime` into the chainspec + DB is the real activation.
- **medium** tools anchor on `header.Root` and break past the deferred fork:
  `cmd/n42-qmdb-export` (fatal), `cmd/qs-canon-probe`, `cmd/n42-state-verify`,
  `cmd/qmdb-proof-fuzz`, `cmd/n42-stateless-realproof`, ZK fast path;
  `replay-run.sh` does not export the gates.
- **low** QMDB AVX-512 leaf kernel equivalence is only tested on AVX-512 hosts
  (the fleet is one CPU type, so a kernel bug would not show as divergence).
- **low** kept arena sets pin the previous block's value slices; batch scratch
  retention; `UseEthereumTxRoot` naming to clean up; native receipts root is
  still keccak concat (a BLAKE3 candidate for the next cross-client item).

## 8. Next levers, in order

1. Run 35zzm and 35zzn (section 5); fix whatever 35zzn exposes, starting with
   section 6 items 1-2.
2. Deferred stage 3: seal before the fold/finish (the header needs only the
   tx root and the parent's fields) -- the cycle becomes the build's parallel
   part.
3. The growth of exec 200->330 and recovery 50->110 over a leg: per-process
   major faults, GC at 8 GB heaps, `thp` fallback (BOX-CLAIM-PROTOCOL's
   measurement note, `hugeprep.py`).
4. Handover block ~3.2 s at one in four blocks (tenure 4).
5. History fold: nosync (its marker makes a lost transaction safe) or a longer
   interval; it still holds the writer ~415 ms every 20 s.
6. Per-node ingress ceiling (~66k tx/s; hint ingest ~4.8 cores a node).

## 9. Rules and pitfalls

- Commit messages in English, no Claude/co-author trailers; code comments in
  English; talk to the user in Chinese; times in America/New_York.
- **Fork gates are CHAIN time.** The chain's block timestamps run ~9 days
  behind the wall clock (seed head 1788393863). 35zzm attempt 1 was void.
- Native chain != eth-el (section 1). Do not propose MPT levers for native.
- Box: follow `BOX-CLAIM-PROTOCOL.md`; chain scripts count only foreign claims
  (`grep -v -- "-gov5$"`) -- counting gov5's own lingering claim cost 4 h 20 min
  on 2026-09-11; check `chain-*.log` for "not launching" if a queued round has
  not started 3 min after ROUND DONE. Cross-session messages to n42-rs expire
  unapproved: use the NOTE files.
- Kill by exact PID, never a self-matching pattern; never end a `&&` chain with
  `&`; install a fleet binary only while the fleet is down.
- Every B/A comparison on fresh dirs (the chain scripts reseed); datadirs grow
  ~85 GB a round otherwise.
- Offline replays in the foreground with `timeout`, box idle only (a
  background replay was killed as "low on memory" with 112 GB free).
- Do not enable `PQPrecompilesTime` (Falcon verify is forgeable).
- "hotstuff: committed block not executed locally" ~100/h in B legs is
  benign timing noise, present since at least 35zzg.
