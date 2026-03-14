# Code Review Audit - 2026-03-14

This document records a broad workspace review focused on correctness, security boundaries, recovery behavior, and backpressure handling.

## Scope reviewed

- `crates/n42-node`
- `crates/n42-network`
- `crates/n42-consensus`
- selected execution/mobile integration surfaces
- bootstrap behavior in `bin/n42-node`

## What was fixed in this pass

### 1. StarHub no longer silently drops `PhoneConnected`

Area:

- `crates/n42-network/src/mobile/star_hub.rs`

Problem:

- the QUIC handshake could succeed and a session could be inserted, but if the hub event channel was full, `PhoneConnected` could still be dropped after retry
- result: the node-side bridge would never authorize the verifier, connected-phone counts could drift, and later RPC/mobile behavior would be inconsistent

Fix:

- switched `PhoneConnected` delivery to reliable `send().await`
- if the bridge receiver is gone, the connection is closed rather than left in a half-alive unauthorized state

### 2. StarHub receipt delivery no longer silently drops attestation receipts

Area:

- `crates/n42-network/src/mobile/star_hub.rs`

Problem:

- valid receipts were previously sent via `try_send`
- under backpressure, receipts could be silently dropped, causing false attestation misses, reward undercount, and hard-to-debug liveness regressions

Fix:

- receipt events now use `send().await`
- if event delivery fails because the receiver is gone, the connection task logs and exits instead of pretending the receipt was accepted

### 3. NetworkService no longer drops high-priority consensus/block events immediately when queues are full

Area:

- `crates/n42-network/src/service.rs`

Problem:

- `emit_event()` routed `ConsensusMessage` and `BlockAnnouncement` to the high-priority channel but still used `try_send`
- if the channel filled, the event was dropped immediately
- for consensus traffic this is much worse than ordinary data loss; it can directly degrade liveness

Fix:

- retained best-effort behavior for low-priority events
- for `ConsensusMessage` and `BlockAnnouncement`, `Full(event)` now moves the event into an internal ordered backlog that the main loop drains reliably instead of dropping or spawning unordered retry tasks

### 4. Removed stale authorized-verifier snapshot APIs

Area:

- `crates/n42-node/src/consensus_state.rs`

Problem:

- runtime authorization is now intentionally session-scoped, but `restore_authorized_verifiers()` and `snapshot_authorized_verifiers()` still existed
- that left a misleading re-entry point for future regressions

Fix:

- removed the stale helpers so authorization cannot accidentally be re-persisted through that API

### 5. Consensus output backpressure no longer escalates all non-commit outputs into immediate fatal errors

Area:

- `crates/n42-consensus/src/protocol/state_machine.rs`

Problem:

- `ConsensusEngine::emit()` previously treated any non-`BlockCommitted` `try_send` failure as `OutputChannelClosed`
- a temporarily full bounded channel was therefore promoted into an immediate consensus failure instead of a short recoverable stall

Fix:

- distinguish `Full` from `Closed`
- retry all output kinds a few times on bounded-channel backpressure
- keep the strongest logging for `BlockCommitted`
- added a regression test that saturates a size-1 output channel and verifies a `ViewChanged` output is delivered after the receiver drains

### 6. Mobile bridge no longer drops key notifications on bounded-channel backpressure

Area:

- `crates/n42-node/src/mobile_bridge.rs`
- `crates/n42-node/src/orchestrator/mod.rs`
- `crates/n42-node/src/orchestrator/consensus_loop.rs`
- `crates/n42-node/src/orchestrator/execution_bridge.rs`

Problem:

- `phone_connected_tx`, `attestation_tx`, and `mobile_packet_tx` were still using lossy `try_send` paths in important runtime edges
- under load, the node could silently lose:
  - new-phone cache-sync triggers
  - threshold-crossing attestation notifications
  - mobile verification task dispatches for committed/imported blocks

Fix:

- `phone_connected_tx` now uses reliable `send().await`
- production `attestation_tx` delivery now waits on `Full` in the bridge event loop instead of spawning detached send tasks
- orchestrator mobile packet dispatch now treats `Full` as backpressure and waits for capacity instead of dropping the verification task

### 7. Transaction propagation no longer silently drops local or forwarded transactions on bounded-channel saturation

Area:

- `crates/n42-node/src/tx_bridge.rs`
- `crates/n42-node/src/orchestrator/mod.rs`

Problem:

- the local tx propagation path still had multiple lossy `try_send` edges:
  - local pool pending txs could fail to reach the orchestrator
  - network-received txs could fail to reach `tx_import`
  - validator-forwarded tx batches could fail to reach the leader's local pool
- under load this degraded from backpressure into silent transaction loss

Fix:

- local `TxPoolBridge -> orchestrator` broadcast now waits for capacity on `Full`
- orchestrator `tx_import` delivery now uses a shared helper that waits on `Full` and only treats `Closed` as terminal
- local-leader and tx-forward-disabled re-ingest paths now use the same reliable enqueue logic

## Key findings by module

### `n42-node`

Strengths:

- good separation between deterministic consensus engine and runtime orchestration
- snapshot validation now rejects invalid state
- mobile authorization path is significantly tighter than before
- startup now avoids forcing HTTP RPC on, and several initialization paths fail with surfaced errors instead of panic-based aborts

Risks still worth tracking:

- operators still need to opt in deliberately if they want HTTP RPC exposed; local/dev defaults no longer force it on
- broad bootstrap path means misconfiguration tends to surface late
- some non-critical `try_send` sites still prefer dropping over backpressure

### `n42-network`

Strengths:

- clear split between validator networking and mobile QUIC ingress
- recent fixes improved lifecycle reliability around connect/disconnect
- failed `PhoneConnected` event delivery now cleans up the just-registered session instead of leaking connection slots

Risks still worth tracking:

- StarHub inbound fanout into individual phone session queues remains intentionally lossy for lagging devices, but queue overflow now feeds session-tier degradation and overflow metrics
- high-volume mobile traffic can still back up shared channels and should be load-tested again after behavior changes

### `n42-consensus`

Strengths:

- engine/output boundary is clean
- `BlockCommitted` already gets special handling in the output channel path

Risks still worth tracking:

- queue sizing and consumer latency still matter, even though bounded backpressure is now retried
- `OutputChannelClosed` still remains fatal, which is correct, but should keep dedicated tests around the consumer side

### execution / mobile / proof side paths

Strengths:

- mobile path no longer relies on snapshot-restored verifier authorization
- proof/JMT side paths are clearly off critical path

Risks still worth tracking:

- background-task lag versus head progression
- packet/receipt throughput under prolonged saturation

## Suggested next review targets

1. Revisit default RPC exposure and CORS defaults for production posture.
2. Stress-test StarHub and bridge together after reliable receipt delivery changes.
3. Audit remaining production `try_send` sites and classify them as:
   - must be reliable
   - okay to drop
   - should become bounded backpressure with metrics
4. Decide whether StarHub outbound command fanout should remain best-effort or move to an ordered async API.
