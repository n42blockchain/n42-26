# Devlog 108: Block-STM SELFDESTRUCT shadow hardening

Date: 2026-07-18  
Task: `docs/codex-task-sync-from-gov5-2026H1.md` P2-4 quarterly cross-client audit

Branch base: `fix/gov5-sync-parallel-validation-order` (`1e5a8ea`). The first
multi-round run on the older P0 validation base reproduced the already-fixed
hot-recipient race in round 7; this branch therefore declares the validation
ordering fix as a hard prerequisite rather than hiding the failure with a retry.

## Finding

The 2026-Q3 Erigon sweep surfaced `455449f0e5` (parallel execution must not
re-credit a beneficiary destroyed in the same transaction). Mapping that defect
family to `n42-parallel-evm` found a broader local bug:

- `execute_single_tx` emitted `AccountWrite::Updated` for every touched account
  and never inspected revm's `Account::is_selfdestructed()` flag;
- consequently `AccountWrite::Destroyed` had no producer, despite consumers in
  `MvMemory` and `merge_tx_state`;
- MVCC storage had no account-wide wipe version, so a transaction after a
  destruction could read an older changed slot or the base-state slot;
- deferred-beneficiary blind reads were unsound if a transaction dynamically
  created or destroyed the beneficiary, because fee deltas before a wipe do not
  commute with that wipe.

The module is still an opt-in benchmark/experimental executor rather than the
reth production block executor, but this is consensus-state code and therefore
must be correct before any future wiring.

## Fix

1. Translate revm selfdestructed accounts to `AccountWrite::Destroyed` and do
   not publish their per-slot writes.
2. Add versioned account-wide storage wipes to `MvMemory`. A slot read compares
   the latest slot write with the latest preceding wipe; writes after recreation
   override the wipe, untouched slots remain zero.
3. On output merge, destruction clears accumulated slots; a later update
   explicitly clears the selfdestruct flag, preserving destroy-then-recreate.
4. If a blind-read deferred beneficiary changes nonce/code or selfdestructs,
   discard the speculative result and re-run the whole block sequentially. This
   keeps the fast path for the normal EOA beneficiary without guessing around a
   non-commutative transition.

## Regression coverage

- old slot -> SELFDESTRUCT wipe -> recreation/new slot in MVCC;
- output merge destroy -> recreate clears old slots and restores a live account;
- Cancun constructor writes storage then SELFDESTRUCTs at the deferred
  beneficiary address, forcing sequential fallback;
- pre-Cancun pre-existing SELFDESTRUCT is classified destroyed, while Cancun
  EIP-6780 preserves the same pre-existing contract.

Validation command:

```bash
cargo test -p n42-parallel-evm
cargo clippy -p n42-parallel-evm --all-targets -- -D warnings
```

Result: 14/14 tests green; the full package ran 30 consecutive rounds green
after rebasing onto the prerequisite; clippy is clean with warnings denied.
