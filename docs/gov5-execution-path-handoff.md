# Gov5 execution-path handoff

This note keeps Gov5 performance numbers comparable with N42-26. Execution
workload and transaction scheduling are separate axes; "parallel" alone is not
an execution mode.

## Current paths

| Stable path | Gov5 entry | Semantics | Canonical/live use |
| --- | --- | --- | --- |
| `historical_parallel_blocks` | `cmd/witness-replay/main.go` | Independent witness-backed historical blocks distributed across workers; each block executes its transactions in order | Offline only; do not stack intra-block Block-STM on top of the worker fleet |
| `historical_sequential` | `internal/ethel/executor.go` with `ParallelEVM=false` | Parent-ordered archive replay/catch-up with batched persistence and roots | Historical canonical state construction |
| `historical_pevm` | `internal/ethel/executor_parallel.go` with `ParallelEVM=true` | Parent-ordered blocks, Block-STM within each block | Experimental historical qualification only |
| `live_sequential` | `internal/blockchain.go` → `StateProcessor.Process` | Consensus/import path, complete receipts/state/root validation | Production default |
| `live_pevm` | `internal/blockchain.go` → `StateProcessor.ProcessParallel` | Intra-block Block-STM on the consensus path | Not qualified; must remain fail-closed |

The first row is already parallel at the block level, but it is not PEVM. Its
throughput must not be reported as live or intra-block PEVM throughput.

## Required Gov5 code change

Implement this on a dedicated Gov5 branch, not on an active replay benchmark
branch:

1. Add a small dependency-free execution-path package with `Workload`
   (`Historical`, `Live`), `Strategy` (`Sequential`, `PEVM`,
   `ParallelBlocks`) and stable labels matching the table above.
2. Make the path explicit at the entry point. Do not infer it from the old
   `InsertBlock(..., isSync bool)` argument; that API is deprecated and
   `InsertChain` currently loses the workload distinction.
3. Have `cmd/witness-replay` select `historical_parallel_blocks`, and have
   `internal/ethel.Executor` select `historical_sequential` or
   `historical_pevm` from its config.
4. Have consensus proposal/import select `live_sequential`. `SetParallelEVM`
   must not silently turn every `BlockChain.insertChain` call into PEVM.
5. Reject `live_pevm` until `internal/parallel` implements storage-wipe and
   incarnation visibility for SELFDESTRUCT/CREATE2 and passes sequential
   differential qualification. A startup warning is insufficient for a
   consensus-writing path.
6. Export duration, calls, blocks, transactions, gas and fallback counters with
   the stable `path` label. Record queue/wait, EVM, root, persistence and total
   phases separately.

## Deferred qualification tests

- Historical sequential replay: restart at every persistence/checkpoint
  boundary and verify identical head/state root.
- Historical PEVM: sequential differential blocks covering
  SELFDESTRUCT/CREATE2, EIP-161 deletion, EIP-7702, pre/post-Prague receipts,
  coinbase observation and abort/re-execution cascades.
- Live sequential: proposal, follower eager import, commit and catch-up metrics
  must all retain the correct workload label.
- Live PEVM gate: every public/config entry must reject it before canonical
  state is mutated until the differential suite is promoted to a release gate.
- Cross-client dashboard: never add `historical_parallel_blocks`,
  `historical_pevm`, `historical_sequential` and `live_sequential` into one TPS
  series.

