# Gov5 ↔ n42-26 production interoperability qualification

Date started: 2026-07-23

This report is the qualification ledger for
`gov5-n42-production-interop-plan.md`. It records implementation gates,
disposable-runtime tests, and the guarded exercise against the preserved
seven-node deployment. Machine-readable evidence and immutable log manifests
are stored in the qualification runtimes named below.

## Current 2026-08-04 qualification — latest Gov5 main and latest stable Reth

Runtime29 is excluded. It started from a 126-file, 17,318,592,333-byte exact
copy of runtime28's clean stop and advanced all six endpoints from 94,423 to
94,435, including two Rust-authored `5+5` blocks. The latest-main gate then
observed Gov5 move from `8e1d27efb...` to `57d5b0d293...`; all nodes stopped
cleanly at `00:13:05Z`, no qualification time is credited, and its data will
not be reused. The exclusion record SHA-256 is `fbed1cf1...c0c68`.

The replacement candidate `94584a4ae5c...` is pushed and includes the two
new P2P commits. They disable peer scoring for a fixed no-discovery peer set,
bound the seen-message cache to 30 seconds, and avoid setting topic score
parameters when scoring is off. They do not change chain data, genesis,
block encoding, or the consensus commit path. Targeted tests, the full suite,
and the full race suite pass. Two independent cold-cache builds are byte
identical at SHA-256 `e062a429...cf3e`. Runtime30 will be copied again from
runtime28's frozen clean data; its strict 24-hour timer starts from zero only
after the latest-main, genesis, copied-boundary, six-endpoint, and nonce gates
all pass.

Runtime30 is now the authoritative run. Its stopped-data copy contains 126
files / 17,318,592,333 bytes with identical source and target record SHA-256
`9bb438ab...e94`. The 905/906 compatibility audit, current-main canary,
completion-auditor preflight, seven-height copied-boundary replay, and final
network-matrix preflight all pass. Genesis is `b71c2810...92ec`, block 92,605
is `b88a3571...5a82`, nonce is `0x11`, txindex remains disabled, all five Gov
consensus peers are authenticated, Rust has `5+5` CommitQC, and equivocations
are zero. No migration or regeneration is required.

The strict zero-transaction window started at `2026-08-04T00:27:48Z`, common
height 94,459 and lag zero; its earliest 24-hour boundary is
`2026-08-05T00:27:48Z`. The head, resource, Gov5-main, and official-Reth
streams plus a fail-closed guardian are live, and the 17-transaction burst
remains locked. The ten-minute milestone passed with 22 head samples / 636
seconds / 66 blocks / maximum lag zero, three same-PID resource samples / 601
seconds, two exact Gov5-main samples / 601 seconds, 18 Rust `5+5` commits,
CommitQC present, and zero equivocations.

Runtime28 is now excluded. At `2026-08-03T23:50:41Z`, the independent guard
observed Gov5 `main` advance from `b8c17d046...` to `8e1d27efb...`; all
milestone and final verifiers failed closed, and all six nodes were cleanly
stopped by `23:52:02Z`. Its formal stream retains 548 samples from
`19:15:19Z` through `23:51:43Z`, 92,695–94,423 / 1,728 blocks, maximum lag
one, zero bad rows, and zero transactions. It cannot be joined to a later
window. The exclusion record binds seven original failure files and all three
formal streams at SHA-256 `ee94f9fb...03db`.

The replacement pushed candidate is `7c777432536a...`, merging the interop
line with `8e1d27efb...` and applying `gofmt` to eight malformed upstream Go
files. Stage 3 now integrates the txindex tail with the node, but it remains
explicitly opt-in behind `N42_TXINDEX_TAIL=1` and runs after the consensus MDBX
transaction. With that variable absent, the old TxLookup path remains active,
no `txindex` directory/range file is created, and the stopped 905-lineage data
requires a fresh runtime copy but no format migration or regeneration. The
first all-package test run exposed only a host-contention performance
assertion; that test then passed five times at 1.91–2.03x, and a second full
run passed. The complete race suite passed, and two independent cold-cache
production builds are byte-identical at SHA-256 `b3e4288f...7c1b`.

The next authoritative run will be a new runtime29 cloned from runtime28's
cleanly stopped data and started with official stable Reth 2.4.1. Its strict
24-hour timer must begin from zero after the new candidate, data, genesis,
copied boundary, live six-endpoint identity, and transaction nonce all pass.

The previous authoritative run was
`runtime-28-gov5-b8c-latest-reth`. Gov5 main advanced during runtime27's
ninth hour from `d12257c92...` to
`b8c17d04614346bace2fbb5c05393bdaf454cf5a`; the strict upstream guard failed
closed, no transaction was released, and runtime27's already-passed eight-hour
evidence was retained as an excluded qualification rather than counted toward
the new window. The current pushed integration candidate is
`a2da47a70f6c83c765d8a626b86ac383a4fb9551`, and its reproducible Gov binary
SHA-256 is `705abbb2084eea36523fa5ee55ccae00060ad472976d9d4ca1b2c98dc56bd664`.
The two upstream commits change only transaction-lookup segment construction
and its rebuildable in-memory tail. The optional `txindex.ranges` file is not
required for the preserved 905-lineage data, and no destructive migration was
performed.

Runtime28 is an exact copy of the cleanly stopped 905-lineage runtime26 state:
124 persistent files / 17,316,415,839 bytes match source and target with
canonical records SHA-256 `1c115b92...37b4b`. Its canary passed across five
Gov5 5.7.906 nodes and one Reth 2.4.1 validator: all six endpoints returned the
same head/hash/state/receipts identity and genesis `b71c2810...1392ec`; Rust
authored views 95,452 and 95,459 with `5+5`, CommitQC was present, and
equivocation evidence was empty. Canary SHA-256 is `13c087af...a914`.

The new zero-transaction head, resource, Gov5-upstream, and official-Reth
streams began at `2026-08-03T19:15:19Z`, first common height 92,695 and lag
zero. Finalizer preflight passed with all six sender nonces at `0x11` and zero
sends. The 1/3/6/8/12/18-hour milestones and guarded finalizer are armed; the
17-transaction burst, archive/QMDB parity, and Rust restart/rejoin remain
gated until the complete 86,640-second strict stream closes.

An earlier seven-minute monitor stream was deliberately excluded after a
static lifecycle audit found its supervisor would have treated the head
monitor's eventual successful exit as a failure and stopped the 87,000-second
resource/upstream tails early. The corrected wrappers keep each successful
monitor supervised through final closure; nodes, chain data, finalizer, and
nonce were not restarted or changed.

The corrected stream's ten-minute composite milestone passed without relaxed
criteria: 22 head samples span 637 seconds and 66 blocks with maximum lag
zero and continuous zero-transaction coverage; three same-PID resource
samples span 600 seconds; and two Gov5-main snapshots span 601 seconds and
both equal `b8c17d046...`. Rust recorded 26 `5+5` leader commits, CommitQC is
present, and equivocation evidence remains empty. Milestone SHA-256 is
`723db1a6b77d4b2276b55c65edd2e309085b60741f04e08fe3ff09f93ac8fd29`.

An early closed-log audit independently scans canonical heights 92,696 through
92,797. All six endpoints return the same continuous 102-block parent chain;
all 17 expected Rust-authored slots match 17 `5+5` log commits with exact
seven-view stride and hash order. The frozen log contains 31 timeout/pacemaker
pairs, all recovered at the next view by Rust `5+5`, with zero pending. All
251 warnings partition into allowed classes, with zero unexpected warnings or
critical signals. Frozen-log, leader, timeout, and log-audit SHA-256 values are
`f976d11c...95e8`, `81e9e574...f097`, `aa9eff62...14c`, and
`0dc84ad1...54a`.

The 15-minute composite milestone also passed. Its conservative upstream
sampling boundary yields 41 head samples / 1,213 seconds / 126 blocks,
maximum lag zero and continuous zero transactions; five same-PID resource
samples / 1,200 seconds; and three exact Gov5-main snapshots / 1,201 seconds.
Rust recorded 36 `5+5` commits with CommitQC and zero equivocations. Milestone
SHA-256 is `0e236d19...9719`.

A separate 905-lineage static-boundary audit re-hashed all 24 immutable Gov
epoch schedules, network configurations/keys, and BLS keystores against the
initial 124-file copy manifest. Every file remains exact. Genesis,
consensus/bootstrap, Rust validator/P2P keys, frozen tools, and both client
binaries retain their expected hashes; advancing chaindata is explicitly and
correctly excluded. Evidence SHA-256 is `6ea80521...203c`.

A second read-only block-identity audit targets the copied-data execution
boundary itself. All six endpoints agree on every selected canonical field at
genesis, bootstrap checkpoint 29, copied persisted head 92,605 and its
immediate predecessor/successor, initial archive head 92,677, and live common
height 92,857. In particular, copied head 92,605 remains
`b88a3571...5a82`, its parent link is exact on all clients, and the next block
is the expected Rust-authored block. The audit compares number, block and
parent hashes, state/receipts/transaction roots, miner, and transaction count;
evidence SHA-256 is `04f58aef...2e82`.

That boundary audit is also enforced after the final Reth rollover. The pushed
verifier at commit `6fc5d326...bae2` passed a mutation-free preflight at nonce
`0x11` and is now waiting on the atomic total-goal result. It continuously
replays all seven historical identities while waiting, then requires the
post-burst nonce `0x22`, exact six-endpoint latest identity, CommitQC, and zero
equivocations on the latest-Reth process before emitting its own final PASS.
Launch evidence SHA-256 is `078089fc...bb6`.

The 30-minute composite milestone passed as well. Its frozen snapshots contain
65 head samples over 1,940 seconds and 198 blocks with maximum lag zero; seven
resource samples from the unchanged Rust PID span 1,801 seconds; and four
reachable, exact Gov5-main samples span 1,802 seconds. Rust has 48 `5+5`
leader commits, CommitQC remains present, and both equivocations and released
transactions remain zero. Milestone SHA-256 is `8276dfae...d6ab`.

The live 905-data compatibility recheck found no implicit migration after the
formal chain advanced from height 92,695 to 92,911. All five Gov datadirs
still contain zero `txindex.ranges` files, all six clients remain exact with
lag zero, and no destructive data conversion occurred. The final gated
17-transaction burst and archive RPC pass will exercise lookup behavior with
new transactions. Recheck evidence SHA-256 is `f5432630...e5a8`.

A closed 30-minute log snapshot provides a deeper canonical check. Heights
92,696 through 92,930 form one continuous 235-block chain on all six
endpoints; all 40 expected Rust slots match ordered `5+5` commits with exact
seven-view stride. All 54 timeout/pacemaker pairs recover at the immediately
following Rust `5+5` view with zero pending, while all 435 warnings partition
into allowed classes with zero unknown or critical signals. The immutable Rust
log, leader audit, timeout audit, and runtime-log audit SHA-256 values are
`cc519e7e...1d4f`, `513f8e01...21a2`, `90432cd5...360f`, and
`30c7d65e...f180`.

The formal producer-distribution audit independently scans 252 consecutive
blocks at heights 92,696–92,947. Rust and each of the five Gov producers
authored exactly 42 blocks, with a continuous parent chain and no
transactions. This exact balance confirms the six active leader slots while
the configured seventh, absent validator is handled by the already-verified
timeout recovery path. Evidence SHA-256 is `80028a25...72c1`.

The repeated live archive/QMDB audit also passed after the common head reached
92,953. Two current reference proofs have byte-identical Gov/Rust roots and
proof encodings and pass the pinned offline verifier. Eleven historical
heights each pass all 19 RPC/root/proof checks, including genesis and the
bootstrap boundary. Evidence SHA-256 is `3d1ab47e...4ff9`.

The same full read-only archive/QMDB audit was repeated again around the
80-minute boundary at common reference height 93,199. Both current proofs
remain byte-identical between Gov and Rust and pass the pinned offline
verifier; all eleven historical heights from genesis through height 5,189
again pass exact RPC/root/proof parity. No transaction or data mutation was
performed. Evidence SHA-256 is
`03f3de7dc023cca83223a1263ae32fa982aaad8eb053a9f874fa5c43cd4d3d57`.
The separate continuously running copied-boundary verifier continues to cover
the high 905 persisted boundary at height 92,605.

The live client-identity matrix confirms that ports 28501–28505 all report
`N42/5.7.906`, while port 29545 reports
`reth/v2.4.1-91725e3/aarch64-apple-darwin`. Every endpoint reports chain ID
`0x477`, is not syncing, and returns the exact genesis tuple and the same block
hash/state/receipts identity at fixed height 92,971. Matrix evidence SHA-256
is `67eea2d0...5351`.

Direct startup/catch-up evidence closes the copied-data lineage chain. The
active Rust process restored persisted head 92,605 and authenticated QMDB root
from snapshot-exact view 95,450 at `19:01:54Z`. It produced canonical block
92,606 with `5+5` only 29.438 seconds later, then continuously authenticated
execution lineage through the six-endpoint formal head 92,695 and authored
the next formal Rust slot at height 92,696. All four checkpoint hashes were
re-read identically from all six RPC endpoints. Evidence SHA-256 is
`586d04fe...4676`.

The formal block-cadence audit scans 301 continuous blocks over 2,942 block
timestamp seconds. Timestamps are strictly increasing, average inter-block
time is 9.81 seconds, and the maximum interval is 40 seconds—well below the
61-second no-stall threshold even with the configured absent-validator
timeout cycle. Evidence SHA-256 is `418c4e10...1071`.

The 50-minute resource trend covers 11 samples / 3,001 seconds / 306 blocks
from the same Rust PID. Threads remain exactly 161 and file descriptors 93;
RSS increased only 11.5 MiB end-to-end, about 13.8 MiB/hour. Reth allocated
data grew about 158 KiB/hour, consensus data stayed flat, and QMDB WAL grew
about 60 KiB/hour. Even a deliberately conservative linear 24-hour RSS
projection is about 551 MiB, below the frozen 1 GiB limit. Evidence SHA-256 is
`383e11e3...0991`.

The conservative one-hour composite milestone passed without relaxed
criteria. Its 120 head samples span 3,607 seconds and 366 blocks with maximum
lag zero and continuous zero-transaction coverage. Thirteen resource samples
from the unchanged Rust PID span 3,601 seconds; peak RSS is 241,040 KiB,
threads remain 161, and file descriptors 93. Seven exact Gov5-main samples
span 3,605 seconds. Rust has 76 `5+5` commits, CommitQC is present, and
equivocations remain zero. Milestone SHA-256 is `64c648af...9505`.

The closed one-hour log audit scans canonical heights 92,696–93,068. All six
endpoints agree on the continuous 373-block parent chain, and all 63 expected
Rust leader slots match ordered `5+5` commits. Every one of 77
timeout/pacemaker pairs recovers at the next Rust `5+5` view with zero pending.
All 624 warnings partition exactly into allowed classes, with zero unknown or
critical signals. Immutable Rust log, leader, timeout, and runtime-log SHA-256
values are `f16a27a1...e644`, `5763da46...4a8a`, `40ff2727...97ca`, and
`e9fb2aca...5410`.

A supplementary one-hour Gov5 process baseline confirms all five original
PIDs remain alive and older than one hour at common height 93,091. RSS ranges
from 140,016 to 141,920 KiB, thread counts from 18 to 19, and every process has
34 open file descriptors; no Gov process was replaced. Evidence SHA-256 is
`df9227ed...1906`.

The one-hour static-boundary recheck was recomputed from the original evidence
rather than copying its verdict. It re-hashed all 24 immutable Gov files, the
genesis/consensus/bootstrap artifacts, Rust validator and P2P keys, every
frozen verifier, and the Gov5/Reth binaries. All current hashes remain exact;
the original 124-file copied-data manifest and entries hashes remain
`561ed6ad...ce5` and `1c115b92...7b4b`. Advancing chaindata is intentionally
excluded, and the audit performs no mutation. Evidence SHA-256 is
`dee51343ac6b82b5f40ed6371783854cf4a87d08fa175c2c125e394803ff929a`.
The reusable read-only rechecker is
`scripts/recheck-gov5-runtime-static-boundary.sh`.

The three-hour gate now has an additional fail-closed deep audit rather than
relying only on sampled head/resource/upstream health. After the existing
three-hour milestone passes, the frozen audit closes on a canonical Rust
block, snapshots all six logs, scans every formal block from 92,696 through
that boundary, requires the exact six-block Rust leader cadence and ordered
`5+5` commits, proves every completed missing-validator timeout recovers at
the next view, partitions every warning, rejects all critical signals, and
re-runs the complete static-boundary check.

An 80-minute full-path rehearsal exposed and correctly rejected a snapshot
boundary race in the first audit implementation: it selected the most recent
historical Rust block, but a newer timeout could be logged before the copy and
its next-view recovery immediately after it. The partial audit still proved
the 505-block canonical leader range, exact `5+5` order, warning partition,
and recovery of all 99 completed timeouts, but fail-closed rejected the one
boundary timeout as pending. Commit `5b855bab...ed4` now waits until the latest
timeout's recovery commit is present in both the live log and committed view
before selecting and freezing the boundary. The superseded artifacts are
preserved under `excluded/`.

The corrected frozen V2 (`aea2c249...a73`, static tool
`b27890ad...10ec`) then passed the same 80-minute gate end-to-end. It scans
heights 92,696–93,218: all 523 blocks form one continuous six-endpoint chain,
all 88 Rust slots are exact ordered `5+5`, all 102 timeouts recover at the next
view with zero pending, all 824 warnings are classified with zero unexpected
or critical signals, and all 24 static Gov files remain exact. Composite and
milestone SHA-256 values are `5210a5e8...fbff` and `817cce64...b878`.
V2 mutation-free preflight SHA-256 is `8e894c62...6565`; persistent
three-hour waiter PID 71290 is armed. Earlier detached/supervisor V1 launch
artifacts remain recoverably excluded and did not affect nodes, chain data,
nonce, or formal timing.

The 80-minute head snapshot contains exactly one transient lag-one sample, at
`20:16:57Z`: common height 93,071 while the fastest endpoint had observed
93,072. The next 30-second sample (31 wall-clock seconds later) is again lag
zero at height 93,073. A dedicated fixed-height replay re-read heights
93,071–93,073 from all six endpoints and compared number, block/parent hashes,
state/receipts/transaction roots, miner, and transaction count. Every field is
exact, the three-block parent chain is continuous, and all blocks are empty;
the complete deep scan through 93,218 independently includes this boundary.
This proves an RPC sampling race rather than a canonical fork. Evidence
SHA-256 is `d5b3339b1a86def1092d213c538bc0e8e6cfed532e6de20534406993fba5f1a3`.

The 90-minute composite gate subsequently passed. Its 180 head samples span
5,426 seconds and 552 blocks; the only lag-one row is the same already-closed
sample above, with no new lag event. Nineteen resource samples span 5,402
seconds and all bind original Rust PID 70765; peak RSS is 248,256 KiB, threads
remain 161, and file descriptors 93. Ten reachable Gov5-main samples span
5,407 seconds and remain exact. Rust has 108 `5+5` commits, CommitQC remains
present, and equivocations and released transactions remain zero. Milestone
SHA-256 is `cc440f0759498a1aca0f3022bb5ebb20665fed58ab952e031fa94b747da353cd`.

The frozen 90-minute resource series also supports a longer-horizon trend
check. Original PID 70765 advances 552 blocks over 5,402 seconds; endpoint RSS
growth is 20,288 KiB, about 13,520 KiB/hour. Holding that deliberately linear
slope for a full day projects roughly 550,152 KiB, well below the 1 GiB gate.
Threads remain exactly 161 and file descriptors 93. Reth allocated data grows
about 133 KiB/hour, QMDB WAL about 60 KiB/hour, and consensus allocated data
does not grow. Evidence SHA-256 is
`0d4fbf81d5483781ea53f8924b045e88f5a5504eb587ba85137fae978fd49699`.

The five Gov5 processes have a matching 90-minute-side audit. Original PIDs
70737/70743/70749/70755/70761 all remain alive and unreplaced after about one
hour 47 minutes of process time. Every node reports `N42/5.7.906`, chain ID
`0x477`, and common height 93,271 with lag zero; all five Gov endpoints and
Rust return the same eight-field fixed-block identity and the expected genesis
hash. Gov RSS ranges from 143,152 to 145,168 KiB, at most 4,832 KiB above the
one-hour snapshot; threads remain 18–19 and file descriptors exactly 34.
Evidence SHA-256 is
`3f5326c45c88cf8d204f21e7af69aa145331a3357af12180162d887cea84989f`.

A full six-producer audit then replaces sampling with every block in 97
complete leader cycles. Across heights 92,696–93,277, all six endpoints return
the same 582-row sequence of number, block/parent hashes, state/receipts/
transaction roots, miner, and transaction count; each endpoint sequence has
SHA-256 `95259664...27dd`. The parent chain is continuous and transaction count
is zero throughout. Rust and each of the five Gov producers author exactly 97
blocks, with every modulo-six slot permanently bound to one producer. This
confirms both mixed-client participation and exact Gov rotation while the
configured seventh absent validator remains covered by the timeout-recovery
audit. Evidence SHA-256 is
`03e36dd50afcbedafa464bf975ce2491cfba8bd0497c14149bcf3eff5da5d336`.

The network/consensus matrix also distinguishes execution peer accounting from
the mixed consensus channel. Each Gov endpoint reports five devp2p peers;
Rust correctly reports zero execution peers, but PID 70765 has five established
connections to Gov ports 30301–30305 and authenticates five distinct validator
peer IDs at indices 1–5. The latest leader-build log reports five connected
validator peers against a quorum requirement of four, direct-push reaches all
five, and Rust view 96,271 commits with `5+5`. CommitQC is present,
equivocations are zero, and the status API's committed block hash resolves to
the same full identity on all six endpoints. Evidence SHA-256 is
`7e7df6e05017f112228bca27c439803ee90d5a1cb94030de371bedb7f53b1f8d`.
Reusable post-restart checker `scripts/audit-gov5-mixed-network-matrix.sh`
has SHA-256 `955580f2...c533`.

That checker is now part of a separate final post-rollover gate. Pushed commit
`3093cc7f...8c5f` adds a frozen verifier that waits for the atomic total-goal
PASS (including the additional latest-Reth hour), then dynamically binds the
new Rust PID and re-runs the complete socket/authentication/quorum/direct-push/
`5+5`/CommitQC/equivocation/committed-block matrix. It additionally requires
both latest and pending nonce `0x22` on all six endpoints before publishing.
Verifier SHA-256 is `be0471b4...c809`; its frozen mutation-free preflight at
nonce `0x11` has SHA-256 `f8a6529c...d729`. Persistent waiter PID 94851 is
armed. This gate sends no transaction and does not alter the existing atomic
total verifier or copied-boundary verifier.

Pushed commit `71d11a6b...bf65` adds the final objective-level completion
auditor. It accepts completion only when the atomic total-goal result, copied
905 boundary result, and post-rollover network result all independently pass;
it then rechecks every repository and remote pin, official stable Reth tag,
both binaries, all six live client identities and genesis, CommitQC, zero
equivocations, empty failure streams, and six-endpoint latest/pending nonce
`0x22`. Auditor SHA-256 is `b87aa985...b3f0`; frozen mutation-free preflight
SHA-256 is `6591008d...1f5f` and explicitly says `completionNotClaimed=true` at
nonce `0x11`. It is intentionally executed manually only after all final
evidence and the final documentation commit are pushed, so its recorded
primary HEAD cannot be made stale by its own handoff documentation.

A post-100-minute 905-data lineage recheck passed after 6,366 formal seconds
and 648 new blocks. All five Gov datadirs still contain zero
`txindex.ranges` and zero migration markers; no destructive migration was
performed, while allocated size increased by only 24 KiB per node from the
earlier compatibility snapshot. All 24 immutable Gov files remain exact. The
audit re-executes the seven six-endpoint historical identities from genesis
through the 92,605 copied persisted head and live boundaries; genesis remains
`b71c2810...1392ec` and copied head remains `b88a3571...5a82`. A current fixed
block is also exact on all six endpoints at nonce `0x11`. Static and composite
evidence SHA-256 values are `1588df47...32e1` and
`64428199aad56c35ab1d852d76c34629f87fbcd887b700cfe22e7f7433b8bf23`.

The strict two-hour composite milestone and its V2 closed-range deep audit
both pass without relaxed acceptance. The 240 head samples span 7,245 seconds,
advance from height 92,695 to 93,433 by 738 blocks, retain continuous zero-
transaction coverage, and have maximum lag one. Twenty-five resource samples
retain original Rust PID 70765 for 7,203 seconds; RSS peaks at 248,256 KiB,
threads at 163, and file descriptors remain 93. All 13 Gov5-upstream samples
remain exact. The deep audit scans canonical heights 92,696–93,434: all 739
blocks are exact on six endpoints, all 124 Rust leader slots match ordered
`5+5` commits, all 138 timeouts recover at the immediately following view with
zero pending, and all 1,113 warnings partition with zero unexpected or
critical signals. The 24 immutable 905-lineage files also re-hash exactly.
Composite and deep-audit SHA-256 values are
`0a4f2057596b690999c07ee6898153bf97eaca54229a22a5c889b7bd48d1b314`
and `e4e87236dfe0e287af92e09f25d342a4456647a9804d7a405b3c7895fe0c61ef`.
No transaction, restart, process replacement, or data mutation occurred.

A fresh post-two-hour network matrix also passes. Every Gov execution endpoint
has five devp2p peers. Rust reports zero execution peers as designed, while
original PID 70765 retains established consensus TCP connections to all five
Gov ports 30301–30305 and five unique authenticated validator peers. Leader
view 96,432 sees five connected peers against quorum four, direct-push reaches
all five, and commits with `5+5`. CommitQC remains present, equivocations are
zero, and the committed block has an exact six-endpoint identity. This audit
is read-only; evidence SHA-256 is
`ac1234e5500bc0b61cf485a5802ac75f6e78986a58f4f3b45473aa7822c79ab0`.

The two-hour archive/QMDB repetition passes as well. At common height 93,457,
both current Gov/Rust reference proofs have exact roots and encoded bytes and
pass the frozen offline verifier. Eleven historical points spanning genesis,
the bootstrap boundary, and heights 999 through 5,189 pass all 209 RPC/root/
proof checks. The operation is read-only; evidence SHA-256 is
`959cb74ecdcb6d20bba6f07d8f925cf1025188c0175d87f248d958503666d3af`.

The frozen completion auditor was then rerun in mutation-free preflight mode
against newly pushed primary HEAD `07682df34e95e3168f16a88192171be05517ccf2`.
Its dynamic primary-HEAD/remote check passes along with all fixed Gov5, Reth,
combination, and dependency pins, both binary hashes, six live identities,
genesis, and nonce `0x11`; it still explicitly reports
`completionNotClaimed=true`. This confirms intermediate evidence-documentation
commits will not create a false source-drift failure at final closure.
Evidence SHA-256 is
`76ddcd3e7f1103c4eb0a412b0dbd6429c3b7ea40750dce4cc795a47afa042396`.

Reusable script `scripts/audit-gov5-burst-readonly.sh` (SHA-256
`4fb70ee35803e23c26be7d8fcb895a3d8980a9a9f3a39e660b76d6a4a4fb1ae4`)
independently decodes and simulates the final 17-transaction burst at the
two-hour state. Every signature recovers the expected sender, every chain ID
is `0x477`, nonces are contiguous from `0x11` through `0x21`, and all 17 raw
hashes match the artifact; intended Rust/Gov ingress counts are 9/8. All six
endpoints retain latest and pending nonce `0x11`, and both deployment and
transfer `eth_call` requests succeed. Gov deployment estimates are `0x12799`
and Rust's is `0x12b0c`, both below signed gas `0x186a0`; all transfers estimate
exactly `0x5208`. The audit sends zero transactions. Evidence SHA-256 is
`206b8ba43a9ca7cbf72fcb4507884ad04a92c056f1a4a6bccafcb53b2f8bbc3d`.

A negative fixture replacing transaction index eight's declared hash with
zero is rejected immediately with exit code one and no PASS output or live-
runtime mutation. Negative evidence SHA-256 is
`2b6f305fb419935f1985578223bd76355931045ff3b0bd9e020b5914d082b6af`.
The adjacent live resource recheck spans 27 samples, 7,803 seconds, and 798
blocks on unchanged Rust PID 70765. RSS peaks at 253,088 KiB, threads at 163,
and descriptors remain 93; head, log, and WAL logical counters are monotonic.
Resource evidence SHA-256 is
`65e04bac6aac5e4197d3a66c009b34e1499ce4847d6d569ebaa9541d1ce7896e`.

The long-run host-capacity audit passes at 267 samples and 8,064 formal
seconds, with a 31-second maximum gap and zero bad rows. The data volume has
730,728,404 KiB available while runtime28 allocates 18,002,992 KiB. Even a
deliberately extreme 1 GiB/hour growth assumption plus a separate 64 GiB
reserve projects only 44,217,392 KiB. Caffeinate PID 72825 holds system,
user-idle, and disk-idle assertions for another 99,502 seconds, exceeding the
87,336-second strict-upstream, post-window, extra-hour, and closure budget by
12,166 seconds. Evidence SHA-256 is
`5f7123675d22b314a7fcd2299ff4c6cfcea2b5ff56a1bf86790256db9ab8bef9`.

Reusable `scripts/audit-gov5-six-producer-range.sh` (SHA-256
`37aace7a053ce22f9963dec422d732ee5b10e5ae9cb9effc8ae96fbd3417e003`)
now atomically retains the six raw JSONL sequences for independent rechecking.
Its stable two-hour scan covers heights 92,696–93,565: 870 blocks and 145 full
six-slot cycles. All six RPC endpoints preserve the same 870-row number/hash/
parent/state/receipts/transactions-root/miner/transaction-count sequence,
SHA-256 `67b6bf6d...f24a`. Parents are continuous, every block is empty, and
Rust plus Gov1 through Gov5 each author exactly 145 blocks in permanently
exact slots.
Evidence SHA-256 is
`c1d3749f3f729a897e62a4a42677428360b916a5dea53a343b915efd36f4229f`.
The first output containing ephemeral paths and the second hash-only output
are recoverably retained under `excluded/` and are not used for acceptance.

Pushed commit `c469bba7a8abca056fea34e59135e887a97fee91` adds a generic
milestone raw-producer waiter, SHA-256 `393d2b36...5b70`. Frozen preflight
SHA-256 `40c69083...2d31` verifies all six nodes, empty failure streams, target
paths, and the exact frozen range auditor. Persistent PID 3492 / session 38246
now waits for the three-hour composite PASS, closes its `endHeight` down to a
full six-slot boundary, and atomically retains all six raw sequences. Launch
evidence SHA-256 is `d3097c9b...642d`; it changes no node or transaction state.

The same frozen tooling now waits independently at six, eight, twelve, and
eighteen hours as PIDs 4853/4854/4856/4861 (sessions
9868/40880/94311/7904). All four preflights verify absent target milestones,
live nodes, and empty failure streams. Each waiter consumes only its matching
composite PASS and writes a distinct JSON/raw directory. Consolidated launch
evidence SHA-256 is `e4bc03f4...d3ab`; all launches are mutation-free.

Pushed commit `98daf559b5324c35e7d274edd1a2bf7ab46a2aa0` adds the
strict-24-hour-specific raw waiter (SHA-256 `20c7f542...2fc6`). It waits only
for `mixed-soak-24h-audit.json`, which the finalizer publishes atomically
before the burst, and fixes its historical range from that zero-transaction
`endHeight`; later transactions cannot change the audited blocks. Frozen
preflight SHA-256 is `af4a0025...a2ad`; PID 7527 / session 50942 is active.
Besides six raw sequences, final output binds the soak and producer audits by
SHA-256. Launch evidence SHA-256 is `a4a3de4d...7aa2`.

The strict three-hour (10,800-second) composite gate passes. Its 359 head
samples span 10,853 seconds, grow by 1,116 blocks, retain continuous zero-
transaction coverage, and have maximum lag one. Thirty-seven resource samples
retain original Rust PID 70765 for 10,805 seconds; RSS peaks at 275,856 KiB,
threads at 163, and descriptors remain 93. All 19 Gov5-main samples are exact
over 10,815 seconds. Rust has 201 `5+5` commits, CommitQC is present, and
equivocations remain zero. No acceptance criterion is relaxed; milestone
SHA-256 is `e09f9a2a72499c72971a91f5f2ba5280d241e383f417e1a2a1cb6cdefaa112be`.

The frozen raw tool closes that exact gate over heights 92,696–93,811. Its
1,116 blocks form 186 complete cycles; Rust and Gov1 through Gov5 each author
186 blocks. All six retained raw sequences have SHA-256
`fb38033f...b0b3`; parents are continuous, slots are exact, and all blocks are
empty. Composite raw evidence SHA-256 is
`984e03f73a043137e7be3b95c57a2b9fce15b6fca4a25121d1195d11147cd9c6`.
The frozen V2 deep audit independently reaches height 93,812 and scans 1,117
canonical blocks. All 187 Rust slots are six-endpoint exact and match ordered
`5+5` logs; all 201 timeouts recover at the next view with zero pending. Its
1,619 warnings partition completely with zero unexpected or critical signals,
and all 24 immutable 905-lineage files remain exact. Deep-audit evidence
SHA-256 is `9fc98f4302ba6cf91c4016deba68f8f784d040af564b30ac8277b9810a328086`.

An independent post-three-hour raw audit covers heights 93,812–93,955. Its
144 blocks form 24 complete cycles; Rust and Gov1 through Gov5 each author 24
blocks. All six raw sequences have SHA-256 `5c6d031f...0565`, with continuous
parents, fixed slots, and zero transactions. Incremental composite evidence
SHA-256 is `b1fe9e66dcfcb829d2f00b71cfa7d4f870596b8dda23ec3166dcbda5ceb40d13`,
showing unchanged rotation through the three-to-six-hour waiting interval.

The matching three-hour archive/QMDB and network-matrix repetitions also pass.
Both current proofs at height 93,823 retain exact Gov/Rust roots and bytes and
pass offline verification; eleven historical points pass all 209 checks.
Original Rust PID 70765 still has five established Gov consensus connections
and five authenticated validators, leader quorum is 5/4, direct-push reaches
all five, and view 96,866 commits with `5+5`. CommitQC, zero equivocations, and
exact six-endpoint committed-block identity remain true. Read-only evidence
SHA-256 values are
`e289e5292eb16ad3c32e99185c53a14a6437e1ded3746ca1afcf28a8bf04fa7f`
and `2e6ca52ca2c0f7ec0a8b8b207d9c519493ee7418f7ed31133cbf0e56cd3e3201`.

The new Rust resource-trend audit passes over the three-hour window. Its 39
same-PID samples span 11,405 seconds and grow by 1,176 blocks. RSS OLS slope is
10.57 MiB/hour; conservatively extending that positive slope to 24 hours gives
486,153 KiB, below the 1,048,576 KiB limit. Threads peak at 163, descriptors
stay exactly 93, Reth data grows only 404 KiB, and consensus data grows zero.
Script/evidence SHA-256 values are `f6c5c495...bb80` and `a264d76f...ae98`;
main/mixed delivery commits are `c5b6673b...087c` and `810cc934...0c4d`.

A 3.5-hour host-capacity recheck remains strict PASS. Its 407 consensus
samples span 12,308 seconds and grow by 1,266 blocks with zero bad rows and
maximum lag one. Disk availability is 730,563,924 KiB against current runtime
18,049,804 KiB; an added 25 GiB growth allowance plus 64 GiB reserve still
fits. Caffeinate PID 72825 has 95,265 seconds remaining, leaving 12,186 seconds
beyond the upstream gate plus 8,400-second final-closure allowance; all three
sleep assertions remain active. Evidence SHA-256 is `ef86aa3a...79d`.

To preserve the running total verifier's launch-time source commitment, the
mixed working directory now tracks the pushed
`qualification/runtime28-combo-ab058` branch; both HEAD and upstream are exact
at pinned `ab058386...3d9e`. Updated delivery branch
`feat/gov5-n42-live-interop-reth-latest` retains `810cc934...0c4d` and
`809db3be...5c81`, and advances to
`ee22f8df1b5d6bf0103096a1bfd8ef38a17c3227`.
After a complete 60-second fail-closed cycle, total verifier PID 83205 remains
alive with no failure evidence. Qualification reproducibility and new-tool
delivery are therefore both retained.

Updated delivery also has an independent local worktree at
`/Users/jieliu/Documents/n42/interop-reth-latest-20260802/n42-26-delivery-latest`.
Its HEAD/upstream are exact at `ee22f8df...3227`; the branch adds the
supplemental waiter above `810cc934...0c4d` and the final completion waiter
above `809db3be...5c81`. SHA-256 values for the 905 data auditor,
final 905 waiter, and resource-trend auditor are respectively
`5bb09bb1...9a67`, `183b7901...08b3`, and `f6c5c495...bb80`. The qualification
directory remains pinned at `ab058386...3d9e`; neither worktree changes the
other.

The frozen copied-boundary verifier passes another post-three-hour preflight.
It re-executes all seven historical boundaries: genesis remains
`b71c2810...1392ec`, copied 905 persisted head 92,605 remains
`b88a3571...5a82`, and every stored identity field is exact on all six
endpoints. Live six-endpoint identity is exact, latest and pending nonce remain
`0x11`, and no mutation occurs. Evidence SHA-256 is
`f60946f5a1c77db2aa1b5a18b0306eab84b34698653325485bf7d61996848632`.

The new read-only 905 data-compatibility audit passes against the live
three-hour runtime. At pinned Gov main `b8c17d0`, variable-width builder
`337cea4` and in-memory tail `b8c17d0` are both still unwired from consensus
commit. All five Gov MDBX files exist under their original live PIDs and every
datadir contains zero `txindex.ranges` files. Chain ID, genesis, copied
persisted head 92,605, live block identity, and latest/pending nonce are exact
on all six endpoints, so no 905 data recopy or regeneration is required.
Script/evidence SHA-256 values are `5bb09bb1...9a67` and `61db5b57...141b`;
the script is also synchronized to mixed-Reth delivery commit
`8e985cf860f03081f0b2c25744cd0b69a8840faf`.

The final 905 data waiter (SHA-256 `183b7901...08b3`) passes preflight and is
active as PID 90131 / session 77080. It waits only for total-goal PASS, then
repeats the same audit with nonce `0x22` after the transaction burst, Reth
restart/catch-up, and extra hour. Final acceptance still requires five live
MDBX files, zero `txindex.ranges`, exact six-endpoint genesis/copied/live
heads, and no recopy requirement. Main/mixed delivery commits are
`1c569bf1...ab6f` and `122bf21c...b4c2`.

Final V2 completion auditor SHA-256 `cf73a512...4093` passes preflight and is
pushed in `40d0e81d...c0b1`. It does not replace the frozen base auditor;
after base PASS it additionally requires strict-24-hour six-endpoint raw and
linkage evidence, final 905 data compatibility, and the full 24-hour resource
trend. It also pins the 17-transaction burst artifact at SHA-256
`6cf05cd0...d750` and recomputes all six raw sequence hashes and line counts.
Current output is read-only `completionNotClaimed=true`; final nonce must be
`0x22`.

V2 and its resource-trend auditor are now frozen into runtime evidence with
unchanged SHA-256 values `cf73a512...4093` and `f6c5c495...bb80`. A fresh
frozen-copy preflight passes, pinning burst artifact `6cf05cd0...d750`, live
nodes, empty failure streams, and nonce `0x11`, while explicitly retaining
`completionNotClaimed=true`. Preflight evidence SHA-256 is
`7109cc4d2aa2075b6747da74aa6c3d55c1bd659e2f8f60315d742f2566d3aa8a`.

The six-hour V2 deep audit has also passed its frozen-source preflight and is
active as PID 82622 / session 95373. Harness, static rechecker, and immutable
905 baseline SHA-256 values are `037cc547...5309`, `b27890ad...10ec`, and
`6ea80521...203c`. It waits only for the strict six-hour composite gate, then
closes Rust leaders, timeout recovery, complete logs, and 24 immutable files
without mutating chain state.

The same frozen source independently preflights and starts the 12/18-hour deep
audits as PIDs 88663/88787 (sessions 15588/65112). Each consumes only its own
strict composite gate and publishes separate closed deep evidence. The
eight-hour gate already has composite and full six-endpoint raw coverage, and
the 24-hour finalizer performs the complete final deep audit, so long-run
coverage is continuous without duplicating state mutation.

Pushed commit `d0353f6c...f4ab` adds generic supplemental waiter SHA-256
`6878355c...e2e8`. Frozen 6/12/18-hour instances run as PIDs
8497/8962/9075 (sessions 55444/45824/39512). Each consumes only its strict
milestone, then automatically repeats the 12-event archive/QMDB audit,
five-socket/five-authenticated-peer network matrix, 905 data compatibility,
and 24-hour resource projection. Frozen network/data/resource auditor SHA-256
values are `955580f2...c533`, `5bb09bb1...9a67`, and `f6c5c495...bb80`.
All preflights pass without mutation.

A pre-six-hour double recheck at approximately 3 hours 45 minutes also passes.
The network matrix confirms original Rust PID 70765 still has all five Gov
consensus sockets and authenticated validators, 5/4 leader quorum, direct push
to five, and a `5+5` commit at view 97,083. CommitQC remains present,
equivocations remain zero, and the committed block is six-endpoint exact. The
905 data audit again pins Gov main `b8c17d0`, all five original live MDBX
processes, and zero `txindex.ranges`; genesis `b71c2810...1392ec`, copied head
92,605 / `b88a3571...5a82`, live identity, and current-stage nonce `0x11` are
six-endpoint exact, so no recopy or regeneration is needed. Read-only evidence
SHA-256 values are `2388c016...10b2` and `eb6e25a3...4371`. The independent
post-burst final audit still requires nonce `0x22`.

The frozen resource-trend audit over the same pre-gate window covers 44 samples
from original Rust PID 70765, 12,906 seconds, and 1,338 blocks. RSS OLS slope is
10.41 MiB/hour and the conservative 24-hour projection is 484,412 KiB, below
the 1 GiB limit; threads peak at 163 and descriptors remain 93. Evidence
SHA-256 is `5f9cd9e2...cb59`. A new six-endpoint raw rotation segment covers
heights 93,956–94,033: 78 blocks form 13 complete cycles, with Rust and Gov1
through Gov5 each producing 13 blocks and all endpoint sequence SHA-256 values
equal to `952ddac5...662e`. Its first parent exactly links to the prior segment's
height-93,955 terminal hash `ea374f94...c06a`; composite evidence SHA-256 is
`3fa0d9a2...4788`.

An independent read-only audit of the final closure DAG confirms the ordering:
the strict finalizer consumes the immutable 24-hour stream, sends the 17-item
burst, runs ten-minute post-burst stability, archive/QMDB parity, Rust restart
and catch-up, and ten-minute post-restart stability. Strict independent
verification then triggers the extra official-Reth-2.4.1 hour; total evidence,
the final nonce-`0x22` 905 data audit, base completion, and V2 completion close
in sequence with no evidence dependency cycle. All seven frozen closure
scripts pass `bash -n`; base/V2 auditor SHA-256 values remain
`b87aa985...b3f0` and `cf73a512...4093`. Fresh preflights both pass at pushed
primary `5e3a3701...e5e`, rechecking source/remotes, binaries, genesis, live
six-endpoint identity, 17-item burst artifact `6cf05cd0...d750`, nonce `0x11`,
and empty failure streams while explicitly retaining
`completionNotClaimed=true` and mutation-free status. Combined preflight
evidence SHA-256 is `a327662e...d06b`.

Commit `171baf79...01af` adds the final completion waiter and synchronizes it
to mixed delivery commit `ee22f8df...3227`. Frozen script SHA-256 is
`47c41aae...1b69`. After the final 905 data audit passes at nonce `0x22`, the
waiter automatically and resumably runs base completion followed by V2
completion. Its 60-second fail-closed loop pins live nodes, Gov5 main, all
three auditor hashes, and every failure stream without changing chain state
early. Frozen preflight passes with evidence SHA-256 `8ea5a15d...224a`.
Production PID 28426 / session 15009 remains alive after a complete cycle with
no failure evidence, removing manual triggering from the last two audits.

A read-only archive/QMDB repetition near four hours of process uptime also
passes. At current height 94,105 both Gov/Rust proof roots and bytes are exact
and pass the frozen offline verifier; all eleven historical points retain exact
RPC/root/proof parity across twelve events. Evidence SHA-256 is
`44f3ffa1...5ccb`. The simultaneous static-boundary recheck recomputes all 24
immutable 905 files, genesis/consensus/bootstrap artifacts, validator and P2P
keys, frozen harness/finalizer/independent/QMDB/total tools, and both Gov/Rust
binaries. Every initial SHA remains exact; evidence SHA-256 is
`b03e479c...ab64`.

The strict four-hour (14,400-second) four-part gate closes without relaxed
acceptance. Its composite has 477 head samples over 14,430 seconds, grows by
1,494 blocks, and retains maximum lag one with zero bad rows. Forty-nine
same-PID resource samples span 14,406 seconds; 25 exact Gov5-main samples span
14,419 seconds with maximum gap 601 seconds. Rust accumulates 264 `5+5`
commits, equivocations remain zero, and no transaction is sent. Composite
evidence SHA-256 is `18d40534...e4a5`.

The frozen six-endpoint raw audit closes heights 92,696–94,189. Its 1,494
blocks form 249 complete cycles; Rust and Gov1 through Gov5 each produce 249
blocks. All six sequences share SHA-256 `f6716c23...c4e1`, with exact parents,
slots, and zero transactions; composite evidence SHA-256 is
`8f48cb81...0fc8`. Independent deep audit reaches height 94,196 across 1,501
blocks. All 251 Rust slots are six-endpoint exact `5+5`; all 265 timeouts
recover in the next view with zero pending. All 2,132 warnings partition with
zero unexpected or critical signals, and 24 immutable files retain their
initial SHAs. Deep-audit and frozen-Rust-log SHA-256 values are
`ef5559e7...771f` and `d994013f...9604`.

The matching supplemental audit also passes. Twelve archive/QMDB events are
exact; five consensus sockets and five authenticated validators retain quorum,
view 97,293 commits with `5+5`, and its block is six-endpoint exact. The 905
data still contains zero `txindex.ranges`; genesis and copied head are exact,
with no regeneration required. Resource OLS slope falls to 8.83 MiB/hour and
24-hour RSS projects to 434,568 KiB. Supplemental and archive/network/data/
resource child evidence SHA-256 values are `e03e8047...3262`,
`c7a6ea03...06a3`, `88a6e3ae...5a8d`, `ecf5e405...e640`, and
`95e27c1c...c1c4`.

The four-hour host-capacity recheck also passes strictly. Its 486 head samples
span 14,703 seconds and grow by 1,518 blocks with maximum lag one and zero bad
rows. Disk availability is 730,451,864 KiB against runtime 18,079,660 KiB; an
added 25 GiB allowance plus 64 GiB reserve still fits. Caffeinate PID 72825 has
92,877 seconds remaining, leaving 11,896 seconds beyond the 87,000-second
upstream gate plus 8,400-second final-closure allowance; all three sleep
assertions remain active. Evidence SHA-256 is `9f7bf2a7...5cc3`.

The four-hour closure linkage then rehashes and cross-binds all four evidence
families. Composite and six-endpoint raw boundaries are both exactly
92,696–94,189, covering 1,494 blocks / 249 cycles. Every raw file has 1,494
rows and identical SHA-256 `f6716c23...c4e1`. Embedded milestone hashes in
both deep and supplemental evidence equal `18d40534...e4a5`; deep coverage
extends through 94,196, and the frozen Rust log remains
`d994013f...9604`. Linkage evidence SHA-256 is `1a04be1f...dd7b` and explicitly
records that the historical window cannot be altered by the later burst.

After four hours, an auxiliary latest-Reth preflight mistakenly invoked the
stale rollover copy under runtime `artifacts/scripts` (SHA-256
`778e77c1...7664`, with its older embedded harness pin). It failed closed at a
static SHA assertion before any transaction, node restart, or chain mutation.
The original 282-byte failure is preserved byte-for-byte at SHA-256
`6b41bea4...8fe8` under
`evidence/excluded-operator-preflight-wrong-rollover-copy-20260803T232257Z/`;
its exclusion record SHA-256 is `6d7d171e...6564`. Latest independent correctly
observed that temporary failure stream and exited; its preserved 204-byte
derived failure has SHA-256 `a58c0ec9...463f`, with derived exclusion record
SHA-256 `4b98bcdc...f5a8`. Formal 24-hour evidence is unchanged.

The formal `evidence/official-reth-stable` rollover copy (SHA-256
`68c1f209...ca0`) then passes a real preflight. Reth v2.4.1 / `91725e3a`, binary
`0a4dbcf3...62b9f`, live six-endpoint identity, and genesis are exact without
mutation; evidence SHA-256 is `cac545d2...79b7`. Dependent waiters are rearmed
with their original frozen parameters: latest independent PID 87801/session
35229, total PID 87802/session 30844, final 905 PID 88093/session 66123, and
completion PID 88094/session 38136. After a complete 60-second cycle, all four
and formal rollover PID 80652 remain alive with empty new failure streams and
continued zero lag.

The recovered latest-Reth independent verifier was then exercised directly,
using the same frozen invocation consumed by PID 87801. Its verifier SHA-256
is `9b90145b...02b5`; official stable tag v2.4.1, live six-endpoint identity,
genesis, and latest/pending nonce `0x11` are exact, and no mutation was
performed. The post-exclusion preflight is PASS at SHA-256
`a6f3ff6f...47de`, independently confirming that the rearmed waiter no longer
references the stale auxiliary rollover copy.

A fresh read-only 5.7.905 data audit at `2026-08-03T23:45:25Z` also remains
PASS. All five live Gov MDBX databases are present and contain zero
`txindex.ranges`; all six endpoints report chain ID `0x477`, genesis
`b71c2810...1392ec`, copied height 92,605 hash `b88a3571...5a82`, and
latest/pending nonce `0x11`. The variable-segment builder and in-memory tail
remain unwired outside their implementations/tests at pinned main
`b8c17d04...`, so data recopy or regeneration is still not required. The
mutation-free evidence SHA-256 is `06a33a4c...5501`.

The strict 2.5-hour (9,000-second) composite gate passes. Its 299 head
samples span 9,034 seconds, grow by 924 blocks, retain continuous zero-
transaction coverage, and have maximum lag one. Thirty-one resource samples
retain original Rust PID 70765 for 9,004 seconds; RSS peaks at 263,824 KiB,
threads at 163, and descriptors remain 93. All 16 Gov5-main samples are exact
over 9,012 seconds. Rust has 170 `5+5` commits, CommitQC is present, and
equivocations remain zero. No acceptance criterion is relaxed; milestone
SHA-256 is `dc639f5d1b46f9b3e88a0e9024c48d7c50609bf477a876119c5dbf695757cdf7`.

The frozen raw producer tooling then consumes that exact milestone boundary.
Heights 92,696–93,619 contain 924 blocks and 154 complete cycles. Each of the
six endpoints retains a 924-row full-identity sequence with the same SHA-256,
`8763d282...6691`. Parents are continuous, every block is empty, and Rust plus
Gov1 through Gov5 each author exactly 154 blocks in fixed slots. Composite raw
evidence SHA-256 is
`448b88f76d6a07fe5a66b365da8244f722df6b86ccebcc1ed6fa13487cf3faa6`.

The frozen V2 deep audit then closes the same milestone on Rust-authored height
93,638. It scans 943 canonical blocks; all 158 Rust slots are exact on six
endpoints and match ordered `5+5` logs. All 172 timeouts recover at the next
view with zero pending. The 1,383 warnings partition completely with zero
unexpected or critical signals, and all 24 immutable 905-lineage files remain
exact. Deep-audit evidence SHA-256 is
`b29b5e2c8213afde8819208a7a255f8501e60e2d6058355d8f93c12f559eeb02`.

Parallel 2.5-hour archive/QMDB and network-matrix repetitions also pass. Both
current proofs at height 93,649 retain exact Gov/Rust roots and bytes and pass
offline verification; eleven historical points pass all 209 checks. Original
Rust PID 70765 still has five established and authenticated Gov consensus
peers, leader quorum is 5/4, direct-push reaches all five, and view 96,663
commits with `5+5`. CommitQC, zero equivocations, and exact six-endpoint
committed-block identity remain true. Read-only evidence SHA-256 values are
`14d50609abd423a9efa21dbdf9c92587c510f31b92c7bdf938da4f67f9f03ec8`
and `66a376f35049cd9dbbd04d708d10350ddd26ec82d4b9c0e64401cac350f30a1a`.

The frozen copied-boundary verifier independently passes another preflight at
2.5 hours. It re-executes all seven historical boundaries: genesis remains
`b71c2810...1392ec`, copied 905 persisted head 92,605 remains
`b88a3571...5a82`, and every stored identity field is exact on all six
endpoints. The live six-endpoint identity is exact, latest and pending nonce
remain `0x11`, and no mutation occurs. Evidence SHA-256 is
`6f0b8128cbaaf156f957b59764144155b138f1c93633aaa93bccbdfc3aaa8245`.

The superseded runtime27 candidate passed all `internal` and `cmd/n42` tests;
two consecutive optimized builds were byte-identical. Its pinned Gov binary
SHA-256 was `72e918d9500169e227ef1a0c9d5dd751dcd7d58f1df0871825b61f196e3fce95`.
The paired Rust binary is official-stable Reth 2.4.1 at
`91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b`, combined and pushed at
`ab05838691e6ec71f5df0faa1d3eefb1fc9d3d9e`, with binary SHA-256
`0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f`.

The repeated Gov5 updates were handled fail-closed. Runtime22 was excluded
when main moved from `ddcdaa2f6...` to `5afabac1f...`; runtime23 was excluded
when it moved to `9c821032e...`; runtime24 was excluded when it moved to
`379046b97...`; runtime25 was excluded when main moved to `d09b3ad00...`; and
runtime26 was excluded after 731 seconds of healthy formal evidence when main
moved to `d12257c92...`. None of these
excluded runs released a transaction. Each exclusion preserves the exact
six-endpoint head, Rust `5+5` leader evidence, old/new upstream commits, and a
recoverable stopped data directory.

Runtime27 is an APFS copy-on-write clone of the cleanly stopped runtime26
state. A full source-versus-target content pass compared all 124 persistent
Gov/Rust files, totaling 17,316,415,839 bytes; both canonical manifests hash
to `1c115b9226bbc303092ae893fa7c0b86a50fae8080adb49e2e7746339be37b4b`.
The JSON evidence SHA-256 is
`50433f934a854e24206e023bb2e9dcb4e398dc60c62afd9d76a0dd39aefe132f`.
The copied genesis artifact remains byte-exact at
`561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687`;
all six RPC endpoints return genesis
`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`.

The current-main canary passed with exact heads, roots, and receipts across
five Gov nodes and one Rust node. Rust authored views 95,452 and 95,459 with
full `5+5` votes, CommitQC was present for all seven configured validators,
and equivocation evidence was empty. The canary evidence SHA-256 is
`1d79bd4ea045b89994ea69574da66c229ec17a0aaf73b491d3fe0a7dc673379b`.
Mutation-free burst, strict-independent, latest-Reth rollover, and
latest-Reth-independent preflights all passed at sender nonce `0x11` with zero
transactions sent.

The independent 15-minute leader checkpoint scanned canonical heights 92,624
through 92,732. All six endpoints returned the same continuous 109-block
parent chain and the same 19 expected Rust-authored slots. Commit-log views
95,473 through 95,599 had exact stride seven, hash order, and `5+5` votes;
checkpoint evidence SHA-256 is
`b73e59ef86bb72691c6a80d1c0131bfc79ce3846a3bf0fd3dc0c954c0fd65ef2`.
The same checkpoint froze Rust log SHA-256 `53381bef...0f691` immediately
after recovery view 95,620. Its independent timeout audit found 24/24 matched
timeout and pacemaker events at exact stride seven, all recovered in the next
view by Rust `5+5`, with zero pending timeouts. The frozen-log partition audit
accounted for all 200 warnings exactly and found zero unexpected warnings or
critical signals. The timeout and log-audit evidence SHA-256 values are
`fbc874d6...13c1c` and `ce464745...c3620` respectively.
The 20-minute resource checkpoint covers five samples over 1,201 seconds with
one unchanged Rust PID and 132 blocks of head growth. Peak RSS was 238,976
KiB, threads 162, and file descriptors 93, all well below the frozen limits;
logical storage and log counters were monotonic. Its snapshot and audit
SHA-256 values are `1dd0d0c9...8a503` and `9389d086...af14`.
The read-only archive/QMDB checkpoint then compared 11 historical RPC points
and two reference proofs at height 92,767. Gov and Rust returned byte-exact
RPC payloads, proof roots, and proof bytes, and the frozen offline verifier
accepted every proof. No transaction or chain mutation was involved; evidence
SHA-256 is `d9ab509e182789d6fbe96084c54d3325aab2ea73e0a6b162737dc7a3411bb26c`.
The 20-minute identity recheck also confirms chain ID `0x477` and, on all six
endpoints, genesis hash `b71c2810...1392ec`, genesis state root
`91a450c1...9941`, and genesis receipts root `56e81f17...b421`. At height
92,773 all six latest block hash/state/receipts tuples were exact; 28 Rust
`5+5` commits had been observed through view 95,641, with CommitQC and zero
equivocations. The latest-identity and detailed-genesis evidence SHA-256 values
are `99d3af7d...0c163` and `6baf439e...deff6`.
The signed burst artifact was also independently decoded offline before any
broadcast. All 17 raw transaction hashes recomputed exactly, every signature
recovered sender `f39fd6e5...2266`, every EIP-155 chain ID was `0x477`, nonces
were continuous from 17 through 33, deployment and transfer semantics were
exact, and the CREATE address recomputed to `9a9f2ccf...63ae`. The alternating
ingress plan contains eight Gov and nine Rust submissions. The reusable
verifier SHA-256 is `90c2bf05...8a51`, PASS evidence SHA-256 is
`caf40cea...b3ce`. Deliberately altered transaction hash, chain ID, nonce
sequence, and ingress-plan artifacts are all rejected.

The 30-minute composite milestone independently froze and re-audited all
three qualification streams. Heads covered 61 samples / 1,826 seconds / 198
blocks with max lag two and zero transactions; resources covered seven samples
/ 1,801 seconds with one PID; Gov5 upstream covered four exact snapshots /
1,803 seconds. Rust had 37 `5+5` commits, CommitQC was present, equivocations
were zero, and no failure evidence existed. Milestone evidence SHA-256 is
`b71a6b466e79c3d2f7a7a56675b0e68a123896bcf1aba7709d1f3b726af1ca29`.

The strict one-hour composite milestone passed at `2026-08-03T10:50:56Z`
without relaxing acceptance. Its frozen head stream contains 121 samples over
3,651 seconds, grows from height 92,623 to 93,031, has a maximum 31-second
sample gap and maximum lag two, and verifies zero transactions throughout.
The resource stream contains 13 samples over 3,602 seconds from the unchanged
Rust PID 89930; peak RSS was 250,096 KiB, threads 162, and file descriptors 93.
Seven upstream samples over 3,605 seconds all resolve exactly to Gov5 main
`d12257c92...`. Rust recorded 71 `5+5` leader commits, CommitQC remained
present, equivocations remained zero, and no failure evidence existed. A
separate read-only recomputation rechecked the milestone, all three frozen
streams, limits, monotonicity, PID identity, and upstream pin. Milestone
evidence SHA-256 is
`40b0c17fd2d512a6ca80593ae22ef902494d82f483d23eba42a6041fcef1506a`.

A closed-log deep audit immediately after that milestone strengthens the
leader-count check into a canonical range proof. It scans all 450 blocks from
92,624 through 93,073, proves a continuous parent chain and exact responses
from all six endpoints, and matches all 75 expected Rust-authored slots to 75
ordered log commits with view stride seven and `5+5` votes. On the same frozen
Rust log, all 77 timeout/pacemaker pairs are complete and recover in the next
view through Rust `5+5`; all 630 warnings partition into the known categories,
with zero unexpected warnings or critical signals. The frozen Rust log,
leader, timeout, and log-audit SHA-256 values are respectively
`2ac01623...6a18`, `ce7dab33...48db`, `d8f76e88...1f26`, and
`1cc39713...7ff0`. The frozen controller chain also passes syntax, hash,
dependency, and output-collision checks; the old runtime paths remaining in
script defaults are overridden by the already-passed runtime27 launch binding.

An additional 90-minute composite milestone passed at
`2026-08-03T11:20:24Z`, again without relaxing acceptance. Its frozen head
stream contains 179 samples over 5,415 seconds, grows 606 blocks from 92,623
to 93,229, has a maximum 31-second gap and maximum lag two, and remains
zero-transaction throughout. The resource stream contains 19 samples over
5,403 seconds from unchanged Rust PID 89930; peak RSS was 253,616 KiB,
threads 162, and file descriptors 93. Ten Gov5-upstream samples over 5,408
seconds all remain exact at `d12257c92...`. Rust recorded 104 `5+5` leader
commits, CommitQC remained present, and equivocations and failure evidence
remained zero. Independent recomputation of the summary and all three frozen
streams passed; milestone SHA-256 is
`28487e6d0d17e05dd33382c06b857180c5bb5ce5482e937cc3ea0c9a8884a158`.
The first attempt to add this optional waiter as a detached shell child was
reaped with an empty log before producing any output; it is preserved under
`excluded/runtime27-ninety-minute-detached-launch-not-persistent/`. The
tool-managed replacement did not restart nodes or formal streams and is the
only attempt counted here.

The 90-minute read-only archive/QMDB rerun also passed at live reference
height 93,241: all 11 historical RPC comparisons were byte-exact, and both
reference proofs had identical Gov/Rust roots and bytes and passed the frozen
offline verifier. Evidence SHA-256 is
`c981bfc57c39b3dffc7b3ef5967141d7da708254d810bbb2fbd86a47384dbe3a`.
The reusable current-main canary recorder now additionally fail-closes on all
six chain IDs, full genesis hash/state/receipts roots, sender latest/pending
nonces, and records client versions while retaining the prior endpoint
`.genesis` string schema. Its SHA-256 is `e4840036...e770`; an intentionally
wrong nonce was rejected with no output or chain mutation. The resulting
height-93,265 checkpoint confirms chain ID `0x477`, genesis
`b71c2810...1392ec` / state root `91a450c1...9941` / receipts root
`56e81f17...b421`, nonce `0x11`, exact six-endpoint latest identity, five
Gov5 5.7.906 clients, official Reth 2.4.1, CommitQC, 110 observed Rust `5+5`
commits, and zero equivocations. Checkpoint SHA-256 is
`3dd0de1c0956375119c1e1a812bd21aab2b0bbb6c0c5962e3a2c550d63442d43`.

The optional two-hour milestone then exposed a resource-auditor semantics
bug, not a node or consensus failure. `du -sk` measures allocated filesystem
blocks: while the same Rust PID advanced normally, consensus allocation moved
from 87,400 to 85,532 and then 87,580 KiB during compaction. Head, log bytes,
QMDB WAL, all resource ceilings, six-endpoint identity, and the end-to-end
allocation change remained healthy. The old auditor nevertheless rejected
the 1,868-KiB transient decrease because it treated allocated blocks as a
logical monotonic counter with a four-KiB tolerance. That first optional
snapshot and failure are preserved under
`excluded/runtime27-resource-compaction-auditor-controller-rearm/` and do not
disqualify the uninterrupted authoritative streams.

The corrected auditor permits nonnegative allocated-block measurements to
decrease during compaction, explicitly reports maximum Reth and consensus
step decreases, and still strictly requires one PID, monotonic head/log/WAL,
bounded sample gaps, head growth, and the RSS/thread/descriptor ceilings. A
synthetic 1,868-KiB compaction passes, while a synthetic logical log-byte
decrease fails; the real frozen resource snapshot also passes. The updated
harness, finalizer, and independent-verifier SHA-256 values are
`037cc547...5309`, `e116089d...f9c0`, and `39b11db6...102d`. Both mutation-free
preflights passed at nonce `0x11`; their SHA-256 values are
`b08efb69...2005` and `7e320726...d0cf`.

Only waiting controllers were rebound. All six node PIDs, all three formal
monitor PIDs, the official-Reth monitor, monitor PID guardian, and caffeinate
remained unchanged; no transaction, restart, chain mutation, or elapsed-time
reset occurred. The new exact-PID guardian is PASS and the strict independent
waiter is again held by the immutable gate. Rearm evidence SHA-256 is
`f534f806afd21948569cdbaec69f4fb406e1b4d2836bce8ec7a8e918355a285c`.

The corrected two-hour composite milestone passes independently. Heads cover
254 samples / 7,690 seconds / 864 blocks with maximum lag two and continuous
zero transactions. Resources cover 26 samples / 7,504 seconds from Rust PID
89930, with maximum RSS 261,344 KiB, 162 threads, 93 descriptors, monotonic
head/log/WAL, and the explicitly reported 1,868-KiB consensus compaction.
Thirteen Gov5-upstream samples span 7,210 seconds and remain exact at
`d12257c92...`. Rust has 148 `5+5` commits, CommitQC is present, and
equivocations and failure evidence are zero. Milestone SHA-256 is
`ce33a8b268acb8a85e0b16b1f0b492c6c76c26fc4922dc08b91abf6cb9cf9806`.

A fresh mutation-free 135-minute identity checkpoint specifically rechecked
the 905-data/906-binary boundary and the previously suspected genesis change.
All six endpoints reported chain ID `0x477`, genesis hash
`0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`,
genesis state root `0x91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941`,
and genesis receipts root
`0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421`.
At height `0x16d65`, latest hash/state/receipts were byte-for-byte identical,
the sender latest and pending nonce remained `0x11`, client versions were Gov5
5.7.906 and official Reth 2.4.1, and 156 Rust `5+5` commits, CommitQC, and zero
equivocations were present. Checkpoint SHA-256 is
`9f1881315a3a11a18d8ee2d6d4c2e8fde652cea285b9057b8be313e4603effb6`.

The unified 140-minute frozen-log audit extends the deep leader proof from
height 92,624 through 93,601. All 978 blocks form one parent-continuous
canonical range, all six endpoints agree on every expected leader block, and
all 163 Rust slots have exact `5+5` log entries, seven-view stride, and hash
order. The same immutable Rust log contains 165 timeout/pacemaker pairs; all
165 recover at the immediately following view with Rust `5+5`, leaving zero
pending timeout. Its 1,351 warnings partition exactly into the accepted
timeout, compact-eviction, Rust-commit, and duplicate-suppression classes,
with zero unknown warnings or critical signals. The immutable log, leader,
timeout, and runtime-log SHA-256 values are `a1ad313e...515a`,
`1eb7eeb5...8bcc`, `56dcb732...d37b`, and `f096a71e...d54c`.

A fixed-path 150-minute rolling composite then passed with 299 head samples /
9,055 seconds / 1,014 blocks / maximum lag two, 31 same-PID resource samples,
and 16 exact Gov5-upstream samples. A second live compaction lowered consensus
allocation by 1,944 KiB in one step and left its end-to-end allocation 740 KiB
below the stream start, while head, log bytes, and QMDB WAL remained monotonic
and every resource ceiling remained satisfied. This independently exercises
the corrected allocated-storage semantics beyond the earlier 1,868-KiB case.
The rolling summary SHA-256 is
`349481a3ee0b4a7ab934345deb140878e06b9a612cf22e99d519c99f7120faa0`.

The formal three-hour milestone and an independent rerun both pass without
relaxed acceptance. Heads contain 358 samples / 10,845 seconds / 1,218 blocks,
with a 31-second maximum gap, maximum lag two, and continuous zero-transaction
coverage. Resources contain 37 samples / 10,806 seconds from Rust PID 89930,
with maximum RSS 268,000 KiB, 162 threads, 93 descriptors, monotonic
head/log/WAL, and the observed 1,944-KiB compaction. Nineteen exact Gov5-main
samples span 10,815 seconds. The milestone records 206 Rust `5+5` commits,
CommitQC, seven validators, zero equivocations, and zero transactions. Its
SHA-256 is `953e03d8cd0f3b955a9e9ae4c1fb2d54475fcd6cd2bea1c5a04560469695d782`;
the independent recheck SHA-256 is
`1e4f317926f5187a26795d166647470f13e5b9c8e8d2cd1928358b99f92376eb`.

The simultaneous identity checkpoint again proves chain ID `0x477`, the full
pinned genesis hash/state/receipts tuple, all-six latest identity, sender nonce
`0x11`, Gov5 5.7.906, official Reth 2.4.1, and zero equivocations; its SHA-256
is `b2afa7bc26b26f56a0cbf8d16aac2f58b45c071e07820ef8778bb70c29f4c3b1`.
The permanent resource-auditor regression test is executable at
`scripts/test-gov5-resource-auditor.sh`; it accepts allocated-block compaction
and rejects log/WAL/head rollback, PID replacement, nonpositive allocation,
and oversized sample gaps. Its SHA-256 is `73822807...f7f`.

The full three-hour canonical leader audit independently scans heights 92,624
through 93,841. All 1,218 blocks are parent-continuous; all 203 expected Rust
slots are byte-identical at the six endpoints and have matching `5+5` log
entries, exact seven-view stride, and exact hash order. One immutable log tree
contains 212 timeout/pacemaker pairs, all recovered by Rust `5+5` at the next
view with zero pending. Its 1,732 warnings partition exactly, with zero unknown
warnings and zero critical signals. The immutable-log, leader, timeout, and
runtime-log SHA-256 values are `4609b765...aac7`, `cd9e2e38...4876`,
`8598364e...764b`, and `6ffb346d...7a18`.

The three-hour dependency-delivery recheck also passes. The pushed mixed-client
combination branch is exactly `ab058386...`, the pushed Reth delivery branch is
exactly `91725e3...`, and the pushed dependency-upgrade branch is exactly
`aec34a0...`; each tracked worktree is clean and each remote branch matches its
local HEAD. Gov5 candidate `d0999e7...` still tracks upstream main `d12257c...`,
the official latest stable Reth tag remains v2.4.1 at `8eb21017...`, and the
Gov5/Rust binary SHA-256 values remain `72e918d9...` and `0a4dbcf3...`.
Machine evidence SHA-256 is
`9d3fbf70a7725ed906bf37fa873c3b5b73624137ec12c137238b4a93c9d27b54`.

The three-hour 905-data static-boundary recheck passes as well. The immutable
copy evidence still binds 124 files / 17,316,415,839 bytes to identical source
and target manifest SHA-256 `1c115b92...`; all 24 epoch schedule, network
configuration, network-key, and BLS-keystore files across the six preserved
Gov data directories still match their initial-copy hashes. Genesis,
consensus/bootstrap configuration, validator/P2P keys, frozen harness,
finalizer, independent verifier, QMDB verifier, and both binaries also retain
their pinned SHA-256 values. Running chaindata is deliberately excluded because
correct block production must mutate it. Evidence SHA-256 is
`b1b4306dc929720719058960d68430344f0b68cc282a226b27ce4d6e45d20955`.

A read-only archive/QMDB checkpoint immediately after the three-hour gate also
passes. At live reference height 93,871, two Gov5 account proofs and Rust QMDB
proofs have identical roots and bytes and both verify offline. Eleven fixed
historical heights from genesis through 5,189 pass 209 block, receipt, log,
state, storage, and proof checks with exact Gov5/Rust RPC results. All six
pending nonces remain `0x11`, and the checkpoint sends no transaction. Its
SHA-256 is `c9336afeb6958cddb2f60f9017c43a242a56f042cbd7cbd822f1b499585ba4be`.

The authoritative zero-transaction stream began at
`2026-08-03T09:49:44Z`, common height 92,623, lag zero. It requires continuous
six-endpoint hash/state/receipt equality, zero transactions in every newly
observed block, maximum lag six, 86,400 seconds of elapsed samples, continuous
Gov5-main and official-Reth-stable pins, and bounded Rust resources. Exact-PID
guardians cover all six nodes, the three evidence streams, finalizer,
immutable-log gate, both independent verifiers, latest-Reth rollover, and
sleep prevention. The one-hour milestone has passed; 3-, 6-, 12-, and 18-hour
milestones remain armed. After the strict window, the guarded finalizer
performs the 17-transaction dual-ingress
burst, archive/QMDB checks, ten-minute post-burst run, controlled Rust restart,
ten-minute post-restart run, immutable log verification, and independent
re-execution. Only then may the additional one-hour latest-stable-Reth run and
its independent verifier begin. No final acceptance is claimed before the
atomic total-goal verifier passes.

The formal four-hour composite milestone also passes without relaxing any
acceptance rule. Its 477 head samples span 14,453 seconds and grow from height
92,623 to 94,249, with a 31-second maximum gap, maximum lag two, and continuous
zero-transaction coverage. Forty-nine resource samples span 14,407 seconds
from the original Rust PID 89930; peak RSS is 269,808 KiB, thread and descriptor
maxima are 162 and 93, and head/log/QMDB WAL remain monotonic while the auditor
correctly records the earlier 1,944-KiB allocated-storage compaction. Twenty-five
Gov5-upstream samples span 14,422 seconds and all match `d12257c...`. Rust has
274 `5+5` leader commits, CommitQC with seven validators, and zero equivocations.
The milestone SHA-256 is
`e5c64c8987a930b9b1a610322d554bdf45a323d760f0845388378da09a495585`.
The 6-, 12-, and 18-hour waiters, strict finalizer, restart/rejoin checks, and
latest-stable-Reth rollover remain armed; this checkpoint releases no
transaction and does not claim final acceptance.

An independent immutable-log audit at the same boundary scans canonical
heights 92,624 through 94,262. All 1,639 blocks are parent-continuous; all 274
expected Rust slots are exact at all six endpoints and match 274 `5+5` commit
records with exact view stride and hash order. All 276 timeout/pacemaker pairs
recover at the next view with zero pending. The 2,252 warnings partition
exactly into allowed classes, with zero unknown warnings or critical signals.
The frozen Rust log, leader, timeout, and runtime-log SHA-256 values are
`a185811f...8e55`, `53270ea6...2ebe`, `59c90704...4076`, and
`e52303d2...100a`.

A second read-only archive/QMDB checkpoint after the four-hour gate also
passes. At live height 94,303, two Gov5 account proofs and Rust QMDB proofs
have identical roots and bytes and both verify offline. Eleven historical
heights from genesis through 5,189 again pass 209 exact RPC and proof checks.
All six pending nonces remain `0x11`; no transaction or process restart was
used. The evidence SHA-256 is
`1060c76b310359b3655a43d0d9c517933290a91eacb3b91cbf5c39ba74785974`.
Two wrapper-only diagnostics are retained under `excluded/`: one misspelled
the frozen verifier environment name and failed before output, while the other
mistakenly expected 11 total records instead of the harness's one live proof
record plus 11 historical records. The correctly bound evidence was validated
in place and was neither appended nor regenerated.

The simultaneous four-hour chain-identity canary again verifies chain ID
`0x477`, genesis hash `b71c2810...1392ec`, genesis state root
`91a450c1...9941`, and empty genesis receipts root `56e81f17...b421` at all
six endpoints. Their live block identity is also exact, both sender nonce
queries remain `0x11`, and client versions remain Gov5 5.7.906 and official
Reth 2.4.1. Rust has 285 unique `5+5` commits, seven-validator CommitQC, and
zero equivocations. The identity evidence SHA-256 is
`3e554ff12f4efcc56b501df7640bb01d6e197e9d9423cf69b62f22f26e3142fb`.

The four-hour 905-data boundary recheck also passes. The original copy remains
bound to identical source and target manifest SHA-256 `1c115b92...37b4b`
(124 files / 17,316,415,839 bytes). All 24 epoch schedule, network config/key,
and BLS-keystore files across the six retained Gov data directories still
match their initial hashes. Genesis, consensus/bootstrap artifacts, validator
and P2P keys, frozen harness/finalizer/independent/QMDB tools, and both client
binaries retain their pinned hashes. Live chaindata remains deliberately
excluded because block production must mutate it. No mutation was performed;
evidence SHA-256 is
`4322ede81bd6d5102cad96e94e35ede59d899bafa458178b8dd7347768c47381`.

The four-hour dependency-delivery recheck passes as well. The primary branch,
Gov5 candidate, mixed-client combination, Reth delivery, and dependency-update
branches are each tracked-clean and exactly equal to their pushed remote
heads. Gov5 remains candidate `d0999e7...` over upstream main `d12257c...`;
the official latest stable Reth release remains v2.4.1 at tag object
`8eb21017...`, and the Gov5/Rust binary hashes remain pinned. Machine evidence
SHA-256 is
`5b7eb21ebc003aafb71ff3b11b105fae4d10aab790047c0d1326cdfef8db6cbe`.

The additional five-hour composite milestone passes without relaxing any
acceptance rule. Its 595 head samples span 18,031 seconds and grow 2,034 blocks
from height 92,623 to 94,657, with a 31-second maximum gap, maximum lag two,
and continuous zero-transaction coverage. Sixty-one same-PID resource samples
span 18,009 seconds; peak RSS is 275,616 KiB, thread and descriptor maxima are
162 and 93, and head/log/QMDB WAL remain monotonic while retaining the observed
1,944-KiB compaction. Thirty-one Gov5-upstream samples span 18,029 seconds and
all equal `d12257c...`. The milestone records 342 Rust `5+5` commits,
seven-validator CommitQC, zero equivocations, zero transactions, and no failure
evidence. Its SHA-256 is
`cffb11780ddee8aca95cefdbe2234ede2309e477bdc09523328f118b154b3d68`.

The five-hour independent immutable-log audit scans heights 92,624 through
94,664. All 2,041 blocks are parent-continuous; all 341 expected Rust slots are
exact at all six endpoints and match 341 `5+5` records with exact view stride
and hash order. All 343 timeout/pacemaker pairs recover at the next view with
zero pending. The 2,794 warnings partition exactly into allowed classes, with
zero unknown warnings or critical signals. The frozen Rust log, leader,
timeout, and runtime-log SHA-256 values are `7390709d...3bec`,
`dfa0365f...eeea`, `6ac00a2a...da71`, and `a9c2593d...031c`.

The formal six-hour composite milestone passes without relaxed acceptance.
Its 715 head samples span 21,668 seconds and grow 2,412 blocks from height
92,623 to 95,035, with a 31-second maximum gap, maximum lag two, and continuous
zero-transaction coverage. Seventy-three same-PID resource samples span
21,610 seconds; peak RSS is 275,616 KiB, thread and descriptor maxima are 162
and 93, and head/log/QMDB WAL remain monotonic while recording the 1,944-KiB
compaction. Thirty-seven exact Gov5-main samples span 21,634 seconds. The
milestone records 405 Rust `5+5` commits, seven-validator CommitQC, zero
equivocations, zero transactions, and no failure evidence. Its SHA-256 is
`c906d490bff8e62eeb741191cc4d4e9e1b44b9e0609651e56af9e15d18d9ef74`.
The 12- and 18-hour waiters and complete guarded closure remain armed.

The simultaneous six-hour chain-identity canary again verifies chain ID
`0x477`, the complete pinned genesis hash/state/receipts tuple, all-six live
block identity, and sender nonce `0x11`. Client versions remain Gov5 5.7.906
and Reth 2.4.1; Rust has 406 unique `5+5` commits, seven-validator CommitQC,
and zero equivocations. Its SHA-256 is
`2db923d8521e310b4cd55af0a7be36a4d56a3a0ff941e5a5a20a7c349a5fd15a`.

The six-hour read-only archive/QMDB checkpoint also passes. At live height
95,047, two Gov5 proofs and Rust QMDB proofs have identical roots and bytes and
verify offline. Eleven historical heights again pass 209 exact RPC/proof
checks; all six pending nonces remain `0x11`. No transaction or restart was
used. Evidence SHA-256 is
`1e4c44543cb8561096d5fcc6f84ac6e33252f2c1116e0d627bd332b2d6849dcc`.

The six-hour independent immutable-log audit scans heights 92,624 through
95,048. All 2,425 blocks are parent-continuous; all 405 expected Rust slots are
exact at all six endpoints and match 405 `5+5` records with exact view stride
and hash order. All 407 timeout/pacemaker pairs recover at the next view with
zero pending. The 3,309 warnings partition exactly into allowed classes, with
zero unknown warnings or critical signals. The frozen Rust log, leader,
timeout, and runtime-log SHA-256 values are `bfee67d8...2327`,
`f2606ff2...17dc`, `8a089dd9...36c7`, and `0ea18d51...f889`. A read-only
nine-hour composite waiter is additionally armed to narrow the 6→12 hour gap.

The additional seven-hour composite milestone also passes without relaxed
acceptance. Its 833 head samples span 25,245 seconds and grow 2,784 blocks,
with a 31-second maximum gap, maximum lag two, and continuous zero-transaction
coverage. Eighty-five samples from the same Rust PID 89930 span 25,212 seconds;
peak RSS is 275,760 KiB, thread and descriptor maxima are 162 and 93, and the
head/log/QMDB-WAL counters remain monotonic while retaining the 1,944-KiB
compaction observation. All 43 upstream samples over 25,239 seconds equal
`d12257c...`. The milestone records 467 Rust `5+5` commits, seven-validator
CommitQC, zero equivocations, zero transactions, and no failure evidence. Its
SHA-256 is
`167b2c53ef9819cbec0ee2dd5abf4e6532da964406b57e46501564b911829756`.

The seven-hour frozen-log incremental audit then scans the post-six-hour Rust
slots from height 95,054 through 95,407. All 59 expected Rust canonical blocks
are exact at all six endpoints and match 59/59 `5+5` records with continuous
parents, exact view stride, and exact hash order. All 467 cumulative
timeout/pacemaker pairs recover at the next view with zero pending. The 3,798
warnings partition exactly into allowed classes, with zero unexpected warnings
or critical signals. The frozen Rust log, leader, timeout, and runtime-log
SHA-256 values are `366baf19...edf4`, `8086be8c...b9e`,
`ce96ce70...efd`, and `66cd4e49...88f8`. No transaction or node restart is
permitted before the 24-hour boundary.

The additional eight-hour composite milestone passes without relaxed
acceptance. Its 953 head samples span 28,884 seconds and grow 3,192 blocks,
with a 31-second maximum gap, maximum lag two, and continuous zero-transaction
coverage. Ninety-seven samples from the same Rust PID 89930 span 28,814
seconds; peak RSS is 276,064 KiB, thread and descriptor maxima are 162 and 93,
and head/log/QMDB-WAL counters remain monotonic while retaining the 1,944-KiB
compaction observation. All 49 upstream samples over 28,844 seconds equal
`d12257c...`. The milestone records 535 Rust `5+5` commits, seven-validator
CommitQC, zero equivocations, zero transactions, and no failure evidence. Its
SHA-256 is
`ba9bb4ed1f2800cea120da2e03def11fdd96a0f9d698adb687fc7a6651b51c0e`.

The eight-hour frozen-log incremental audit scans the post-seven-hour Rust
slots from height 95,408 through 95,815. All 68 expected Rust canonical blocks
are exact at all six endpoints and match 68/68 `5+5` records with continuous
parents, exact view stride, and exact hash order. All 535 cumulative
timeout/pacemaker pairs recover at the next view with zero pending. The 4,346
warnings partition exactly into allowed classes, with zero unexpected warnings
or critical signals. The frozen Rust log, leader, timeout, and runtime-log
SHA-256 values are `d81f611a...4df2`, `72a2e549...bb9d`,
`aa5cf464...a6de`, and `c961ced4...31d1`.

Two runtime27 canary dry runs produced no mutation: the controller wait loop
did not yet continue across slots in which Rust was not the leader. Their
zero-byte outputs were moved under `excluded/`; the corrected canary and all
four preflights were rerun from fresh output paths before formal timing.
At 10:02Z a static controller audit found that the still-gated strict
independent waiter had been launched with the detached latest-Reth build
worktree instead of the branch-backed dependency-delivery Reth worktree. The
waiter, immutable gate, controller guardian, and total verifier were replaced
before any final summary or transaction release. Nodes and evidence monitors
were not restarted, the formal stream remained continuous, all six nonces
remained `0x11`, and a machine-readable correction record preserves both PID
sets. The corrected waiter is again held by the immutable gate and targets
`chore/reth-upstream-20260726 @ 91725e3aa...` with exact upstream equality.
Both independent-verifier waiters now also require their temporary verifier
output to be non-empty before parsing and atomically publishing it, giving an
immediate local failure if a future verifier exits without producing evidence.

## Current 2026-08-02 baseline — GOV5 5.7.906

The current-main Gov5 candidate is pushed as
`integration/gov5-interop-current-main-20260801 @ 8915b4cc0`. Its pinned
upstream cutoff is `origin/main @ 920f7536eb263b6744b48f28dfeb77f4c2798c1a`,
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
`c533db8` (Reth 2.4.1). Qualification tooling and evidence fixes through
`9347828` are pushed on the same branch without changing that measured runtime
binary.

The broader compatible dependency refresh is independently delivered and
pushed at `chore/deps-latest-20260721 @ aec34a0cd465e8fdbb598b90bc778fe96e25d6c0`;
its manifest and devlog pin the newer paired Reth 2.4.1 revision
`91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b` on
`chore/reth-upstream-20260726`. That delivery branch is not
misrepresented as an ancestor of the measured interop binary: this runtime
uses the interop branch's own locked dependency graph against `c533db8`, plus
Rust 1.97.1. The independent delivery locks all 37 newly available
semver-compatible maintenance updates and the two dependency edges added by the
post-release Reth fixes; a subsequent dry-run locks zero packages. Its locked
all-target workspace check, complete workspace tests, and warnings-denied
all-target Clippy gate pass against `91725e3aa`. Reth's own 24 integration
tests, package tests, full-workspace all-target check, and warnings-denied
all-target Clippy also pass. The Reth default build now keeps revmc/LLVM 22 JIT
opt-in, its dev-node tests use bounded 60-second readiness with unique ports,
and the obsolete `alloy-node-bindings` dev edge has been removed. No dependency
or execution binary in the active qualification runtime was changed after the
strict window began.

The measured interop code has now also been validated directly against that
newer Reth revision, rather than relying only on the separate dependency
delivery branch. Branch `feat/gov5-n42-live-interop-reth-latest` commit
`50ad7ed` combines the current interop head with Reth
`91725e3aa8f2a0bbc5a425e931a2f2b2f31b2a7b`. The only lockfile changes are the
`parking_lot` and `libc` dependency edges required by the newer Reth crates.
The combination passes locked full-workspace/all-target check and test,
warnings-denied all-target Clippy, and nightly formatting. The all-target test
exposed and fixed a stale JMT benchmark fixture that described absent accounts
as modifications; synthetic accounts applied to a fresh tree are now correctly
marked as creations. Its optimized `n42-node` reports Reth 2.4.1 commit
`91725e3aa` and has SHA-256
`0a4dbcf30d7cc9944a7cd7c96a25c1ebf862df10bde76210a381ef492e362b9f`.
This additional build did not replace or otherwise disturb the frozen strict
runtime.

Commits
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
`51e68918560be65f8e5221f02a3d544a7baf42bed9aa86655623449a4fd765d0` and
Rust `d917782b906176119172e656005218be34ec3d5ad1b7241c0c53f8f6d593da2d`.

Independent 5.7.905, initial 5.7.906, and pinned-current 5.7.906 `init` runs
against the qualification genesis all regenerated block zero as
`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`, matching
the Rust H2 configuration and the preserved Gov5 chain. The independently
queried pinned-current block-zero state root and receipts root also match the
runtime chain. A six-endpoint boundary audit additionally compared blocks 0,
85,290, 85,291, 85,380, 85,381, 85,386, and 85,387. Every hash and root was
exact, and the 85,290→85,291 upgrade edge, 85,380→85,381 copied-data edge, and
85,386→85,387 runtime-local leader edge all retained exact parent continuity.
An independent live recheck at `2026-08-02T19:05:53Z` also hashes the three
5.7.905/5.7.906 genesis artifacts byte-for-byte identically as
`561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687`.
All six live endpoints return genesis hash
`b71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`,
state root `91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941`,
and the expected empty transaction and receipt roots; their current heads are
also exact. The immutable recheck evidence SHA-256 is
`ace0a62bfcbc2e33b100a839728f8d9e3a0eb7b6046bd22b5c8a65b15bd0e00e`.
The first long-test runtime copied the verified chain data while excluding old
MDBX locks, PID files, LOCK files, and IPC sockets; it was
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
The intentionally absent seventh validator also exercises timeout recovery on
every rotation. An early long-run audit paired all 49 observed timed-out views
with a successful Rust commit at exactly `view + 1`; none remained in flight,
and the live status retained a seven-validator committed QC.

The first pinned 5.7.906 strict zero-transaction attempt started from an exact
common head at `2026-08-02T14:23:42Z` in
`runtime-18-gov5-906-latest-reth`. It later failed closed after an operator
diagnostic suspended the Rust task and is excluded from acceptance as described
below. Its independent transaction preflight confirmed nonce 17 on all six
endpoints and sent zero transactions. A separate read-only rehearsal used the
17 previously finalized transactions at blocks `0x92ea..0x9322` and passed 258
exact six-endpoint comparisons covering full and hash-only blocks, receipts,
logs, every transaction object and receipt, balances, nonce, code, and storage.
This directly confirms that the old Rust-only `blockTimestamp` response-shape
failure remains fixed in the current binaries without contaminating the new
zero-transaction window. The pinned 5.7.906 binaries also passed a fresh
read-only archive rehearsal: 209 exact Gov/Rust RPC comparisons across 11
historical heights, two byte-exact current-head QMDB reference proofs, and all
current plus historical proofs verified offline against their expected roots.
A controller audit also found and fixed a release race: the finalizer could
previously release the burst at the 86,400-second
acceptance threshold while the deliberately 86,640-second zero-transaction
monitor was still running. Commit `dc65f36` now waits for that monitor to close
and re-audits its complete immutable stream before sending anything. The
replacement evidence recorded `transactionsSent:0`; that controller correction
did not itself restart any node or monitor. Live latest-906 participation and
leader handoff are therefore proved, while the 24-hour gate remains IN PROGRESS
and is not declared PASS until a complete unpolluted interval, the
17-transaction burst, post-burst archive parity, restart/rejoin, and final
leader audits complete.
Commit `96de5cb` additionally makes the continuously sampled Gov5 upstream
identity a hard acceptance gate: `origin/main` must remain exactly
`f3dbeba4694590e6478780ac8a14e900f7dd7505` for at least 86,400 seconds, every
ten-minute snapshot must be reachable and exact, and a fresh remote lookup
must still match before the burst and final summary. The restart stage now
waits for exact six-endpoint canonical identity before beginning its ten-minute
stability interval and records that rejoin delay. Replacing only the
then-waiting finalizer did not restart any node, resource sampler, upstream
sampler, or formal monitor. The now-excluded attempt's first upstream milestone
passed with seven reachable, exact snapshots over
3,605 seconds, a maximum 601-second gap, and a fresh remote lookup still equal
to `f3dbeba4694590e6478780ac8a14e900f7dd7505`.
Commit `9ac3ce1` promotes the missing-validator recovery check into the reusable
`audit-timeout-recovery` qualification command and the final PASS controller.
Its first live invocation observed 65 timeout events: all 64 whose successor
view was already committed had an exact `view + 1` Rust leader commit with
`votes=5+5`; the single current timeout was correctly classified as in flight,
the pacemaker and timeout sets were identical, and every timeout view was seven
views after the previous one. Empty logs and unavailable consensus status are
rejected rather than treated as vacuous success.
The first attempt produced a one-hour in-flight milestone at
`2026-08-02T15:24:23Z`:
120 formal samples spanned 3,621 seconds, grew from block 85,410 to 85,818,
had a maximum 31-second sample gap, maximum endpoint lag one, zero failures,
and contiguous zero-transaction verification. A same-head leader audit scanned
432 canonical blocks and found all 72 Rust slots with continuous parents,
byte-identical hashes on all six endpoints, `votes=5+5`, and exact seven-view
stride. The paired timeout audit proved recovery for all 71 completed timeout
events; one current timeout remained correctly in flight. Thirteen resource
samples held file descriptors at 93 and threads at 161--162 while RSS remained
between 217,680 and 251,552 KiB. This milestone is retained as diagnostic
history only: the later failure disqualifies the complete stream, so none of
its elapsed time counts toward the replacement window.
The final controller is stricter than this in-flight milestone: commit
`e4f40f2` waits for a Rust-authored recovery head and requires zero pending
timeouts before it can emit the qualification summary. A live closed-point
rehearsal passed with 73 of 73 timeout events recovered and none pending.
Commit `e505a32` also makes runtime-log classification fail closed. At its
one-hour rehearsal all 659 Rust warnings partitioned exactly into 81 view
timeouts, 81 matching pacemaker transitions, 81 compact-output evictions for
81 Rust leader commits, seven duplicate-vote suppressions, and 409 duplicate
commit-vote suppressions. No unknown warning remained, and all five Gov logs
plus the Rust log contained zero error, panic, fatal, or equivocation signals.
The final qualification summary now embeds and hash-binds this audit.
Commit `ebcb736` closes the remaining resource-evidence gap. The reusable
resource auditor requires a continuous single-PID stream, at least 86,400
seconds between evidence endpoints, no gap above 360 seconds, monotonic head,
Reth data, consensus data, QMDB WAL, and log counters, plus explicit 1 GiB RSS,
256-thread, and 256-file-descriptor ceilings. Its one-hour rehearsal passed 15
samples over 4,202 seconds with 474 blocks of growth, RSS 217,680--258,032 KiB,
161--162 threads, and exactly 93 descriptors. The finalizer waits for the
resource monitor to close before auditing and hash-binding the immutable file.
Live catch-up telemetry also remained at `buffered=1` for every observed
release and recorded zero bounded-buffer overflow errors, excluding the
configured 131,072-block emergency capacity as an accumulating live map.

At `2026-08-02T15:41:51Z`, the first formal monitor appended a fail-closed
`rpc unavailable` row for Rust port 29545; the finalizer stopped at
`15:41:53Z` and released no transaction. The Rust log had stopped at a view
timeout while the process still existed and accepted TCP without servicing
RPC. The timing and a subsequently captured parked-thread sample identify the
operator's concurrent `vmmap -summary` inspection as a macOS task suspension,
not an internal panic or a canonical-chain disagreement. Once the diagnostic
released the task, the same Rust PID resumed, reconnected, and returned to
exact six-endpoint identity without a restart by `15:45:45Z`. Fail-closed
semantics still require a fresh full window. The 23 incident files are
preserved under
`excluded/diagnostic-task-suspension-20260802T154151Z/`; SHA-256 values are
`0388a8a1a141eee0aa8000dc8e29ddd2aa4992de135cf2465a3204f7feb64848`
for the excluded formal stream,
`dfb7a6b3c5923c5c55d67c4954e2f89c1718e3e181068fce0b9fd590fce6a7de`
for its failure stream,
`03388971ee7a15b4cc8508542301b3b9aea7c3fc9b5b6dcd25a2355585e7d474`
for the Rust log, and
`9ed8290e30f00feabcbea8b759975af7e8229b5351bb46719ece0242f3ce31d8`
for the task sample. Commit `72ba377` now also records the exact failed RPC
port and phase in every fail-closed monitor row.

The clean persisted-state restart exposed two expected startup warning classes
that the earlier runtime-log gate had never encountered: one FCU `SYNCING`
payload retry and five Gov-peer responses indicating that the peer does not
advertise Rust state sync before authenticated block catch-up takes over.
Commit `18eff35` classifies those exact messages, bounds them to two payload
retries and ten peer fallbacks across the initial start plus final controlled
restart, and no longer mistakes a structured `error=` field for an ERROR-level
log. All other warning text and every ERROR, panic, fatal, or equivocation
signal still fail closed. The live audit then passed all 130 warnings with the
complete partition, zero unknown warnings, and zero critical signals.
The 30-minute audit subsequently encountered a valid Rust commit line carrying
the tracing context `process_event{...}:` between its INFO level and message.
Commit `c66b504` accepts that optional context in both commit-count and timeout
recovery parsing while leaving the exact commit fields, `votes=5+5`, view
sequence, hash sequence, and warning partition checks unchanged. The repaired
live audit matched all 44 leader commits and the recovery audit closed all 43
timeout events with zero pending.

Rust was then restarted from the same persisted Reth, consensus, QMDB, and
genesis data with the pinned binary. It authored and finalized block 85,948
(`0x1867bf2f5b8ab91b527595a9e8e1c2c1017d226abba739d40308505970753126`)
with all five Gov nodes contributing `votes=5+5`. A clean 180-second startup
preflight passed 35 six-endpoint samples with maximum lag zero, progressed from
block 85,953 to 85,971, and sent no transaction; its SHA-256 is
`f2c4dc0fe46a66a401d0f053a5cf3af29e8e111e4d887e834d5cb7f7a50d6153`.
The replacement authoritative 86,640-second zero-transaction stream and the
independent 87,000-second resource and Gov5-upstream streams all started from
empty files at `2026-08-02T15:55:37Z`. Their first sample was exact at block
85,977 with lag zero; the upstream sample remained exactly
`f3dbeba4694590e6478780ac8a14e900f7dd7505`, and the resource sample recorded
212,624 KiB RSS, 161 threads, and 93 file descriptors for Rust PID 30367. A
fresh finalizer preflight again confirmed nonce 17 on all six endpoints and
`transactionsSent:0`. No elapsed time from the excluded attempt is credited.

The replacement window's first immutable milestone is preserved under
`evidence/milestones/replacement-one-hour-20260802T1700Z/`. Its 130 formal
samples span 3,925 seconds, grow from block 85,977 to 86,415, have a maximum
31-second sample gap and maximum lag two, and verify every covered block as
zero-transaction. The canonical audit scans 468 blocks and matches all 78
expected Rust slots on all six endpoints with continuous parents, exact hashes,
`votes=5+5`, and seven-view log stride. The immutable timeout snapshot closes
all 77 timeout events below its committed view and correctly retains one
current event in flight; a live Rust-head closure immediately before the copy
closed 77 of 77 with none pending. All 641 snapshot warnings partition exactly,
with zero unknown or critical signals. Fourteen resource samples span 3,902
seconds under one PID, hold RSS at 212,624--254,368 KiB, threads at 161--162,
and descriptors at 93. Seven reachable Gov5-upstream snapshots span 3,605
seconds and remain exact. The formal snapshot SHA-256 is
`cdbb96bdeaeea97766a9d3ea74432ada5884d04b76e712abafa168f18c700608`;
the SHA-256 of its complete manifest is
`0928d0907dac3171b7cf27d1d65aff91cfbc525209066e0b18c4545ec5c30b13`.
This is a milestone only; the live 24-hour streams continue from their original
first samples and no transaction has been released.

The next immutable milestone is
`evidence/milestones/replacement-two-hour-20260802T1757Z/`. Its 242 formal
samples span 7,333 seconds, grow from block 85,977 to 86,805, retain a maximum
31-second gap and maximum lag two, and remain transaction-free. The canonical
audit scans 858 blocks and matches all 143 expected Rust slots across all six
RPC endpoints with continuous parents, exact hashes, `votes=5+5`, and exact
seven-view stride. The snapshot closes all 142 timeouts below its committed
view and correctly records one current timeout in flight; the immediately
preceding live Rust-head audit closed 142 of 142 with none pending. All 1,163
warnings partition exactly, with zero unknown or critical signals. Twenty-five
resource samples span 7,204 seconds under Rust PID 30367, with RSS no higher
than 261,952 KiB, at most 162 threads, and 93 descriptors. Thirteen Gov5
upstream samples span 7,210 seconds and remain exact. The formal snapshot
SHA-256 is
`945f6db9db99ec7621bec3cea46e2def1665d248b2bd66a2f2b05c7be23f5527`;
the complete manifest SHA-256 is
`f05a19fcd64169f97042eeb67e6be72d96397c450b6bec64ec40db3ff1ed7b8f`.
This remains an in-progress milestone and releases no transaction.

The three-hour immutable milestone is preserved under
`evidence/milestones/replacement-three-hour-20260802T1859Z/`. Its 366 formal
samples span 11,104 seconds, grow from block 85,977 to 87,231, retain a
maximum 31-second sample gap and maximum lag two, and remain transaction-free.
The canonical audit scans 1,284 blocks and matches all 214 expected Rust slots
across all six RPC endpoints with continuous parents, exact hashes,
`votes=5+5`, and exact seven-view stride. The Rust-head snapshot closes all
214 timeout events with none pending. All 1,740 warnings partition exactly,
with zero unknown or critical signals. Thirty-eight resource samples span
11,106 seconds under Rust PID 30367, with RSS no higher than 270,976 KiB, at
most 162 threads, and 93 descriptors; all storage and log counters remain
monotonic. Nineteen Gov5 upstream samples span 10,815 seconds and remain exact
at `f3dbeba4694590e6478780ac8a14e900f7dd7505`. The formal snapshot SHA-256 is
`b3ae220377502010359efb70819da3dcc516758bd2f7b96979e9c3dbe62273c1`;
the complete manifest SHA-256 is
`4c45d9b93551bfb74e24e4ce86d54642d67e8afd3177d4dc48f98f6a691071b7`.
This remains an in-progress milestone and releases no transaction.

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
- n42-26: `7a96b97` (`docs: record historical transaction parity rehearsal`)
- n42-26: `dc65f36` (`fix: close zero-transaction monitor before burst`)
- n42-26: `96de5cb` (`test: pin Gov5 upstream through final qualification`)
- n42-26: `9ac3ce1` (`test: hard-gate timeout recovery in final qualification`)
- n42-26: `e4f40f2` (`test: close final audit at Rust recovery point`)
- n42-26: `e505a32` (`test: hard-gate runtime warning classification`)
- n42-26: `ebcb736` (`test: audit 24-hour Rust resource stability`)
- n42-26: `72ba377` (`test: identify failing soak endpoint`)
- n42-26: `18eff35` (`test: classify bounded startup fallback warnings`)
- n42-26: `c66b504` (`test: accept traced Rust commit log context`)
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
  `2026-08-02T15:55:37Z`; the prior diagnostic-suspended stream is excluded
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
| P4 fault and lifecycle matrix | IN PROGRESS | all failed, superseded, incomplete, or operator-contaminated windows remain preserved and excluded; 5.7.905 catch-up, leader handoff, archive parity, and a 4,633-second soak pass, but its strict window was superseded; Gov5 5.7.906 candidate `8915b4cc0` is fully tested and reproducibly built; runtime-18's authoritative replacement started from zero at `2026-08-02T20:37:47Z`, and its first complete hour passes exact six-endpoint heads/roots, zero-transaction coverage, Rust leader cadence, timeout recovery, resource bounds, log classification, archive parity, and current-main identity; the full 24-hour stream, burst, and restart/rejoin remain pending |
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

## 2026-08-02 Gov5 5.7.906 current-main reselection

Gov5 `origin/main` advanced to
`920f7536eb263b6744b48f28dfeb77f4c2798c1a`. The preceding mixed-client
window was therefore excluded rather than credited toward qualification. Its
13,847 seconds, 456 samples, 1,566 blocks of growth, maximum lag two, and zero
transactions are preserved under
`excluded/gov5-906-superseded-upstream-20260802T194556Z/`; the exclusion
manifest SHA-256 is
`cb5fb95b25b35e06f4536cc5629dfe037ca3b839aac3affa01635fc28a4a644f`.

The new integration candidate is pushed at
`integration/gov5-interop-current-main-20260801 @
8915b4cc07d82dc195daee2e8e741ea5e8446068`. It includes current main and a
build fix that pins the Go build ID to the source commit and replaces
libmdbx's compile-time timestamp with `reproducible`. Two independent builds,
each preceded by `go clean -cache`, now produce the same executable SHA-256:
`51e68918560be65f8e5221f02a3d544a7baf42bed9aa86655623449a4fd765d0`.
`go test ./...`, the targeted P2P/txspool race suite, and
`go test -race ./...` all pass. The executable identifies itself as
5.7.906 at candidate commit `8915b4cc`.

The data migration check found a consensus-critical distinction. Starting an
empty 5.7.906 directory with only `--chain private` creates built-in genesis
`0x75ca525a980dad7c9faf1b8ceea38e6bb4276ca6b65a7ffac2a9858a7c1c8a32`
with state root
`0x471b9d2c852cdffce5dfb636b9e77d90c6e0a5af129b44db8db70bd4cf615570`.
That is not this interop chain. Explicitly running `n42 init --profile n42
--chain private --data.dir <dir> artifacts/genesis.json` produces the required
genesis
`0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`,
state root
`0x91a450c13f9deab2c9edf5832c96008862e7cc1169599f68461c3ec947099941`,
and empty transaction/receipt root
`0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421`.
The pinned genesis artifact SHA-256 is
`561808693c76b356e51f8f5961304e68f3167943c17145bda056612041dca687`.
`--p2p.genesis-override` changes handshake identity only and cannot repair an
incorrect local database. Consequently, the qualification launcher now
refuses empty, partial, or wrong-artifact Gov directories: each validator must
use an explicitly initialized database or a validated copy of the 5.7.905
data, with its original validator and network keys retained.

All five retained 5.7.905/5.7.906 validator databases remained on the required
`b71c...` lineage. Before replacement, APFS snapshots were captured under
`snapshots/pre-gov5-8915b4cc0/`. The validators were then stopped, replaced,
and restarted one at a time without deleting or regenerating data. The final
post-rollout recovery stream has SHA-256
`56680c89a56121d84cd9d099c21d1bd6ba2e84ccdb4fc6158c437e2592deda94`:
58 samples over 301 seconds, 29 blocks of growth, maximum lag zero, zero
transactions, and exact block hash, state root, and receipt root across five
Gov endpoints and the Rust/Reth endpoint. This recovery check is a preflight,
not a substitute for the fresh strict 24-hour window.

The fresh strict window started at `2026-08-02T20:37:47Z`. Its formal
zero-transaction monitor runs for 86,640 seconds at 30-second intervals; the
single-PID Rust resource monitor and current-main monitor each run for 87,000
seconds. The first formal sample was exact at height 87,860 with lag zero.
The resource stream is bound to Rust PID 97040, and the upstream stream is
fail-closed on any value other than `920f7536...`. The signed transaction
preflight found nonce `0x11` on all six endpoints and sent zero transactions.
The launch manifest SHA-256 is
`aed63bf9a0933fdd9565f6701b42db84172060228cc7e60e5c5ce78be6ea5780`.
No earlier elapsed time is credited. If all three immutable streams pass, the
armed finalizer will release the 17-transaction burst, run a ten-minute
post-burst parity window and archive proof checks, restart Rust, require exact
rejoin, and run a final ten-minute stability window before issuing PASS.

The two fail-closed branches were exercised independently after launch. A
deliberately wrong expected `origin/main` exited with status one after writing
one mismatch sample and did not create a completion marker. An otherwise valid
runtime with no initialized MDBX also exited with status one before creating a
Gov PID. The retained regression summary is `PASS` with SHA-256
`fd4116349fd582f30b3f419b1f19666dd11728b5eea5a69337d47839eaa464db`.

An in-flight controller rehearsal then exposed a log-boundary defect before it
could invalidate the completed window. The strict launch intentionally rotated
the Rust log, whose first canonical Rust commit is block 87,843, but the
finalizer still defaulted its final leader audit to historical block 85,387.
The chain scan therefore had more history than the immutable log could prove.
The finalizer now derives the start height from the first committed hash in the
strict Rust log, resolves that hash through RPC, requires it to be canonical and
Rust-authored, and rejects any configured override that differs. The repaired
audit scanned blocks 87,843 through 88,179: all 57 expected Rust slots had exact
six-height cadence, continuous parents, identical hashes on all six endpoints,
`votes=5+5`, exact seven-view stride, and matching log order. Concurrently, all
58 completed missing-validator timeouts recovered at the next view, and all 472
Rust warnings partitioned into known bounded classes with zero critical signal.
This controller-only correction did not restart a node or monitor and released
no transaction. The replacement manifest is `PASS` with SHA-256
`87804ce5a583903eefdbcbc70f9b3bc1a3b84749fdd6240165ca0fea65232932`.

The replacement window's first complete one-hour milestone is also `PASS`.
Its 120 formal samples span 3,631 seconds, progress from block 87,860 to
88,268, have a maximum 32-second gap and maximum endpoint lag two, and retain
contiguous zero-transaction coverage. Thirteen resource samples bind one Rust
PID for 3,603 seconds: RSS is at most 257,392 KiB, threads at most 162, file
descriptors exactly 93, and all storage/log counters are monotonic. Seven Gov5
upstream samples span 3,605 seconds and remain reachable and exact at
`920f7536...`. The canonical scan covers blocks 87,843 through 88,263 and finds
all 71 Rust slots with six-height cadence, continuous parents, exact six-RPC
hashes, `votes=5+5`, and seven-view stride. All 72 completed missing-validator
timeouts recover at the next view, while 584 warnings partition exactly with
zero critical signal. The leader, timeout, and log milestone SHA-256 values are
`1e4dc93b0b512fc6041ba07ec260c8dcf186f8a28298207f281166dae1ea4faa`,
`91d1d4c2155d6a40dc3389e1bf1d9634f9fdc8afe2060366361735504d2b2481`, and
`fd741aecd494defd941c5eada37eafd52edde7542681826a05cff01724abdf16`.
A fresh read-only archive rehearsal at the same milestone also passes: one
current-head Gov/Rust QMDB reference proof is byte-exact and offline-verified,
and 11 historical heights from genesis through block 5,189 have exact RPC,
root, and offline proof parity. Its 12-record evidence SHA-256 is
`8308a1487ff492976a1387b818f6a63b1cbecc5f8a5261e842723f0da7957f7b`.

A second genesis check inside the strict window independently re-hashes the
5.7.905, initial-5.7.906, and current-5.7.906 artifacts. All three remain
byte-exact at `56180869...`. Every Gov and Rust RPC returns block-zero hash
`b71c2810...`, state root `91a450c1...`, and empty transaction/receipt root
`56e81f17...`; all six also return one exact live head. This proves the local
databases, not merely `--p2p.genesis-override`, remain on the required lineage.
No transaction had been sent. The evidence SHA-256 is
`f212a205701b9ffd8d3745904f288979294c379606e39500b02a41df27682dcb`.

The companion data-boundary recheck performs 54 independent RPC reads across
all six endpoints at block 0 and eight upgrade/runtime boundary heights. The
85,290→85,291 905/906 edge, 85,380→85,381 copied-data edge,
85,386→85,387 runtime Rust-leader edge, and 87,842→87,843 strict-log edge all
have exact parent continuity. Every sampled hash/root/miner is identical on all
endpoints, all four successor blocks are Rust-authored as expected, and all
sampled blocks remain empty. The evidence SHA-256 is
`9720ebb62b897cedd17a222fb88b041d8089c13f313374fec99a9d3a2083f3e2`.

The strict finalizer was reloaded once more without touching any node or
long-running monitor so its final PASS summary binds the post-burst,
post-restart, and archive-parity evidence paths and SHA-256 values directly.
All six sender nonces remained `0x11`, no transaction was sent, and the strict
window continued on the original node and monitor PIDs. The replacement
evidence SHA-256 is
`9b7d9bde9899f3388f3c598f9ef72f3899dbd04727e2ccbb5c72f6f88725d800`.

A subsequent executable review found and fixed the deferred restart QC check's
missing jq field prefix before the check could gate the 24-hour result. The
same change now verifies zero equivocations both before and after restart,
measures rejoin from before shutdown rather than after RPC recovery, and
records RPC recovery separately. Only the finalizer was reloaded; node and
monitor PIDs, the zero-transaction window, and the sender nonce were unchanged.
The replacement evidence SHA-256 is
`4145d5e1d1ab95b9895fdcddc5956b8bbc3f14b8c79deb291f0f49f5828acbba`.

Before release, all 17 pre-signed burst transactions were also decoded
offline with Cast 1.5.1. Their signer, chain ID 1143, contiguous nonces 17--33,
hashes, gas fields, deployment bytecode, transfer targets, and values are
exact. The nonce-17 contract derivation matches the pinned expected address,
and the sender balance is identical on all six endpoints. This validation sent
no transaction. Its evidence SHA-256 is
`73e8ce72a6a7b743cbfd7c46e0850f2aa30f03a92bf47e36d79f0680498b6e23`.

The controlled Rust restart no longer depends on mutable validator material in
an older runtime. The validator BLS key and P2P key were copied with mode 0600
into the strict runtime, verified byte-for-byte against the keys used by the
running node, and pinned by SHA-256 in both the restart path and final summary.
Only the finalizer was reloaded; the node and long-running monitor PIDs were
unchanged and no transaction was sent. The replacement evidence SHA-256 is
`4b1fcc18ef25d6a88de88ae599af06f7b42bc76a11d80a83c2a8da9bff78a1ef`.

The restart closure also pins the local genesis artifact, peer-bound consensus
configuration, and bootstrap bundle by SHA-256. The finalizer validates all
three when it starts, repeats the validation immediately before restarting
Rust, and binds the values into the final PASS summary. Reloading this guard
did not change any node or long-running monitor PID and sent no transaction.
The replacement evidence SHA-256 is
`9539a714aff3f7ad2249a0fc7a7411f7779d362c700aefc9fa133325dd1541c1`.

The qualification harness and offline QMDB proof verifier used after the
24-hour wait were frozen into the strict runtime as well. Their pinned copies
successfully reran the in-flight soak audit and the complete Gov/Rust archive
RPC plus offline-proof parity test. The finalizer now refuses any different
tooling bytes and records both SHA-256 values in its final summary. No node or
long-running monitor PID changed and no transaction was sent. The replacement
evidence SHA-256 is
`958d641ca632f5837e056ad673bb586632cf1b4413000376b60a4526cd205aa8`.

A read-only execution preflight confirms that the deployment and transfer
calls succeed on all six endpoints and that every estimate fits within its
signed gas limit. Transfer estimates are exactly 21,000 on Gov and Rust. The
contract-creation estimate is `0x12799` on Gov and `0x12b0c` on Rust, reflecting
client-local estimation heuristics rather than an execution or consensus
difference; both are below the signed `0x186a0` limit, and all `eth_call`
results are the same. No state changed and no transaction was sent. The
evidence SHA-256 is
`77eb639a5412bc39263bb71a52c2aab2ae3aeb27ed8bc5e1e1578d875276d389`.

Finally, the finalizer itself was frozen into the strict runtime and launched
with its expected SHA-256 supplied out of band. A negative preflight proved
that a different SHA is rejected before any action; the exact-SHA preflight
then passed all identity, source, genesis, key, tooling, upstream, and nonce
checks with zero transactions sent. This finalizer is PID 52570; all node and
long-running monitor PIDs remain the strict-launch processes. The launch
evidence SHA-256 is
`a1b999f37f3660ae9fd569c6dd7af2599b1e67de21667ce58d7f99e8060212a6`.

An independent final verifier now checks the finalizer rather than trusting
its PASS at face value. Its preflight independently verified the live six-node
chain and genesis, CommitQC and zero equivocations, every frozen input, all
four source/upstream relationships, official Reth stable `v2.4.1`, and nonce
`0x11` with no transaction sent. After the final artifacts appear, the same
verifier will recompute every evidence SHA and re-evaluate the 24-hour,
transaction, restart, leader, timeout, log, resource, and live-chain gates.
The preflight evidence SHA-256 is
`d1ea4a04a6f6c2a7cc3f3cb6bae3a2e9376fa7213167e78471a6ebe266137b75`.

The independent verifier was subsequently hardened to compare every embedded
audit object with its source file and to re-execute the raw soak, resource,
leader, timeout, log, archive RPC, and offline QMDB proof checks. A frozen,
SHA-bound copy passed both negative and positive identity preflights. A second
fail-closed waiter, PID 57182, now watches frozen finalizer PID 52570 and will
publish the independent result atomically only after the final summary exists.
No node or long-running monitor PID changed and no transaction was sent. The
waiter launch evidence SHA-256 is
`b8848b6603077748e47578b316d85f3ea953933431af21900f438a7b4147464f`.

A fresh requirement-level recheck also recomputed the preserved P4 fault
matrix and selected evidence hashes. The 512-block rejoin, gossip/rotor vote
recovery, Gov/Rust TC and NewView paths, invalid payload isolation, four crash
boundaries, two epoch transitions, Byzantine/crash threshold, and bounded
backpressure recovery remain PASS without relaxed requirements. Thus the only
open P4 item is this authoritative 24-hour stream and its burst, post-burst,
and controlled restart/rejoin closure. The recheck evidence SHA-256 is
`93f8162618ba23a3070d1490d2ffb71856a219d188eabc012eb34d73fd76c616`.

The machine-readable P0, P1, P2, P3, and P5 requirement audits were likewise
reopened and checked rather than inferred from this report. Every top-level and
nested requirement remains PASS, the complete Rust/Go P0 gate record remains
PASS, and all six authoritative file hashes were recomputed. No requirement
was relaxed. The consolidated recheck evidence SHA-256 is
`d42ee31493f05e87f7b191d1e98168c370c76c5c0f135a4bd5c6055db299079e`.

The strict two-hour milestone independently passes all six active streams.
Across 240 head samples and 7,282 seconds, the chain grew by 822 blocks with
maximum lag two and continuous zero-transaction coverage. Twenty-five resource
samples retain Rust PID 97040, at most 275,536 KiB RSS, and exactly 93 file
descriptors. The canonical audit covers 846 blocks and 141 exact Rust leader
slots with `5+5` votes on all six endpoints; all 141 timeouts recovered at the
next view with none pending. All 1,147 warnings partition exactly with zero
unknown or critical signals, and 13 Gov5 upstream samples remain exact over
7,210 seconds. The combined milestone SHA-256 is
`b89f7cc0880b67f68f15ab1d7595ed903db6bfffe89c6b00dfe7745f04e7178c`.
