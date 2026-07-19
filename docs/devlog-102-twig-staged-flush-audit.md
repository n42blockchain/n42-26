# Twig staged-flush / WAL durability audit (gov5 2026H1 P1-2)

Date: 2026-07-18

## Scope

This audit covers the QMDB binary-tree/Twig sidecar's two mutation boundaries:

- consensus-service staging in `orchestrator/consensus_loop.rs`;
- `PersistentTwig` WAL, in-memory tree and snapshot ordering in
  `n42-jmt/src/persistent.rs`.

The Twig root is the default QMDB-style binary-tree commitment. It is not
byte-equal to the legacy execution-root adapter, and the target N42 tree must
not be described as MPT.

## Result

No remaining memory-before-durability window was found.

At the consensus boundary, a committed block's `StateDiff` remains staged until
reth has returned `Valid`/`Accepted`. Rejected diffs are discarded, retryable
imports retain the diff, missing diffs stop ordered sidecar progress, and the
single FIFO worker applies confirmed diffs in committed-view order.

At the Twig boundary, `PersistentTwig::apply_diff` executes this sequence:

1. append `[version, length, StateDiff]` to the WAL;
2. `sync_data` the WAL record;
3. apply the diff to the in-memory Twig tree;
4. when due, durably replace the snapshot and only then truncate the WAL.

An append or rollback error poisons the live sink. A poisoned sink refuses every
later apply and checkpoint, preventing a later block from reusing the failed
version. If in-memory apply fails after the WAL is durable, the sink likewise
stops; restart replays the durable record from the last good snapshot or fails
loudly. A checkpoint failure retains the WAL and poisons the session.

## Added acceptance coverage

The deterministic read-only-WAL fault test now also drops and reopens the Twig
sidecar after the failed append. It proves all required invariants:

- the live in-memory version and root do not advance;
- subsequent apply and flush operations fail closed;
- restart recovers exactly the last durable version and root;
- the failed staged diff cannot silently appear after recovery.

## Verification

```text
cargo test -p n42-jmt twig_apply_failure_poisons_sink_and_fails_fast
  1 passed; 0 failed

cargo test -p n42-jmt
  103 passed; 0 failed
```

The full crate run includes Twig WAL crash recovery, corrupt/truncated WAL,
checkpoint-failure recovery, snapshot/reopen and genesis-reseed protection.
