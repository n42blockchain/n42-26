# Workspace Coverage Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task by task.

**Goal:** Raise measured Rust workspace line coverage to at least 70% with meaningful tests.

**Architecture:** Measure the default-feature workspace with LLVM source coverage, rank uncovered code, add behavior tests, then repeat the measurement. Keep third-party dependencies and generated code outside the report; retain all workspace production crates and binaries. LLVM source-file line totals include inline test modules; they are not pure production-only line totals.

**Tech Stack:** Rust 1.97.1, Cargo, LLVM 22, Python 3.

**Spec:** User request: “补全整个项目的覆盖率到70%”.

## Global Constraints

- Preserve existing uncommitted changes and the pinned reth/Alloy/revm versions.
- Report scope and exclusions explicitly; do not omit low-coverage production modules to meet the threshold.
- The SP1 guest, mobile native applications, contracts, and operational scripts have separate toolchains and are not Rust workspace members.
- Use writable build/cache paths under `.artifacts`; the existing `target` symlink and sibling reth checkout are outside the writable sandbox.
- Keep changes uncommitted for user review.

## Review Focus

- Missing coverage data or a failed test must not result in a successful coverage gate.
- Stale profiles must not contribute to a new measurement.
- Aggregate percentages must be weighted by executable lines, not averaged across files.
- Tests must assert results, state transitions, validation errors, or persistence behavior.
- Existing tests and pending source changes must be represented by the measured checkout.

## Task 1: Establish a reproducible baseline

- [x] Inspect workspace, existing coverage scripts, toolchains, and local changes.
- [x] Attempt `cargo test --offline --locked --workspace --no-run`.
- [x] Build against the repository's reth patches in a writable snapshot.
- [x] Execute all workspace unit/integration tests under LLVM instrumentation and export file-level coverage.
- [x] Record baseline totals and prioritize uncovered production behavior.

## Task 2: Add behavior tests guided by the baseline

- [x] Add boundary, failure, and successful-path tests for actionable coverage gaps.
- [x] Run the observer test suite and include all new ingest tests in the full workspace run.
- [x] Re-run the complete workspace suite and coverage calculation; line totals exceed 70%, while sandbox socket permissions block seven existing tests.

## Task 3: Make measurement repeatable and verify

- [x] Add a documented workspace coverage entry point with a 70% threshold and report artifacts.
- [x] Verify threshold failures, successful summary data, and missing-data handling.
- [x] Run applicable formatting/checks and review the final diff.
- [x] Record the exact measured coverage, test results, scope, and remaining blockers.
- [ ] Obtain a clean gate run in an environment permitting local TCP/UDP listeners; CI has been configured but not executed here.

## Execution record

- Initial `--locked` failure: the pending `n42-node` manifest added `rayon`, but its lockfile dependency list was missing that entry. Offline Cargo resolution adds only that dependency-list entry.
- Initial build failure: sibling reth lacks the pending import-batch API patch. A writable snapshot will apply the existing patch set, without editing sibling dependencies or lowering version pins.
- Work in the current checkout to preserve all pending changes; use an isolated build snapshot instead of creating a git worktree from the older committed source.
- Added 9 observer tests and 4 ingest tests; observer test suite: 25 passed. Added 7 gate tests; Python discovery suite: 21 passed.
- Final workspace run: 1,710 passed, 7 failed due to sandbox `Operation not permitted` on socket binding, 26 ignored. Coverage: 59,157 / 80,967 lines = 73.0631%. Gate correctly exited 2.
- LLVM export warned that 10 unmangled mobile FFI functions have mismatched profiles. A diagnostic export identified the names; the normal JSON export succeeded. Report this limitation, do not claim a warning-free measurement.
- `cargo check --workspace --all-targets --offline --locked` passed against the patched snapshot; all Rust sources were byte-compared to the original checkout. Formatting and diff whitespace checks passed.
- Independent review found no actionable correctness issues in tests, gate, workflow, and scope documentation.
