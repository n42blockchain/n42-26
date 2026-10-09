# N42 Decision Gateway Phase 1 plan

**Scope:** Implement only a read-only node/CI shadow classifier and evaluation
harness. The approved five-phase roadmap and `docs/decision/roadmap-status-20260926.md`
define the boundary. Existing on-chain proposal relay remains independent.

1. Define bounded JSONL event and structured result schemas. Reject invalid
   domain, oversized text and identifiers; redact source material before model
   calls. Unit-test malformed and adversarial inputs first.
2. Record a deterministic rules-only baseline for every event. Call a pinned
   Jev model only for eligible events; validate its complete choice distribution,
   selected label and confidence. An unavailable model must not suppress a rule
   alert. Unit-test this fallback and the read-only action boundary.
3. Emit append-only shadow records and aggregate labelled A/B metrics: severe
   recall, false-positive rate, domain accuracy, latency, model-call reduction
   and estimated cost per 10,000 events. Unit-test metrics on a synthetic corpus.
4. Add offline CLI and operator documentation. Run tests and syntax checks.
   Evaluate real labelled data and API cost before marking the Phase 1 gate passed.

**Acceptance:** The CLI has no node RPC write, shell-action, wallet or chain
transaction capability. A model response can only change a recorded shadow
classification and escalation recommendation. The gate remains open without
real-event A/B evidence.
