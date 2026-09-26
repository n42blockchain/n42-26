# N42 × Jev roadmap status (2026-09-26)

The proposed five-phase roadmap is a staged product plan. A previous, separate
testnet M1 exists in this workspace as untracked files: a public-proposal
DecisionHub, Jev relay, TypeScript SDK and example DApp. It has not been
committed to this branch. That work is a limited prototype of Phase 5, not evidence
that the earlier gates passed. Its live TypeSafe call, deployment and end-to-end
test are still unverified; see `docs/decision/README.md`.

| Phase | Repository state | Gate status |
| --- | --- | --- |
| 1 Gateway and node/CI shadow mode | Existing Jev HTTP client is specific to on-chain public proposals. No node/CI event classifier or shadow A/B harness before this roadmap work. | Open: real labelled events, severe-miss and cost comparison needed. |
| 2 App/Agent router | Proposal-specific DApp and SDK are present only as untracked workspace files; no general intent router with `QUERY`, `SIMPLE_ACTION`, `TRANSACTION`, `COMPLEX_AI`, `UNCERTAIN` outcomes. | Blocked by Phase 1 gate. |
| 3 Wallet pre-check | No semantic pre-signing risk layer. Wallet flow remains the only authorization path. | Blocked by Phase 2 gate. |
| 4 PeerDAS / Block-STM hints | No Jev-derived asynchronous hints; none should enter validation or per-transaction critical paths. | Research only after earlier gates. |
| 5 Decision Network | Untracked `DecisionHub` and `n42-decision-relay` cover one provider and one public-proposal use case. General provider identity, nonce-bound receipts, aggregation and micro-payments are absent. | Local prototype only; not a production network. |

Phase 1 accepts sanitized node/CI event records as read-only input. A deterministic
rule decision is always recorded. Jev is evaluated in shadow mode and cannot
trigger a node, peer, chain or wallet action. Missing credentials, malformed
responses, unsupported domains and low confidence produce an explicit
`need_deep_analysis` / human-review route rather than a silent healthy result.

The Phase 1 go/no-go gate requires a labelled real-event corpus from at least
node logs, CI failures, benchmark regressions and storage/network incidents.
Compare rules-only and rules-plus-Jev on severe-event recall, false-positive
rate, domain accuracy, detection latency, System-2 escalation rate and cost per
10,000 events. No deployment or later phase is implied by unit-test success.
