# Devlog 56: Consensus Evidence Persistence & Validator Change Protocol

## Overview

This phase addresses three critical gaps in the consensus layer:
1. No on-chain/persistent proof of consensus finality per block
2. Validator set changes were local-only (split-brain risk)
3. BitVec serde was 8x bloated

## Design Decisions

### Evidence Storage: MDBX table vs header extra_data

**Chosen**: MDBX `n42_consensus_evidence` table + `parent_beacon_block_root` hash reference.

**Alternatives considered**:
- QC in `extra_data`: exceeded 32B Ethereum limit (QC is ~140-200B); required
  non-standard `MAX_EXTRA_DATA_SIZE=4096`. Abandoned — evidence stored in MDBX,
  header restored to standard 32B limit.
- Beacon chain model (separate CL block): over-engineered for N42's single-process
  architecture.

**Rationale**: Ethereum uses `parent_beacon_block_root` (32B hash) to reference CL data.
N42 repurposes this field for `Blake3(ConsensusEvidence)`, keeping the header format
fully Ethereum-compatible while storing the full evidence (QC + mobile attestation)
in a dedicated MDBX table.

### Validator Changes: Proposal-carried vs on-chain

**Chosen**: Leader includes `validator_changes` in Proposal message, BLS signature
covers `(view, block_hash, changes_hash)`. Followers sync from leader's Proposal.

**Security constraint**: Decide message cannot carry full changes (no BLS signature),
so only a `validator_changes_hash` is included for missing-change detection. Followers
that miss the Proposal emit `SyncRequired`.

**Sync recovery**: `SyncBlock` now carries `validator_changes` so syncing nodes can
reconstruct epoch transitions.

### packed_bits Serde

Replaced bitvec's default per-bool serialization with `(u16 bit_count, Vec<u8> packed_bytes)`.
Result: 500-validator signers field drops from 508B to 65B (8x reduction).
Strict `len == needed` validation on deserialize prevents OOM from crafted data.

## Implementation Details

### New Types
- `ConsensusEvidence` / `MobileEvidence` (n42-jmt/evidence_store.rs): hand-rolled
  binary codec, ~140-350 bytes per block
- `ValidatorChange` enum (n42-primitives/messages.rs): `Add` / `Remove`
- `EvidenceStore`: MDBX table with `get_root()` zero-decode fast path
- `TooManyValidatorChanges` error: DoS limit (MAX=4 per proposal)

### Protocol Changes
- `CONSENSUS_PROTOCOL_VERSION`: 1 → 2
- `SNAPSHOT_VERSION`: 1 → 2 (packed_bits format)
- Proposal signature: 40B → 72B (includes changes_hash)
- Decide: added `validator_changes_hash: B256`
- `EngineOutput::BlockCommitted`: added `validator_changes`
- `SyncBlock`: added `validator_changes`
- `CommittedBlock`: added `validator_changes`

### Hardfork Upgrade
- ChainSpec: `cancun_activated()` → `prague_activated()` (Pectra)
- Enables `requests_hash` (EIP-7685) in header

### Crash Recovery
- Evidence write changed from fire-and-forget to awaited `spawn_blocking`
- Startup reads `evidence_store.get_root(committed_block_count)` to restore
  `last_evidence_root`
- Snapshot v1 auto-discarded on load (packed_bits incompatible)

## Issues Found and Fixed

| Severity | Issue | Fix |
|----------|-------|-----|
| Critical | validator_changes not bound by BLS signature | proposal_signing_message covers changes_hash |
| Critical | Sync path missing validator_changes | SyncBlock carries changes |
| High | Sync import doesn't update consensus metadata | Full metadata update on sync |
| High | Evidence write fire-and-forget (crash = stale root) | Awaited spawn_blocking |
| High | extra_data 4096B breaks with restored 32B limit | Removed (evidence in MDBX) |
| Medium | authenticated_signer used wrong signing domain | Fixed to use proposal.validator_changes |
| Medium | Add+Remove same address bypasses validation | dedup_by_key in commit |
| Medium | packed_bits OOM from oversized raw_bytes | Strict len==needed check |

## Status

- [x] packed_bits serde helper
- [x] ConsensusEvidence MDBX table + encode/decode
- [x] parent_beacon_block_root = evidence root
- [x] Validator changes in Proposal (BLS-signed)
- [x] Decide carries changes_hash for detection
- [x] Sync carries validator_changes
- [x] Evidence wired into startup (main.rs + components.rs)
- [x] Crash recovery from MDBX
- [x] Prague hardfork upgrade
- [x] extra_data restored to 32B
- [x] Clippy -D warnings clean (lib targets)
- [ ] Wire AttestationStore for mobile evidence (TODO in consensus_loop.rs:388)
- [ ] Full `cargo check --all-targets` on macOS/Linux
