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

type OpenState = dyn Fn(B256) -> Option<StateProviderBox> + Send + Sync;

/// Checks proposed Profile-1 blocks without executing them.
#[derive(Clone)]
pub struct DeferredChecker {
    chain_spec: Arc<ChainSpec>,
    open: Arc<OpenState>,
}

impl std::fmt::Debug for DeferredChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredChecker").finish_non_exhaustive()
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
        }
    }

    fn rules(&self, number: u64, timestamp: u64) -> IntrinsicRules {
        intrinsic_rules(self.chain_spec.as_ref(), number, timestamp)
    }

    /// The check (blocking: sender recovery and state reads).
    pub fn check(&self, payload: &ExecutionData) -> DeferredCheck {
        let hash = payload.block_hash();
        let result = self.check_inner(payload);
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
        result
    }

    fn check_inner(&self, payload: &ExecutionData) -> DeferredCheck {
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

        let chain_id = self.chain_spec.chain().id();
        let transactions = &payload.payload.as_v1().transactions;
        let decoded: Vec<_> = transactions
            .par_iter()
            .map(|raw| n42_tx_types::N42TxEnvelope::decode_2718_exact(raw.as_ref()))
            .collect();
        let mut txs = Vec::with_capacity(decoded.len());
        for (index, tx) in decoded.into_iter().enumerate() {
            match tx {
                Ok(tx) => txs.push(tx),
                Err(error) => {
                    return DeferredCheck::Refused(format!(
                        "deferred execution: block {} tx {index} does not decode: {error}",
                        header.header.number
                    ));
                }
            }
        }
        let plan_txs: Vec<PlanTx> = txs
            .par_iter()
            .map(|tx| {
                let sender = match tx.chain_id() {
                    Some(id) if id != chain_id => Err(format!("invalid chain id for signer: have {id} want {chain_id}")),
                    _ => tx.recover_signer().map_err(|error| error.to_string()),
                };
                plan_tx(tx, sender)
            })
            .collect();
        let plan_header = PlanHeader {
            number: header.header.number,
            gas_limit: header.header.gas_limit,
            base_fee: header.header.base_fee_per_gas,
        };
        let rules = self.rules(header.header.number, header.header.timestamp);
        let plan = match deferred_tx_plan(plan_header, rules, &plan_txs) {
            Ok(plan) => plan,
            Err(refusal) => return DeferredCheck::Refused(refusal.0),
        };
        if plan.is_empty() {
            return DeferredCheck::Checked;
        }
        let Some(state) = (self.open)(parent) else {
            return DeferredCheck::Retry(format!("the post-state of parent {parent} is not open here"));
        };
        match check_sender_states(header.header.number, &plan, |address| {
            state
                .basic_account(address)
                .map(|account| account.map(sender_state))
        }) {
            Ok(()) => DeferredCheck::Checked,
            Err(Ok(error)) => DeferredCheck::Retry(format!("state read: {error}")),
            Err(Err(refusal)) => DeferredCheck::Refused(refusal.0),
        }
    }
}
