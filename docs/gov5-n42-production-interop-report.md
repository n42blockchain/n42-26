# Gov5 ↔ n42-26 production interoperability qualification

Date started: 2026-07-23

This report is the qualification ledger for
`gov5-n42-production-interop-plan.md`. It records implementation gates,
disposable-runtime tests, and the guarded exercise against the preserved
seven-node deployment. Machine-readable evidence and immutable log manifests
are stored in the qualification runtimes named below.

## Current 2026-08-02 baseline — GOV5 5.7.906

The current-main Gov5 candidate is pushed as
`integration/gov5-interop-current-main-20260801 @ b70505738`. Its pinned
upstream cutoff is `origin/main @ f3dbeba4694590e6478780ac8a14e900f7dd7505`,
version 5.7.906. A live remote-reference check immediately before the strict
window confirmed that both hashes still match the pushed branches. This
release changes transaction gossip from protobuf to RLP, removes the remaining
non-consensus/non-gRPC protobuf producers, bounds deployment logging, and gives
freezer bodies and the transaction-log table compact codecs. The compact log
reader accepts both old protobuf and new compact records, so the stopped
5.7.905 data can be copied without migration. The merged candidate passed full
`go test ./...` plus race-enabled tests for p2p, sync, HotStuff, rawdb, block,
transaction, logging, and `cmd/n42`.

The active Rust qualification binary was built from
`feat/gov5-n42-live-interop @ fc15007` against the separate Reth worktree at
`c533db8` (Reth 2.4.1). Qualification tooling fixes through `b4aceb1` are
pushed on the same branch without changing that measured runtime binary. Commits
`ac1fc06` and `4a11238` add an explicitly configured, hard-capped authenticated
Gov5 catch-up buffer and retain each buffered block's already-verified H2 view
independently of the 2,048-entry live binding cache. Commit `161d64a` also
removes an O(n²) full-tree prune from each received ancestor while preserving
incremental removal whenever the durable execution head advances. Commits
`63f97db` through `d079c63` serialize and rotate ancestry requests, deduplicate
overlapping walkers, retire completed metadata, and gate the full readiness
scan until the suffix reaches the durable head. Commit `fc15007` makes empty
QMDB transitions O(1), replaces the per-block full branch-file rewrite with a
checksummed append-only WAL that still fsyncs every accepted block, and validates
persisted empty ancestry in linear time. The production catch-up default remains
2,048 blocks; the current qualification run explicitly uses 131,072. The
targeted consensus/service/network suites passed 570 tests (220, 186, and 164),
and the complete n42-node library suite passed another 167 tests; zero failures.
The n42-node all-target Clippy gate also passed with warnings denied. The
latest pinned 5.7.906 Gov binary was built twice byte-for-byte identically. The
active runtime hashes are Gov5
`fe24cf475bdd362229faaf22e48f65af5011e4abf714d46fe0f83b3b496a9f1f` and
Rust `d917782b906176119172e656005218be34ec3d5ad1b7241c0c53f8f6d593da2d`.

Independent 5.7.905, initial 5.7.906, and pinned-current 5.7.906 `init` runs
against the qualification genesis all regenerated block zero as
`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`, matching
the Rust H2 configuration and the preserved Gov5 chain. The independently
queried pinned-current block-zero state root and receipts root also match the
runtime chain. The first long-test runtime copied the verified chain data while
excluding old MDBX locks, PID files, LOCK files, and IPC sockets; it was
`/Users/jieliu/Documents/n42/live-interop-20260721/runtime-15-gov5-905-interop`.
Its startup attempt is preserved under runtime-15 logs. The 5 Gov5 nodes reached
the same head, but the two Reth 2.4.1 Rust processes did not open RPC within
the 10-minute readiness budget when given the copied legacy Reth database;
therefore no acceptance time was credited and no P4 evidence file was
created. Runtime-16 therefore regenerates both Rust databases from the
authenticated bootstrap bundle instead of copying the legacy Reth database:
`/Users/jieliu/Documents/n42/live-interop-20260721/runtime-16-gov5-905-fresh-reth`.
Its first repaired Rust node opened RPC immediately, replayed block zero
through checkpoint 29, and authenticated the complete 84,617-block reverse
ancestry from block 84,646 to block 30 in 30.5 seconds. It then executed every
block in height order with `new_payload(Valid)` and an exact per-height hash
match against Gov5. A guarded restart at durable Reth/QMDB height 27,986 loaded
the same canonical hash and QMDB lineage, then resumed the remaining ancestry
without regenerating or replacing Gov5 data. The Rust node reached the exact
five-Gov head at block 84,756 on `2026-08-02T12:32:02Z`. At its next leader
opportunity it built block 84,757, hash
`280756fff9eb440e1f156a6e82634a0d531eca197cc977bb4d3c8529f0d4395f`,
with fee recipient `81d4c1f92ddb837cb46f82280d9b491b101fa582`; five Gov5 peers supplied
both voting rounds (`votes=5+5`), Rust committed it in 88 ms, and all six RPC
endpoints returned that block as canonical. Subsequent Rust-authored blocks
occur exactly every six committed heights in the intentionally five-Gov plus
one-Rust live topology, with no missing or extra Rust producer slot.

While that first strict 5.7.905 window was accumulating, upstream advanced to
5.7.906. The 905 stream was stopped without releasing the transaction burst,
preserved under
`runtime-16-gov5-905-fresh-reth/excluded/gov5-905-superseded-by-906-20260802T1346Z/`,
and excluded from final acceptance. At the final 905 upgrade snapshot all six
endpoints were exact at block 85,290. Runtime-17 copied the stopped Gov and
Reth data with MDBX, Reth, and IPC lock files excluded, then replaced only the
Gov executable. All five 5.7.906 Gov nodes and the unchanged Rust/Reth node
opened the copied data at block 85,290. Rust next authored block 85,291; all
five Gov nodes supplied both voting rounds and all six endpoints accepted the
same canonical block. The initial 906 audit scanned blocks 85,291 through
85,320: all five expected Rust slots were exact, every commit had `votes=5+5`,
the view stride was seven, and every endpoint returned the same hashes.

The pre-upgrade 5.7.905 archive check passed 209 Gov/Rust RPC, state, and storage
comparisons across 11 historical heights. Two current-head Gov5 QMDB proofs
were byte-for-byte equal to Rust archive proofs, and all 24 current plus
historical proofs authenticated their expected root and key with the offline
Rust verifier. The first 5.7.906 window accumulated 701 seconds and 72 blocks
with maximum lag zero and no parity failure, but was superseded without sending
transactions when same-version upstream commits advanced the pinned candidate.
Runtime-18 then copied the exact stopped chain state with lock files excluded
and replaced only the Gov executable with the reproducible pinned-current
binary. Its first runtime-local Rust leader was block 85,387. A startup audit
through block 85,404 found all three expected Rust slots, exact six-height
cadence, continuous parents, identical canonical hashes on all six endpoints,
exact seven-view stride, and `votes=5+5` for every commit.

The authoritative pinned 5.7.906 strict zero-transaction window started from
an exact common head at `2026-08-02T14:23:42Z` in
`runtime-18-gov5-906-latest-reth`; it runs for 86,640 seconds and acceptance
still requires at least 86,400 seconds between the first and last evidence
samples. Its independent transaction preflight confirmed nonce 17 on all six
endpoints and sent zero transactions. Live latest-906 participation and leader
handoff are therefore proved, while the 24-hour gate remains IN PROGRESS and is
not declared PASS until the full interval, 17-transaction burst, post-burst
archive parity, restart/rejoin, and final leader audits complete.

## Source and binary identity

Interop branches:

- n42-26: `feat/gov5-n42-live-interop`
- N42-gov5: `integration/gov5-interop-current-main-20260801`

Pushed implementation commits:

- n42-26: `68601ca` (`feat(interop): qualify production Gov5 compatibility`)
- n42-26: `e9c413c` (`fix(interop): retry missing blocks across connected peers`)
- n42-26: `a682a68` (`fix(network): fan out authenticated missing-block fetches`)
- n42-26: `4ed4fe8` (`fix(consensus): deduplicate timeout relays before fanout`)
- n42-26: `ab1bb95` (`test(interop): fail P6 on equivocation evidence`)
- n42-26: `21ea922` (`fix: bound hybrid sync recovery deadlines`)
- n42-26: `e1c4f99` (`fix: prioritize interop execution bodies`)
- n42-26: `517b13d` (`fix(interop): preserve ordered gov5 execution catchup`)
- n42-26: `242502c` (`fix(interop): rearm staged Gov5 catchup`)
- n42-26: `24210f0` (`fix(interop): preserve live H2 bindings at capacity`)
- n42-26: `2a25359` (merge the two 2026-07-25 interoperability audit fixes)
- n42-26: `8134235` (`perf(consensus): batch-verify H2-v4 signatures`)
- n42-26: `04ab69e` (`perf(consensus): batch-verify H2-v4 timeout certificates too`)
- n42-26: `afafd37` (`fix(interop): normalize gov5 RPC transaction metadata`)
- n42-26: `851c1b7` (`fix(interop): normalize empty gov5 receipt logs`)
- n42-26: `f49422f` (`fix(interop): normalize gov5 log response shapes`)
- n42-26: `6180ec5` (`fix(rpc): scope batch normalization by request method`)
- n42-26: `1b8d52b` (`style(rpc): make disabled batch path explicit`)
- n42-26: `ac1fc06` (`fix(interop): bound configurable Gov5 catch-up buffer`)
- n42-26: `4a11238` (`fix(interop): retain catch-up authentication views`)
- n42-26: `161d64a` (`perf(interop): prune catch-up history incrementally`)
- n42-26: `63f97db` (`fix(interop): rotate serialized Gov5 ancestry fetches`)
- n42-26: `5091fb4` (`fix(interop): deduplicate staged ancestry walkers`)
- n42-26: `f753716` (`perf(interop): retire successful Gov5 fetch metadata`)
- n42-26: `d079c63` (`perf(interop): gate full ancestry readiness scan`)
- n42-26: `fc15007` (`perf(interop): append QMDB catch-up durability log`)
- n42-26: `65a7718` (`fix(interop): report idle zero-tx samples accurately`)
- n42-26: `b8896ae` (`fix(interop): qualify live Gov5 archive proofs`)
- n42-26: `11cbb42` (`fix(interop): stage burst for configured topology`)
- n42-26: `790e16c` (`fix(interop): measure soak between evidence samples`)
- n42-26: `9b22d0c` (`test(interop): audit soak evidence continuity`)
- n42-26: `c32ccbf` (`test(interop): audit Rust leader cadence`)
- n42-26: `7bd72b2` (`test(interop): bind Rust blocks to Gov vote logs`)
- n42-26: `bdcff17` (`test(interop): monitor Rust soak resources`)
- n42-26: `1297077` (`fix(interop): parameterize clock snapshot topology`)
- n42-26: `55bea3f` (`test(interop): automate final 905 qualification`)
- n42-26: `86c0829` (`test(interop): align restart after Rust leader`)
- n42-26: `a4e24bd` (`test(interop): qualify current Gov5 release`)
- n42-26: `97454e9` (`test(interop): scope leader audit to current runtime`)
- n42-26: `1236936` (`docs: record gov5 906 live qualification`)
- n42-26: `b4aceb1` (`test: pin latest gov5 906 qualification runtime`)
- N42-gov5: `b027f3040` (`feat(interop): harden Gov5 mixed-client operation`)
- N42-gov5: `34021c3f7` (`test: make hive genesis fixture self-contained`)
- N42-gov5: `a70f7cf68` (`test: share hive fixture across packages`)
- N42-gov5: `a35aa6293` (`fix(ethel): join state-root stream producers`)
- N42-gov5: `520ea7bb7` (5.7.905 current-main interoperability candidate)
- N42-gov5: `32d6ceccb` (5.7.906 current-main interoperability candidate)
- N42-gov5: `b70505738` (pinned latest 5.7.906 interoperability candidate)

The original P4 qualification binary was built at `24210f0`. The preceding
`21ea922` commit bounds both consensus state-sync requests and
orchestrator-owned Gov5 body fetches, rotates retry peers, and makes
unsupported state-sync attempts fail explicitly instead of remaining
pending. `e1c4f99` additionally routes Gov5 block gossip and hash-fetch
lifecycle events through the reliable consensus lane in H2 participant mode,
preventing an always-readable consensus queue from starving already-arrived
execution bodies. `517b13d` releases authenticated cached bodies in execution
height order, retains the direct successor until the durable execution head
advances, and drains newly bound bodies before later consensus events in the
same batch can commit descendants. `242502c` additionally rechecks that staged
suffix after asynchronous Engine API completion advances the durable parent,
so release no longer depends on a later network message, and re-arms stale
hash-fetch de-duplication after the transport deadline. `24210f0` replaces
hash-order eviction in the bounded authenticated H2 block-view cache with
explicit FIFO insertion order. This preserves a newly authenticated live
binding even when its hash is numerically smaller than all older keys.
`ab1bb95` changes only
the P6 shell monitor. The Gov5 qualification binary was built at `b027f3040`;
the two later Gov5 commits only make an existing test fixture self-contained
in clean checkouts:

- Rust original P4 `n42-node`:
  `7fcec8e3ad22fab37d265c5509fb461684f248e57e9f5ded02e79ea3c947ce31`
- Rust selected T2 `n42-node`, built from `f49422f`:
  `c0ce2778b1deaa329416d56ced26b2c40463b6133a0a172281c3d077191e1e4d`
- Rust selected T9 `n42-node`, isolated locked release built from checkpoint
  `a72180e`: `b03eb3eddcd14a5b81fac6af900cd12b1819221507308fc0e77965c7edc55fae`
- Rust selected replay-horizon `n42-node`, built twice identically from
  `8fa9c817c`:
  `391185a473ee86f6ae4ec8d9ad7be3a458a7e7994ea7553c6852c64c7d8a236e`
- Gov5 `n42`:
  `fa02d37c1e7b480a1c3196d318cd7bc79fb2d4247e5977331b79151873a82ae7`
- Gov5 current-main integration `n42`, built twice identically from
  `912a01d29`:
  `86b61c2d710e09bf5efddac7631d450278930acd4671e6c74362de8e63057452`
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
unchanged for the entire observer window. Selected T9 Rust hash `b03eb3ed...`
and Gov5 hash `fa02d37c...` are staged separately for the maintenance-window
participant phase; the superseded T2 Rust artifact remains preserved.

The selected T2 source baseline is `f49422f`, and the Gov5 branch baseline is
`a35aa6293`; see the T2 gate evidence and `source-remote-ref-audit.jsonl`.
The former contains both audit fixes, all three H2-v4 batch-verification call
sites, and the Gov5-profile-only RPC normalization required by the failed P4
burst. The response layer distinguishes receipt-embedded logs from top-level
`eth_getLogs` results, preserving Gov5's established `null`, empty-data, and
numeric-index shapes without changing Ethereum-profile output. The latter
fixes the data race found by the mandatory full Go race gate. Live remote
audits require the n42 branch to contain that implementation and match the
local checkpoint or final-report commit exactly. This in-progress
qualification ledger is committed as a checkpoint. Its final PASS update
remains pending until all runtime gates finish.

The live checkpoint branch is `feat/gov5-n42-live-interop`. The separately
verified integration branch `integration/gov5-interop-main` is pushed at
`d579eb4` and contains the 39-commit interoperability line, current `main`,
the audit fixes, and the four cross-port hardening commits. `main` remains at
`3bbad4b` until the replacement P4 window ends, so the measured runtime and
delivery baseline cannot be confused.
The T9 isolated-build source checkpoint is `a72180e`; the authoritative H2-v4
batch-verification tip is `e89425b`. Its exact three-call-site content is
present on the live branch as `8134235` plus `04ab69e`; a tree comparison of
the affected consensus and devlog paths is exact. T2 full gates passed against
`f49422f`: Go full race, Rust format/check/Clippy/workspace tests, the isolated
release build, staged-binary signature and SHA checks, 360 exact seven-endpoint
historical RPC comparisons, committed-QC/equivocation checks, and a ten-minute
dual-new-binary liveness window all passed. The immutable T2 handoff is
`runtime-12-existing-seven-qualification/evidence/T2.PASS`; it binds the
selected source and binary plus the exact P6 finalizer and final clean-gate
controller hashes.

`t2-f49422f-both-rust-consensus-health-v2.jsonl` independently resolves both
current live Rust processes through their executable mappings. Both map to the
same staged release with SHA-256 `c0ce2778...`; the separately staged P6
participant and Gov5 artifacts also retain their expected hashes.

## Qualification runtimes

### Disposable mixed committee

- Chain ID: `1143`
- Genesis:
  `0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`
- Runtime:
  `/Users/jieliu/Documents/n42/live-interop-20260721/runtime-11-production-qualification`
- Topologies exercised: 6 Gov5 + 1 Rust, then 5 Gov5 + 2 Rust

### Current pinned 5.7.906 strict runtime

- Runtime:
  `/Users/jieliu/Documents/n42/live-interop-20260721/runtime-18-gov5-906-latest-reth`
- Topology: 5 Gov5 + 1 Rust/Reth validator
- Gov5 candidate: `b70505738`, upstream cutoff `f3dbeba46`
- Genesis:
  `0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`
- Formal evidence: `evidence/mixed-soak-24h.jsonl`, started
  `2026-08-02T14:23:42Z`
- Automated completion controller:
  `scripts/gov5-current-qualification-finalizer.sh`

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
| P4 fault and lifecycle matrix | IN PROGRESS | all prior failed, superseded, or incomplete windows remain preserved and excluded; 5.7.905 live catch-up, leader handoff, archive parity, and a 4,633-second six-endpoint soak passed, but upstream advanced before its strict window completed; the first 5.7.906 stream passed 701 seconds with maximum lag zero but was superseded by same-version upstream commits; pinned candidate `b70505738` was fully tested, reproducibly built, and resumed from exact copied chain data; runtime-18's authoritative zero-transaction stream started at `2026-08-02T14:23:42Z` after exact genesis, binary, endpoint, leader, and zero-transaction preflights |
| P5 minimal full archive+ parity | PASS | 209 RPC comparisons, 22 offline proof checks, export/import and corruption recovery |
| P6 existing seven-node rollout | IN PROGRESS | observer cold bootstrap, exact epoch crossing, and the independent 24-hour read-only window pass remain valid; continuity-v2 was excluded from final handoff continuity after the host-sleep gap, and continuity-v3 started at `2026-07-28T08:39:00Z` without restarting the healthy read-only observer; no participant has been activated |

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
network-lane, `517b13d` ordered catch-up, and `242502c` release-trigger
corrections, the new targeted network and consensus regressions pass 3 and 5
tests respectively, and both packages pass all-target Clippy with warnings
denied. The authoritative full clean-worktree rerun at `242502c` remains
deliberately gated after P6 and must pass before `FINAL_GATES.PASS`; it is not
inferred from the earlier P0 run.

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

The eligible formal successor that began at `2026-07-24T14:05:20Z` failed
closed at `2026-07-24T17:15:34Z`. Its last sample was still inside the lag
bound at three, but Rust2's duplicate-publication counter changed from two to
four, so the independent guard terminated the monitor and the finalizer
withheld the signed transaction burst. Both Rust execution heads then remained
at block 20541 while all five Gov5 nodes served block 20542 and continued to
20555. The complete 188-sample failed stream, guard/finalizer/supervisor
sentinels, and both logs are retained under the
`p4-ordered-catchup-release-trigger-failure-20260724T171534Z-*` prefix and are
excluded from acceptance.

The logs show block 20542 arriving while its parent 20541 was still completing
asynchronous `new_payload`/FCU. The direct successor was correctly staged, but
the staged-suffix release was only retried by later body/consensus handling;
the durable parent advance itself did not trigger another check. Stale
network-level hash de-duplication could also suppress a later orchestrator
retry if every terminal request event was lost. Commit `242502c` rechecks the
staged suffix after every drained execution lifecycle event and re-arms
hash-fetch state after the transport deadline without weakening hash, QC, or
identity validation.

The production binary was rebuilt in an isolated target directory and pinned
at SHA-256 `126ddec6...`. Both Rust validators reused their original persisted
datadirs. On restart, each released a 64-block authenticated suffix beginning
at the previously missing block 20542, then smaller five- and one-block
suffixes, reached a durable execution head, and rejoined live consensus. The
independent recovery regression recorded 64 samples across 658 seconds, zero
failures, maximum lag one, 111 blocks of progression, exact seven-endpoint
roots, and an empty transaction list for every newly finalized block.

The entirely fresh formal successor began at `2026-07-24T18:11:44Z`. Each
sample compared five Gov5 and two Rust RPCs at the minimum common height,
checked block hash, QMDB state root, receipts root, lag at most four, and every
newly finalized block's empty transaction list. The baseline bound source
commit `242502c`, release SHA-256 `126ddec6...`, recovered heads, and exact
warning/deadline counters after the restart transient had stopped.

That window also failed closed at `2026-07-24T21:14:08Z`. Its 180 formal
samples covered 10,888 seconds, advanced the common height from 20,788 to
22,637, and had maximum lag two through the last sample. The three-hour
milestone was written before the failure, but the later failure disqualifies
the entire window and the milestone does not count toward a replacement
window. Rust2's duplicate-publication counter changed from the pinned baseline
of nine to eleven, so the independent guard terminated the monitor; the
finalizer withheld the transaction burst and the supervisor recorded the
qualification failure.

At capture time all five Gov5 RPCs had advanced beyond block 22,644 while both
Rust execution RPCs remained at 22,637. Block 22,638, hash
`0x054530908490257ae5d3402223480cd355d647f484b31432df20eeed74fe44c9`,
was the first canonical block absent from both Rust RPCs. The retained logs
show repeated authenticated body-fetch deadline retries, Engine API
`fork_choice_updated` calls returning no payload ID for later Rust leader
slots, timeout rebroadcasts, and duplicate publication results. This is a
production liveness failure, not merely a warning-counter presentation issue;
the duplicate counter gate remains unchanged. The precise catch-up/fetch
lifecycle defect must be corrected, verified against the persisted databases,
and followed by a new full 86,400-second window from zero.

The failed sample stream, baseline, three fail-closed sentinels, pre-failure
three-hour milestone, complete Rust logs, incident audit, and verified
SHA-256 manifest are preserved under
`p4-timeout-rebroadcast-execution-stall-failure-20260724T211408Z-*`. The
incident audit SHA-256 is
`7395506d4de02566c8fcadc8a4faf6ded349b62ffc88fb94eace6da7cbf93ce9`.
No database was deleted, rewritten, recreated, or reformatted.

The first missing body had reached both Rust network services before its
proposal was authenticated. The authenticated block-view cache was already
at its 2,048-entry bound. It used `BTreeMap::pop_first()` as if that operation
removed the oldest binding; it actually removed the numerically smallest
hash. The new live `0x05...` binding therefore evicted itself, leaving the
body in the unbound cache after its fetch tracker had cleared. Descendants
could activate catch-up but could not reconnect to the durable execution
head.

Commit `24210f0` adds explicit FIFO insertion order and evicts the true oldest
binding. The deterministic regression fills all 2,048 entries with higher
hashes, inserts a low `0x05` live hash, and proves that the new hash remains
while the oldest entry is removed. The consensus-service suite passes all 182
tests; its all-target Clippy gate passes with warnings denied and its package
format check is clean. The release was rebuilt from a locked isolated target
and pinned at SHA-256 `7fcec8e3...`.

Both Rust validators restarted under a controlled guardian using their
original persisted datadirs. Rust1 released a 63-block reverse-delivered
suffix beginning with block 22,638; Rust2 had already persisted most of that
suffix during the first recovery attempt. All seven endpoints now serve block
22,638 with the exact original hash, state root, receipts root, and empty
transaction list. The independent recovery monitor recorded 64 samples over
655 seconds, zero failures, maximum lag zero, 112 blocks of progress, exact
seven-endpoint roots, and unchanged warning/deadline counters. No database
was edited, recreated, reformatted, or removed.

The next eligible formal window began from zero at `2026-07-24T22:06:27Z`.
Its monitor is scheduled for 86,640 seconds and acceptance still requires at
least 86,400 seconds between its first and last samples, at least 1,400
samples, no sample gap above 120 seconds, maximum lag four, exact
seven-endpoint roots, contiguous zero-transaction history, and unchanged
warning and deadline counters. Its independent ten-minute audit records 11
samples across 608 seconds, zero failures, maximum lag zero, a maximum
61-second gap, and 104 blocks of progress. A separate fifteen-minute audit
also resolves both Rust processes to the exact pinned release and checks all
seven RPC endpoints.

The same replacement window subsequently crossed independently captured
three-, four-, six-, twelve-hour-plus, and eighteen-hour milestones without
interruption.
The immutable
three-hour snapshot contains 179 samples across 10,828 seconds, zero
failures, maximum lag one, maximum sample gap 62 seconds, and 1,843 blocks of
progress. Its SHA-256 is
`bfeee2164b1d54a55f4cbceafa72bbac7d3138ca96cafae402daf7585bbb2008`.
The immutable four-hour snapshot contains 238 samples across 14,420 seconds,
zero failures, maximum lag one, maximum sample gap 62 seconds, and 2,456
blocks of progress. Its SHA-256 is
`7f9a77aae644f977433405c9946fca36fa05ce6a7e5618bb5f6e84896d674f0f`.
The six-hour snapshot contains 357 samples across 21,662 seconds, zero
failures, maximum lag one, maximum sample gap 62 seconds, and 3,688 blocks of
progress. Its SHA-256 is
`271839e869fea37177cf9952b1a60c7efa3a794a6e3fc4ce9cdc5965825d1d41`.
The twelve-hour-plus snapshot contains 798 samples across 48,620 seconds,
zero failures, maximum lag one, maximum sample gap 67 seconds, and 8,266
blocks of progress. Its SHA-256 is
`87e90e191c3ad03de013d01dd424c2c21877f7bead028c9620d30990ab2a8f46`.
The eighteen-hour snapshot contains 1,063 samples across 64,819 seconds,
zero failures, maximum lag one, maximum sample gap 67 seconds, and 11,011
blocks of progress. Its SHA-256 is
`57492d217f589439fa0575f674a6c8771a0fc5baa49e322696dca1797e2ff6ae`.
All five audits verify contiguous empty-block coverage, unchanged warning and
deadline counters, two Rust nodes with the same committed view and hash,
seven validators, committed QCs, and zero authenticated equivocations. The
concurrent P6 observer remained read-only with zero failures or write
violations. All intermediate audits are explicitly
`PASS_MILESTONE_ONLY`: they prove live progression only, do not count toward
the full-window gate, and do not inherit the prior failed window's
three-hour milestone.

The formal monitor then ended naturally with 1,420 samples across 86,601
seconds, zero failures, maximum lag one, maximum sample gap 67 seconds, and
unchanged warning/deadline counters. Its independent guard passed. All 17
signed transactions subsequently finalized successfully through alternating
Gov5 and Rust ingress, but the burst parity gate failed closed before the
post-burst liveness monitor: `eth_getBlockByNumber("0x9322", true)` differed
between implementations. All five Gov5 responses were byte-exact with one
another, both Rust responses were byte-exact with one another, and the only
JSON difference was Rust's extra
`result.transactions[0].blockTimestamp = "0x6a65360d"` field. The block hash,
state root, receipts root, transactions root, transaction body, and signature
fields were exact. All seven canonical heads and roots remained exact after
the failure, with zero authenticated equivocations.

This is an RPC response-shape interoperability failure, not the HIGH-2 silent
vote/execution-stall symptom. P4 is therefore FAIL, the final PASS marker was
not written, and the whole window remains preserved but ineligible. T2 may
now proceed: it must include the audit fixes, remove this Gov5-incompatible
field from the Gov5 H2 profile without weakening the exact parity gate,
rebuild the pinned binary, and restart P4 from zero.

T2 then failed closed twice during one-node historical preflight before
selection. The first candidate removed `blockTimestamp` but exposed Gov5's
empty receipt `logs:null` shape; the second candidate fixed that field but
exposed the distinct receipt-log `topics:null` and top-level `eth_getLogs`
data/index shapes. Each candidate was immediately rolled back before the
second Rust node was replaced. The final `f49422f` candidate was built in a
new empty target directory and pinned at SHA-256 `c0ce2778...`. It passed 300
exact comparisons in the one-new/one-old topology, 29 liveness samples across
291 seconds with zero failures, then 360 exact comparisons after replacing
the second Rust node. The final dual-new-binary monitor recorded 57 samples
across 591 seconds, zero failures, maximum lag one, and 99 blocks of progress.
Both Rust nodes reported the same committed view/hash, seven validators,
committed QCs, and zero authenticated equivocations. The replacement P4
window therefore started from zero at `2026-07-26T03:56:35Z` without changing
any existing database. Its first sample was healthy at common height 41,024:
all seven endpoints returned the same block hash, state root, and receipts
root, lag was zero, and the finalized interval contained no transactions.
The authoritative stream is `p4-f49422f-zero-tx-24h.jsonl`; its immutable
baseline is `p4-f49422f-formal-soak-baseline.jsonl`.

That controller exited after its last sample at `2026-07-26T16:29:41Z`.
The preserved stream contains 736 samples over 45,186 seconds, zero failed
samples, maximum lag one, maximum sample gap 81 seconds, and 7,551 blocks of
progress. All seven node processes remained live and exact; a later health
snapshot found common height 52,364 with identical block hash, state root,
and receipts root. The unmeasured gap invalidates the timing stream even
though no chain invariant failed. No burst was released and no `P4.PASS` was
written. `p4-f49422f-control-plane-interruption-20260726T162941Z.jsonl`
classifies and excludes it. Because T9 must now select a new source and
binary anyway, P4 will restart from zero on that exact baseline rather than
spend another formal window on the superseded `f49422f` binary.

One response-layer issue discovered after this start is deliberately deferred
until the formal window closes. `Gov5RpcCompatService::call` limits rewriting
to `eth_*`, while `batch` currently enables recursive rewriting for every
method in the batch. A mixed Gov5H2 batch can therefore rewrite nested
`blockTimestamp`, empty `logs`, or empty `topics` in `n42_*`, `debug_*`, or
`trace_*` results even though the corresponding single calls remain
unchanged. This is T9: batch responses must be matched back to their request
methods and only `eth_*` successes normalized, preserving IDs, order, errors,
notifications, and extensions. The current zero-transaction P4 stimulus does
not exercise that path, and no source, binary, database, acceptance value, or
window process was changed. The discovery, mobile-RPC risk, intentional
Gov5-only `eth_getLogs` quantity shape, and post-window acceptance tests are
recorded in `t9-rpc-batch-method-scope-discovery.jsonl`. T9 must close before
final delivery; it does not inherit or erase any eligible P4 time. Participant
activation is fail-closed behind `T9.PASS`, which will bind the post-fix source,
binary, and replacement finalizer hashes.

T9 is implemented by `6180ec5` plus the explicit disabled-path style follow-up
`1b8d52b`. Batch request IDs are associated with their methods before the
inner service is called; only successful responses whose IDs map uniquely to
`eth_*` requests are normalized. `n42_*`, `debug_*`, and `trace_*` responses
remain shape-compatible with their single-call paths, while an ID reused
across eligible and ineligible method families is conservatively left
unchanged. Seven focused regressions, including mixed families and ambiguous
duplicate IDs, pass. Format, all-target check, all-target Clippy with warnings
denied, and the complete workspace test run all pass; the latter contains 46
result records and zero failures. A `--locked` release build in an empty
isolated target completed in 39 minutes 39 seconds, and the resulting arm64
Mach-O passed an actual `--version` launch. `T9.PASS` was written at
`2026-07-27T06:01:08Z` and binds source checkpoint `a72180e`, binary SHA-256
`b03eb3eddcd14a5b81fac6af900cd12b1819221507308fc0e77965c7edc55fae`,
and SHA-256 values for every gate log. Exact binary copies are staged in both
runtime artifact directories without replacing any active process. The
authoritative audit is `t9-rpc-batch-method-scope-pass.jsonl`.

The pinned T9 binary was then introduced into the disposable committee one
Rust validator at a time without changing either database, key, port, or
consensus configuration. Rust-1 passed 30 exact seven-endpoint samples over
299 seconds with zero failures, maximum lag zero, and 54 blocks of progress.
Only then was Rust-2 replaced; both new binaries passed another 30 samples
over 299 seconds with zero failures, maximum lag one, and 51 blocks of
progress. Both live mixed-batch probes preserved the exact single-call
`n42_consensusStatus` shape and the untouched `debug_*` method error, both
validators retained CommitQC with validator count seven, and authenticated
equivocation remained zero. Their executable mappings independently hash to
`b03eb3ed...`.

The fresh P4 zero-transaction stream started at `2026-07-27T06:38:31Z`.
Its first sample matched all seven endpoints at height 57,184 with lag zero,
the authenticated state and receipt roots, and zero-transaction coverage
through that exact height. The monitor, independent consensus/counter guard,
and hash-bound finalizer are all active; no failure artifact is nonempty.
The immutable baseline is `p4-b03eb3ed-formal-soak-baseline.jsonl`, the
authoritative stream is `p4-b03eb3ed-zero-tx-24h.jsonl`, and the rollout plus
restart audit is `t9-b03eb3ed-rollout-and-p4-restart-audit.jsonl`. No elapsed
time from any prior P4 stream is credited. The 86,400-second threshold is not
reachable before `2026-07-28T06:38:31Z`; the 90,000-second control duration
provides sample-count and timing margin before the signed burst finalizer.

The immutable 12-hour-plus milestone at `2026-07-27T19:42:25Z` records 775
samples over 46,992 seconds, zero failures, maximum lag one, maximum sample
gap 64 seconds, and 8,003 blocks of progress. Every newly covered block was
empty and exact across the seven endpoints. Warning and deadline counters
remain byte-for-byte equal to the baseline; both Rust validators report the
same CommitQC view and hash, validator count seven, and zero authenticated
equivocation. The concurrent P6 observer recovery stream had 892 read-only
samples with zero failures and maximum lag one. This is deliberately only
`PASS_MILESTONE_ONLY`; no burst was released and neither P4 nor P6 is closed.
The audit is `p4-b03eb3ed-formal-12h-plus-milestone-audit.jsonl`.

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
identities are bound in the fresh baseline. The new formal guard SHA-256 is
`eef209a050162320e5a776e0307199b8ed4e0ff9a2080f060b91b8f56a9e037d`;
the P4 finalizer SHA-256 is
`352181219661b1968bd7cc9f1c3a84b51f5516df021cba80837e9e74d7a929e3`.
Neither the previous P4 evidence nor its failure sentinels are reused.

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
- `p4-ordered-catchup-release-trigger-failure-20260724T171534Z-formal-window.jsonl`
- `p4-release-trigger-fix-build-provenance.jsonl`
- `p4-release-trigger-fix-executable-identity-audit.jsonl`
- `p4-release-trigger-fix-recovery-summary.jsonl`
- `p4-release-trigger-fix-control-chain-rearm-audit.jsonl`
- `p4-timeout-rebroadcast-execution-stall-failure-20260724T211408Z-formal-window.jsonl`
- `p4-timeout-rebroadcast-execution-stall-failure-20260724T211408Z-incident-audit.jsonl`
- `p4-timeout-rebroadcast-execution-stall-failure-20260724T211408Z-manifest.sha256`
- `p4-binding-fifo-root-cause-and-fix.jsonl`
- `p4-binding-fifo-fix-build-provenance.jsonl`
- `p4-binding-fifo-fix-executable-identity-audit.jsonl`
- `p4-binding-fifo-fix-missing-block-exact-audit.jsonl`
- `p4-binding-fifo-fix-recovery-summary.jsonl`
- `p4-binding-fifo-formal-soak-baseline.jsonl`
- `p4-binding-fifo-formal-10m-milestone-audit.jsonl`
- `p4-binding-fifo-formal-independent-15m-audit.jsonl`
- `p4-binding-fifo-formal-3h-snapshot.jsonl`
- `p4-binding-fifo-formal-3h-independent-milestone-audit.jsonl`
- `p4-binding-fifo-formal-4h-snapshot.jsonl`
- `p4-binding-fifo-formal-4h-independent-milestone-audit.jsonl`
- `p4-binding-fifo-formal-6h-snapshot.jsonl`
- `p4-binding-fifo-formal-6h-independent-milestone-audit.jsonl`
- `p4-binding-fifo-formal-12h-plus-snapshot.jsonl`
- `p4-binding-fifo-formal-12h-plus-independent-milestone-audit.jsonl`
- `p4-binding-fifo-formal-18h-snapshot.jsonl`
- `p4-binding-fifo-formal-18h-independent-milestone-audit.jsonl`
- `p4-burst-full-transaction-rpc-parity-failure-audit.jsonl`
- `p4-burst-full-transaction-rpc-parity-failure-manifest.sha256`
- `p4-burst-full-block-gov5-reference.json`
- `p4-burst-full-block-rust-reference.json`
- `p4-binding-fifo-fix-control-chain-rearm-audit.jsonl`
- `overall-goal-alignment-binding-fifo-audit.jsonl`

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
restarted for the FIFO binding correction, both extension schedules remained
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

Because the RPC-compatibility correction makes P4 restart from zero, another
read-only observer stream was overlapped before the preceding extension could
expire. The durable stream began at `2026-07-25T23:16:06Z`; its independent
guard requires a live pinned monitor PID, evidence age at most 150 seconds,
lag at most four, `observerReadOnly=true`, and
`hasCommittedQc=false`. A first non-durable background launch exited after
one healthy sample; that control-plane launch failure and its correction are
preserved, while the original observer stream remained healthy throughout.
No participant activation or consensus-state write occurred. The fresh P6
finalizer proves continuity from the completed formal observer stream
through the post-threshold extension, the final overlap, and this durable T2
overlap, with every internal and handoff gap bounded at 120 seconds. It stops
all observer guards immediately before activation and is itself bound by
`T2.PASS` at SHA-256
`8f0091b1f78936387b2e6acd43e085eb721069ad6f1dbd6454bb28d63b1dbb83`.
The final-overlap monitor ended naturally at `2026-07-26T05:30:00Z` after
overlapping the durable stream; the durable stream and its independent guard
were later found stopped, with their final eligible sample at
`2026-07-26T14:27:17Z`. The stream had 901 samples over 54,671 seconds; its
single failed sample records the dead monitor PID. At discovery the observer
execution head remained at 65,537 while the seven Gov5 nodes had advanced to
72,554. This is a control/observer continuity failure, not a committee
failure, and it invalidates the handoff to participant activation.
`p6-t2-observer-lag-and-monitor-exit-20260726T142717Z.jsonl` preserves the
failure.

Two restart attempts used the unchanged observer binary and the existing
database. Neither opened RPC or voted. The second attempt
failed closed while replay-validating `gov5_qmdb_branches.bin`: one retained
block has missing ancestry, excessive depth, or a divergent root. The branch
file and 16-byte vote log remained unchanged at SHA-256 `e00b23ab...` and
`374708ff...`; no repair, rebuild, format, compaction, prune, or deletion was
performed. The exact binary, logs, hashes, and a post-failure exact seven-Gov5
health snapshot are in
`p6-observer-restart-qmdb-fail-closed-20260726T223713Z.jsonl`. The failed
observer database is quarantined from participant preparation until its
retained branch can be authenticated against the execution archive and a
fail-closed recovery is qualified.

The read-only branch audit resolved the combined error without weakening it.
The persisted file contains exactly 65,537 retained blocks: every parent is
present, every operation list is empty, every stored root equals the
authenticated base root, and there is no self-parent. The one rejected block
was therefore the depth-65,537 boundary against the default 65,536 replay
limit, not corrupt or divergent state. With the unchanged `73cd5bc9...`
binary and database, `N42_QMDB_REPLAY_DEPTH=131072` replayed every retained
block from the authenticated base, opened RPC, caught up, and then remained
exact with all seven Gov5 endpoints for more than five hours. At the recorded
milestone all eight endpoints matched at height 78,062; the observer still
reported `hasCommittedQc=false`, and the vote-log hash remained
`374708ff...`.

The verbose recovery/catch-up log reached 128,404,080,710 bytes before any
new formal continuity stream existed. It was stopped cleanly and recoverably
compressed to a 4,198,278,956-byte Zstd archive with content checksum and
SHA-256 recorded; no database file was removed or rewritten by log rotation.
A second cold start with the same binary, database, and explicit replay depth
again passed full validation and rejoined exactly. The eligible continuity-v2
stream started at `2026-07-27T04:44:25Z`; its independent guard binds the
observer and monitor PIDs, maximum sample age 150 seconds, maximum lag four,
exact roots, read-only status, and no participant activation. An earlier
two-sample controller-launch preflight is explicitly excluded rather than
spliced into v2. The recovery, compression, and v2 handoff are archived in
`p6-observer-replay-depth-recovery-20260727.jsonl`,
`p6-observer-depth-recovery-launch-handoff.jsonl`, and
`p6-observer-depth-recovery-v2-until-activation.jsonl`.

After T9 was registered, the waiting old-binary P6 finalizer was
stopped before `P4.PASS` and before any participant state existed. A release
barrier now requires both `P4.PASS` and a hash-bound `T9.PASS` before executing
the post-fix P6 finalizer. T9 is now satisfied by `b03eb3ed...`; P4 remains
pending, so participant activation is still blocked. This control-only rearm
did not restart P4, any node, or the observer and is archived in
`p6-t9-release-barrier-rearm-audit.jsonl`.

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
- `p4-binding-fifo-p6-observer-overlap-schedule-audit.jsonl`
- `p6-binding-fifo-release-preactivation-audit.jsonl`
- `p6-pending-proposal-r1-assertion-install-audit.jsonl`
- `p6-active-rollback-rehearsal-preparation-audit.jsonl`
- `p6-observer-t2-rpc-rerun-v2-until-activation.jsonl`
- `p6-t2-rpc-rerun-v3-overlap-guard-health.jsonl`
- `p6-observer-overlap-guard-handoff-t2-rpc-rerun-correction.jsonl`

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
The safety scan now also correlates the execution-validation
`pending_proposal` marker, the R1 send marker, and later view changes. If a
participant holds a pending proposal but never sends R1 before a later view
begins, the five-second guard fails closed and invokes the same immediate
rollback path. A healthy synthetic trace passes and a silent trace is rejected
at its exact view; the participant and final-state summaries require zero such
silent views.
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
6. actively rehearse `rollback-replacement` on this isolated qualification
   copy: stop Rust, reopen the untouched Gov5 validator directory, prove seven
   exact-root convergence for ten minutes, verify all three snapshot
   manifests, archive the redacted command timeline, and only then reactivate
   the single Rust participant. Any earlier fail-closed invariant invokes the
   same rollback immediately.

## Monitoring and preservation controls

`scripts/gov5-interop-qualification.sh` and
`scripts/gov5-existing-seven-qualification.sh` fail closed on unavailable RPC,
hash/root mismatch, unbounded lag, missing archive data, or participant
identity mismatch.

The qualification also owns an explicit idle-system and disk-idle sleep
assertion for the remaining P4/P6 windows. Host-capacity evidence records
954 GiB available storage, low file-descriptor counts, sub-2 ms local RPC
latency, and the assertion process identity.

The live phase ledger was reconciled against the completion plan without
changing any acceptance threshold.
`overall-goal-alignment-binding-fifo-audit.jsonl` binds the plan content hash,
live branch, integration branch, audit-fix tip, and selected H2-v4 batch
commit. It records P0, P1, P2, P3, P5, and the T2 rebuild as complete. The
`f49422f` P4 stream started at `2026-07-26T03:56:35Z` and is now also
ineligible because its controller exited before the threshold; both prior
windows remain immutable. This post-T2 alignment is independently recorded in
`overall-goal-alignment-f49422f-p4-restart-audit.jsonl`; it reports no
deviation and does not claim completion. T9 was subsequently added as a
response-scope gate and passed on pinned binary `b03eb3ed...`. That binary
passed one-at-a-time rollout and is now the active fresh P4 baseline.
No elapsed time from either excluded P4 stream will be credited to the new
binary. The required order is now let this new P4 window finish and pass,
qualify the P6 observer recovery and continuity handoff, activate the single
participant for its 24-hour replacement window,
active rollback rehearsal, final gates, main integration, report commit, and
push in their required order.

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

## 2026-07-27 current-main follow-up and replay-depth boundary

The T9-bound P4 window failed closed at `2026-07-27T20:17:18Z`; it is not a
completed soak and no transaction burst was released. Its 810 samples covered
49,118 seconds, advanced the common execution height from 57,184 to 65,537,
contained zero parity failures, and had maximum lag one. Both Rust validators
then rejected block 65,538 with
`QMDB ancestry exceeds the configured replay depth 65536`. Execution catch-up
repeated the same failure, the Rust leader could not build, and deterministic
timeout re-publication produced the duplicate-publish warnings that the formal
guard observed. The guard terminated the monitor and the controller recorded
exit 143 exactly as required. The live nodes were not counted as healthy merely
because consensus later continued above their execution head; this window is
preserved and excluded.

The resulting HIGH fix, commit `9d26d38`, removes the participant CLI's
duplicated 65,536 default and uses the shared QMDB replay-depth constant, now
bounded at 1,048,576.
`scripts/gov5-interop-qualification.sh` explicitly supplies the same value so
an operational launcher cannot silently fall back to a smaller horizon.
Explicit lower limits still fail closed, and no block, root, or operation is
skipped during reconstruction. The existing Rust databases have not been
deleted, reformatted, compacted, or pruned. A new binary and a fresh P4 window
are required after the code gates and one-at-a-time recovery complete.

Gov5 also changed materially while this gate was open. The online interop
branch remains frozen at `a35aa629`, while current `origin/main` is
`8797f080`. Their isolated merge is pushed as
`integration/gov5-interop-current-main-20260727 @ 912a01d29`; its two parents
are those exact commits. The merge retains both cross-client wire-fixture
suites, combines exact-phase/authenticated-view recovery with the new durable
vote journal and canonical-chain divergence diagnostics, and preserves the
RPC metrics double-check lock. A non-marker merge defect was also corrected:
the standalone phase key is now loaded for both v1 and v2 durable consensus
records, so the v2 journal cannot silently reset a recovered `TimedOut` phase.
Targeted HotStuff and JSON-RPC tests and the full `go test ./...` suite pass.

Because the Gov5 update touches consensus persistence, payload trust, protocol
bounds, direct block transfer, and QMDB persistence, the prior P4 evidence does
not qualify the new Gov5 binary. The next formal baseline therefore requires a
reproducible build of `912a01d29`, one-at-a-time Gov5 rollout, the replay-depth
fixed Rust binary, exact-root and cross-client preflights, and a completely new
86,400-second zero-transaction window. `main` remains frozen until that
baseline is selected and passes.

## 2026-07-28 selected current-main and replay-horizon baseline

The Gov5 integration branch is pushed at
`integration/gov5-interop-current-main-20260727 @
912a01d29fdc64d55b780be8d46e3dcd7519adb0`; the remote ref was re-read and
matched exactly. Targeted consensus/RPC tests, `go test ./...`, and
`go test -race ./...` pass. Two clean `make n42` builds produced the identical
pinned SHA-256
`86b61c2d710e09bf5efddac7631d450278930acd4671e6c74362de8e63057452`.
No newer Gov5 `origin/main` existed at the selection checkpoint.

The n42 replay-horizon branch is pushed at
`feat/gov5-n42-live-interop @
8fa9c817c4d99de21304a8bf7f6acd60374f6b9d`. Format, all-target check,
Clippy with warnings denied, targeted replay tests, and the full workspace
suite pass. Two release builds produced the identical pinned SHA-256
`391185a473ee86f6ae4ec8d9ad7be3a458a7e7994ea7553c6852c64c7d8a236e`.
The bounded default and the qualification launcher's explicit value are both
1,048,576; explicit smaller limits still fail closed.

The five Gov5 validators were stopped and replaced one at a time. Each
graceful stop was followed by an immutable copy of its original database
before the new process was started. The snapshot stream SHA-256 values for
Gov1 through Gov5 are, in order:
`3540d7416a9c78426db923aa6f73eddc9000f1d064b80edd52c517415e35f43e`,
`88c9104f13c9806df32732206b94e84964dae9a51496aa8ecbc5f51133700dc8`,
`9eda22f76c0eab9e86b13c0822a64f49ac3abb367110b6d699f1f5d7d3976043`,
`23168aeb3e2441dace6715a6ae723e9581ad5218bdfe6332980c42d895fbb7cd`,
and `ae94cc0d92f9530f3dda626d9aa068907f71df429785715341610182c8afdfbb`.

The two Rust validators were then stopped and replaced one at a time, again
without deleting or rewriting either retained database. Their pre-rollout
snapshot stream SHA-256 values are
`e79d7d37fa997a2d9e46b46c02f67b4a3f5095966458b400cc234500b93f7057`
and
`e98b16a253885ca606be4c5b4d51d7daf45deda630d71e6a7fe55cb72ac0b8f8`.
Each new process replayed the complete retained lineage with the explicit
1,048,576 limit before opening RPC and catching the live chain. Three
successive comparisons across all five Gov5 and both Rust endpoints had one
height, block hash, state root, and receipts root per round.

P6 remains isolated from this rollout. Its observer, continuity monitor, and
fail-closed guard are alive; samples remain `ok=true`, lag zero, read-only,
and without a committed QC. No participant PID, participant directory, or
replacement marker exists. n42 `origin/main` remains frozen at
`3bbad4ba530bc8f93ee4aebcb64584c1b0b67da6`.

The post-rollout preflight contains 30 samples from
`2026-07-28T03:17:08Z` through `2026-07-28T03:22:06Z`, zero failures,
maximum lag zero, and one state and receipts root throughout. Its SHA-256 is
`a4aa53b92970a19d156701e51172c6888c87fd57af266eb6ebfd391044cbd776`.
The formal controller then performed an independent two-sample preflight and
armed the immutable baseline at height 66,534. The authoritative zero-
transaction stream began at `2026-07-28T03:23:23Z`; the acceptance threshold
cannot be reached before `2026-07-29T03:23:23Z`. Monitor PID 12863, guard PID
12913, and finalizer PID 12914 were all alive after launch, the first formal
sample was exact with lag zero, no prior window was reused, and the signed
burst remains unreleased.

That stream is now excluded. The host entered clamshell sleep at
`2026-07-28T06:32:30Z`; the next sample at `08:15:37Z` produced a 6,187-
second gap, violating the immutable 120-second limit. At detection, 204
samples contained zero parity failures, maximum lag one, contiguous zero-
transaction coverage, unchanged warning/deadline counters, equal Rust
committed views and hashes, and zero authenticated equivocations. The
qualification failure is therefore control-plane continuity, not chain
safety. The signed burst was never released.

The stream and its baseline, launch/failure records, logs, and PID records are
preserved under
`excluded/p4-current-main-replay1m-20260728T032323Z-sleep-gap/`; the formal
stream SHA-256 is
`d7efb14b389a6511e3f34a73cd01cbe65a614d8dbae6b4e0f7b97b9b4c4839ad`.
The formal guard now checks both last-sample freshness and the largest adjacent
gap during the running window and terminates immediately above 120 seconds.
The replacement controller is launched under the macOS sleep inhibitor.
Neither binary, database, nor topology changes, and none of the excluded
elapsed time is reusable.

The same host sleep invalidated the P6 continuity-v2 handoff stream. It
contained 1,560 healthy samples and maximum lag one, but adjacent gaps of
5,801 and 448 seconds caused its freshness guard to fail closed at
`2026-07-28T08:15:36Z`. This does not revoke the already completed independent
P6 observer 24-hour gate, but v2 cannot establish the final ≤120-second
observer-to-participant handoff. Its monitor, evidence, guard records, and PID
records are preserved under
`excluded/p6-observer-depth-recovery-v2-20260728-sleep-gap/`.

The observer process remained healthy and was not restarted. Continuity-v3
started from a fresh sample at `2026-07-28T08:39:00Z`; that sample was exact,
lag zero, read-only, and had no committed QC. The v3 guard checks current
sample freshness and the maximum adjacent gap over the entire new stream.
Monitor PID 63345 and guard PID 63480 were alive after launch, no failure
sentinel existed, and participant activation remained absent.

With both P4 and P6 controllers held under explicit macOS sleep-inhibitor
assertions, the fresh P4 controller completed its independent two-sample
preflight and armed at height 68,722. The authoritative formal stream began at
`2026-07-28T08:40:30Z`; its first sample was exact at height 68,723 with lag
zero and contiguous zero-transaction verification. Monitor PID 64529, the
running freshness/gap guard PID 64578, and finalizer PID 64585 were alive
after launch, with no failure sentinel. The 86,400-second acceptance threshold
cannot be reached before `2026-07-29T08:40:30Z`; no time from the excluded
stream is credited.

At `2026-07-28T11:15:30Z`, the first post-restart immutable milestone was
`PASS_MILESTONE_ONLY`: 153 P4 samples covered 9,247 seconds with zero
failures, maximum lag one, maximum sample gap 63 seconds, and 1,571 blocks of
progress. Zero-transaction coverage was contiguous, all warning/deadline
counters matched baseline, both Rust nodes reported the same committed view
and hash with a valid CommitQC, and authenticated equivocations remained
zero. All seven execution endpoints were exact. The concurrent P6-v3 stream
had 156 healthy read-only samples, maximum lag one, and maximum gap 62
seconds. The audit is
`p4-current-main-replay1m-formal-2h-plus-milestone-audit.jsonl`; it does not
close P4 or release the burst.

At `2026-07-29T04:45:03Z`, P4 had 1,191 samples spanning 72,265 seconds,
zero failures, maximum lag one, maximum gap 63 seconds, and 12,309 blocks of
progress. P6 continuity-v3 concurrently had 1,197 healthy read-only samples
spanning 72,325 seconds with maximum gap 62 seconds. P4 remained below its
86,400-second threshold and no burst or participant activation had occurred.

The P6 current-main/replay-horizon preactivation gate now binds Rust
`8fa9c817` / `391185a4...`, Gov5 `912a01d` / `86b61c2d...`, the original
independent observer 24-hour PASS, and the fresh continuity-v3 stream. Both
pinned binaries are staged and exact in runtime 12; the existing observer and
Gov5 processes remain unchanged until P4 PASS. The finalizer, including
participant 24-hour monitoring, restart/rejoin, pending-proposal-without-R1
guard, active rollback rehearsal, and final mixed reactivation, is ARMED under
a sleep inhibitor since `2026-07-29T04:51:36Z`. Participant state and the
replacement marker remain absent.
