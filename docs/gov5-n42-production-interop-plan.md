# Gov5 ↔ n42-26 production interoperability plan

Date: 2026-07-23

## Target

Gov5 and n42-26 must be interchangeable implementations of one N42 custom-chain
node, in the same sense that geth and reth implement one Ethereum execution
network. Both clients must:

- follow and participate in the same H2 consensus;
- execute the same replay-v2 QMDB state transitions and agree on block hash,
  receipts root, and state root;
- recover from existing authenticated data without deleting the preserved
  seven-node, replay-history, or performance datasets;
- retain and serve enough historical data for a minimal full archive+ profile.

## Current readiness

| Gate | Status | Evidence |
|---|---|---|
| H2 wire, Snappy, BLS domains, QC/TC | Complete | Cross-language vectors and full shadow verification |
| QMDB slot log, proof binding, replay-v2 roots | Complete | Portable bootstrap and cross-client roots |
| Far catch-up and finalized-range authentication | Complete | 2,883-block live catch-up |
| Existing-network observer/follower | Complete | Live Gov5 seven-node Noise/Yamux/H2 following |
| Execution-gated Rust vote | Complete in isolated committee | Rust imported `Valid`, voted at view 149, Gov5 committed |
| Out-of-order batch catch-up | Code and unit gate complete | Non-Valid child no longer advances validated watermark |
| Automatic validator cold start | Not complete | Latest participant run required an explicitly prepared consensus snapshot |
| Rust leader handoff | Not complete | Production code exists; no full live Rust-proposal → Gov5-votes → CommitQC run yet |
| Fault/rejoin/epoch soak | Not complete | Mixed-client long-run matrix still pending |
| Minimal full archive+ parity | Not complete | Historical QMDB state/proof/RPC retention still needs end-to-end serving tests |

Operationally:

- **Read-only follow:** ready now, behind explicit observer gates.
- **Voting follower:** beta-ready only in a disposable/staged mixed committee.
- **Existing seven-node validator replacement:** not yet production-ready.
- **Rust leader and archive+ service:** not yet production-ready.

## Completion plan

### P0 — land the current safety and participation baseline

Deliverables:

1. compact-output bad-block-cache poisoning fix and regression;
2. H2 vote release only after `new_payload(Valid)`;
3. exact Noise PeerId validator routing without weakening BLS authorization;
4. authenticated native/replay QMDB checkpoint profile selection;
5. validated-only eager-import watermark for out-of-order catch-up;
6. Rust and Go full check, lint, and test gates.

Exit criterion: both interop branches are pushed and reproducible from clean
worktrees on reth 2.4.1.

### P1 — post-fix live follower and catch-up qualification

Run a new disposable `runtime-10` copied from authenticated artifacts, never
from the preserved runtime directories.

Scenarios:

1. start Rust 20+ blocks behind six Gov5 validators;
2. deliver finalized blocks in deliberately reversed/concurrent scheduling;
3. require every missing parent to reach `new_payload`;
4. require the Rust execution/QMDB head to converge to Gov5;
5. keep following for at least 1,000 committed blocks;
6. restart Rust twice from its own persisted state and reconverge.

Exit criterion: no manual state-file editing, no `Syncing` loop, exact block
hash/state root at head, and no bad-block-cache entry for honest blocks.

### P2 — automatic participant bootstrap and recovery

Replace the manual consensus snapshot step with an authenticated bootstrap
bundle containing:

- chain id and genesis hash;
- finalized checkpoint block/hash/QMDB root;
- CommitQC/locked QC and certificate view;
- validator set plus exact PeerId bindings;
- last execution-validated head;
- content digest and replay protection.

The node must verify the bundle before materialization, persist it atomically,
and refuse mixed or regressing components.

Exit criterion: a blank Rust datadir can join as a validator using only the
genesis, validator key, trusted peers, and authenticated bundle; cold restart
requires no operator edits.

### P3 — bidirectional leader handoff

Use the isolated 6 Gov5 + 1 Rust committee, then 5 Gov5 + 2 Rust.

Required sequence:

1. Gov5 leader proposes, Rust executes and votes;
2. Rust becomes leader, builds a difficulty-0 Gov5-compatible header and block;
3. all Gov5 followers import before voting;
4. Gov5 votes form the QC/CommitQC accepted by both clients;
5. leadership rotates through every validator twice;
6. roots and canonical heads remain identical.

Exit criterion: at least 28 consecutive mixed-client leader views with no
manual intervention and no client-specific fork.

### P4 — mixed-client fault and lifecycle matrix

Automate:

- Rust disconnect/rejoin while 512+ blocks behind;
- lost direct vote with gossip/rotor recovery;
- timeout, NewView, and TC formation with either client as leader;
- forged compact output and invalid payload isolation;
- process crash between QMDB commit, execution validation, vote persistence,
  and CommitQC persistence;
- validator add/remove and epoch transition;
- one Byzantine peer and up to the configured crash-fault threshold;
- 24-hour zero-transaction soak followed by a transaction burst.

Exit criterion: safety roots never diverge; liveness resumes within bounded
views after the fault clears; restart never double-votes.

### P5 — minimal full archive+ parity

Add and verify:

1. immutable finalized block/body/receipt retention;
2. historical replay-v2 QMDB checkpoints and proofs;
3. state reconstruction or retained snapshots at the advertised archive floor;
4. `eth_getBlock*`, receipts, logs, transaction lookup, and historical state
   RPC comparison between Gov5 and n42-26;
5. pruning configuration that cannot delete data below the published archive
   floor;
6. export/import and corruption-recovery drills.

Exit criterion: a fresh node bootstraps from preserved artifacts and serves the
same sampled historical RPC/proof answers as Gov5 across genesis, epoch
boundaries, recent history, and the archive floor.

### P6 — guarded introduction to the existing seven-node deployment

Deployment order:

1. attach Rust as a non-voting observer using read-only network access;
2. compare head, finality, QMDB root, receipts, and lag continuously;
3. take recoverable snapshots of the validator being replaced;
4. during a maintenance window, replace exactly one Gov5 validator with Rust;
5. require two full leader rotations and a restart/rejoin drill;
6. roll back immediately on root divergence, missed leader slot, double-vote
   evidence, or unbounded execution lag.

No existing database is reformatted, compacted, pruned, or removed as part of
this rollout.

Exit criterion: one Rust validator operates for 24 hours, including leader
slots and restart, while all seven canonical heads and roots remain identical.

## Go/no-go checklist for the existing seven nodes

Production consensus participation is allowed only when all are true:

- P1 through P4 are green on clean disposable runtimes;
- bootstrap is automatic and cryptographically bound;
- Rust leader handoff is proven;
- full workspace and Go suites are green from clean checkouts;
- operator rollback artifacts and commands have been rehearsed;
- observer comparison reports no divergence before validator replacement.

Until then, observer/follower attachment is allowed; validator replacement is
not.
