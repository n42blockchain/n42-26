# Seven-Node Cap and Critical-Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Produce a fully audited 220k-cap seven-node baseline and a block-hash-joined critical-path report that identifies the first production-path optimization needed for 400k sustained TPS.

**Architecture:** Keep consensus and Engine validation unchanged. Expand the bounded authenticated block-by-hash recovery transport, determine the actual large-receipt RPC failure, and make the auditor able to verify every receipt at 220k without dropping checks. Analyze existing and new node logs against audited block hashes, then run a clean fleet qualification under the shared claim.

**Tech Stack:** Rust/libp2p/Reth, Python 3 standard library, Bash, H2-v4/QMDB native fleet.

**Spec:** `docs/superpowers/specs/2026-09-26-seven-node-400k-design.md`. This is the first of separate plans; data from it selects the subsequent builder/import/cadence optimization plan.

## Global Constraints

- Primary target: 400,000 successful committed TPS sustained; stretch: 600,000 TPS.
- Seven validators, five-of-seven CommitQC, native H2-v4, QMDB-only reads, full signature, receipt, state-root and canonical-persistence checks.
- 60 seconds unscored warmup; same-binary A/B/A when comparing an optimization; include first and subsequent 15/30-second windows and the 60-second total.
- Three quiet checks and both shared claims before heavy work; never stop another driver's tasks.
- Do not count old bypass-flag, Go Gov5 or n42-rs numbers as equivalent without a validation/workload mapping.

## Review Focus

- A peer declares a legal small block but sends a Snappy frame that expands past the declaration: reject without unbounded allocation (Task 2).
- A recovery block is larger than the new single-block cap: reject before materialization and leave range-response bounds unchanged (Task 2).
- An audited 220k block has a missing, duplicate, reordered or failed receipt: qualification fails (Task 3).
- An RPC publishes a block after CommitQC with a short delay: retry only the captured hash, never substitute another head (Task 3).
- A log event belongs to an empty or unaudited block, or a different node: do not put it on a measured transaction block's critical path (Task 1).

---

### Task 1: Block-hash-joined critical-path report

**Files:**
- Create: `scripts/native_seven_timeline.py` — reads one campaign leg, audited hashes, ingest times and seven node logs; writes JSON and TSV summaries.
- Create: `scripts/test_native_seven_timeline.py` — parser and join fixtures.
- Modify: `scripts/run-native-seven-leg.sh` — invoke timeline report after a successful strict audit, preserving raw logs on failure.

**Interfaces:**
- Consumes: `result-<tag>/qualification/h2-audit.json`, `ingest-start.ns`, `ingest-end.ns`, and `runtime-<tag>/logs/node[0-6].log`.
- Produces: `result-<tag>/timeline.json` with `blocks[]` keyed by audited `hash`, `successful_transactions`, per-node timestamps and stage durations; `timeline.tsv` with medians/p90, count and missing-event count per stage, plus 15-second successful-commit windows.

- [ ] **Step 1: Write failing tests** for audited-hash filtering, seven-node identity, duplicate/missing events, empty blocks, ingest-boundary windows and non-additive overlapping stages. Use short synthetic log lines with explicit timestamps and hashes; assert exact `timeline.json` fields.
- [ ] **Step 2: Run** `python3 -m unittest scripts/test_native_seven_timeline.py -v`; expect tests to fail before the module exists.
- [ ] **Step 3: Implement** `build_timeline(campaign: Path, tag: str) -> dict` and `main(argv: list[str] | None = None) -> int` in `scripts/native_seven_timeline.py`. Parse only named cadence/pack/finish/validation/import events, retain missing values rather than imputing zero, and join by audited hash and node identity. Keep elapsed wall-clock edges distinct from overlapping duration fields.
- [ ] **Step 4: Run** the unit tests, then run the parser on `.artifacts/native-seven-rekey-warm60-20260925` for `rekey-a1`, `rekey-b`, `rekey-a2`; expect 128,672/257,344/128,672 audited first-window successful commits and no unmatched transaction-bearing block hashes.
- [ ] **Step 5: Add** the post-audit invocation to `scripts/run-native-seven-leg.sh`; test syntax with `bash -n scripts/run-native-seven-leg.sh`. Commit only these files.

### Task 2: Bounded large-block recovery

**Files:**
- Modify: `crates/n42-network/src/gov5_rpc.rs` — single-block-by-hash decoded/wire caps and codec tests.
- Test: existing `gov5_rpc` unit-test module in that file.

**Interfaces:**
- Consumes: existing `Gov5BlockByHashCodec` request/response and authenticated identity checks.
- Produces: the same protocol and response type, accepting decoded RLP up to 32 MiB while still rejecting oversized declarations, frames and decompression expansion. The range-response 64 MiB chunk and aggregate caps remain separate.

- [ ] **Step 1: Add failing codec tests** that round-trip a 24 MiB incompressible-equivalent block response, reject 32 MiB + 1 bytes on encode and decode, and reject a declared-small/Snappy-expanded frame. Verify block bytes and fork digest are unchanged.
- [ ] **Step 2: Run** `cargo test -p n42-network gov5_rpc --lib -- --nocapture`; expect the large-block round-trip to fail at the old 1 MiB cap.
- [ ] **Step 3: Change** `MAX_GOV5_BLOCK_SIZE` in `gov5_rpc.rs` to `32 * 1024 * 1024` and keep `MAX_SNAPPY_FRAME_SIZE` tied to that cap. Preserve declaration-bounded decompression and all identity checks.
- [ ] **Step 4: Rerun** the focused Rust tests and `git diff --check`; commit this bounded transport change.

### Task 3: 220k receipt audit transport

**Files:**
- Modify: `scripts/h2-tps-audit.py` — explicit RPC failure classification and bounded receipt retrieval.
- Modify: `scripts/test-h2-tps-audit.py` — 220k-shaped block and receipt transport fixtures.
- Modify if the failure is server-side: `crates/n42-node/src/rpc.rs` and its local tests, or node RPC launch configuration in `scripts/chain94-fleet.py`. Do not increase a generic JSON-RPC limit before capturing the failed method, status and response size.

**Interfaces:**
- Consumes: exact committed block hash from `eth_getBlockByHash`, `eth_getBlockReceipts` or a bounded native receipt-page method if the standard response cannot be served safely.
- Produces: the existing `receipt_summary(receipts, block)` result, with the complete ordered receipt list on all seven nodes; no count-only or root-only shortcut.

- [ ] **Step 1: Add failing audit tests** for a large receipt response failure, retry of the same captured hash, missing/duplicate/reordered/failed receipts, and oversized response rejection. Assert the audit never reports a passed TPS when any receipt is unavailable.
- [ ] **Step 2: Run** `python3 scripts/test-h2-tps-audit.py`; expect the new large-response case to fail. Inspect the retained 220k failure in `.artifacts/native-seven-ab-v2-20260924/` and record the exact server/client error, method and size before selecting the narrow server or client fix.
- [ ] **Step 3: Implement** `fetch_block_receipts(url: str, block_hash: str, expected_count: int) -> list[dict]` in `scripts/h2-tps-audit.py`. Use the existing whole-block RPC when it fits; if it cannot safely fit, add a bounded native page endpoint keyed by immutable block hash and contiguous receipt indices, rejecting gaps and identity changes. Preserve `receipt_summary` for independent root/status/log verification.
- [ ] **Step 4: Run** auditor tests plus node RPC tests if changed; test with a 220k fixture and verify exact count, order and root. Commit only the receipt transport/auditor change.

### Task 4: Quiet-hardware 220k qualification and decision

**Files:**
- Create: `scripts/run-native-seven-220k-baseline.sh` — immutable-tag warmup and 60-second scored leg under the existing claim-owned leg runner.
- Create: `docs/benchmarks/20260926-seven-node-220k-critical-path.md` — configuration, hashes, windows, QC/receipt/root evidence, phase profile and next bottleneck.

**Interfaces:**
- Consumes: Tasks 1–3, existing `scripts/chain94-fleet.py`, `scripts/run-native-seven-leg.sh`, `scripts/qualify-1m-tps.sh` and Gov5 `build/perf7/box-claim.py`.
- Produces: preserved campaign artifacts and a decision on the first optimization path (builder, follower import, or cadence), with no claimed 400k result unless the complete gate passes.

- [ ] **Step 1: Write a controller check** in `scripts/run-native-seven-220k-baseline.sh`: reject absent dual Codex claim and existing artifact tags; record binary/workload SHA, 220000 cap and all execution switches. Run `bash -n` and a no-claim refusal check.
- [ ] **Step 2: Under the shared box-claim controller**, build release binaries, run focused tests, 60-second unscored warmup and one 60-second 220k scored leg. Wait naturally for other claims; do not modify their processes or claims.
- [ ] **Step 3: Require** strict seven-node auditor pass, zero unverified or failed committed transactions, matching QC and roots, post-run seven-node verification and a timeline with audited-hash match rate 100%. If any fails, retain artifacts, fix the cause and rerun under a new immutable tag.
- [ ] **Step 4: Compare** per-block critical path, first/subsequent windows, resources and previous 100k shape without treating the cross-cap difference as a causal code uplift. Choose the largest measured wall-clock limiter, write the next bounded optimization plan, and commit the report and controller.
