# Devlog 133 — H2-v4 mixed-client participant

Date: 2026-07-22

## Outcome

The static-validator H2-v4 profile now has a default-off, bidirectional
participant path between gov5 and n42-26. Rust can consume and produce all
seven H2 message kinds, execute gov5 block bodies, vote with the shared POP
BLS domains, form/verify QC and TC values, and publish a gov5-compatible block
when it is leader.

Gov5's service previously published `/n42/h2/4/ssz_snappy` only as a shadow
feed. The matching gov5 feature branch now also subscribes to that topic,
checks the exact chain identity and zero `changes_hash`, then submits the
canonical decoded message to the normal H2 state machine. The legacy topic
remains active during migration.

## Rust participant boundary

`N42_GOV5_H2_PARTICIPANT=1` is mutually exclusive with observer mode and is
deliberately fail-closed. It additionally requires:

- the explicit gov5 header profile and interop genesis hash;
- an authenticated replay-v2 QMDB execution base;
- a local BLS key already present in the configured validator set;
- the static validator profile (`epoch_length=0`) and no epoch schedule.

These gates keep the preserved seven-node performance network and its history
on the read-only observer path. A mixed validator deployment must use an
isolated static-validator chain or an explicitly planned validator
replacement; reusing a live validator key while its gov5 process is running
would create a duplicate signer and is not supported.

## Consensus and wire compatibility

- Shared H2-v4 signing-domain construction lives in `n42-primitives`.
- Native NUL and H2-v4 POP BLS ciphersuites stay strictly separated.
- Proposal, Vote, CommitVote, PrepareQC, Timeout, NewView, Decide, QC and TC
  use the selected signing profile throughout the state machine.
- Gov's LSB-first signer bitmap is converted exactly to Rust's `Msb0`
  representation; malformed lengths and unused high bits are rejected.
- Genesis QC has an explicit empty-wire conversion. Non-genesis aggregate
  signatures must be exactly 96 bytes.
- Nonzero validator-change commitments and proposal change lists are rejected
  by the first interoperable profile.

## Block execution and production

Gov block gossip is decoded as strict
`[header, tx_bytes, verifiers, rewards, zkproof?]` RLP. Transaction roots and
the hash-authenticated gov5 header profile are checked before conversion to an
Engine payload. An early block body is retained only in a bounded unbound
cache; it becomes executable only after the normal H2 state machine accepts
the matching Proposal or Decide. A merely decodable, unsigned envelope cannot
bind a body hash.

For a Rust leader, the standard locally built Engine payload is normalized to
the live gov5 header shape (`zero ommers`, `difficulty=0`, `N42H || view ||
96-byte seal reservation`) and rehashed before gossip, proposal and local
`new_payload`. Transactions and execution commitments are unchanged. The
profile assumes matching fork/reward semantics and a chain without gov5
committee-pool-only header commitments; those features need a later shared
builder contract.

Compact execution output is not trusted across clients. Mixed followers use
the full deterministic execution path, preserving the compact-output poison
hardening.

## Verification

Regression coverage includes:

- all seven native/H2 conversions and exact bitmap order;
- H2 POP-domain QC/TC formation and native-domain rejection;
- chain identity and zero-change enforcement;
- oversized/canonical wire rejection;
- unauthenticated Proposal cannot bind a gov5 block body;
- standard Engine payload normalization and gov5 block RLP round trip;
- participant output routes to H2-v4 only;
- gov5 accepts a valid Rust-domain H2 ingress message and rejects a nonzero
  changes hash.

Full workspace and gov5 repository gates are recorded in the merge commit
verification for this change.
