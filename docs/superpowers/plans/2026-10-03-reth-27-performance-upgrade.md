# Reth 2.7.0 Performance Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade the N42 Reth integration to the immutable Reth v2.7.0 commit and measure its seven-node performance without changing Gov5/QMDB correctness semantics.

**Architecture:** Preserve the current dirty Reth checkout, port N42's four Reth integration patches on an isolated v2.7.0 source branch, then update N42 adapters and dependency pins in small commits. Explicitly retain immediate persistence until a separate partial-persistence design proves QMDB and restart safety; benchmark only builds that pass the full seven-node audit.

**Tech Stack:** Rust 1.97.1, Cargo workspace/path dependencies, Reth 2.7.0, Gov5 H2-v4, QMDB, Bash and Python 3 benchmark tooling.

**Spec:** `docs/superpowers/specs/2026-10-02-reth-27-performance-upgrade-design.md`

## Global Constraints

- Target Reth base: `3d592ece6de8c4559987416a544fc215fd6d6921` (tag `v2.7.0`); final pin is the exact N42-adapted commit on top of this base.
- Do not reset, clean, overwrite, or silently reformat the current `../reth` checkout; it has 407 modified or untracked paths.
- Preserve Gov5/SP1/replay protocol pins and change other dependency versions only when required by Reth or an independently documented security/compatibility need.
- Keep QMDB-only reads, transaction/signature checks, receipts, roots, five-of-seven CommitQC, canonical persistence, and restart verification enabled.
- Keep immediate persistence semantics; do not enable Reth state masking in this migration.
- Use immutable source, binary, workload and configuration hashes for every performance leg.
- Retain a performance change only when A/B/A bookends are valid, sustained windows improve beyond bookend drift, and every correctness gate passes.
- Create small commits grouped by Reth source patches, N42 API compatibility, dependency pins, and benchmark evidence; pushing requires writable Git metadata.

## Review Focus

- An archive or patch command accidentally mutates the existing dirty `../reth` source or omits an untracked N42 Reth hook; capture source hashes and assert the old checkout remains byte-for-byte unchanged.
- Reth defaults silently enable partial persistence or state masking; test the effective node configuration and reject startup if the requested immediate-persistence settings are unavailable.
- A QMDB provider read falls back to Reth DB, uses the wrong block version, or returns an unverified state value; exercise exact-version reads, fail-closed behavior and zero mismatch/error counters.
- A v2.7 transaction-root or import-batch API change causes N42's encoded header, receipts or state root to diverge; compare against the serial/standard fallback for valid and invalid fixtures.
- An upgrade appears faster only because the workload, host load, receipt set, scoring window or engine settings changed; compare immutable manifests and invalidate the A/B/A group on any mismatch.

---

### Task 1: Preserve the current Reth dependency source

**Files:**
- Create: `.artifacts/reth-upgrade/current-source-manifest.json` (ignored local evidence)
- Create: `.artifacts/reth-upgrade/current-dirty.patch` (ignored local evidence)
- Create: `.artifacts/reth-upgrade/untracked-source/` (only Reth untracked source files, not build output)
- Create: `scripts/test_prepare_reth_upgrade.py`
- Create: `scripts/prepare-reth-upgrade.py`

**Interfaces:**
- Consumes: `../reth` Git HEAD, worktree diff and untracked source paths.
- Produces: an immutable manifest containing HEAD, status count and SHA-256 hashes, plus a recoverable copy of tracked and untracked source changes. It must not write under `../reth`.

- [ ] **Step 1: Write failing tests** for refusing an absent/non-Git source, recording the exact `HEAD`, capturing a modified tracked file and an untracked source file, and excluding an untracked `target` symlink/build directory.
- [ ] **Step 2: Run** `python3 -m unittest scripts/test_prepare_reth_upgrade.py -v`; confirm the preservation tool is missing.
- [ ] **Step 3: Implement** `prepare(source: Path, destination: Path) -> dict` in `scripts/prepare-reth-upgrade.py`. Save the patch and copied untracked source only under the destination; never invoke reset, clean, checkout or write commands against the source.
- [ ] **Step 4: Run** the unit tests and create the preservation snapshot at `.artifacts/reth-upgrade/`. Compare the source's before/after status and hashes; expect no source changes and all 407 entries represented in the manifest.
- [ ] **Step 5: Commit** only the reusable preservation tool and its tests; keep machine-specific snapshots ignored.

### Task 2: Port N42 Reth integration to v2.7.0

**Files:**
- Modify: `patches/reth-payload-transactions-root.patch`
- Modify: `patches/reth-qmdb-state-reader.patch`
- Modify: `patches/reth-import-batch.patch`
- Modify: `patches/reth-gov5-cached-state-root.patch`
- Modify: `patches/reth-n42-perf.patch` (required for the payload execution and broadcast cache APIs used by current N42 call sites)
- Modify: `scripts/apply-reth-patches.sh`
- Test: `scripts/test_apply_reth_patches.py`
- Modify in isolated Reth checkout: the exact Reth files identified by the four existing patches.

**Interfaces:**
- Consumes: clean Reth v2.7.0 source at the pinned SHA from a separate writable checkout; Task 1 preservation evidence.
- Produces: the complete set of N42-required patches, including any semantic integration found only in the preserved dirty diff, each with recorded context; an idempotent apply/check command fails on an unknown source base.

- [ ] **Step 1: Write failing tests** for clean v2.7 patch application, second-application idempotence, modified-context refusal, and one missing required patch.
- [ ] **Step 2: Run** `python3 -m unittest scripts/test_apply_reth_patches.py -v`; confirm the new compatibility cases fail.
- [ ] **Step 3: Audit the Task 1 diff** against N42's actual Reth call sites. Classify formatting-only edits separately; turn every required semantic edit not represented by the five patch files (including untracked N42 source) into a named patch before migration. Do not lose any behavior required by the current N42 build.
- [ ] **Step 4: In a separate writable Reth checkout**, use `git worktree add --detach <path> 3d592ece6de8c4559987416a544fc215fd6d6921`. Reapply each required N42 patch individually, updating only its API context and behavior. Preserve the existing sibling checkout unchanged.
- [ ] **Step 5: Update** `scripts/apply-reth-patches.sh` to verify the exact v2.7.0 base and final patch set before applying anything, check all required patches before changing a file, and remain idempotent.
- [ ] **Step 6: Run** the patch-tool tests, `git apply --check` for every required patch against the isolated source, and `git diff --check`; expect clean application and no changes to the old checkout.
- [ ] **Step 7: Commit** each Reth patch separately in the isolated Reth checkout, then commit the patch application/test changes in N42.

### Task 3: Adapt N42 code to the upgraded Reth APIs

**Files:**
- Modify: `Cargo.toml`
- Modify: Reth-dependent manifests under `bin/` and `crates/` (only where Cargo reports required changes)
- Modify: Reth call sites in `bin/n42-node/`, `crates/n42-node/`, `crates/n42-execution/`, and `crates/n42-consensus-service/`
- Test: `crates/n42-node/tests/qmdb_provider_hook.rs`
- Test: `crates/n42-node/tests/native_cached_qmdb_builder.rs`
- Test: `crates/n42-node/tests/native_parallel_qmdb_builder.rs`
- Test: `crates/n42-node/tests/parallel_qmdb_builder.rs`
- Test: affected execution and network unit-test modules

**Interfaces:**
- Consumes: Task 2 patched Reth v2.7 source and existing N42 adapters.
- Produces: N42 source that compiles against the exact upgraded Reth APIs while retaining the same serialized headers, QMDB read contract, import validation result and serial fallback behavior.

- [ ] **Step 1: Record the current focused test results** from the pinned Reth baseline using the tests listed above; preserve full output in `.artifacts/reth-upgrade/`.
- [ ] **Step 2: Run** `cargo check --workspace --all-targets --offline --locked` against the v2.7 source and record the first API failures without broad formatting or unrelated edits.
- [ ] **Step 3: Adapt** only the failing N42 call sites and manifests. Keep custom provider and batch APIs behind the existing N42 integration boundary; do not relax validation or add a fallback that hides QMDB errors.
- [ ] **Step 4: Run focused tests** for QMDB provider hooks, cached/parallel builder output, import-batch receipts, Gov5 header/transaction roots and execution fallback. Expect root/header/receipt equality with the serial reference and fail-closed behavior for mismatches.
- [ ] **Step 5: Run** `cargo check --workspace --all-targets --offline --locked`; expect success with the exact v2.7.0 source and all four patches.
- [ ] **Step 6: Commit** the N42 API adaptations separately from Cargo version/lockfile changes.

### Task 4: Preserve immediate persistence configuration

**Files:**
- Modify: `bin/n42-node/src/main.rs` and the node configuration builder it invokes
- Modify: `scripts/chain94-fleet.py`
- Modify: `scripts/run-native-seven-leg.sh`
- Test: native-node configuration tests and `scripts/test_chain94_fleet.py`

**Interfaces:**
- Consumes: Reth v2.7 persistence arguments and the current seven-node launch configuration.
- Produces: an explicit, recorded setting that disables state masking and partial-persistence backlogs for the initial migration; startup fails rather than silently using a different persistence mode.

- [ ] **Step 1: Write failing tests** asserting every native validator receives the explicit immediate-persistence configuration, malformed/unsupported settings fail startup, and manifests include the effective values.
- [ ] **Step 2: Run** focused Rust and Python tests; confirm they fail when the persistence fields are absent or nonzero.
- [ ] **Step 3: Implement** the smallest configuration change using the exact v2.7 CLI/node-builder API. Preserve all other node settings.
- [ ] **Step 4: Run** the focused tests and a seven-node configuration-only startup/verification smoke; expect the effective persistence mode to be immediate and node hashes/roots to agree.
- [ ] **Step 5: Commit** the persistence configuration and its tests as a standalone change.

### Task 5: Refresh required dependency pins

**Files:**
- Modify: root `Cargo.toml`
- Modify: affected crate `Cargo.toml` files
- Modify: `Cargo.lock`
- Modify: `README.md` and dependency audit/devlog entry

**Interfaces:**
- Consumes: Task 3 API-compatible source and Reth v2.7's exact workspace dependency versions.
- Produces: a locked dependency graph plus `reth-source.lock`, which records upstream base `3d592ece6de8c4559987416a544fc215fd6d6921` and the final N42-adapted Reth commit; each non-Reth dependency bump is explained by an actual compatibility requirement or a separate security finding.

- [ ] **Step 1: Write a dependency audit fixture/check** that verifies the source revision, Reth package versions and protocol-critical dependency pins against the migration manifest.
- [ ] **Step 2: Run** the audit before changes; confirm it reports the current Reth 2.4.1 dependency baseline and expected mismatches.
- [ ] **Step 3: Update** Reth path references as needed, `reth-primitives-traits`, Alloy, `alloy-evm`, `revm`, and other direct pins only to versions required by the adapted Reth source. Keep unrelated dependency refreshes separate. Add `reth-source.lock` with the upstream base SHA, adapted Reth commit SHA, and source URL; add `scripts/check-reth-source.sh` to require the local path checkout HEAD to match the adapted SHA.
- [ ] **Step 4: Run** `cargo metadata --offline --locked --format-version 1` and `cargo tree --offline --locked -d`; review duplicate Reth/Alloy/revm versions and resolve incompatible duplicates rather than accepting parallel type universes.
- [ ] **Step 5: Run** the dependency audit, `scripts/check-reth-source.sh`, and `cargo check --workspace --all-targets --offline --locked`; expect the local checkout to match the adapted revision, no unexplained direct version drift, and successful resolution.
- [ ] **Step 6: Commit** the Reth pin and lockfile independently from any justified non-Reth dependency updates.

### Task 6: Full semantic and workspace verification

**Files:**
- Modify: only code/tests found necessary by Tasks 2–5
- Create: `.artifacts/reth-upgrade/verification-manifest.json`
- Modify: `docs/devlog-177-reth-27-upgrade-20261003.md`

**Interfaces:**
- Consumes: upgraded dependency graph, patched source and immediate-persistence settings.
- Produces: recorded command outputs and source/config hashes demonstrating semantic compatibility before a fleet performance run.

- [ ] **Step 1: Run** focused provider, state-root, receipt, Gov5 header, batch-import, persistence and restart tests; expect all to pass.
- [ ] **Step 2: Run** `cargo test --offline --locked --workspace`; record all pass/fail counts and classify environment-only socket failures without counting them as passes.
- [ ] **Step 3: Run** `cargo check --workspace --all-targets --offline --locked`, formatting checks on changed files, and `git diff --check`; expect a clean verification gate.
- [ ] **Step 4: Run** `scripts/verify-native-seven-rekey-build.sh` or the current equivalent under its required shared-hardware claim; expect seven nodes to agree on CommitQC, transaction/receipt/state roots and post-restart state before scoring.
- [ ] **Step 5: Record** exact Reth SHA, patch SHAs, lockfile hash, test outputs and remaining blockers in the verification manifest and devlog.
- [ ] **Step 6: Commit** the verification record separately from code changes.

### Task 7: Seven-node Reth performance A/B/A

**Files:**
- Create: `scripts/run-native-seven-reth-27-ab.sh`
- Create: `scripts/summarize_native_seven_reth_ab.py`
- Create: `scripts/test_summarize_native_seven_reth_ab.py`
- Create: `docs/benchmarks/20261003-seven-node-reth-27-ab.md`

**Interfaces:**
- Consumes: Task 6 verified baseline and upgraded binaries, immutable workload/genesis, shared-claim controller and existing strict seven-node auditor/timeline.
- Produces: A/B/A report with A1/B/A2 successful committed TPS, first and sustained windows, full configuration and source hashes, resource/persistence metrics, and all correctness-gate outcomes.

- [x] **Step 1: Write failing summarizer tests** for A-bookend drift, changed workload/config hashes, missing nodes, failed receipts, incomplete windows, and a valid report that compares sustained windows rather than only total TPS.
- [x] **Step 2: Run** `python3 -m unittest scripts.test_summarize_native_seven_reth_ab -v`; confirm tests fail before the summarizer exists.
- [x] **Step 3: Implement** `summarize(campaign: Path) -> dict` in `scripts/summarize_native_seven_reth_ab.py`; reject mixed source/config/workload hashes and any leg without full seven-node receipt/QC/root/post-restart audit.
- [x] **Step 4: Implement** the runner with 60-second unscored warmup, immutable A1/B/A2 tags, one Reth revision as the only variable, shared-claim precondition, three quiet checks 30 seconds apart between legs, binary hash checks, restart audits, and cleanup limited to its own fleet.
- [x] **Step 5: Run** unit tests, runner refusal checks without a claim, and `bash -n scripts/run-native-seven-reth-27-ab.sh`; the current invocation exits before starting nodes when the required shared claim is absent.
- [ ] **Step 6: Under an uncontended shared-hardware claim**, run A/B/A and require all seven validators' full audits and restart checks to pass. Invalidate the group for changed hashes, host overlap, missing receipts, invalid blocks, new timeout certificates, or A-bookend drift above the campaign threshold.
- [ ] **Step 7: Run** the summarizer and write the result, including regressions or inconclusive outcomes, to the benchmark report. Do not claim the upstream 5–10% or 8.6% improvements as N42 results.
- [ ] **Step 8: Commit** runner, summarizer/tests and evidence in separate small commits; push only when Git metadata is writable.

## Preflight decisions

- Tasks 1–5 form the source/API/configuration chain. Task 6 consumes all five and is the gate for Task 7.
- Task 2 must inspect all preserved dirty source changes, not assume that the four committed patch files describe every N42 behavior in the current Reth checkout.
- The Reth checkout currently has 407 dirty entries, the worktree is detached at `23316e3ff8adca8c3bd5085ff0565fcae019202a`, and its Git metadata is read-only in the current session. Task 1 and Task 2 require a later writable Reth Git checkout. Until then, only read-only audit and workspace-local plan changes are safe.
- The N42 Git metadata is also read-only in the current session. Implementation can proceed in workspace files, but the commit steps cannot complete until Git metadata is writable.
- The repository already contains an uncommitted broad performance/coverage change set. Each implementation task must stage only its listed files and preserve all other changes.
