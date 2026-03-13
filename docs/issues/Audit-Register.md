# Audit and Maturity Register

This document tracks notable design and security findings discovered during code audit and maturity review.

Status meanings:

- `Open`: not fixed in the reviewed codebase
- `Fixed in working tree`: fixed locally but not yet treated as released baseline
- `Mitigated`: partially addressed, still needs verification
- `Accepted behavior`: deliberate product choice, not treated as a code defect

## Security findings register

| ID | Area | Finding | Status | Notes |
|---|---|---|---|---|
| SEC-001 | mobile ingress | handshake pubkey was not bound to receipt pubkey, allowing a single connection to impersonate many verifiers | Fixed in working tree | enforced in `star_hub.rs` by checking receipt pubkey against handshake pubkey |
| SEC-002 | mobile bridge | receipts could create arbitrary tracked blocks and trigger reward/attestation flow for unknown blocks | Fixed in working tree | receipt path now requires pre-registered/tracked block |
| SEC-003 | RPC attestation | RPC accepted arbitrary self-proving BLS identities without external authorization | Mitigated | authorization now depends on runtime verifier registration; verify full lifecycle on release branch |
| SEC-004 | snapshot recovery | invalid snapshots were once allowed into recovered consensus state | Accepted behavior / mitigated | current policy is to reject invalid snapshot load and start fresh rather than recover it |
| SEC-005 | authorization lifecycle | verifier authorization persisted across restart, weakening live-session semantics | Fixed in working tree | track until verified by tests |
| SEC-006 | disconnect path | dropped disconnect events could leave verifier authorized after disconnect | Fixed in working tree | disconnect event path changed to reliable send |

## Maturity findings

| ID | Area | Finding | Status | Notes |
|---|---|---|---|---|
| MAT-001 | bootstrap density | `bin/n42-node` startup path is feature-dense and difficult to reason about casually | Open | deserves a dedicated bootstrap/runbook doc and more smoke tests |
| MAT-002 | cross-module invariants | mobile authorization/security spans multiple files and crates | Open | likely needs explicit invariants tests |
| MAT-003 | release validation | full release-gate checklist is implied across devlogs/tests but not centralized | Open | recommend creating a release checklist doc and CI gate |
| MAT-004 | operator docs | operator-facing runbooks are thinner than engineering devlogs | Open | startup, recovery, and incident docs should be added |

## Recommended follow-up work

1. Add targeted regression tests around verifier authorization connect/disconnect semantics.
2. Add a release checklist covering snapshot handling, mobile receipt path, RPC exposure, and reward safety.
3. Add operator runbooks for fresh start, snapshot corruption, and mobile bridge degraded mode.
4. Periodically reconcile this register with changes in the lower-case `docs/` devlogs.
