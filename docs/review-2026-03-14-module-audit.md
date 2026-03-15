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

### 9. Startup and leader-payload handling no longer hide failures behind panic or silent decode loss

Areas:

- `bin/n42-node/src/main.rs`
- `crates/n42-node/src/orchestrator/consensus_loop.rs`
- `crates/n42-node/src/orchestrator/execution_bridge.rs`

Problems:

- deterministic libp2p key derivation still used `expect`, turning an initialization defect into a panic
- dev-mode random BLS fallback also still used `expect`
- leader payload feedback deserialized the same `BlockDataBroadcast` twice and silently ignored decode failures
- JMT / bundle extraction paths used `ok()?`, which meant malformed payload side data disappeared without enough diagnostics
- payload-build path still had a guarded `unwrap()` on `beacon_engine`

Fix:

- deterministic ed25519 derivation now returns `eyre::Result` and startup propagates a structured error instead of panicking
- random BLS key generation now exits with an explicit startup error instead of unwinding
- leader payload feedback now decodes once, logs malformed broadcasts, and reuses the decoded struct
- JMT / bundle extraction now log decode, decompress, and JSON parse failures with block hash context
- payload build now handles a missing `beacon_engine` without panic and resets its in-flight markers before returning

### 10. NetworkService no longer drops several important data-plane events immediately under queue pressure

Area:

- `crates/n42-network/src/service.rs`

Problem:

- `emit_event()` only treated consensus/block traffic as reliable
- sync requests/responses, forwarded transaction batches, verification receipts, blob sidecars, and peer lifecycle events still went through the lower-priority channel as fire-and-forget
- under consumer backpressure those events could be dropped immediately, degrading sync progress, tx forwarding, receipt propagation, and peer-state visibility

Fix:

- added a separate bounded-drain reliable backlog for important data-plane events
- `SyncRequest`, `SyncResponse`, `SyncRequestFailed`, `TxForwardReceived`, `VerificationReceipt`, `BlobSidecarReceived`, `PeerConnected`, and `PeerDisconnected` now retry through that backlog instead of being dropped on first `Full`
- ordinary mempool gossip remains best-effort

### 11. request-response ACK / rejection failures are now observable

Area:

- `crates/n42-network/src/service.rs`

Problem:

- direct-consensus, block-direct, and tx-forward handlers all used `let _ = send_response(...)`
- if the response channel was already gone, the node lost that signal silently
- this made it harder to distinguish invalid input from peer disconnect / request-response teardown races

Fix:

- all those `send_response(...)` calls now log explicit warnings on failure
- behavior remains fire-and-forget, but the failure mode is no longer silent

## Key findings by module

### `n42-node`

Strengths:

- good separation between deterministic consensus engine and runtime orchestration
- snapshot validation now rejects invalid state
- mobile authorization path is significantly tighter than before
- startup now avoids forcing HTTP RPC on, and several initialization paths fail with surfaced errors instead of panic-based aborts
- reward participant fanout no longer uses an unbounded queue; finalized-attestation rewards now obey bounded backpressure
- orchestrator internal completion signals (`BlockReady`, eager-import done, bg-import done, build-complete) no longer use unbounded queues
- leader payload feedback and JMT / proof-side bundle extraction now emit structured diagnostics instead of silently swallowing malformed data

Risks still worth tracking:

- operators still need to opt in deliberately if they want HTTP RPC exposed; local/dev defaults no longer force it on
- broad bootstrap path means misconfiguration tends to surface late
- some non-critical `try_send` sites still prefer dropping over backpressure

### `n42-network`

Strengths:

- clear split between validator networking and mobile QUIC ingress
- recent fixes improved lifecycle reliability around connect/disconnect
- failed `PhoneConnected` event delivery now cleans up the just-registered session instead of leaking connection slots
- sharded mobile event fan-in is now bounded, so a stalled bridge no longer creates unbounded cross-shard event growth
- `NetworkHandle` control-plane queues are now bounded, so external command producers no longer have an infinite buffering path into the swarm task
- important data-plane events now have a reliable backlog instead of being dropped immediately on first queue saturation
- request-response ACK / rejection failures are now logged instead of silently discarded

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
