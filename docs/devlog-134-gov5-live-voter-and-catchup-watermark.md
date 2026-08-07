# devlog-134: Gov5 live voter and catch-up watermark

Date: 2026-07-23

## Outcome

The H2-v4 participant crossed the observer boundary on the isolated replay-v2
network:

- a fresh Rust observer executed Gov5 blocks 1 through 95 with `new_payload=Valid`;
- its execution head matched Gov5 block 95
  (`416300726f...e984`) and QMDB root
  (`91a450c13f...9941`);
- after checkpoint-start bootstrap, the Rust validator imported Gov5 block 97,
  released its vote only after execution returned `Valid`, and sent the vote to
  the Gov5 leader;
- the Gov5 committee committed that view. The persisted Rust state records
  `last_voted_view=149`.

This is evidence that the Rust implementation can follow and vote in the same
H2 network. It does not yet qualify the Rust implementation for an unattended
production leader slot: the test bootstrap copied an authenticated consensus
snapshot and advanced its execution checkpoint explicitly.

All runs used disposable `runtime-09-leader-handoff` state. Existing seven-node,
replay-history, and performance data were not modified or cleaned.

## Batch catch-up defect

The next catch-up batch exposed a local execution scheduling defect. Follower
eager-import tasks run concurrently. The old block-number guard used
`fetch_max` before calling `new_payload`. If block 101 ran first and returned
`Syncing` because block 100 was not yet known, the guard still advanced to 101.
Blocks 98 through 100 were then discarded as duplicates and could never supply
the missing ancestry.

The guard is now a validated watermark:

1. it suppresses a height only when an equal or higher height has already
   received `new_payload(Valid)`;
2. `Syncing`, `Accepted`, `Invalid`, and transport errors do not advance it;
3. leader and follower eager-import paths use the same rule.

The regression test starts at validated height 95, simulates a high child
returning `Syncing`, proves heights 98-100 remain eligible, and proves that only
explicit `Valid` outcomes advance the watermark.

## Remaining production gate

Before replacing a member of the existing seven-node deployment, run an
unattended mixed-client soak that covers:

- cold restart and authenticated consensus-state recovery without manual JSON
  editing;
- Rust leader proposal, Gov5 validation/votes, and CommitQC formation;
- long catch-up batches, disconnect/rejoin, timeout/new-view, and epoch/validator
  transition;
- archive retention and RPC comparison against the preserved replay-v2 data.

Until those gates pass, the safe deployment modes are a seventh-independent
observer or a staged follower/voter in an isolated mixed committee.

The production completion sequence and go/no-go criteria are tracked in
[`gov5-n42-production-interop-plan.md`](gov5-n42-production-interop-plan.md).
