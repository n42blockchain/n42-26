//! Hands a block this node built to the engine tree as already executed
//! (`N42_LEADER_INSERT_EXECUTED=1`), instead of validating it through
//! `newPayload`.
//!
//! The builder executed the block and derived its QMDB root; the proposal
//! carries the same transactions under the normalized gov5 header. What
//! `newPayload` would add for such a block is a second decoding of the body
//! and a second pass over the receipts and the root. Followers validate the
//! block in full before they vote, as before.
//!
//! The block is rebuilt from the builder's decoded body and the payload's
//! header fields, and must hash to the payload's block hash. Its QMDB
//! candidate is committed only if it stands on the block's parent and
//! produced the state root in that header. Any step that does not hold leaves
//! the block to `newPayload`.

use alloy_primitives::Address;
use alloy_rpc_types_engine::ExecutionData;
use n42_tx_types::{Block, N42Primitives, Receipt};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_primitives::BuiltPayloadExecutedBlock;
use reth_primitives_traits::RecoveredBlock;
use reth_trie_common::{HashedPostState, HashedStorage};
use std::sync::{Arc, Mutex, OnceLock};

type CachedExecution = (BlockExecutionOutput<Receipt>, Vec<Address>);

/// The block the payload builder resolved last.
static LAST_BUILT: Mutex<Option<Arc<RecoveredBlock<Block>>>> = Mutex::new(None);

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("N42_LEADER_INSERT_EXECUTED").is_ok_and(|v| v == "1"))
}

pub(super) fn remember_built(block: &Arc<RecoveredBlock<Block>>) {
    if enabled()
        && let Ok(mut last) = LAST_BUILT.lock()
    {
        *last = Some(Arc::clone(block));
    }
}

/// Inserts the block `payload` names if this node built it. Returns whether
/// the engine tree has it queued ahead of every later engine message.
pub(super) async fn try_insert(payload: &ExecutionData) -> bool {
    let Some(built) = LAST_BUILT.lock().ok().and_then(|mut last| last.take()) else {
        return false;
    };
    let same_block = built.header().parent_hash == payload.parent_hash()
        && built.header().number == payload.block_number()
        && built.body().transactions.len() == payload.payload.transactions().len();
    if !same_block {
        return false;
    }
    let started = std::time::Instant::now();
    let payload = payload.clone();
    let prepared = tokio::task::spawn_blocking(move || prepare(&payload, built)).await;
    let Ok(Some(block)) = prepared else {
        metrics::counter!("n42_own_block_insert_total", "outcome" => "refused").increment(1);
        return false;
    };
    let prepare_ms = started.elapsed().as_secs_f64() * 1000.0;
    let queued = reth_node_builder::submit_executed_block(Box::new(block))
        .await
        .unwrap_or(false);
    metrics::counter!("n42_own_block_insert_total", "outcome" => if queued { "queued" } else { "not_taken" })
        .increment(1);
    metrics::histogram!("n42_own_block_insert_ms", "stage" => "prepare").record(prepare_ms);
    metrics::histogram!("n42_own_block_insert_ms", "stage" => "total")
        .record(started.elapsed().as_secs_f64() * 1000.0);
    queued
}

fn prepare(
    payload: &ExecutionData,
    built: Arc<RecoveredBlock<Block>>,
) -> Option<BuiltPayloadExecutedBlock<N42Primitives>> {
    let stage = |name: &'static str, since: std::time::Instant| {
        metrics::histogram!("n42_own_block_insert_ms", "stage" => name)
            .record(since.elapsed().as_secs_f64() * 1000.0);
    };
    let store = crate::payload::builder_qmdb_store()?;
    let hash = payload.block_hash();
    let rebuild_started = std::time::Instant::now();
    let transactions_root = reth_evm::payload_cache::payload_transactions_root(&hash)
        .unwrap_or(built.header().transactions_root);
    // The header first, over the built body without its transactions: it
    // must hash to the payload's block hash before anything is committed.
    // The transactions, which another holder of the built block may make
    // this a copy of, are taken beside the commit below.
    let header = crate::engine_validator::gov5_block_from_built_body(
        payload,
        n42_tx_types::BlockBody {
            transactions: Vec::new(),
            ommers: built.body().ommers.clone(),
            withdrawals: built.body().withdrawals.clone(),
        },
        transactions_root,
    )
    .inspect_err(|error| {
        tracing::warn!(target: "n42::el", %hash, %error, "own block does not rebuild from the built body");
    })
    .ok()?
    .into_sealed_header();
    stage("rebuild", rebuild_started);
    let (parent_hash, state_root) = (header.parent_hash, header.state_root);
    let binding = store.prepared_binding(parent_hash, state_root).ok()??;
    let (output, senders) =
        reth_evm::payload_cache::take_payload_execution::<CachedExecution>(&hash)?;
    let (committed, (hashed_state, body)) = rayon::join(
        || {
            let started = std::time::Instant::now();
            let committed = store.commit_bound_candidate(parent_hash, hash, state_root, binding);
            stage("qmdb_commit", started);
            committed
        },
        || {
            rayon::join(
                || {
                    let started = std::time::Instant::now();
                    let hashed = hashed_post_state(&output.state);
                    stage("hashed_state", started);
                    hashed
                },
                || {
                    let started = std::time::Instant::now();
                    let body = Arc::unwrap_or_clone(built).into_block().body;
                    stage("body", started);
                    body
                },
            )
        },
    );
    let sealed = reth_primitives_traits::SealedBlock::from_sealed_parts(header, body);
    if !matches!(committed, Ok(Some(root)) if root == state_root) {
        // `newPayload` takes the block from here, with the execution it reuses.
        tracing::warn!(target: "n42::el", %hash, ?committed, "own block's QMDB candidate was not committed");
        reth_evm::payload_cache::store_payload_execution(hash, (output, senders));
        return None;
    }
    reth_evm::payload_cache::remove_payload_transactions_root(&hash);
    Some(BuiltPayloadExecutedBlock {
        recovered_block: Arc::new(RecoveredBlock::new_sealed(sealed, senders)),
        execution_output: Arc::new(output),
        hashed_state: Arc::new(hashed_state),
        trie_updates: Arc::default(),
    })
}

/// [`HashedPostState::from_bundle_state`] with the keccaks spread over the
/// rayon pool: a full block changes about two accounts per transfer.
fn hashed_post_state(bundle: &reth_trie_common::bundle_state::BundleState) -> HashedPostState {
    use rayon::prelude::*;
    let accounts: Vec<_> = bundle.state.iter().collect();
    let hashed: Vec<_> = accounts
        .par_iter()
        .map(|(address, account)| {
            let storage = HashedStorage::from_iter(account.storage.iter().map(|(slot, value)| {
                (
                    alloy_primitives::keccak256(alloy_primitives::B256::from(*slot)),
                    value.present_value,
                )
            }));
            (
                alloy_primitives::keccak256(address),
                account.info.as_ref().map(Into::into),
                (!storage.is_empty()).then_some(storage),
            )
        })
        .collect();
    hashed.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use reth_trie_common::KeccakKeyHasher;
    use evm2::evm::AccountInfo;
    use reth_trie_common::bundle_state::BundleState;

    #[test]
    fn parallel_hashed_post_state_matches_reth() {
        let info = |n: u64| AccountInfo { nonce: n, balance: U256::from(n), ..Default::default() };
        let bundle = BundleState::builder(1..=1)
            .state_present_account_info(Address::repeat_byte(1), info(1))
            .state_present_account_info(Address::repeat_byte(2), info(2))
            .state_storage(
                Address::repeat_byte(2),
                [(U256::from(7), (U256::ZERO, U256::from(9)))].into_iter().collect(),
            )
            .state_address(Address::repeat_byte(3))
            .build();
        assert_eq!(
            hashed_post_state(&bundle),
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state()),
        );
    }
}
