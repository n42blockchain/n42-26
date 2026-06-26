# devlog-93 - quorum-gate 7-node behavioral validation

Date: 2026-06-26
Base: `e65d99f Merge pull request #19 from n42blockchain/feat/mobile-evidence-quorum-gate`
Worktree: `/Users/jieliu/Documents/n42/n42-26-quorum-validation`
Artifacts: `/Users/jieliu/Documents/n42/n42-26-quorum-validation/.artifacts/quorum-gate-7node-20260626-030100`

## Scope

Validated the leader-proposal quorum gate added in PR #19 on a real local 7-validator `n42-node` network. For `n=7`, `f=2`, quorum is `5`, so the leader must observe `4` connected validator peers before triggering the proposal build.

The run used the release node binary:

```sh
cargo build --release --bin n42-node
scripts/validate-quorum-gate-7node.sh
```

Relevant test environment knobs:

- `N42_VALIDATOR_COUNT=7`
- `N42_STARTUP_DELAY_MS=500`
- `N42_BASE_TIMEOUT_MS=30000`
- `N42_MAX_TIMEOUT_MS=60000`
- `N42_FAST_PROPOSE=0`
- discovery disabled for local port isolation
- deterministic P2P and validator keys generated per scenario

## Result Summary

| Scenario | Result | First build gate | First block evidence | Time |
| --- | --- | --- | --- | --- |
| Baseline all-7 | PASS | build at 5 connected / 4 needed | block 1, view 1, signer_count 7, log votes 5+5 | 8s |
| Staggered 0,1,2 then 3,4,5,6 | PASS | early height 0; build at 5 / 4 | block 1, view 1, signer_count 7, log votes 5+5 | 32s total, 14s after late nodes |
| Leader-1 alone then rest | PASS | early height 0; build at 5 / 4 | block 1, view 1, signer_count 7, log votes 5+5 | 32s total, 16s after late nodes |
| Mid-run partition, stop 4,5,6 | PARTIAL / FAIL restore | height stayed 1 while partitioned | no second block; only block 1 CE observed | did not resume within 120s after restore |
| Single-node dev | PASS | needed 0; build immediately | block 1, view 1, signer_count 1, log votes 1+1 | 1s |

## Startup Gate Evidence

### Staggered Start

Started validators `0,1,2`, waited 15s, then started `3,4,5,6`.

Observed:

- `early_height=0` while only three validators were online.
- View-1 leader log contained `leader for view 1, waiting for validator quorum (warmup floor)` with `needed_quorum_peers=4`.
- Recheck loop stayed below quorum with connected counts `1` and `2`.
- First payload build happened only after the leader had `5` connected validator peers.
- First committed block was `view=1` with `votes=5+5`.
- CE dump for block 1: `signer_count=7`, `packed_signers=0xe6`.
- No timeout/NewView markers appeared before the first commit on the view-1 leader log.

Evidence files:

- `staggered-0-1-2-then-rest/validator-1.log`
- `staggered-0-1-2-then-rest/result.json`
- `staggered-0-1-2-then-rest/analysis.json`
- `staggered-0-1-2-then-rest/evidence-block-1.json`

### Leader Starts First

Started validator `1` alone, waited 15s, then started the rest.

Observed:

- `early_height=0` while the view-1 leader was alone.
- View-1 leader repeatedly logged `leader build timer reached; checking validator peer quorum` with `connected=0`.
- First payload build happened only after `5` connected validator peers.
- First committed block was `view=1` with `votes=5+5`.
- CE dump for block 1: `signer_count=7`, `packed_signers=0x7a`.
- No timeout/NewView markers appeared before the first commit on the view-1 leader log.

Evidence files:

- `leader-1-alone-then-rest/validator-1.log`
- `leader-1-alone-then-rest/result.json`
- `leader-1-alone-then-rest/analysis.json`
- `leader-1-alone-then-rest/evidence-block-1.json`

## Baseline Comparison

All-7 local baseline finalized block 1 in 8s. The staggered run finalized block 1 in 32s wall time, which includes the forced 15s delayed start and local peer-authentication time; after late nodes were launched it took 14s. The leader did not produce an orphan view-1 proposal during the forced delay, and the first committed block was still the view-1 proposal with a valid quorum.

This validates the startup behavior the gate was meant to harden: non-simultaneous bootstrap parks the leader instead of emitting a sub-quorum proposal.

## Mid-Run Partition

Scenario: start 7 validators, wait for block 1, stop validators `4,5,6`, wait, then restart them.

Observed:

- Before partition: height 1.
- During partition: height stayed 1.
- After restore wait window: height stayed 1.
- Block 1 CE existed with `signer_count=7`.
- Later timeout diagnostics showed `build_started=false` and no new committed block after block 1.
- No second CE was produced, so no duplicate QC/safety violation was observed.

This is not a pass for the requested restore criterion. It confirms no progress below quorum in this local run, but restore did not resume finality within the 120s wait window. That needs a follow-up focused on reconnect/catch-up or per-view liveness after node restart; it should not be counted as a fully green partition validation.

Evidence files:

- `mid-run-partition-kill-4-5-6/validator-*.log`
- `mid-run-partition-kill-4-5-6/result.json`
- `mid-run-partition-kill-4-5-6/analysis.json`
- `mid-run-partition-kill-4-5-6/evidence-block-1.json`

## Single-Node Sanity

For `n=1`, `quorum_peers_needed=0`.

Observed:

- First build triggered immediately with `connected=0`, `needed=0`.
- First commit log reported `votes=1+1`.
- CE dump for block 1: `signer_count=1`, `packed_signers=0x80`.
- Time to first block: 1s.

## Verdict

The view-1 startup gate is behaviorally validated for the two non-simultaneous bootstrap cases:

- No proposal build was observed below quorum.
- First committed block stayed at view 1.
- First block QC evidence had signer_count 7 in both 7-node delayed-start cases.
- No pre-commit timeout/NewView churn was observed in the delayed-start leader logs.

The full pass criteria are not completely green because the mid-run partition restore did not resume finality in this local harness. Treat startup gating as validated, and track partition restore as a separate liveness follow-up before claiming per-view partition recovery is fully validated.
