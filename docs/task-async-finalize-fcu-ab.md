# Stage 8 — async finalize-FCU off the hot path: status + A/B task

Branch: `feat/caplin-cl-stage3-6`. Part of the Caplin EL-seam refactor.

## Status: CODE ALREADY IMPLEMENTED (flag-gated)

Stage 8 ("move finalize-`forkChoiceUpdated` off the consensus select-loop hot path,
env-gated for A/B") is **already implemented** in the `ConsensusService` driver — it was an
existing feature, preserved verbatim through the stages 3-7 refactor and now lives in
`crates/n42-consensus-service/src/orchestrator/`:

- Flag: `N42_ASYNC_FINALIZE_FCU=1` (default `false`) → field `async_finalize_fcu`
  (`mod.rs:389`, env read in `async_finalize_fcu_enabled`).
- When enabled, `finalize_committed_block` (`consensus_loop.rs:614`) **`tokio::spawn`s** the
  finalize FCU off the loop instead of awaiting it inline, records
  `n42_fcu_latency_ms{attempt="first"|"first_err"}` + `n42_async_finalize_fcu_spawned_total`,
  and reports `(view, block_hash, commit_qc, finalized)` via `finalize_done_tx`.
- The completion is consumed by a `select!` arm on `finalize_done_rx` (`mod.rs:1162`) →
  `handle_finalize_done` (`consensus_loop.rs:723`), so finalization is processed
  asynchronously without blocking the next view.
- Default path (flag off) awaits the FCU inline (`consensus_loop.rs:651`) — behaviour
  unchanged for existing deployments.

So there is **no new code** for stage 8; the port refactor (the FCU now goes through
`ExecutionLayer::fork_choice_updated`) is exactly what makes spawning it clean.

## Remaining work: the A/B measurement (DEFERRED to real-machine)

The plan's stage 8 calls for an **A/B** proving the async path is a win against the
devlog-69 in-loop-FCU stall. This needs a real testnet and is deferred until datc frees the
box (per "留待真机运行"). Run when E2E is available:

1. Build release: `cargo build --release -p n42-node-bin -p e2e-test`.
2. **OFF run** (baseline): `N42_ASYNC_FINALIZE_FCU=0`, 7-node testnet (`export
   N42_INJECT_PORT=19900`, port preflight 19900..19906), a sustained-load scenario
   (e.g. the devlog-82/86 continuous-ingest workload, or E2E scenario 4 correctness
   profile). Record: commit cadence (inter-block p50/p95/max), `n42_fcu_latency_ms`
   distribution, and whether any in-loop-FCU stalls (devlog-69 symptom) appear.
3. **ON run**: identical, `N42_ASYNC_FINALIZE_FCU=1`.
4. Compare. Win condition: ON shows lower inter-block p95/max (no FCU stall blocking the
   next view) with **no** finalization-correctness regression (every committed block still
   finalizes; `finalize_done_rx` keeps up — watch for channel backlog at depth 256). If ON
   is a clear win and correctness holds, flip the default to `true`; otherwise keep it
   opt-in and record the numbers in a devlog.
5. Gate: this A/B is also part of the **pre-merge E2E** for the whole
   `feat/caplin-cl-stage3-6` branch (scenarios 1/3/4 must pass regardless of this flag).
