# N42 local-first System-1 Decision Engine roadmap

Date: 2026-09-26. This roadmap supersedes the Jev-first Phase 1–5 ordering in
`roadmap-status-20260926.md`; the existing on-chain public-proposal relay is a
separate, untracked workspace prototype. Jev is an external benchmark and
optional fallback, not the default decision provider.

The route is deterministic rules → local System-1 → Jev on uncertain cases →
System-2/human on complex or high-risk cases. Every provider is outside
consensus, transaction validity, wallet signing and the per-transaction Block-STM
critical path. Provider failure cannot block normal chain operation.

| Phase | Work | Go/no-go evidence |
| --- | --- | --- |
| 1, P0 | One versioned real-event corpus and identical Rules / GLiClass / Jev evaluation | Adjudicated eight-class labels and escalation labels; accuracy, per-class recall/FPR/FNR, UNKNOWN rate, p50/p95/p99 latency, QPS, CPU/RAM, cost per million and System-2 routing rate. AI must improve useful triage without worse severe misses. |
| 2, P0 | Independent local CPU GLiClass provider behind `DecisionProvider`; calibrated local-first routing | Compare against Phase 1 holdout. The 80% local share is a research target, not a preset safety threshold. |
| 3, P1 | SetFit few-shot experiment on N42-specific labelled data | Measurable holdout improvement over GLiClass/Jev, not training loss alone. |
| 4, P1/P2 | Multi-head ModernBERT-base experiment for fault, severity and escalation | Enough independently labelled data, calibrated holdout improvement and deployable CPU resource profile. |
| 5, P1 | App/Agent router | Task success, incorrect-route rate, latency and cost; wallet actions still require user authorization. |
| 6, P1 | Wallet semantic-risk signal | Zero bypass of nonce, balance, gas, signature, simulation or wallet approval checks. |
| 7, P2 | Optional server/PC/mobile local runtime | Per-device latency, memory, battery and fail-open core-operation measurements. |
| 8, P2 | Asynchronous PeerDAS and Block-STM hints | End-to-end throughput/availability win while protocol verification remains deterministic. |
| 9, P3 | Distributed Decision Network | Signed, nonce-bound, expiring provider receipts, replay checks, aggregation policy and settlement audit. |

Phase 1 labels are `NORMAL`, `NETWORK`, `CONSENSUS`, `EXECUTION`, `STORAGE`,
`CONFIGURATION`, `PERFORMANCE`, `UNKNOWN`, plus `need_escalation` (`YES`/`NO`).
Adjudicators must be blind to provider predictions on a holdout split. Keep
incident groups together across train/calibration/test to avoid duplicate-log
leakage; retain normal and difficult unknown cases. Do not interpret a raw
model score as correctness probability without N42-specific calibration.

The provider contract returns only a label, model/version, raw score and
escalation recommendation. A rule may force escalation; a model cannot clear
that force. Jev unavailability must produce an explicit unresolved result,
never a healthy assertion. Human-labelled ground truth remains separate from
provider output. Record exact model artifact revision, label schema revision,
input hash and benchmark host to make comparisons reproducible.

The official [GLiClass project](https://github.com/Knowledgator/GLiClass)
documents a local `GLiClassModel` / `ZeroShotClassificationPipeline` and an
optional HTTP serving path. Neither its published speed examples nor any
confidence threshold are N42 performance evidence. The Phase 1 benchmark must
measure this repository's actual CPU workload before choosing a deployment.

Current state: the previous Jev-only shadow tool remains for compatibility;
the new eight-class corpus joiner, strict same-event benchmark, rules baseline,
GLiClass/Jev provider adapters and read-only local-first gateway prototype are
in `scripts/`. No GLiClass model, calibrated N42 corpus or real Jev comparison
has run. A 1,000-line node-log smoke sample has no adjudicated labels and
cannot satisfy the Phase 1 gate. The gateway falls back on explicit `UNKNOWN`
or provider error; low-score fallback is deliberately not enabled before
N42-specific score calibration.

The first unlabelled review queue now contains 207 candidates from a preserved
seven-node leader log, a coverage CI failure list and a seven-node qualification
log. Its ignored manifest at `.artifacts/decision-system1-candidates-20260926-manifest.json`
records exact source SHA-256 hashes and sampling parameters. No candidate has
been promoted to ground truth.
