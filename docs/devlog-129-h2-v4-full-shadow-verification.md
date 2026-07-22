# Devlog 129 — H2-v4 full shadow verification

Date: 2026-07-22

## Outcome

The Rust Gov5 observer now cryptographically verifies every H2-v4 consensus message kind before accepting it as shadow traffic. Proposal, Vote, CommitVote, PrepareQC, Timeout, NewView, and Decide remain read-only; only a successfully verified Decide may drive the existing finalized live-import path.

This closes the cryptographic observation gate needed before mixed-client participation. It does not enable Rust proposal production or voting.

## Verification boundary

The verifier independently enforces:

- chain-bound H2-v4 POP signing domains for proposals, votes, commit votes, timeouts, and new views;
- round-robin leader selection for Proposal and NewView;
- sender membership for all individual signatures;
- wrapper/QC view and block-hash identity for PrepareQC and Decide;
- canonical Gov5 signer bitmaps, `n-f` quorum, and aggregate BLS signatures for QC and TC;
- exact canonical genesis-QC representation;
- HighQC/HighTC verification and the `TC.view + 1 == NewView.view` transition;
- validator-change hash binding wherever the Gov5 H2-v4 domain carries it.

Successful and rejected shadow messages are counted per kind. The first successful message of each kind is logged once, and the periodic observer status reports the seven verified/rejected totals without logging every consensus message.

## Tests and live evidence

Deterministic four-validator tests construct and verify all seven message kinds, including aggregate QC/TC and NewView. Negative tests reject a wrong leader and a signature replayed under another validator-change hash.

A fresh Rust observer was bootstrapped from the authenticated replay-v2 block-49/QMDB checkpoint and attached to a temporary seven-validator Gov5 cluster. Normal traffic independently verified Proposal, Vote, PrepareQC, CommitVote, and Decide. Pausing only the temporary current leader for one pacemaker interval produced and verified a real Timeout. Gov5 formed a TC and emitted NewView; the deterministic cross-language-compatible NewView path is covered by the full-message test, while the single live NewView gossip frame was missed during peer reconnection churn. No H2-v4 shadow rejection was observed.

The live run also exposed the next availability gate: at a gap larger than the 512-entry live cache, Gov5's status handshake disconnects the observer periodically, and the single-block fetch can lose its response before completing the recursive parent walk. Short-gap live following remains proven by devlog 128, but joining a far-ahead existing network requires a persistent Gov5 status-compatible session or a bounded finalized-range catch-up protocol before validator participation is considered.

All runtime work used temporary copies. Existing seven-node data, replay history, and performance datasets were not modified.

## Participation boundary

The node is suitable for continued non-voting interop observation after this change. Validator mode remains disabled until transport/session compatibility, far-gap catch-up, outbound H2-v4 production, membership/key lifecycle, and mixed-client fault/partition tests pass together.
