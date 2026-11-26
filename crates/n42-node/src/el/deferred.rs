//! The Profile-1 deferred proposal check over reth's state (design 3.4): the
//! carried fields of a proposed block against this node's result for its
//! parent, and the includability of its transactions against the parent's
//! post-state ([`n42_consensus_service::deferred_check`], gov5's
//! `CheckDeferredBlock`). It reads; it never executes.

use std::sync::Arc;

use alloy_consensus::Transaction as _;
use alloy_consensus::transaction::SignerRecoverable as _;
use alloy_eips::Decodable2718 as _;
use alloy_primitives::B256;
use alloy_rpc_types_engine::ExecutionData;
use n42_consensus_service::deferred_check::{
    DeferredCheck, IntrinsicRules, PlanHeader, PlanTx, SenderState, check_sender_states,
    deferred_tx_plan,
};
use rayon::prelude::*;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_evm::SenderRecoveryCache;
use reth_primitives_traits::transaction::signed::RecoveryError;
use reth_storage_api::StateProviderBox;

/// The fork rules intrinsic gas depends on at `number` / `timestamp`.
pub(crate) fn intrinsic_rules<C: EthChainSpec + EthereumHardforks + ?Sized>(
    spec: &C,
    number: u64,
    timestamp: u64,
) -> IntrinsicRules {
    IntrinsicRules {
        homestead: spec.is_homestead_active_at_block(number),
        istanbul: spec.is_istanbul_active_at_block(number),
        shanghai: spec.is_shanghai_active_at_timestamp(timestamp),
        prague: spec.is_prague_active_at_timestamp(timestamp),
        glamsterdam: n42_execution::profile1_amsterdam_execution_at(spec)
            .is_some_and(|at| at <= timestamp),
    }
}

/// gov5's view of a transaction for the deferred plan.
pub(crate) fn plan_tx(
    tx: &n42_tx_types::N42TxEnvelope,
    sender: Result<alloy_primitives::Address, String>,
) -> PlanTx {
    PlanTx {
        sender,
        nonce: tx.nonce(),
        gas_limit: tx.gas_limit(),
        max_fee_per_gas: tx.max_fee_per_gas(),
        max_priority_fee_per_gas: tx.max_priority_fee_per_gas(),
        to: tx.kind().to().copied(),
        value: tx.value(),
        blob_hashes: tx.blob_versioned_hashes().map_or(0, <[B256]>::len),
        input: tx.input().clone(),
        access_list: tx.access_list().map(|list| {
            (
                list.len() as u64,
                list.iter().map(|item| item.storage_keys.len() as u64).sum(),
            )
        }),
        authorizations: tx.authorization_list().map_or(0, |list| list.len() as u64),
    }
}

/// A sender's state as the deferred rules read it.
pub(crate) fn sender_state(account: reth_primitives_traits::Account) -> SenderState {
    SenderState {
        nonce: account.nonce,
        balance: account.balance,
        code_hash: account.bytecode_hash,
    }
}

/// Up to this many uncached senders are recovered on the check's own
/// thread (about 30 us each) rather than on the rayon pool.
const SERIAL_RECOVERY_MAX: usize = 32;

type OpenState = dyn Fn(B256) -> Option<StateProviderBox> + Send + Sync;

/// Checks proposed Profile-1 blocks without executing them.
#[derive(Clone)]
pub struct DeferredChecker {
    chain_spec: Arc<ChainSpec>,
    open: Arc<OpenState>,
    /// reth's shared secp256k1 sender cache (filled by the transaction
    /// ingress and the engine's payload conversion); `None` recovers every
    /// sender here.
    senders: Option<SenderRecoveryCache>,
}

/// Where a deferred check spent its time, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeferredCheckTimings {
    /// Header lookup and the carried-fields comparison.
    pub carried_ms: f64,
    /// Decoding the transactions and recovering their senders.
    pub senders_ms: f64,
    /// The stateless plan (`deferred_tx_plan`).
    pub plan_ms: f64,
    /// Opening the parent's post-state.
    pub open_ms: f64,
    /// The senders' nonce, balance and code reads.
    pub state_ms: f64,
    /// Transactions whose sender came from a cache.
    pub cached_senders: usize,
    /// Transactions in the block.
    pub transactions: usize,
}

/// The sender of `tx` if a cache holds it. The caches hold only senders
/// recovered (and so verified) from the transaction with that hash, with
/// the same rules as `recover_signer`; a failed recovery is never cached.
fn cached_sender(
    cache: Option<&SenderRecoveryCache>,
    tx: &n42_tx_types::N42TxEnvelope,
) -> Option<alloy_primitives::Address> {
    match tx {
        n42_tx_types::N42TxEnvelope::Eth(eth) => cache?.get(eth.tx_hash()),
        n42_tx_types::N42TxEnvelope::AltSig(alt) => {
            n42_tx_types::AltSigSenderCache::global().get(alt.hash())
        }
    }
}

/// `recover_signer`, filing the sender in the cache the import reads.
fn recover_sender(
    cache: Option<&SenderRecoveryCache>,
    tx: &n42_tx_types::N42TxEnvelope,
) -> Result<alloy_primitives::Address, RecoveryError> {
    match tx {
        n42_tx_types::N42TxEnvelope::Eth(_) => match cache {
            Some(cache) => cache.recover(tx),
            None => tx.recover_signer(),
        },
        n42_tx_types::N42TxEnvelope::AltSig(alt) => tx.recover_signer().inspect(|sender| {
            n42_tx_types::AltSigSenderCache::global().insert(*alt.hash(), *sender)
        }),
    }
}

impl DeferredChecker {
    /// `open` gives the post-state of a block this node has executed.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        open: impl Fn(B256) -> Option<StateProviderBox> + Send + Sync + 'static,
    ) -> Self {
        Self {
            chain_spec,
            open: Arc::new(open),
            senders: None,
        }
    }

    /// Reads and fills reth's shared sender cache.
    pub fn with_sender_recovery_cache(mut self, cache: Option<SenderRecoveryCache>) -> Self {
        self.senders = cache;
        self
    }

    fn rules(&self, number: u64, timestamp: u64) -> IntrinsicRules {
        intrinsic_rules(self.chain_spec.as_ref(), number, timestamp)
    }

    /// The check (blocking: sender recovery and state reads).
    pub fn check(&self, payload: &ExecutionData) -> DeferredCheck {
        self.check_timed(payload).0
    }

    /// [`Self::check`] and where it spent its time.
    pub fn check_timed(&self, payload: &ExecutionData) -> (DeferredCheck, DeferredCheckTimings) {
        let hash = payload.block_hash();
        let mut timings = DeferredCheckTimings::default();
        let result = self.check_inner(payload, &mut timings);
        for (stage, ms) in [
            ("carried", timings.carried_ms),
            ("senders", timings.senders_ms),
            ("plan", timings.plan_ms),
            ("open", timings.open_ms),
            ("state", timings.state_ms),
        ] {
            metrics::histogram!("n42_deferred_check_stage_ms", "stage" => stage).record(ms);
        }
        metrics::counter!("n42_deferred_check_senders_total", "source" => "cache")
            .increment(timings.cached_senders as u64);
        metrics::counter!("n42_deferred_check_senders_total", "source" => "recovered")
            .increment((timings.transactions - timings.cached_senders) as u64);
        tracing::info!(
            target: "n42::profile1",
            %hash,
            transactions = timings.transactions,
            cached_senders = timings.cached_senders,
            carried_ms = format_args!("{:.2}", timings.carried_ms),
            senders_ms = format_args!("{:.2}", timings.senders_ms),
            plan_ms = format_args!("{:.2}", timings.plan_ms),
            open_ms = format_args!("{:.2}", timings.open_ms),
            state_ms = format_args!("{:.2}", timings.state_ms),
            "deferred check stages"
        );
        let outcome = match &result {
            DeferredCheck::Checked => "checked",
            DeferredCheck::Retry(_) => "retry",
            DeferredCheck::Refused(_) => "refused",
            DeferredCheck::Unsupported => "unsupported",
        };
        metrics::counter!("n42_deferred_check_total", "outcome" => outcome).increment(1);
        if let DeferredCheck::Refused(reason) = &result {
            let kind = if reason.starts_with("carried") { "deferred_result" } else { "includability" };
            metrics::counter!("n42_blocks_refused_total", "reason" => kind).increment(1);
            tracing::warn!(target: "n42::profile1", %hash, %reason, "deferred check REFUSED the proposal");
        }
        (result, timings)
    }

    fn check_inner(
        &self,
        payload: &ExecutionData,
        timings: &mut DeferredCheckTimings,
    ) -> DeferredCheck {
        let elapsed_ms = |since: std::time::Instant| since.elapsed().as_secs_f64() * 1000.0;
        let started = std::time::Instant::now();
        let hash = payload.block_hash();
        let parent = payload.parent_hash();
        // A block this node has executed: its post-state reads its own
        // transactions' nonces as used. It passed full validation (gov5
        // skips the check for an applied block).
        if n42_consensus::executed_fields(&hash).is_some() {
            return DeferredCheck::Checked;
        }
        let Some(header) = n42_consensus::remembered_profile1_header(&hash) else {
            return DeferredCheck::Retry("the block's header is not admitted here".into());
        };
        if header.header.parent_hash != parent {
            return DeferredCheck::Refused("the payload's parent is not the header's".into());
        }
        let Some(parent_fields) = n42_consensus::executed_fields(&parent) else {
            return DeferredCheck::Retry(format!("the result of parent {parent} is not known here"));
        };
        let link = n42_consensus::deferred_depth::ParentLink {
            number: header.header.number.saturating_sub(1),
            hash: parent,
            parent_hash: B256::ZERO,
        };
        let expected = match n42_consensus::deferred_depth::expected_fields(
            &link,
            1,
            parent_fields,
            n42_consensus::executed_fields,
        ) {
            Ok(expected) => expected,
            Err(error) => return DeferredCheck::Retry(error.to_string()),
        };
        if let Some(field) = n42_consensus::ExecutedFields::carried_by(&header.header)
            .first_difference(&expected)
        {
            return DeferredCheck::Refused(format!(
                "carried {field} is not this node's result for parent {parent}"
            ));
        }

        timings.carried_ms = elapsed_ms(started);

        // Decode on this thread (the lowest undecodable index is refused
        // before any sender is looked at, as before), take the senders the
        // ingress or the import already recovered, and recover only the rest
        // in parallel: a block of gossiped transactions never waits for the
        // rayon pool the import is busy in.
        let senders_started = std::time::Instant::now();
        let chain_id = self.chain_spec.chain().id();
        let transactions = &payload.payload.as_v1().transactions;
        let cache = self.senders.as_ref();
        let mut txs = Vec::with_capacity(transactions.len());
        for (index, raw) in transactions.iter().enumerate() {
            match n42_tx_types::N42TxEnvelope::decode_2718_exact(raw.as_ref()) {
                Ok(tx) => txs.push(tx),
                Err(error) => {
                    timings.senders_ms = elapsed_ms(senders_started);
                    return DeferredCheck::Refused(format!(
                        "deferred execution: block {} tx {index} does not decode: {error}",
                        header.header.number
                    ));
                }
            }
        }
        timings.transactions = txs.len();
        let mut senders: Vec<Option<Result<alloy_primitives::Address, String>>> = txs
            .iter()
            .map(|tx| match tx.chain_id() {
                Some(id) if id != chain_id => Some(Err(format!(
                    "invalid chain id for signer: have {id} want {chain_id}"
                ))),
                _ => cached_sender(cache, tx).map(Ok),
            })
            .collect();
        let misses: Vec<usize> = (0..txs.len())
            .filter(|&index| senders[index].is_none())
            .collect();
        timings.cached_senders = txs.len() - misses.len();
        let recover =
            |&index: &usize| recover_sender(cache, &txs[index]).map_err(|error| error.to_string());
        // A few misses (a lossy cache slot, a transaction that skipped the
        // ingress) cost less here than a wait for a busy pool.
        let recovered: Vec<_> = if misses.len() <= SERIAL_RECOVERY_MAX {
            misses.iter().map(recover).collect()
        } else {
            misses.par_iter().map(recover).collect()
        };
        for (index, sender) in misses.into_iter().zip(recovered) {
            senders[index] = Some(sender);
        }
        let plan_txs: Vec<PlanTx> = txs
            .iter()
            .zip(senders)
            .map(|(tx, sender)| plan_tx(tx, sender.expect("every sender is cached or recovered")))
            .collect();
        timings.senders_ms = elapsed_ms(senders_started);
        let plan_started = std::time::Instant::now();
        let plan_header = PlanHeader {
            number: header.header.number,
            gas_limit: header.header.gas_limit,
            base_fee: header.header.base_fee_per_gas,
        };
        let rules = self.rules(header.header.number, header.header.timestamp);
        let plan = deferred_tx_plan(plan_header, rules, &plan_txs);
        timings.plan_ms = elapsed_ms(plan_started);
        let plan = match plan {
            Ok(plan) => plan,
            Err(refusal) => return DeferredCheck::Refused(refusal.0),
        };
        if plan.is_empty() {
            return DeferredCheck::Checked;
        }
        let open_started = std::time::Instant::now();
        let state = (self.open)(parent);
        timings.open_ms = elapsed_ms(open_started);
        let Some(state) = state else {
            return DeferredCheck::Retry(format!("the post-state of parent {parent} is not open here"));
        };
        let state_started = std::time::Instant::now();
        let checked = check_sender_states(header.header.number, &plan, |address| {
            state
                .basic_account(address)
                .map(|account| account.map(sender_state))
        });
        timings.state_ms = elapsed_ms(state_started);
        match checked {
            Ok(()) => DeferredCheck::Checked,
            Err(Ok(error)) => DeferredCheck::Retry(format!("state read: {error}")),
            Err(Err(refusal)) => DeferredCheck::Refused(refusal.0),
        }
    }
}
