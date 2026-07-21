# Devlog 117 — H2-v4 opt-in production profile

Date: 2026-07-21

## Outcome

gov5 can now run a new static-validator custom chain with chain-bound H2-v4
signatures while n42-26 follows the same network as a non-voting execution and
finality observer. The profile is opt-in and leaves every existing legacy
chain byte-for-byte unchanged.

The work builds on the portable replay-v2 QMDB bootstrap, the seven-message
legacy codec, shared H2-v4 domain/envelope fixtures, and the cross-language BLS
CommitQC fixture recorded in devlogs 112–116.

## gov5 production wiring

Set the following field in the genesis HotStuff configuration on every
validator of a new chain:

```json
{
  "hotstuff": {
    "interopV4": true
  }
}
```

At startup gov5 derives the H2-v4 chain identity from the unsigned 64-bit
chain ID and canonical genesis hash. The actual production state machine then
uses the v4 preimages for Proposal, prepare Vote/QC, CommitVote/CommitQC,
Timeout/TC, NewView, embedded proof checks, view jumps, and header-QC
canonicalization. The leader's proposal signature and self-vote are kept
separate because v4 assigns them different phase domains.

Every engine-produced Decide continues over gov5's validator transport and is
also best-effort published as a chain-bound envelope on:

```text
/n42/h2/4/ssz_snappy
```

gov5's subscription filter and peer scoring recognize that exact versioned
topic, so Rust peers are not rejected at the pubsub boundary.

## Static-validator boundary

This first production profile fixes `changes_hash` to zero. The HotStuff admin
API rejects add/remove-validator proposals while H2-v4 is enabled. Operators
must not configure epoch validator-set changes for this profile. A future
protocol revision must define identical validator-change hashing and activation
semantics in both clients before reconfiguration can be enabled.

`interopV4` defaults to false. The existing seven-node/performance network,
its databases, replay snapshots, QMDB state, and archive+ history require no
migration and must not be cleaned or regenerated for this feature.

## n42-26 observer behavior

n42-26 subscribes to both legacy gov5 H2 gossip and the versioned v4 topic.
Legacy messages remain diagnostic-only. A v4 Decide becomes a proven sync
target only after all of the following hold:

- envelope chain ID and genesis hash match the local custom chain;
- Decide view/block match the embedded CommitQC;
- signer bitmap is canonical and reaches the validator quorum;
- the aggregate BLS signature verifies in the shared POP ciphersuite against
  the chain-bound v4 Commit preimage.

The observer never signs or votes. Execution history continues through the
Ethereum execution-layer peer/sync path, while replay-v2 QMDB snapshots provide
portable state bootstrap and root verification.

## Verification

- gov5 actual-engine test forms a v4 PrepareQC, CommitQC, and Decide with four
  validators; the shared v4 verifier accepts it and the legacy verifier rejects
  it.
- gov5 single-validator timeout test forms and verifies a v4 NewView.
- gov5 service test decodes its published v4 Decide envelope and pins the
  canonical topic.
- gov5 P2P filter test accepts v4 and rejects an unknown H2 version.
- Rust/Go fixed fixtures pin all domains, envelope bytes, Snappy bytes, and an
  aggregate CommitQC produced by Go and verified by Rust.
- replay-v2 QMDB cross-client verification covers both the 1,000-block sample
  and the preserved 87.8-million-slot historical dataset without modifying the
  source database.

This milestone establishes the safe custom-chain baseline: gov5 validators
produce H2-v4 finality, n42-26 verifies it read-only, and both implementations
share replay bootstrap data and chain identity without disturbing deployed
legacy data.
