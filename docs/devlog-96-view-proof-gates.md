# Devlog 96: View Progression Proof Gates

Date: 2026-07-18

Task: `docs/codex-task-sync-from-gov5-2026H1.md`, S2

## Outcome

The S2 audit found and closed two paths where one Byzantine validator could
move an honest node without quorum evidence:

1. `try_qc_view_jump` used the enclosing message view as part of the jump
   target. A valid historical QC plus an arbitrarily large signed message view
   could therefore move the receiver much farther than the QC proved.
2. A single future-view Timeout inside `FUTURE_VIEW_WINDOW` was immediately
   verified, adopted, and rebroadcast. That allowed one signer to pull honest
   validators forward one at a time without first forming a TC.

After this change, a QC proves only `qc.view + 1`; the enclosing message view is
used only for bounded buffering. Future timeouts are buffered until the node
reaches their view through a valid QC, CommitQC, or quorum TC/NewView.

## Audit matrix

| Surface | Before | Result |
|---|---|---|
| QC-based view jump | Target was `min(msg_view, qc.view + 10_000)` | Fixed: target is exactly `qc.view + 1` |
| Future Timeout | One valid signer could advance and trigger an honest rebroadcast | Fixed: one future Timeout is buffered and cannot advance view |
| TC collection | `HashMap<validator_index, ...>` plus duplicate rejection | Sound; malicious duplicate-delivery test added |
| TC construction | Quorum checked; individual signatures rechecked unless already verified | Hardened with exact collector/validator-set size check |
| TC consumption | NewView checks leader signature, `tc.view == new_view - 1`, quorum bitmap, aggregate signature, and embedded high QC | Sound; aggregate/view and below-quorum bitmap tests added |
| Pacemaker timer | Single-owner event loop recreates and drops `Sleep` after every selected branch | Structurally immune; view token and current-deadline checks added at the trigger point |

## C12: same-height candidates and restart divergence

The gov5 lowest-hash candidate rule is not directly applicable to n42-26's
authoritative path. N42 does not select a canonical block from a local set of
same-height execution candidates. A proposal is identified by `(view,
block_hash)`, only the expected leader may propose in that view, and a CommitQC
selects the exact hash. Reth may temporarily hold sibling payloads in its engine
tree, but `head_block_hash` advances only after the committed hash receives an
execution-valid `fork_choiceUpdated`; view changes clear protocol-local pending
proposal/vote state without making a speculative sibling canonical.

Restart persistence already restores `current_view`, `locked_qc`,
`last_committed_qc`, `last_voted_view`, validator epoch state, and the exact
execution-validated `(view, hash)` pair. NewView verifies a full TC; Decide and
state sync carry a CommitQC and select an exact hash.

The audit did find one restart deadlock edge in embedded-CommitQC catch-up. A
CommitQC for a validator-change block is signed with a non-zero
`validator_changes_hash`, while the bounded hash cache is intentionally empty
after restart. Every restarted follower could reject the first proposal that
extended its own persisted CommitQC because re-verification fell back to the
zero hash. The engine now recognizes an *exact* QC already held as its local
locked/committed certificate. That certificate was verified before it entered
the crash-safe state, so the first post-restart proposal can extend it without
depending on an ephemeral cache. All other incoming certificates still undergo
normal aggregate verification.

## Reconnect liveness follow-up

The ordered S1-S5 validation exposed a second restart edge that the original
Scenario 9 assertion did not detect. With the S1 `n-f` threshold, a
three-validator network correctly needs all three timeout votes. It therefore
halts while one validator is offline. After that validator restarted, however,
all processes were healthy but the chain remained at height 57.

The next-view leader had missed one timeout while it was disconnected. Honest
validators periodically rebroadcast the same signed timeout, but those bytes
are identical within a view, so GossipSub's message-id cache rejected the
repeat as a duplicate. The leader consequently remained one vote short of a TC
and could not leave the timed-out engine phase.

The recovery path now has two layers:

1. Locally generated Timeout and NewView broadcasts are also sent directly to
   every known validator peer. GossipSub remains the normal fanout path.
2. After fully verifying a received Timeout, a non-leader relays it to the
   next-view leader. This happens before collector duplicate rejection so a
   repeated timeout can repair a late-join gap through another healthy direct
   stream. Unverified messages are never relayed.

Scenario 9 V6 previously counted a live restarted process as success even when
zero blocks were committed after recovery. It now records the height at
recovery and requires at least one later committed block. This stricter check
first reproduced the halt as a real 6/7 failure, then passed after the relay
fix: node 2 restarted at height 57, all three validators reached height 107,
50 blocks committed after recovery, and all seven checks passed.

N42 also does not inherit gov5's 60-second base-timeout setting: the chainspec
defaults are 4 seconds for single-validator dev and 20 seconds for deterministic
multi-validator dev; testnet scripts commonly use 10 seconds. No timing change
was made in this security-focused task.

## Compatibility

No message field, serialization, or signing domain changed, so the consensus
wire protocol remains version 3. Mixed old/new nodes can still exchange and
verify QC/TC messages. A rolling deployment is format-compatible, but the old
nodes retain the single-timeout/msg-view liveness attack until upgraded;
operators should upgrade all validators promptly. A coordinated stop is not
required by this task.

## Verification

- `cargo test -p n42-consensus protocol:: --lib`: 118 passed
- Required malicious cases pass:
  - `test_message_view_cannot_jump_past_qc_successor`
  - `test_duplicate_timeout_from_one_signer_cannot_form_tc`
  - `test_single_timeout_at_window_boundary_is_buffered`
- Restart/C12 regression:
  - `test_recovered_commit_qc_with_changes_can_justify_first_proposal`
- TC proof checks:
  - `test_verify_tc_checks_quorum_bitmap_and_aggregate_signature`
  - `test_build_tc_rejects_validator_set_size_mismatch`
- `cargo check --all-targets`: passed
- `cargo clippy --all-targets -- -D warnings`: passed
- `cargo test --workspace`: passed
- `cargo test -p n42-consensus --test chaos_7node`: 12 passed
- Reconnect regressions:
  - `test_on_timeout_repeat_rebroadcasts`
  - `test_verified_repeat_timeout_is_relayed_to_next_leader`
  - `test_timeout_rebroadcast_directly_reaches_reconnected_validators`
- Strict Scenario 9 (`60s`, crash at `15s`, `10s` downtime, `500ms` block
  interval): 7/7 passed; recovery height 57, final height 107 on all nodes,
  50 post-recovery blocks, 387 transactions, and 10 sampled hashes consistent

The first workspace run exposed one stale chaos-test assumption rather than a
code failure: `test_timeout_convergence_from_different_views` expected repeated
single-signer future timeouts to pull seven nodes together. The test now first
proves that one future timeout cannot advance a lagging node, then constructs a
five-distinct-signer TC and verifies that the expected leader's signed NewView
converges all seven nodes before three normal commit rounds. The corrected test
and the subsequent full workspace run are green.
