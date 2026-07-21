# QMDB + H2v4 interop post-merge audit (2026-07-21)

## Scope

- Audited tree: `main @ f904813238f93bf715670c5b6564f94aafbec3f8`.
- Merge parents: `29aa2203f00eb714660f357070a08a5d36e6b813` and
  `1657902d67d0df4eb598459a70c3f8d60ce1efee`.
- Paired execution dependency: reth 2.4.1 integration at
  `c533db8bad6f300be93ec047ecffc717b08957f8`.
- Cross-client reference: gov5 H2v4/QMDB branch at
  `0e17be4e4d9a6436d7aa5bb5e5cfbf6b93a8566d`.

This is a post-merge source audit. It verifies that the merge preserved the
previously reviewed interop implementation and that the QMDB verifier fixes did
not introduce a conflicting source resolution. Live multi-process operation is
a separate acceptance gate.

## Result

No new HIGH, MEDIUM, or LOW runtime findings were found in the merged delta.

The merge commit referenced this report, but the file was not present in the
merged tree. Adding the report closes that traceability gap; it does not change
runtime behavior.

The independent compact-output bad-block-cache issue documented in
`docs/audit-v0.5.0-post-merge-2026-07-21.md` is not part of this merge delta and
remains isolated from the interop branch.

## Merge-integrity checks

- `f904813^1..f904813` contains the intended QMDB compatibility core, H2 wire
  codec, H2v4 envelope/domain verifier, observer wiring, fixtures, and docs.
- The source files in the merge result are identical to the already reviewed
  feature-side parent. The first-parent-only contribution is the gov5-sync
  acceptance/audit documentation and its stream-v2 regression test.
- No conflict resolution changed the observer, codec, BLS, or QMDB source.
- `git diff --check` found only pre-existing blank-line-at-EOF notices in three
  devlogs; no source whitespace error or conflict marker is present.

## Security and compatibility review

### Observer isolation

- Legacy gov5 and H2v4 topic subscriptions are created only by
  `NetworkService::new_gov5_h2_observer`.
- Normal validator/full-node construction passes no interop topics or chain
  identity, so existing networks retain their prior subscriptions and voting
  behavior.
- Received legacy messages are decoded and reported only. They never enter the
  Rust voting state machine.
- H2v4 messages can only advance the observer's proven sync target after a
  valid Decide proof; they do not produce votes or commits.

### Wire and chain binding

- Both clients use raw Snappy and the same `/n42/h2/4/ssz_snappy` topic.
- The envelope binds the `N42H2V4` magic, little-endian chain ID, genesis hash,
  validator-changes hash, and a bounded inner message.
- Rust rejects oversized envelope, QC, TC, signature, bitmap, and HighTC
  fields on both encode and decode paths. In particular, Vote/CommitVote and
  Timeout encode paths enforce the same 4096-byte HighTC limit as decode.
- Bitmap count, exact byte length, validator cap, and unused high bits are
  canonicalized before finality verification.

### Finality proof

- Decide view/hash must equal its CommitQC view/hash.
- The signer bitmap must exactly match the locally configured validator-set
  length and contain at least the authoritative `n-f` quorum.
- Signer public keys are selected by bitmap index and the aggregate signature
  is verified under gov5's BLS POP ciphersuite.
- The signed commit domain binds chain ID, genesis hash, view, block hash, and
  changes hash. The initial production profile is explicitly static-validator,
  so gov5 signs and publishes a zero changes hash and disables its reconfiguration
  RPC while H2v4 is enabled.

### QMDB portable replay verifier

- Chain ID, genesis hash, block checkpoint, claimed root, positional slot count,
  every slot record, and the final Blake3 content digest are authenticated.
- Entry count must equal `next_slot`, and slot numbers must be contiguous.
- Values are capped at 16 MiB and reused through one buffer.
- Attacker-controlled `next_slot` no longer causes an up-front twig-root
  allocation. Streaming verification retains one twig plus one root per
  completed twig and fails truncated input on demand.
- Full and partial twigs, dead frozen leaves, active bits, empty state, and a
  non-power-of-two number of twigs use the same root construction as the gov5
  reference vectors.
- `--expect-root` provides a machine-checkable equality gate against the root
  reported by gov5, rather than merely checking snapshot self-consistency.

## Commands run

```text
cargo test -p n42-network h2_
cargo test -p n42-consensus-service h2_
cargo test -p n42-twig-core qmdb_compat
go test ./internal/consensus/hotstuff ./lib/qmdb
```

Results:

- N42 network H2 tests: 6 passed, 0 failed.
- N42 H2 finality tests: 1 passed, 0 failed.
- N42 QMDB compatibility tests: 9 passed, 0 failed.
- gov5 HotStuff and QMDB packages: passed.

The full merged-tree `cargo check --all-targets`, clippy with warnings denied,
and workspace test suite were independently rerun from a clean checkout before
this audit and were all green.
