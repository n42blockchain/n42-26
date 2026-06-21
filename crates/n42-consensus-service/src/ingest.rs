//! Thin hook into the node-side ingest virtual-block-credit subsystem. The
//! orchestrator arms a virtual block credit after a follower accepts a payload,
//! but the credit's shared state + consume side live in `n42-node`'s ingest
//! server (entangled with pool config). To avoid pulling that node code into the
//! consensus crate, the node registers its real arming fn at startup via
//! [`set_block_credit_hook`]; the driver calls [`note_virtual_block_credit`],
//! which dispatches through the hook (no-op until registered — matching the
//! credit-disabled path in tests).

use std::sync::OnceLock;

static BLOCK_CREDIT_HOOK: OnceLock<fn(usize, &'static str)> = OnceLock::new();

/// Registered once at node startup with the node's real credit-arming fn.
pub fn set_block_credit_hook(hook: fn(usize, &'static str)) {
    let _ = BLOCK_CREDIT_HOOK.set(hook);
}

/// Arm a virtual block credit (fire-and-forget). Dispatches to the node hook if
/// registered; otherwise a no-op.
pub(crate) fn note_virtual_block_credit(tx_count: usize, source: &'static str) {
    if let Some(hook) = BLOCK_CREDIT_HOOK.get() {
        hook(tx_count, source);
    }
}
