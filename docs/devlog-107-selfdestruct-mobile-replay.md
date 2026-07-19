# Devlog 107: SELFDESTRUCT mobile replay adversarial coverage

Date: 2026-07-18
Task: `docs/codex-task-sync-from-gov5-2026H1.md` P1-6
Branch: `fix/gov5-sync-p1-rework-20260719`

## Question

gov5 needed an address-only wipe sidecar because its mobile witness replayer
retained a state tree: pre-Cancun SELFDESTRUCT can delete storage slots that the
destroying block never reads, so the replayer otherwise cannot enumerate them.

n42-26 mobile V2 has a different boundary. `StreamReplayDB` is a per-block,
keyless value stream and computes a receipts root; it does not retain a
persistent QMDB/Twig state tree between blocks. An unread old slot is never materialized on the
phone, so it cannot survive there as a ghost leaf. The production chain is
Cancun-active, where EIP-6780 also preserves storage for an existing contract.

The correct acceptance test is therefore real EVM fork behavior plus identical
IDC/mobile replay, not importing gov5's stateful-replayer sidecar.

## Added real-EVM cases

All cases execute first against `ReadLogDatabase`, then replay the encoded log
with `StreamReplayDB` and compare receipts and receipts roots.

1. **Same-block CREATE2 recreate boundary**
   - Shanghai: create → write slots → SELFDESTRUCT → recreate the same address;
     the recreated child is live and the old slots are zero.
   - Cancun: the inter-transaction SELFDESTRUCT does not delete the child, so
     the second CREATE2 collides and the original slots remain.
2. **Unread-slot ghost case**
   - A pre-existing Shanghai contract has slots 1 and 2 populated, but its
     destroy path performs no SLOAD.
   - The captured log contains zero `Storage` entries; both outputs mark the
     account destroyed and expose no non-zero mobile state for those slots.
3. **Cancun same-transaction exception**
   - A pre-existing contract is destroyed under Shanghai but retained under
     Cancun.
   - A Cancun constructor that writes a slot and immediately SELFDESTRUCTs
     leaves no account, proving the same-transaction delete exception.

The tests use actual signed transactions, CREATE2 bytecode, contract execution,
fork-specific headers, bundle state, state-diff classification, and calculated
receipt tries. No mock execution result is used.

## Validation

- `cargo test -p n42-node --test stream_v2_pipeline`: 9 passed (existing 6 +
  new 3).
- `cargo test -p n42-node --test stream_v2_pipeline selfdestruct -- --nocapture`:
  3 passed; emitted logs confirm the unread-slot case contains no storage read.
- `cargo clippy -p n42-node --test stream_v2_pipeline -- -D warnings`: passed.
- `git diff --check`: passed.

## Decision

No production bug was found in the n42 mobile replay path, and no wipes sidecar
is added. A stateful mobile QMDB verifier in the future would change this
conclusion and must carry an authenticated wipe set or maintain a complete
per-account storage index.
