// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! A plain value transfer applied without the interpreter.
//!
//! Adapted from n42-rs `fast_transfer.rs` at 466c1839791afadba7f1dd4d48c5518440dcafba.
//! Retains N42-26 custom precompiles and adds local environment eligibility checks.
//!
//! The upstream fleet used 163,000 transfers per block and measured about
//! 1.4 us of REVM work per transfer (not a local performance claim): a journal, a frame, a
//! call into an account with no code, the journal's finalisation. The state
//! transition of such a transfer is three balance changes and a nonce, and
//! this module applies exactly that -- with revm's own arithmetic, in revm's
//! own order, producing the accounts revm's journal would hand back and the
//! result its handler would build -- and only when it can prove revm would
//! succeed and charge exactly the base cost. Anything else, and every
//! transaction on a fork this module has not been checked against, goes to
//! the interpreter unchanged. Every node executes every transaction either
//! way; nothing is skipped, and the post-state is the same to the byte.
//!
//! What qualifies: a call (legacy, EIP-2930 or EIP-1559) with no calldata, no
//! access list, no blob and no authorisation, from an account without code,
//! to an account without code that is not a precompile, with the nonce, the
//! balance and the fees revm's pre-execution checks demand, on Cancun, Prague or
//! Osaka. The sender, the recipient and the block's beneficiary must be three
//! distinct accounts, the recipient must not be left empty (EIP-161) and the
//! beneficiary must already exist and not be empty, so that no account's
//! existence changes in a way this module would have to model.
//!
//! `N42_FAST_TRANSFER=1` turns it on; it is off by default so that a fleet
//! can measure it against the interpreter on the same binary.

#[cfg(test)]
use crate::evm_factory::N42EvmFactory;
use alloy_evm::{Database, EthEvm, Evm, EvmEnv, eth::EthEvmContext, precompiles::PrecompilesMap};
use alloy_primitives::{Address, Bytes, U256};
use revm::{
    Inspector,
    context::{BlockEnv, CfgEnv, TxEnv},
    context_interface::{
        Block as _, Cfg as _, Transaction as _,
        result::{
            EVMError, ExecutionResult, HaltReason, Output, ResultAndState, ResultGas, SuccessReason,
        },
    },
    primitives::{TxKind, hardfork::SpecId},
    state::{Account, EvmState, TransactionId},
};

/// The gas of a call with no calldata: the whole cost of a qualifying transfer.
const TRANSFER_GAS: u64 = 21_000;

/// How many transfers have taken this path in this process.
static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Transfers reported by completed or dropped EVM instances in this process.
pub fn hits() -> u64 {
    HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Why transfers were sent to the interpreter instead, by reason, so a
/// fleet that shows no hits says which check refused them.
static REJECTED: [std::sync::atomic::AtomicU64; 12] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 12];

/// The refusals so far, by reason: shape, fork, configuration, limits, fees,
/// parties, sender, balance, recipient, beneficiary, arithmetic, inspecting.
pub fn rejected() -> [u64; 12] {
    std::array::from_fn(|i| REJECTED[i].load(std::sync::atomic::Ordering::Relaxed))
}

// Count locally while executing; publish once when the EVM is finished or dropped.
// A global atomic increment per transfer would contend across execution lanes.
#[derive(Default)]
struct TransferStats {
    hits: u64,
    rejected: [u64; 12],
}

impl Drop for TransferStats {
    fn drop(&mut self) {
        if self.hits != 0 {
            HITS.fetch_add(self.hits, std::sync::atomic::Ordering::Relaxed);
            metrics::counter!("n42_fast_transfer_total").increment(self.hits);
        }
        for (reason, count) in self.rejected.iter().copied().enumerate() {
            if count != 0 {
                REJECTED[reason].fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                const REASONS: [&str; 12] = [
                    "shape",
                    "fork",
                    "configuration",
                    "limits",
                    "fees",
                    "parties",
                    "sender",
                    "balance",
                    "recipient",
                    "beneficiary",
                    "arithmetic",
                    "inspecting",
                ];
                metrics::counter!("n42_fast_transfer_fallback_total", "reason" => REASONS[reason])
                    .increment(count);
            }
        }
    }
}

/// Whether `N42_FAST_TRANSFER=1` is set.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("N42_FAST_TRANSFER")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

fn environment_refusal<DB: Database, I: Inspector<EthEvmContext<DB>>>(
    inner: &EthEvm<DB, I, PrecompilesMap>,
) -> Option<usize> {
    let cfg = inner.cfg_env();
    let block = inner.block();
    // Cancun through Osaka. Cancun has no EIP-7623 floor; Prague adds it.
    // Amsterdam transfer logs/state gas are not represented by this path.
    let spec = cfg.spec();
    if !spec.is_enabled_in(SpecId::CANCUN) || spec.is_enabled_in(SpecId::AMSTERDAM) {
        return Some(1);
    }
    // Every check revm makes before executing, as revm makes it; a
    // configuration that relaxes any of them is not modelled here.
    if cfg.is_nonce_check_disabled()
        || cfg.is_balance_check_disabled()
        || cfg.is_eip3607_disabled()
        || cfg.is_fee_charge_disabled()
        || cfg.is_base_fee_check_disabled()
        || cfg.is_priority_fee_check_disabled()
        || cfg.is_block_gas_limit_disabled()
        || cfg.is_eip7623_disabled()
        || cfg.is_amsterdam_eip8037_enabled()
        || cfg.is_amsterdam_eip2780_enabled()
        || cfg.gas_params() != &revm::context_interface::cfg::GasParams::new_spec(*spec)
        || block.prevrandao().is_none()
        || block.blob_excess_gas_and_price().is_none()
    {
        return Some(2);
    }
    None
}

/// [`EthEvm`] with the transfer path in front of the interpreter.
pub struct N42Evm<DB: Database, I> {
    inner: EthEvm<DB, I, PrecompilesMap>,
    /// An inspector is watching: every transaction goes through the
    /// interpreter, which is what the inspector expects to see.
    inspecting: bool,
    fast: bool,
    environment_refusal: Option<usize>,
    stats: TransferStats,
}

impl<DB: Database, I> std::fmt::Debug for N42Evm<DB, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("N42Evm")
            .field("inspecting", &self.inspecting)
            .field("fast", &self.fast)
            .finish_non_exhaustive()
    }
}

impl<DB: Database, I: Inspector<EthEvmContext<DB>>> N42Evm<DB, I> {
    pub(crate) fn new(inner: EthEvm<DB, I, PrecompilesMap>, inspecting: bool, fast: bool) -> Self {
        let environment_refusal = if fast {
            environment_refusal(&inner)
        } else {
            None
        };
        Self {
            stats: TransferStats::default(),
            environment_refusal,
            inner,
            inspecting,
            fast,
        }
    }

    /// Transfers executed by this EVM before it is finished.
    pub const fn fast_transfer_hits(&self) -> u64 {
        self.stats.hits
    }

    fn refused<T, E>(&mut self, reason: usize) -> Result<Option<T>, E> {
        self.stats.rejected[reason] += 1;
        Ok(None)
    }

    /// The transfer's result and post-state, if the transaction qualifies and
    /// revm would succeed on it; `None` sends it to the interpreter. A database
    /// error is the same error the interpreter would have hit loading the
    /// account.
    pub(crate) fn transfer(&mut self, tx: &TxEnv) -> Result<Option<ResultAndState>, DB::Error> {
        // The transaction's shape.
        let TxKind::Call(to) = tx.kind else {
            return self.refused(0);
        };
        if !tx.data.is_empty()
            || tx.tx_type > 2
            || !tx.access_list.0.is_empty()
            || !tx.authorization_list.is_empty()
            || !tx.blob_hashes.is_empty()
            || tx.gas_limit < TRANSFER_GAS
            || (tx.tx_type != 0 && tx.chain_id.is_none())
        {
            return self.refused(0);
        }
        let cfg = self.inner.cfg_env();
        let block = self.inner.block();
        if let Some(reason) = self.environment_refusal {
            return self.refused(reason);
        }
        if tx.chain_id.is_some_and(|id| id != cfg.chain_id())
            || tx.gas_limit > cfg.tx_gas_limit_cap()
            || tx.gas_limit > block.gas_limit()
        {
            return self.refused(3);
        }
        let basefee = block.basefee() as u128;
        if tx.gas_price < basefee || tx.gas_priority_fee.is_some_and(|tip| tip > tx.gas_price) {
            return self.refused(4);
        }
        let caller = tx.caller;
        let floor_gas = if cfg.spec().is_enabled_in(SpecId::PRAGUE) {
            TRANSFER_GAS
        } else {
            0
        };
        let beneficiary = block.beneficiary();
        if to == caller || to == beneficiary || caller == beneficiary {
            return self.refused(5);
        }
        // Use the actual map: this client has 0x0302 and callers can install
        // additional precompiles at arbitrary addresses through components_mut.
        if self.inner.precompiles().get(&to).is_some() {
            return self.refused(5);
        }

        // The accounts, loaded the way the journal would load them: through
        // the same database, so its cache holds them as the pre-state.
        let value = tx.value;
        let db = self.inner.db_mut();
        let Some(sender) = db.basic(caller)? else {
            return self.refused(6);
        };
        if !sender.is_code_hash_empty_or_zero()
            || sender.nonce != tx.nonce
            || sender.nonce == u64::MAX
        {
            return self.refused(6);
        }
        let Ok(max_spending) = tx.max_balance_spending() else {
            return self.refused(7);
        };
        if max_spending > sender.balance {
            return self.refused(7);
        }
        let recipient = db.basic(to)?;
        match &recipient {
            Some(info) if !info.is_code_hash_empty_or_zero() => return self.refused(8),
            // An account that stays empty after being touched is deleted
            // (EIP-161); the interpreter models that, this does not.
            Some(info) if value.is_zero() && info.is_empty() => return self.refused(8),
            None if value.is_zero() => return self.refused(8),
            _ => {}
        }
        let Some(coinbase) = db.basic(beneficiary)? else {
            return self.refused(9);
        };
        if coinbase.is_empty() {
            return self.refused(9);
        }

        // revm's arithmetic: the caller pays gas_limit at the effective price
        // and the value, then gets the unused gas back at the same price; the
        // beneficiary receives the used gas at the price above the base fee.
        let effective_price = tx.effective_gas_price(basefee);
        let Some(gas_cost) = effective_price.checked_mul(TRANSFER_GAS as u128) else {
            return self.refused(10);
        };
        let Some(sender_balance) = sender
            .balance
            .checked_sub(value)
            .and_then(|b| b.checked_sub(U256::from(gas_cost)))
        else {
            return self.refused(10);
        };
        let Some(recipient_balance) = recipient
            .as_ref()
            .map_or(U256::ZERO, |r| r.balance)
            .checked_add(value)
        else {
            return self.refused(10);
        };
        let tip = effective_price.saturating_sub(basefee);
        let Some(reward) = tip.checked_mul(TRANSFER_GAS as u128) else {
            return self.refused(10);
        };
        let Some(coinbase_balance) = coinbase.balance.checked_add(U256::from(reward)) else {
            return self.refused(10);
        };

        // The accounts as the journal would return them: touched, with the
        // pre-state kept as the original, and a recipient that did not exist
        // marked as loaded that way.
        // Three accounts, sized once: growing from empty reallocated twice per
        // transaction, 326,000 allocations a full block on both the builder and
        // the follower.
        let mut state: EvmState = EvmState::with_capacity_and_hasher(4, Default::default());
        let mut sender_account = Account::from(sender);
        sender_account.info.balance = sender_balance;
        sender_account.info.nonce += 1;
        sender_account.mark_touch();
        state.insert(caller, sender_account);
        let mut recipient_account = match recipient {
            Some(info) => Account::from(info),
            None => Account::new_not_existing(TransactionId::ZERO),
        };
        recipient_account.info.balance = recipient_balance;
        recipient_account.mark_touch();
        state.insert(to, recipient_account);
        let mut coinbase_account = Account::from(coinbase);
        coinbase_account.info.balance = coinbase_balance;
        coinbase_account.mark_touch();
        state.insert(beneficiary, coinbase_account);

        // The result revm's handler builds for a call into an account without
        // code: it stops, spends the base cost, refunds nothing, and the
        // EIP-7623 floor for no calldata is the base cost too on Prague+;
        // Cancun's result must retain a zero floor even though used gas agrees.
        let result = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::new_with_state_gas(TRANSFER_GAS, 0, floor_gas, 0),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        };
        Ok(Some(ResultAndState::new(result, state)))
    }
}

impl<DB, I> Evm for N42Evm<DB, I>
where
    DB: Database,
    I: Inspector<EthEvmContext<DB>>,
{
    type DB = DB;
    type Tx = TxEnv;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &CfgEnv<SpecId> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(&mut self, tx: TxEnv) -> Result<ResultAndState, Self::Error> {
        if self.fast && self.inspecting {
            self.stats.rejected[11] += 1;
        }
        if self.fast
            && !self.inspecting
            && let Some(done) = self.transfer(&tx).map_err(EVMError::Database)?
        {
            self.stats.hits += 1;
            return Ok(done);
        }
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (DB, EvmEnv<SpecId, BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inspecting = enabled;
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&DB, &I, &PrecompilesMap) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut DB, &mut I, &mut PrecompilesMap) {
        self.inner.components_mut()
    }
}

#[cfg(test)]
mod tests {
    //! The transfer path against the interpreter: same result, same accounts.
    use super::*;
    use alloy_evm::EvmFactory;
    use alloy_primitives::{TxKind, address};
    use revm::{
        Database as _, DatabaseCommit,
        database::{CacheDB, EmptyDB},
        state::AccountInfo,
    };

    const SENDER: Address = address!("0x1000000000000000000000000000000000000001");
    const RECIPIENT: Address = address!("0x2000000000000000000000000000000000000002");
    const EXISTING: Address = address!("0x3000000000000000000000000000000000000003");
    const COINBASE: Address = address!("0x4000000000000000000000000000000000000004");
    const BASEFEE: u64 = 1_000;

    fn env() -> EvmEnv {
        let mut cfg = CfgEnv::new_with_spec(SpecId::OSAKA);
        cfg.chain_id = 1;
        let block = BlockEnv {
            beneficiary: COINBASE,
            basefee: BASEFEE,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        EvmEnv::new(cfg, block)
    }

    fn db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            SENDER,
            AccountInfo {
                balance: U256::from(10u128.pow(20)),
                nonce: 7,
                ..Default::default()
            },
        );
        db.insert_account_info(
            EXISTING,
            AccountInfo {
                balance: U256::from(5),
                nonce: 3,
                ..Default::default()
            },
        );
        db.insert_account_info(
            COINBASE,
            AccountInfo {
                balance: U256::from(1),
                ..Default::default()
            },
        );
        db
    }

    fn tx(to: Address, value: u128, tx_type: u8, gas_price: u128, tip: Option<u128>) -> TxEnv {
        TxEnv {
            tx_type,
            caller: SENDER,
            gas_limit: 50_000,
            gas_price,
            gas_priority_fee: tip,
            kind: TxKind::Call(to),
            value: U256::from(value),
            data: Bytes::new(),
            nonce: 7,
            chain_id: Some(1),
            ..Default::default()
        }
    }

    /// Runs `tx` on both paths from the same pre-state; returns (fast, slow),
    /// each as the result and the committed accounts of the three parties.
    fn both(tx: TxEnv) -> Vec<(ResultAndState, Vec<Option<AccountInfo>>)> {
        [true, false]
            .into_iter()
            .map(|fast| {
                let mut evm = N42EvmFactory::with_fast_transfers(fast).create_evm(db(), env());
                let out = evm
                    .transact_raw(tx.clone())
                    .expect("the transaction executes");
                let (mut db, _) = evm.finish();
                db.commit(out.state.clone());
                let infos = [SENDER, RECIPIENT, EXISTING, COINBASE]
                    .into_iter()
                    .map(|a| {
                        db.basic(a)
                            .expect("cache")
                            .map(|i| AccountInfo { code: None, ..i })
                    })
                    .collect();
                (out, infos)
            })
            .collect()
    }

    fn assert_same(tx: TxEnv, expect_fast: bool) {
        let runs = both(tx.clone());
        let (fast, slow) = (&runs[0], &runs[1]);
        assert_eq!(fast.0.result, slow.0.result, "result");
        assert_eq!(fast.1, slow.1, "committed accounts");
        // The accounts the path hands back: only the touched ones, and for
        // those the same info and the same existence flag as the journal's.
        for (address, account) in &fast.0.state {
            let theirs = slow
                .0
                .state
                .get(address)
                .expect("the interpreter loaded it too");
            assert_eq!(account.info, theirs.info, "info of {address}");
            assert_eq!(
                account.is_touched(),
                theirs.is_touched(),
                "touched {address}"
            );
            assert_eq!(
                account.is_loaded_as_not_existing(),
                theirs.is_loaded_as_not_existing(),
                "not-existing flag {address}"
            );
        }
        let mut evm = N42EvmFactory::with_fast_transfers(true).create_evm(db(), env());
        assert_eq!(
            evm.transfer(&tx).expect("no database error").is_some(),
            expect_fast,
            "the path taken"
        );
    }

    /// The two paths through revm's `State`, the layer the node persists
    /// from: the bundle (accounts, their statuses, the reverts) must be the
    /// same, or the block written to the database is not.
    fn bundles(tx: TxEnv) -> Vec<revm::database::BundleState> {
        use revm::database::{State, states::bundle_state::BundleRetention};
        [true, false]
            .into_iter()
            .map(|fast| {
                let mut state = State::builder()
                    .with_database(db())
                    .with_bundle_update()
                    .build();
                {
                    let mut evm =
                        N42EvmFactory::with_fast_transfers(fast).create_evm(&mut state, env());
                    let out = evm
                        .transact_raw(tx.clone())
                        .expect("the transaction executes");
                    evm.db_mut().commit(out.state);
                }
                state.merge_transitions(BundleRetention::Reverts);
                state.take_bundle()
            })
            .collect()
    }

    fn assert_same_bundle(tx: TxEnv) {
        let b = bundles(tx);
        let (fast, slow) = (&b[0], &b[1]);
        assert_eq!(fast.state.len(), slow.state.len(), "accounts in the bundle");
        for (address, account) in &slow.state {
            let ours = fast.state.get(address).expect("account in our bundle");
            assert_eq!(ours.info, account.info, "info {address}");
            assert_eq!(
                ours.original_info, account.original_info,
                "original info {address}"
            );
            assert_eq!(ours.status, account.status, "status {address}");
            assert_eq!(ours.storage, account.storage, "storage {address}");
        }
        assert_eq!(fast.reverts, slow.reverts, "reverts");
        assert_eq!(fast.contracts.len(), slow.contracts.len(), "contracts");
    }

    #[test]
    fn bundle_of_a_transfer_to_a_new_account() {
        assert_same_bundle(tx(RECIPIENT, 12_345, 2, 5_000, Some(300)));
    }

    #[test]
    fn bundle_of_a_transfer_to_an_existing_account() {
        assert_same_bundle(tx(EXISTING, 1, 2, 5_000, Some(300)));
    }

    #[test]
    fn bundle_of_two_transfers_in_one_block() {
        use revm::database::{State, states::bundle_state::BundleRetention};
        let b: Vec<revm::database::BundleState> = [true, false]
            .into_iter()
            .map(|fast| {
                let mut state = State::builder()
                    .with_database(db())
                    .with_bundle_update()
                    .build();
                {
                    let mut evm =
                        N42EvmFactory::with_fast_transfers(fast).create_evm(&mut state, env());
                    for (nonce, to) in [(7u64, RECIPIENT), (8u64, RECIPIENT)] {
                        let mut t = tx(to, 5, 2, 5_000, Some(300));
                        t.nonce = nonce;
                        let out = evm.transact_raw(t).expect("executes");
                        evm.db_mut().commit(out.state);
                    }
                }
                state.merge_transitions(BundleRetention::Reverts);
                state.take_bundle()
            })
            .collect();
        assert_eq!(b[0].state.len(), b[1].state.len());
        for (address, account) in &b[1].state {
            let ours = &b[0].state[address];
            assert_eq!(
                (&ours.info, &ours.original_info, ours.status),
                (&account.info, &account.original_info, account.status),
                "{address}"
            );
        }
        assert_eq!(b[0].reverts, b[1].reverts, "reverts");
    }

    #[test]
    fn eip1559_transfer_to_a_new_account() {
        assert_same(tx(RECIPIENT, 12_345, 2, 5_000, Some(300)), true);
    }

    #[test]
    fn eip1559_transfer_to_an_existing_account() {
        assert_same(tx(EXISTING, 1, 2, 5_000, Some(300)), true);
    }

    #[test]
    fn tip_capped_by_the_max_fee() {
        assert_same(tx(EXISTING, 1, 2, 1_100, Some(300)), true);
    }

    #[test]
    fn legacy_transfer() {
        assert_same(tx(EXISTING, 99, 0, 2_000, None), true);
    }

    #[test]
    fn zero_value_to_a_new_account_goes_to_the_interpreter() {
        assert_same(tx(RECIPIENT, 0, 2, 5_000, Some(300)), false);
    }

    #[test]
    fn a_transfer_to_self_goes_to_the_interpreter() {
        assert_same(tx(SENDER, 1, 2, 5_000, Some(300)), false);
    }

    #[test]
    fn a_wrong_nonce_goes_to_the_interpreter() {
        let mut t = tx(EXISTING, 1, 2, 5_000, Some(300));
        t.nonce = 8;
        let mut evm = N42EvmFactory::with_fast_transfers(true).create_evm(db(), env());
        assert!(evm.transact_raw(t).is_err(), "the interpreter rejects it");
    }

    fn compare_case(database: CacheDB<EmptyDB>, environment: EvmEnv, tx: TxEnv, hit: bool) {
        use revm::database::{State, states::bundle_state::BundleRetention};
        let run = |enabled| {
            let mut state = State::builder()
                .with_database(database.clone())
                .with_bundle_update()
                .build();
            let (result, hits) = {
                let mut evm = N42EvmFactory::with_fast_transfers(enabled)
                    .create_evm(&mut state, environment.clone());
                let result = evm.transact_raw(tx.clone()).map(|out| {
                    let result = out.result;
                    evm.db_mut().commit(out.state);
                    result
                });
                (result, evm.fast_transfer_hits())
            };
            state.merge_transitions(BundleRetention::Reverts);
            (result, state.take_bundle(), hits)
        };
        let (fast, fast_bundle, hits) = run(true);
        let (slow, slow_bundle, slow_hits) = run(false);
        assert_eq!(hits, u64::from(hit), "incorrect path for {tx:?}");
        assert_eq!(slow_hits, 0);
        assert_eq!(fast, slow, "result or validation error differs for {tx:?}");
        assert_eq!(
            fast_bundle, slow_bundle,
            "persisted bundle differs for {tx:?}"
        );
    }

    #[test]
    fn supported_forks_and_transaction_types_match_complete_bundles() {
        for spec in [SpecId::CANCUN, SpecId::PRAGUE, SpecId::OSAKA] {
            let mut environment = env();
            environment.cfg_env.set_spec_and_mainnet_gas_params(spec);
            for tx_type in 0..=2 {
                for to in [RECIPIENT, EXISTING] {
                    for value in [0, 1, 12_345] {
                        let tip = (tx_type == 2).then_some(300);
                        compare_case(
                            db(),
                            environment.clone(),
                            tx(to, value, tx_type, 5_000, tip),
                            to != RECIPIENT || value != 0,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn malformed_transactions_preserve_interpreter_errors() {
        let good = tx(EXISTING, 1, 2, 5_000, Some(300));
        let mut cases = Vec::new();
        for nonce in [6, 8, u64::MAX] {
            cases.push(TxEnv {
                nonce,
                ..good.clone()
            });
        }
        for chain_id in [None, Some(2)] {
            cases.push(TxEnv {
                chain_id,
                ..good.clone()
            });
        }
        for gas_limit in [20_999, 30_000_001, u64::MAX] {
            cases.push(TxEnv {
                gas_limit,
                ..good.clone()
            });
        }
        cases.push(TxEnv {
            gas_price: 999,
            ..good.clone()
        });
        cases.push(TxEnv {
            gas_priority_fee: Some(5_001),
            ..good.clone()
        });
        cases.push(TxEnv {
            value: U256::MAX,
            ..good.clone()
        });
        for transaction in cases {
            compare_case(db(), env(), transaction, false);
        }
    }

    #[test]
    fn environment_overrides_and_missing_header_fields_fall_back() {
        let good = tx(EXISTING, 1, 2, 5_000, Some(300));
        let mut cases = Vec::new();
        let mut missing_randomness = env();
        missing_randomness.block_env.prevrandao = None;
        cases.push(missing_randomness);
        let mut missing_blob_price = env();
        missing_blob_price.block_env.blob_excess_gas_and_price = None;
        cases.push(missing_blob_price);
        let mut wrong_gas_schedule = env();
        wrong_gas_schedule.cfg_env.gas_params =
            revm::context_interface::cfg::GasParams::new_spec(SpecId::FRONTIER);
        cases.push(wrong_gas_schedule);
        let mut no_nonce_check = env();
        no_nonce_check.cfg_env.disable_nonce_check = true;
        cases.push(no_nonce_check);
        let mut experimental_state_gas = env();
        experimental_state_gas.cfg_env.enable_amsterdam_eip8037 = true;
        cases.push(experimental_state_gas);
        let mut experimental_intrinsic_gas = env();
        experimental_intrinsic_gas.cfg_env.enable_amsterdam_eip2780 = true;
        cases.push(experimental_intrinsic_gas);
        for spec in [SpecId::SHANGHAI, SpecId::AMSTERDAM] {
            let mut environment = env();
            environment.cfg_env.set_spec_and_mainnet_gas_params(spec);
            cases.push(environment);
        }
        for environment in cases {
            compare_case(db(), environment, good.clone(), false);
        }
    }

    #[test]
    fn account_edge_cases_fall_back_without_changing_results() {
        let good = tx(EXISTING, 1, 2, 5_000, Some(300));
        for (address, info) in [
            (
                SENDER,
                AccountInfo {
                    nonce: 7,
                    balance: U256::from(1),
                    ..Default::default()
                },
            ),
            (
                SENDER,
                AccountInfo {
                    nonce: u64::MAX,
                    balance: U256::MAX,
                    ..Default::default()
                },
            ),
            (
                EXISTING,
                AccountInfo {
                    balance: U256::MAX,
                    ..Default::default()
                },
            ),
            (COINBASE, AccountInfo::default()),
            (
                COINBASE,
                AccountInfo {
                    balance: U256::MAX,
                    ..Default::default()
                },
            ),
        ] {
            let mut database = db();
            database.insert_account_info(address, info);
            compare_case(database, env(), good.clone(), false);
        }
        for to in [SENDER, COINBASE] {
            compare_case(db(), env(), tx(to, 1, 2, 5_000, Some(300)), false);
        }
        let mut environment = env();
        environment.block_env.beneficiary = SENDER;
        compare_case(db(), environment, good, false);
    }

    #[test]
    fn n42_randomness_precompile_is_preserved_even_with_empty_calldata() {
        let to = Address::from(crate::precompile_random::RANDOMNESS_ADDRESS);
        let mut environment = env();
        let randomness = alloy_primitives::B256::repeat_byte(0x67);
        environment.block_env.prevrandao = Some(randomness);
        let transaction = tx(to, 1, 2, 5_000, Some(300));
        compare_case(db(), environment.clone(), transaction.clone(), false);
        let mut evm = N42EvmFactory::with_fast_transfers(true).create_evm(db(), environment);
        let result = evm.transact_raw(transaction).unwrap();
        assert_eq!(
            result.result.output().unwrap().as_ref(),
            alloy_primitives::keccak256(randomness).as_slice()
        );
        assert_eq!(evm.fast_transfer_hits(), 0);
    }

    #[test]
    fn dynamically_installed_high_address_precompile_is_never_treated_as_eoa() {
        use revm::precompile::{PrecompileFn, PrecompileId};
        let mut evm = N42EvmFactory::with_fast_transfers(true).create_evm(db(), env());
        evm.precompiles_mut().apply_precompile(&RECIPIENT, |_| {
            Some(
                (
                    PrecompileId::custom("test-high-address"),
                    crate::precompile_random::revm_precompile_fn as PrecompileFn,
                )
                    .into(),
            )
        });
        let result = evm
            .transact_raw(tx(RECIPIENT, 1, 2, 5_000, Some(300)))
            .unwrap();
        assert_eq!(evm.fast_transfer_hits(), 0);
        assert_eq!(result.result.output().unwrap().len(), 32);
    }

    #[test]
    fn inspector_creation_and_toggling_keep_interpreter_observable() {
        use revm::inspector::NoOpInspector;
        let mut evm = N42EvmFactory::with_fast_transfers(true).create_evm_with_inspector(
            db(),
            env(),
            NoOpInspector,
        );
        let transaction = tx(EXISTING, 1, 2, 5_000, Some(300));
        assert!(
            evm.transact_raw(transaction.clone())
                .unwrap()
                .result
                .is_success()
        );
        assert_eq!(evm.fast_transfer_hits(), 0);
        evm.set_inspector_enabled(false);
        assert!(
            evm.transact_raw(transaction.clone())
                .unwrap()
                .result
                .is_success()
        );
        assert_eq!(evm.fast_transfer_hits(), 1);
        evm.set_inspector_enabled(true);
        assert!(evm.transact_raw(transaction).unwrap().result.is_success());
        assert_eq!(evm.fast_transfer_hits(), 1);
    }

    #[test]
    fn mixed_fast_and_contract_transactions_preserve_storage_and_reverts() {
        use revm::{
            bytecode::Bytecode,
            database::{State, states::bundle_state::BundleRetention},
        };
        let contract = address!("5000000000000000000000000000000000000005");
        // SSTORE(0, 1), STOP. Both paths must use the interpreter for this account.
        let code = Bytecode::new_raw(Bytes::from_static(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x00]));
        let mut database = db();
        database.insert_account_info(
            contract,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
        let run = |enabled| {
            let mut state = State::builder()
                .with_database(database.clone())
                .with_bundle_update()
                .build();
            let (results, hits) = {
                let mut evm =
                    N42EvmFactory::with_fast_transfers(enabled).create_evm(&mut state, env());
                let mut results = Vec::new();
                for (offset, to) in [RECIPIENT, contract, EXISTING, contract, RECIPIENT]
                    .into_iter()
                    .enumerate()
                {
                    let mut transaction = tx(to, 1, 2, 5_000, Some(300));
                    transaction.nonce += offset as u64;
                    let out = evm.transact_raw(transaction).unwrap();
                    assert!(out.result.is_success());
                    results.push(out.result);
                    evm.db_mut().commit(out.state);
                }
                (results, evm.fast_transfer_hits())
            };
            state.merge_transitions(BundleRetention::Reverts);
            (results, state.take_bundle(), hits)
        };
        let (fast_results, fast_bundle, hits) = run(true);
        let (slow_results, slow_bundle, slow_hits) = run(false);
        assert_eq!(hits, 3);
        assert_eq!(slow_hits, 0);
        assert_eq!(fast_results, slow_results);
        assert_eq!(fast_bundle, slow_bundle);
        assert!(!fast_bundle.state[&contract].storage.is_empty());
    }

    #[derive(Debug, thiserror::Error)]
    #[error("injected state read failure")]
    struct ReadFailure;

    impl revm::database_interface::DBErrorMarker for ReadFailure {}

    #[derive(Debug)]
    struct FailingDatabase;

    impl revm::Database for FailingDatabase {
        type Error = ReadFailure;
        fn basic(&mut self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ReadFailure)
        }
        fn code_by_hash(
            &mut self,
            _: alloy_primitives::B256,
        ) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Err(ReadFailure)
        }
        fn storage(&mut self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(ReadFailure)
        }
        fn block_hash(&mut self, _: u64) -> Result<alloy_primitives::B256, Self::Error> {
            Err(ReadFailure)
        }
    }

    #[test]
    fn read_errors_are_propagated_instead_of_treated_as_missing_accounts() {
        for enabled in [false, true] {
            let mut evm =
                N42EvmFactory::with_fast_transfers(enabled).create_evm(FailingDatabase, env());
            assert!(matches!(
                evm.transact_raw(tx(EXISTING, 1, 2, 5_000, Some(300))),
                Err(EVMError::Database(ReadFailure))
            ));
            assert_eq!(evm.fast_transfer_hits(), 0);
        }
    }
}
