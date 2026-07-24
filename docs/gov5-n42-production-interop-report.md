# Gov5 ↔ n42-26 production interoperability qualification

Date started: 2026-07-23

This report is the qualification ledger for
`gov5-n42-production-interop-plan.md`. It records implementation gates,
disposable-runtime tests, and the guarded exercise against the preserved
seven-node deployment. Machine-readable evidence and immutable log manifests
are stored in the qualification runtimes named below.

## Source and binary identity

Interop branches:

- n42-26: `feat/gov5-n42-live-interop`
- N42-gov5: `feat/gov5-n42-live-interop`

Pushed implementation commits:

- n42-26: `68601ca` (`feat(interop): qualify production Gov5 compatibility`)
- n42-26: `e9c413c` (`fix(interop): retry missing blocks across connected peers`)
- n42-26: `a682a68` (`fix(network): fan out authenticated missing-block fetches`)
- n42-26: `4ed4fe8` (`fix(consensus): deduplicate timeout relays before fanout`)
- n42-26: `ab1bb95` (`test(interop): fail P6 on equivocation evidence`)
- n42-26: `21ea922` (`fix: bound hybrid sync recovery deadlines`)
- n42-26: `e1c4f99` (`fix: prioritize interop execution bodies`)
- n42-26: `517b13d` (`fix(interop): preserve ordered gov5 execution catchup`)
- N42-gov5: `b027f3040` (`feat(interop): harden Gov5 mixed-client operation`)
- N42-gov5: `34021c3f7` (`test: make hive genesis fixture self-contained`)
- N42-gov5: `a70f7cf68` (`test: share hive fixture across packages`)

The current Rust qualification binary was built at `517b13d`. The preceding
`21ea922` commit bounds both consensus state-sync requests and
orchestrator-owned Gov5 body fetches, rotates retry peers, and makes
unsupported state-sync attempts fail explicitly instead of remaining
pending. `e1c4f99` additionally routes Gov5 block gossip and hash-fetch
lifecycle events through the reliable consensus lane in H2 participant mode,
preventing an always-readable consensus queue from starving already-arrived
execution bodies. `517b13d` releases authenticated cached bodies in execution
height order, retains the direct successor until the durable execution head
advances, and drains newly bound bodies before later consensus events in the
same batch can commit descendants. `ab1bb95` changes only the P6 shell
monitor. The Gov5 qualification binary was built at `b027f3040`; the two later
Gov5 commits only make an existing test fixture self-contained in clean
checkouts:

- Rust `n42-node`:
  `a66eaa8b10139677704f38e81fd2efb6b8516905769947cc69c69a3e70f7d72c`
- Gov5 `n42`:
  `fa02d37c1e7b480a1c3196d318cd7bc79fb2d4247e5977331b79151873a82ae7`
- Gov5 `n42-qmdb-export`:
  `faa7cf2c0dc4f21903313e0d4f679a88876607eca2b343f4938e4e3c79a2437b`
- Gov5 `peerid`:
  `60b11438c4a294409f5a1ca546ceadeb4d6affc4688cfff19845d7da05c2b290`

The disposable P4 qualification intentionally retained failed discovery
windows. Missing-block single-source suppression, request fanout, timeout
relay feedback, and finally independent state-sync/body-fetch deadlines were
corrected in the Rust follow-up commits above. The formal zero-transaction
window is restarted from zero after every production correction; only the
replacement window using the final Rust hash above can count toward
acceptance.

The P6 observer began with immutable Rust hash
`73cd5bc9cf59715a0126a2e7cb6697b1ef5de30a28933c53eadfd092c341b10c`.
Observer qualification remains read-only and keeps that startup binary
unchanged for the entire window. Rust hash `a66eaa8b...` and Gov5 hash
`fa02d37c...` are staged separately for the maintenance-window participant
phase.

The n42 implementation baseline is `517b13d`, and the Gov5 branch baseline is
`a70f7cf`; see `source-remote-ref-audit.jsonl`. Live remote audits require the
n42 branch to contain that implementation and match the local checkpoint or
final-report commit exactly. This in-progress qualification ledger is
committed as a checkpoint. Its final PASS update remains pending until all
runtime gates finish.

`p4-lag5-source-fix-executable-identity-audit.jsonl` independently resolves
both current live Rust processes through their executable mappings. Both map
to the same staged release with SHA-256 `a66eaa8b...`; the separately staged
P6 participant and Gov5 artifacts also retain their expected hashes.

## Qualification runtimes

### Disposable mixed committee

- Chain ID: `1143`
- Genesis:
  `0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`
- Runtime:
  `/Users/jieliu/Documents/n42/live-interop-20260721/runtime-11-production-qualification`
- Topologies exercised: 6 Gov5 + 1 Rust, then 5 Gov5 + 2 Rust

### Preserved seven-node deployment

- Chain ID: `1143`
- Genesis:
  `0xdd96ceb7730fb4a01f6c42aa42908f8e3f7fb02c665829ec6bd96493079f3658`
- Original data:
  `/Users/jieliu/Documents/n42/live-interop-20260721/runtime-01/hotstuff_testnet`
- Qualification control and Rust data:
  `/Users/jieliu/Documents/n42/live-interop-20260721/runtime-12-existing-seven-qualification`
- Isolated Gov5 ports: P2P `34300..34306`, RPC `31500..31506`
- Observer ports: consensus `22980`, RPC `30510`

No preserved database was initialized again, reformatted, compacted, pruned,
or removed.

## Gate ledger

| Gate | Result | Principal evidence |
|---|---|---|
| P0 safety and participation baseline | PASS | `post-timeout-dedup-full-gates.jsonl`; Rust release workspace tests and Clippy; Go full and race suites |
| P1 follower and catch-up | PASS | authenticated reverse/concurrent ancestry logs, 1,000+ following blocks, persisted restart recovery |
| P2 automatic bootstrap and recovery | PASS | chain-bound bundle, blank-datadir materialization, replay receipt, cold restart |
| P3 bidirectional leader handoff | PASS | `p3-5gov-2rust-28views-pass.jsonl`; 44 consecutive exact blocks covering more than two rotations |
| P4 fault and lifecycle matrix | IN PROGRESS | all injected fault cases pass; actual 24-hour zero-transaction window and following burst are running |
| P5 minimal full archive+ parity | PASS | 209 RPC comparisons, 22 offline proof checks, export/import and corruption recovery |
| P6 existing seven-node rollout | IN PROGRESS | observer cold bootstrap and exact epoch crossing pass; the actual 24-hour read-only observer window passes, continuity guard remains active, and participant activation waits for P4 |

## P0 — safety and participation baseline

Implemented and tested:

- bad-block isolation distinguishes a forged declared hash from a
  deterministically invalid executed payload;
- an H2 vote is released only after `new_payload(Valid)`;
- Gov5 Noise PeerIDs are bound exactly to validator identities while BLS
  authorization remains mandatory;
- native and replay-v2 QMDB checkpoint identities are explicit and cannot be
  mixed;
- out-of-order eager import advances only the execution-validated watermark;
- leader selection fails closed on an empty validator set;
- participant and observer identities accept `@file` secret references so raw
  BLS and P2P secrets are not placed in process arguments or environment
  values.

Final source gates:

- `cargo test --release --workspace`: PASS
- `cargo clippy --release --workspace --all-targets -- -D warnings`: PASS
- `cargo +nightly fmt --all -- --check`: PASS
- `go test ./...`: PASS
- `go test -race ./...`: PASS

Those entries are the completed P0 baseline gates. For the later `e1c4f99`
network-lane and `517b13d` ordered catch-up corrections, the directly affected
`n42-network` and `n42-consensus-service` suites pass 342 tests in total and
both packages pass Clippy with warnings denied. The authoritative full
clean-worktree rerun at `517b13d` remains deliberately gated after P6 and must
pass before `FINAL_GATES.PASS`; it is not inferred from the earlier P0 run.

The final P0 exit criterion was also rerun from three detached, clean
worktrees pinned to n42-26 `4ed4fe8c898885de47415b0e737104efbb698c94`,
reth `c533db8bad6f300be93ec047ecffc717b08957f8` (workspace version 2.4.1),
and Gov5 `a70f7cf68d9a19ccf485da007d46da3337d5817a`. Every Rust and Go command
above passed and all three worktrees remained clean.

The first clean Gov5 run correctly exposed that three tests depended on an
ignored nested `tests/eth-hive` checkout. The exact fixture was moved into
tracked root `testdata` with the same SHA-256
`e63be600b65a48a81fa631f4f2f57f78d195166f1d6dffb18954626794ed3978`;
the failed discovery runs remain archived, and the full and race suites then
passed from a newly created clean worktree. The pre-existing dirty reth
worktree was not used or modified.

Machine-readable record:
`runtime-11-production-qualification/evidence/post-timeout-dedup-full-gates.jsonl`.
The clean-worktree record and hashed logs are:
`runtime-11-production-qualification/evidence/p0-clean-worktree-reproducibility.jsonl`
and `runtime-11-production-qualification/evidence/clean-worktree-reproducibility/`.
The P0 requirement mapping is recorded in
`runtime-11-production-qualification/evidence/p0-baseline-requirement-audit.jsonl`.

## P1 — follower, out-of-order catch-up, and restart

A blank Rust node joined more than 20 blocks behind six Gov5 validators. The
authenticated H2 ancestry arrived in reverse and concurrent batches. Rust
released parents in execution order, passed every parent through Engine API
validation, and converged without editing state files.

The node then:

- followed 5,743 blocks beyond bootstrap checkpoint 29 at the recorded P1
  summary point;
- restarted twice from its own persisted consensus, execution-lineage, Reth,
  and QMDB state;
- reconverged after each restart;
- retained no bad-block-cache entry for an honest declared block.

The preserved logs include explicit
`releasing reverse-delivered authenticated Gov5 ancestry in execution order`
and `new_payload(Valid)` records. The summary binds 50 reverse-ancestry batches,
156 validated parents, and three restart logs by SHA-256:
`runtime-11-production-qualification/evidence/p1-follower-catchup-restart-summary.jsonl`.
The complete P1 mapping is
`runtime-11-production-qualification/evidence/p1-follower-requirement-audit.jsonl`.

## P2 — authenticated automatic bootstrap

The bootstrap bundle binds:

- chain ID and genesis hash;
- finalized block number, hash, state root, and receipts root;
- CommitQC, locked QC, certificate view, and execution-validated head;
- full validator set and exact PeerID bindings;
- sequence number and content digest.

Materialization verifies every component before an atomic write, refuses
regression or same-sequence replacement, and persists a replay receipt. A
blank participant imported the authenticated finalized range and cold
restarted with no operator edits.

The preserved-seven observer independently repeated this from an empty Reth
database using a bundle ending at block 898. It imported all 898 blocks and
matched:

- block:
  `0x663a60f7aa9259c1d2e57cd780750bdae5ff14025936afcd958b92bf54f080aa`;
- QMDB state root:
  `0x76ae4240ad9c782c46911141ace395d9afb75d6f4fb0425287315e12cfeb4a4c`.

Evidence:
`runtime-12-existing-seven-qualification/evidence/p6-observer-cold-bootstrap.jsonl`.
The complete requirement-to-evidence mapping is recorded in
`runtime-11-production-qualification/evidence/p2-bootstrap-requirement-audit.jsonl`.

## P3 — bidirectional leader handoff

Both 6 Gov5 + 1 Rust and 5 Gov5 + 2 Rust were exercised. Gov5 proposals were
executed and voted by Rust; Rust proposals were normalized to the Gov5 H2
header profile, independently executed by Gov5, and committed from mixed
votes.

The final continuous record spans common heights 1108 through 1151: 44
consecutive blocks, zero divergence, and no client-specific fork. All seven
validators led at least six blocks. Rust leaders formed mixed-client CommitQCs
with five prepare and five commit votes.

Evidence:

- `runtime-11-production-qualification/evidence/p3-5gov-2rust-28views-pass.jsonl`
- `runtime-11-production-qualification/evidence/p3-leader-rotation-counts.jsonl`
- `runtime-11-production-qualification/evidence/p3-leader-handoff-requirement-audit.jsonl`

## P4 — fault and lifecycle matrix

Completed matrix cases (live fault injection except where explicitly enforced
by the release chaos/regression suite):

- Rust disconnect and rejoin from more than 512 blocks behind;
- lost direct traffic recovered by GossipSub/rotor dissemination;
- timeout, NewView, and TC recovery;
- an authenticated validator forging an invalid BLS vote;
- forged compact output and invalid payload isolation (release
  chaos/regression suite, followed by live liveness/root monitoring);
- process aborts immediately after `qmdb_committed`,
  `execution_validated`, `vote_persisted`, and `commit_qc_persisted`;
- validator-set transition across two live epochs;
- one Byzantine validator and configured crash-fault threshold;
- sustained message backpressure followed by bounded recovery.

Selected evidence:

- `p4-rust1-512-rejoin-final.jsonl`
- `p4-forged-validator-live.jsonl`
- `p4-crash-qmdb_committed.log`
- `p4-crash-execution_validated.log`
- `p4-crash-vote_persisted.log`
- `p4-crash-commit_qc_persisted.log`
- `p4-epoch4-transition-live.jsonl`
- `p4-epoch5-transition-live.jsonl`
- `p4-backpressure-fix-recovery-final-v2.jsonl`
- `p4-fault-matrix-audit.jsonl`

The first long window exposed a missing-block recovery path that relied too
heavily on one source; the second exposed incomplete request fanout. After
both were fixed, a later window discovered a distinct timeout feedback loop:
`process_timeout` relayed every verified duplicate before timeout-collector
deduplication. Dual H2-v4 GossipSub publication, validator-direct delivery,
and validator fanout amplified those duplicates until inbound
request-response stream capacity was exhausted and authenticated block-body
recovery stalled. The final correction relays each newly collected timeout
once after deduplication while preserving the originator's periodic timeout
rebroadcast for reconnect recovery.

The next formal window ran cleanly for more than three hours before exposing
a different long-idle case: an expired state-sync request remained pending,
and a Gov5 body fetch could be silently suppressed without a terminal event.
The `21ea922` release gives both operations independent deadlines, retries
ordinary state sync across all connected peers, and rotates Gov5 body fetches
without weakening range, QC, or identity validation.

That replacement formal window began at `2026-07-24T06:00:05Z`, but it also
failed closed at `2026-07-24T09:06:42Z`: one Rust execution head remained at
15826 while the maximum head reached 15833, producing a real lag of seven.
The transaction burst was not released. The immutable network log proves that
each Gov5 block body had reached the Rust network process on schedule, yet
`ExecuteBlock` continued to report `pending_data=false`. Gov5 block gossip and
hash-fetch responses were still routed through the lower-priority data
channel, while the orchestrator's biased select continuously consumed a
nonempty consensus channel. When the data lane was finally serviced, blocks
15827 through 15833 executed and canonicalized in about 0.46 seconds. This
excludes Engine API throughput, missing fetch fanout, the historical RPC
audit, and database corruption as causes.

Commit `e1c4f99` moves Gov5 block bodies and fetch completion/failure events to
the reliable consensus lane only for H2 participants; observer isolation is
unchanged. The network and consensus-service suites pass 162 and 178 tests,
respectively, and both packages pass Clippy with warnings denied. Both Rust
validators were then restarted sequentially with the new release and their
original persisted datadirs. Rust1 immediately caught up from 15869 to 15900;
Rust2 then caught up from 15885 to 15915. No database was edited, copied,
recreated, or reformatted.

All failed discovery windows and their exact diagnoses remain preserved; none
counts toward acceptance. The `e1c4f99` release then passed a new recovery
regression with 61 samples spanning 652 seconds, zero failures, a maximum lag
of one, and 111 blocks of progression. Every sample matched across five Gov5
and two Rust endpoints. Both Rust logs prove persisted QMDB/snapshot recovery,
reverse-ancestry release in execution order, and arrival at a durable
execution head.

The formal successor that began at `2026-07-24T09:43:41Z` also failed closed
at `2026-07-24T12:56:25Z`. Both Rust consensus engines had accepted the
CommitQC for block 18012, but both execution heads remained at 18007, so the
measured lag reached five against the unchanged maximum of four. The signed
transaction burst remained unreleased, and that entire window is archived and
excluded.

The retained network evidence shows that the Gov5 bodies arrived before the
failure. Several bodies could remain in the hash-keyed unbound cache until a
later consensus event authenticated them, after which the cache drained in
hash order rather than execution-height order. The ordinary asynchronous path
also failed to retain the direct successor while it was still awaiting Engine
API completion. A later child could therefore activate catch-up without the
exact first parent needed to reconnect to the durable execution head. A
restart recovered from the unchanged persisted data, excluding a missing or
corrupted database.

Commit `517b13d` sorts every authenticated cached-body release by execution
height, retains the direct successor until durable execution advances, and
immediately drains newly authenticated bindings before processing later
consensus events from the same batch. The consensus-service suite passes 180
tests, the network suite passes 162 tests, and both packages pass Clippy with
warnings denied. The production node binary was rebuilt in an isolated target
directory to avoid stale executable reuse and differs byte-for-byte from the
prior release. Both Rust validators were restarted sequentially from their
original persisted datadirs and independently resolved to SHA-256
`a66eaa8b...`; no database was edited or recreated.

The new eligible formal successor began from zero at
`2026-07-24T14:05:20Z`, after the corrected release passed an independent
653-second recovery regression with 64 samples, zero failures, a maximum lag
of one, and 111 blocks of progression. Each formal sample compares five Gov5
and two Rust RPCs at the minimum common height, checks block hash, QMDB state
root, receipts root, lag at most four, and every newly finalized block's empty
transaction list. The new baseline binds source commit `517b13d`, release
SHA-256 `a66eaa8b...`,
recovered heads, exact warning counters, and deadline counters. Evidence is
appended to `p4-zero-tx-24h-soak-priority-lane-final.jsonl`; it is accepted
only when the first-to-last sample interval reaches at least 86,400 seconds
with no failed sample and no post-baseline counter growth. The already-signed
17-transaction burst follows only after that full replacement window passes.

A fail-closed finalizer is armed against the formal monitor. It cannot release
the burst unless every sample, historical empty-block interval, lag bound, and
all warning and deadline counters pass. An independent 30-second guard also
requires both Rust nodes to retain a seven-validator CommitQC, remain within
the four-view bound, agree on the committed hash whenever their views are
equal, and report zero authenticated equivocation evidence. It writes a PASS
summary only after the full 86,400-second sample interval; the finalizer
also rejects any formal sample gap above 120 seconds and refuses to release
the burst without that summary. After the burst, the finalizer also requires
ten minutes of continued seven-endpoint exact-root liveness. The finalizer
pins the signed 17-transaction artifact's exact SHA-256 and 0600 mode both
when it starts and immediately before broadcast, so the independently audited
raw signatures cannot be replaced after preflight. Script hashes and process
identities are recorded in
`p4-priority-lane-finalizer-guard.jsonl`.

Additional evidence:

- `p4-soak-discovered-fetch-fallback-incident.jsonl`
- `p4-soak-discovered-single-source-suppression-incident.jsonl`
- `p4-soak-discovered-timeout-relay-feedback-incident.jsonl`
- `p4-soak-discovered-stale-sync-and-fetch-deadline-incident.jsonl`
- `p4-formal-discovered-leader-payload-id-execution-lag-incident-root-cause-and-fix.jsonl`
- `p4-sync-deadline-recovery-10m.jsonl`
- `p4-sync-deadline-recovery-summary.jsonl`
- `p4-priority-lane-fix-recovery-10m.jsonl`
- `p4-priority-lane-fix-recovery-summary.jsonl`
- `p4-priority-lane-formal-soak-baseline.jsonl`
- `p4-priority-lane-formal-progression-audit.jsonl`
- `p4-signed-transaction-burst-preflight.jsonl`
- `p4-formal-live-progression-audit.jsonl`
- `p4-formal-independent-historical-rpc-audit.jsonl`
- `p4-formal-20260724T094341Z-lag5-failed.jsonl`
- `p4-ordered-catchup-fix-recovery-summary.jsonl`
- `p4-lag5-source-fix-build-provenance.jsonl`
- `p4-lag5-source-fix-executable-identity-audit.jsonl`
- `p4-lag5-control-chain-rearm-audit.jsonl`

## P5 — minimal full archive+ parity

The archive profile retains immutable finalized block/body/receipt data and a
block-bound historical QMDB branch/proof index. The RPC advertises its archive
floor and refuses pruning below it.

Qualification results:

- 11 sampled heights across genesis, block 29, epoch boundaries, recent
  history, and the archive head;
- 19 exact Gov5/Rust comparisons per height, 209 total;
- block-by-number/hash in hash-only and full-transaction forms, receipts,
  logs, transaction count, balance, nonce, code, storage, and proofs;
- two QMDB keys per sampled height, 22 restored offline proof verifications;
- exact proof root binding and proof-byte comparison to the Gov5 reference;
- full snapshot export/import into a fresh directory;
- checksum-detected corruption followed by clean recovery;
- startup refusal when pruning would violate the archive floor.

Evidence:

- `p5-archive-rpc-parity.jsonl`
- `p5-qmdb-restored-offline-verification.jsonl`
- `p5-qmdb-archive-rpc-live.jsonl`
- `p5-archive-corruption-recovery.jsonl`
- `p5-pruning-rejected.jsonl`
- `p5-archive-requirement-audit.jsonl`

The operational archive is a manifest-verified full Reth + consensus/QMDB
snapshot. An ERA-only import was deliberately not represented as equivalent:
Gov5's legacy receipt dialect is not an Ethereum ERA receipt stream.

## P6 — guarded existing-seven introduction

Before attachment:

- block 0 through 898 were exported read-only from the existing Gov5 data;
- a chain-bound observer bundle and exact seven-PeerID configuration were
  created;
- the observer imported from a blank database;
- the validator selected for replacement received a verified pre-rollout
  snapshot with SHA-256 manifest.

The seven Gov5 validators then resumed on isolated ports. Rust restarted from
its imported state with the same stable observer PeerID
`12D3KooWFxgMF1PAgc6pWvxiCRcj8P7iXtvTQy86hcPtdW1UQBLc`, connected to all
seven Gov5 peers, and continued from block 898.

The final Gov5 release artifact independently opened a copy of the preserved
validator-6 database and returned the exact block-898 hash and state root.
Evidence:
`p6-final-gov-database-open-smoke.jsonl`. The source snapshot was not opened
for writing.

At heights 997 through 1002, including the epoch-1000 crossing, all eight RPC
endpoints returned the exact same block hash, QMDB state root, and receipts
root. The observer reports `hasCommittedQc=false`; its 16-byte vote log is
hashed at the start and will be compared again at the end.

The actual observer window began at `2026-07-23T14:39:47Z` and closed after
1,433 samples at `2026-07-24T14:44:24Z`, spanning 86,677 seconds. It has zero
failed samples or read-only violations, maximum lag 1, maximum sample gap 63
seconds, and advances from block 933 to 22,602. Its closed SHA-256 is
`f5a9d3261156160f6f0f33805620b3ff5f798d590755cd44507eff49b80a192d`.
`p6-observer-24h-soak.jsonl` compares:

- all seven Gov5 heads and the observer;
- common block hash, state root, and receipts root;
- head lag, archive floor, and retained block count;
- read-only consensus status.

A post-threshold read-only guard took over at `2026-07-24T14:45:00Z`, 36
seconds after the formal window's last sample, and continues the same
comparisons until the P4 gate releases participant activation, so the
observer-to-maintenance interval does not become an unmeasured gap. The
threshold and handoff are independently recorded in
`p6-observer-24h-independent-threshold-audit.jsonl`. After the P4 window was
restarted for the priority-lane correction, both extension schedules remained
lengthened to 86,400 seconds. A second overlapping guard begins at
`2026-07-25T05:30:00Z`, before the new P4 threshold, and the fail-closed P6
finalizer stops both guards immediately before activation. Activation also
requires both handoff gaps to be at most 120 seconds and the last observer
sample to be no more than 120 seconds old. Each observer evidence stream also
has a 120-second maximum consecutive-sample-gap gate. Before activation, the
node6 network key was
read through an `@file` reference and independently derived the exact configured PeerID
`16Uiu2HAmGHiKh3pqQZ32tb3iM6TMJqqCYXKhH7aXh5aUCYU6d3wc`; the unique BLS
key and both secret files retain mode 0600. No secret material was emitted.

The observer intentionally does not rewrite voting consensus state: its
snapshot remains at view 900 while its Reth/QMDB head continues following the
network. The participant handoff was therefore audited explicitly at live
block 15,891. All eight endpoints agreed on the exact block and roots, and the
canonical Gov5 header's `N42H` prefix decoded to view 15,893. Participant
startup first proves the exact Reth head through authenticated QMDB lineage,
then validates that canonical header and uses its embedded view as the
execution-validity guard. It fails closed if no exact hash/view mapping can be
proved; no snapshot edit or guessed block-height/view mapping is used. A
verified QC successor controls the subsequent view jump, and an H2 round-1
vote remains blocked until the matching block is execution-valid.

Evidence:

- `p6-observer-post-24h-guard.jsonl`
- `p6-observer-24h-independent-threshold-audit.jsonl`
- `p6-continuous-observer-guard-update.jsonl`
- `long-window-transition-schedule-audit.jsonl`
- `p6-observer-independent-archive-rpc-audit.jsonl`
- `secret-reference-runtime-audit.jsonl`
- `p6-preactivation-readiness.jsonl`
- `p6-participant-identity-preflight.jsonl`
- `p6-pre-marker-failure-rollback-regression.jsonl`
- `p6-restart-sample-boundary-regression.jsonl`
- `p6-finalizer-guard.jsonl`

The participant monitor also polls `n42_equivocations` every five seconds and
requires zero authenticated evidence in every formal sample. A separate
fail-closed safety guard rejects any Rust-leader `timeout_triggered` event and
matches every Rust `leader_build_start` to a committed block within 60 seconds.
It compares the build and commit log timestamps at millisecond resolution, so
a commit arriving after the deadline cannot become passing merely because it
appears before the next five-second poll.
The guard filters each phase log once per poll and parses the resulting leader,
commit, and timeout events in one linear pass. This replaces an earlier
pre-activation implementation that rescanned the full log once for every
historical Rust leader build and would have amplified CPU and I/O late in the
24-hour window. Positive retained-log, exact-60,000-ms, 60,001-ms,
missing-commit, timeout, negative-elapsed, and whole-second timestamp controls
are recorded in `p6-safety-guard-linear-log-parser-regression.jsonl`.
Any safety violation terminates the measured monitor and immediately invokes
the rollback trap. The restart is scheduled immediately after a validator-0
committed view so it cannot intentionally consume the next Rust leader slot,
and it may begin only while the latest formal participant sample is at most 30
seconds old. This leaves a measured margin before the next 120-second sample
and removes the planned-restart/RPC-sampling race.
The rollback trap also restores Gov5 validator 6 when that process is already
stopped even if failure occurs before the replacement marker is created,
closing the maintenance-snapshot pre-marker recovery gap.
The same guard remains mandatory during the final post-rollback mixed
reactivation window. The participant stream additionally rejects consecutive
sample gaps above 180 seconds; rollback and final mixed-health streams reject
gaps above 90 seconds. Its parser was checked against the retained P4 incident
log and correctly detected the real missed Rust leader view 12179 while
accepting live zero-equivocation RPC responses; see
`p6-safety-guard-parser-regression.jsonl`.

Before any later participant start can truncate its live log, the controller
copies the completed observer, pre-restart participant, post-restart 24-hour
participant, and final mixed-reactivation logs into immutable evidence
snapshots. Their SHA-256 values are bound into the P6 summary, rechecked by the
final audit, and covered by the final evidence manifest. Each snapshot is also
scanned without emitting the pattern for the exact preserved node6 network
and BLS key material; any match fails the phase before archival.

After the full observer window, the runbook will:

1. stop the observer and take a manifest-verified copy of its current Reth and
   QMDB state;
2. stop Gov5 validator 6 and take a second maintenance-window snapshot at its
   current head;
3. start Rust with validator 6's exact BLS key and secp256k1 PeerID via
   `@file` references;
4. restart the remaining Gov5 peers with the Rust QUIC address;
5. verify two full leader rotations, restart/rejoin Rust, and continue the
   exact-root monitor for an actual 24 hours;
6. use `rollback-replacement` to stop Rust and reopen the untouched Gov5
   validator directory if any fail-closed invariant triggers.

## Monitoring and preservation controls

`scripts/gov5-interop-qualification.sh` and
`scripts/gov5-existing-seven-qualification.sh` fail closed on unavailable RPC,
hash/root mismatch, unbounded lag, missing archive data, or participant
identity mismatch.

The qualification also owns an explicit idle-system and disk-idle sleep
assertion for the remaining P4/P6 windows. Host-capacity evidence records
954 GiB available storage, low file-descriptor counts, sub-2 ms local RPC
latency, and the assertion process identity.

The live phase ledger was reconciled against the plan without changing any
acceptance threshold. `overall-goal-alignment-audit.jsonl` records P0, P1, P2,
P3, and P5 as complete; P4 and the read-only part of P6 as running; and keeps
P6 participant activation, rollback, final gates, report commit, and push in
their required order.

After all runtime gates pass, the final requirement audit hashes every file
under the runtime-11 and runtime-12 evidence directories into
`final-evidence-manifest.sha256` and verifies the manifest before emitting
`FINAL_AUDIT.PASS`. Before creating that manifest it requires every JSONL in
both evidence trees to be nonempty and parse successfully. Immediately before
doing so it resolves every evidence and source path named by the P0 through P5
and P4 fault-matrix requirement audits, and requires every cited log line to
be within the referenced file. It also verifies the pinned
SHA-256 values of both qualification harnesses, the P4 formal guard and
finalizer, the P6 finalizer, and the final clean-gate controller. It resolves
the executable mapping of every live process in the final 6-Gov5-plus-1-Rust
topology, requires each mapped inode to equal the current path inode, and
hashes those exact files. It then queries both source origins with live
`git ls-remote` checks rather than trusting cached remote-tracking refs.
The report's final PASS state is committed only after that point; completion
additionally requires pushing that final update and independently confirming
the resulting remote branch reference.

An early structural preflight covered 101 current JSONL files with zero parse
failures and resolved all 49 unique requirement references, including all six
line-number references. The authoritative final audit repeats those checks
after the remaining append-only streams have closed; the preflight is recorded
in `final-evidence-structure-preflight.jsonl`.

Existing preserved artifacts remain in place:

- runtime-01 Gov5 node directories;
- `finalized-771-898.v1`;
- `node0-qmdb.portable`;
- replay-history and performance datasets;
- the dirty sibling reth worktree.

An accidental empty version-probe directory was retained under a descriptive
name rather than deleted. Qualification runtimes are additive and disposable;
they do not replace source data.
