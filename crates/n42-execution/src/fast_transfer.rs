// Copyright (c) 2017-2025 N42 Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The plain value transfer as an evm2 transaction handler.
//!
//! Lifted from n42-rs `crates/n42/engine-types/src/fast_handler.rs` (with the
//! EIP-7708 log helper of its `direct_transfer.rs`).
//!
//! evm2 already completes a call to an account without code without running
//! the interpreter (`Evm::execute_call_message`), so what is left to save is the
//! first frame around it: the code load, the message, two state checkpoints and
//! the dispatch. This handler wraps the stock Ethereum handlers for types 0, 1 and
//! 2 (0x50 executes as 2): the stock `prepare` validates the transaction, bumps
//! the nonce and charges the upfront fee exactly as before; then, for a
//! transaction that qualifies, the value moves through evm2's own
//! `State::transfer` and the stock settlement (`settle_initial_frame_gas`, the
//! default hooks' `settle_transaction`) refunds the sender and pays the
//! beneficiary. Anything that does not qualify goes to the stock `execute`.
//!
//! What qualifies: a call with no input and no access list, on Cancun, Prague,
//! Osaka or Amsterdam (EIP-2780's intrinsic, EIP-8037's new-account state gas and
//! EIP-7708's transfer log are applied as the stock path applies them), with no
//! inspector, to an account without code that is not a precompile, where the
//! sender, the recipient and the beneficiary are three accounts, the recipient
//! is not left empty by a zero-value call (EIP-161), and the sender can pay the
//! value after the upfront charge.
//!
//! Every write goes through the EVM's own state, so the transaction's change
//! stream — and with it the block's bundle, the touched accounts and the
//! restored-slot record — is what the stock handler produces.
//!
//! `N42_FAST_TRANSFER=1` turns it on; it is off by default so that a fleet
//! can measure it against the stock handlers on the same binary.

use alloy_consensus::{TxEip1559, TxEip2930, TxLegacy};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use evm2::{
    ethereum::{self, TxEnvelope},
    handler::{DefaultTxHandlerHooks, GasSettlement, TxHandlerHooks},
    interpreter::{GasTracker, InstrStop, MessageResultExt},
    registry::{handler, HandlerResult, TxRegistry, TxRequest},
    Evm, EvmTypes, SpecId, TxResult,
};

/// Transfers this handler completed, since start.
static HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Transactions it handed to the stock `execute`, since start.
static DELEGATED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Transfers the handler completed, since start.
pub fn hits() -> u64 {
    HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Transactions the handler gave to the stock `execute`, since start.
pub fn delegated() -> u64 {
    DELEGATED.load(std::sync::atomic::Ordering::Relaxed)
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

#[cfg(test)]
std::thread_local! {
    /// Transfers the handler completed on this thread: the tests run beside
    /// each other, and the process-wide counter is all of theirs.
    static TAKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Whether the handler is registered on `spec`: Cancun, Prague, Osaka and
/// Amsterdam (the EIP-2780 intrinsic, the EIP-8037 new-account state gas and
/// the EIP-7708 transfer log are modelled). n42-rs registers it from Prague;
/// Cancun is this chain's default fork and the one the revm-era fast transfer
/// started at, and the handler reads nothing Prague added.
pub fn covers(spec: SpecId) -> bool {
    spec.enables(SpecId::CANCUN) && SpecId::AMSTERDAM.enables(spec)
}

/// Whether `version`'s Amsterdam transfer rules are all on or all off: the
/// fast paths model EIP-2780, EIP-8037 and EIP-7708 together and nothing in
/// between.
pub fn amsterdam_features_consistent(version: &evm2::Version) -> bool {
    use evm2::EvmFeatures;
    let eip2780 = version.feature(EvmFeatures::EIP2780);
    eip2780 == version.feature(EvmFeatures::EIP8037) && eip2780 == version.feature(EvmFeatures::EIP7708)
}

/// `base` (the stock registry for `spec`) with types 0, 1 and 2 handled by
/// this module when [`covers`] says so; `base` unchanged otherwise.
pub fn registry<T: EvmTypes<Tx = TxEnvelope>>(spec: SpecId, base: TxRegistry<T, TxResult<T>>) -> TxRegistry<T, TxResult<T>> {
    if !covers(spec) {
        return base;
    }
    // A registry is made with each EVM: the counts so far are published here
    // rather than once per transfer.
    metrics::counter!("n42_fast_transfer_total").absolute(hits());
    metrics::counter!("n42_fast_transfer_fallback_total", "reason" => "delegated")
        .absolute(delegated());
    base.with_handler(
        0,
        TxEnvelope::as_legacy,
        handler(
            |req: &mut TxRequest<'_, '_, T, TxLegacy>| {
                let tx = req.tx.inner();
                let shape = Shape::of(req.tx.signer(), U256::from(tx.gas_price), tx.gas_limit, tx.to, &tx.input, tx.value, true);
                Ok((ethereum::legacy::prepare_with_hooks::<T, DefaultTxHandlerHooks>(req)?, shape))
            },
            |req: TxRequest<'_, '_, T, TxLegacy>, (prepared, shape)| match transfer(req.host, req.envelope, &shape)? {
                Some(result) => Ok(result),
                None => ethereum::legacy::execute_prepared::<T, DefaultTxHandlerHooks>(req, prepared),
            },
        ),
    )
    .with_handler(
        1,
        TxEnvelope::as_eip2930,
        handler(
            |req: &mut TxRequest<'_, '_, T, TxEip2930>| {
                let tx = req.tx.inner();
                let shape = Shape::of(
                    req.tx.signer(),
                    U256::from(tx.gas_price),
                    tx.gas_limit,
                    tx.to,
                    &tx.input,
                    tx.value,
                    tx.access_list.is_empty(),
                );
                Ok((ethereum::eip2930::prepare_with_hooks::<T, DefaultTxHandlerHooks>(req)?, shape))
            },
            |req: TxRequest<'_, '_, T, TxEip2930>, (prepared, shape)| match transfer(req.host, req.envelope, &shape)? {
                Some(result) => Ok(result),
                None => ethereum::eip2930::execute_prepared::<T, DefaultTxHandlerHooks>(req, prepared),
            },
        ),
    )
    .with_handler(
        2,
        TxEnvelope::as_eip1559,
        handler(
            |req: &mut TxRequest<'_, '_, T, TxEip1559>| {
                let tx = req.tx.inner();
                let gas_price = ethereum::effective_gas_price(
                    U256::from(tx.max_fee_per_gas),
                    U256::from(tx.max_priority_fee_per_gas),
                    req.host.block().basefee,
                );
                let shape =
                    Shape::of(req.tx.signer(), gas_price, tx.gas_limit, tx.to, &tx.input, tx.value, tx.access_list.is_empty());
                Ok((ethereum::eip1559::prepare_with_hooks::<T, DefaultTxHandlerHooks>(req)?, shape))
            },
            |req: TxRequest<'_, '_, T, TxEip1559>, (prepared, shape)| match transfer(req.host, req.envelope, &shape)? {
                Some(result) => Ok(result),
                None => ethereum::eip1559::execute_prepared::<T, DefaultTxHandlerHooks>(req, prepared),
            },
        ),
    )
}

/// What the qualification and the settlement read of a transaction, taken
/// before the stock `prepare` consumes the request.
#[derive(Debug, Clone, Copy)]
struct Shape {
    caller: Address,
    gas_price: U256,
    gas_limit: u64,
    /// The recipient of a call with no input and no access list; `None`
    /// for anything else.
    to: Option<Address>,
    value: U256,
}

impl Shape {
    fn of(caller: Address, gas_price: U256, gas_limit: u64, to: TxKind, input: &Bytes, value: U256, no_access_list: bool) -> Self {
        let to = match to {
            TxKind::Call(to) if input.is_empty() && no_access_list => Some(to),
            _ => None,
        };
        Self { caller, gas_price, gas_limit, to, value }
    }
}

/// The transfer after the stock `prepare`, or `None` (nothing written) when
/// the transaction does not qualify and the stock `execute` must run it.
fn transfer<T: EvmTypes>(host: &mut Evm<'_, T>, envelope: &T::Tx, shape: &Shape) -> HandlerResult<Option<TxResult<T>>> {
    let delegate = || {
        DELEGATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(None)
    };
    let Some(to) = shape.to else { return delegate() };
    let caller = shape.caller;
    let beneficiary = host.block().beneficiary;
    if !covers(host.spec_id())
        || !amsterdam_features_consistent(host.version())
        || host.inspector().is_some()
        || to == caller
        || to == beneficiary
        || caller == beneficiary
        || host.precompiles().contains(&to)
    {
        return delegate();
    }
    let recipient_empty = {
        let state = host.state_mut();
        let (recipient_empty, recipient_codeless) = {
            let recipient = state.account(&to)?;
            (recipient.get().is_none_or(evm2::AccountInfo::is_empty), recipient.is_empty_code_hash())
        };
        if !recipient_codeless || (shape.value.is_zero() && recipient_empty) {
            return delegate();
        }
        if state.account(&caller)?.balance() < shape.value {
            return delegate();
        }
        recipient_empty
    };
    // The intrinsic gas of a call with no input and no access list, as the
    // stock `prepare` computed it (no hooks: no adjustment, no state gas;
    // EIP-2780's decomposed base on Amsterdam).
    let (intrinsic, floor_gas) =
        ethereum::intrinsic_and_floor_gas(host.version(), caller, TxKind::Call(to), &Bytes::new(), 0, 0, shape.value);
    let (execution_gas_limit, reservoir) = ethereum::initial_gas_and_reservoir(host.version(), shape.gas_limit, intrinsic, 0);
    let mut tx_gas = GasTracker::new_with_execution_gas_and_reservoir(execution_gas_limit, reservoir);
    // EIP-2780's runtime phase (`prepare_initial_frame`): a value to an empty
    // recipient pays EIP-8037's new-account state gas, reservoir first. When
    // it does not fit the stock path makes the out-of-gas halt; nothing has
    // been written yet.
    let mut charged_state_gas = 0;
    if host.version().feature(evm2::EvmFeatures::EIP2780) && !shape.value.is_zero() && recipient_empty {
        let charge = host.version().gas_params.new_account_state_gas();
        if tx_gas.spend_state(charge).is_err() {
            return delegate();
        }
        charged_state_gas = charge;
    }
    if !host.state_mut().transfer(&caller, &to, &shape.value)? {
        // Unreachable: the balance was checked above.
        return delegate();
    }
    // EIP-7708: the transfer's log, as `execute_call_message` emits it.
    if host.version().feature(evm2::EvmFeatures::EIP7708)
        && host.logs_observed()
        && let Some(log) = eip7708_transfer_log(caller, to, shape.value)
    {
        host.log(log);
    }
    // What `execute_call_message` returns for an account without code: the
    // frame starts with what the runtime phase left.
    let mut result = MessageResultExt {
        stop: InstrStop::Stop,
        gas: GasTracker::new_with_execution_gas_and_reservoir(tx_gas.remaining(), tx_gas.reservoir()),
        ..MessageResultExt::default()
    };
    ethereum::settle_initial_frame_gas(&mut tx_gas, &mut result, charged_state_gas);
    let settled = <DefaultTxHandlerHooks as TxHandlerHooks<T>>::settle_transaction(
        host,
        envelope,
        GasSettlement {
            caller,
            gas_price: shape.gas_price,
            gas_limit: shape.gas_limit,
            floor_gas,
            initial_state_gas: 0,
            state_refund: 0,
            result,
        },
    )?;
    HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(test)]
    TAKEN.with(|taken| taken.set(taken.get() + 1));
    Ok(Some(settled))
}

/// EIP-7708's log for `value` moved from `from` to `to` (evm2's
/// `eip7708_transfer_log`): none for a zero value or a self-transfer.
pub fn eip7708_transfer_log(from: Address, to: Address, value: U256) -> Option<alloy_primitives::Log> {
    if value.is_zero() || from == to {
        return None;
    }
    let topics = vec![
        evm2::constants::EIP7708_TRANSFER_TOPIC,
        alloy_primitives::B256::left_padding_from(from.as_slice()),
        alloy_primitives::B256::left_padding_from(to.as_slice()),
    ];
    Some(alloy_primitives::Log {
        address: evm2::evm::SYSTEM_ADDRESS,
        data: alloy_primitives::LogData::new_unchecked(topics, Bytes::copy_from_slice(&value.to_be_bytes::<32>())),
    })
}


#[cfg(test)]
mod tests {
    //! The handler against the stock handlers: the same result, the same
    //! change stream, the same block.
    use super::*;
    use crate::{N42EvmConfig, N42EvmFactory};
    use alloy_consensus::{BlockBody, Header};
    use alloy_eips::eip2930::{AccessList, AccessListItem};
    use alloy_primitives::{B256, Signature};
    use evm2::{
        bytecode::Bytecode,
        evm::{AccountInfo, CacheDB, EmptyDB},
    };
    use reth_chainspec::{ChainSpec, ChainSpecBuilder};
    use reth_evm::{ConfigureEvm, EvmEnv as _, EvmEnvFor};
    use reth_primitives_traits::Recovered;
    use std::sync::Arc;

    const FUND: u64 = 1_000_000_000_000_000_000;
    const BENEFICIARY: u64 = 0xbe;
    const CONTRACT: u64 = 0xc0de;
    const FRESH: u64 = 0xf00d;
    const BASEFEE: u64 = 7;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fork {
        Shanghai,
        Cancun,
        Prague,
        Osaka,
        Amsterdam,
    }

    fn addr(i: u64) -> Address {
        Address::left_padding_from(&(0x1_0000 + i).to_be_bytes())
    }

    fn chain(fork: Fork) -> Arc<ChainSpec> {
        let builder = ChainSpecBuilder::mainnet();
        Arc::new(
            match fork {
                Fork::Shanghai => builder.shanghai_activated(),
                Fork::Cancun => builder.cancun_activated(),
                Fork::Prague => builder.prague_activated(),
                Fork::Osaka => builder.osaka_activated(),
                Fork::Amsterdam => builder.amsterdam_activated(),
            }
            .build(),
        )
    }

    fn config(fork: Fork, fast: bool) -> N42EvmConfig {
        N42EvmConfig::with_evm_factory(chain(fork), N42EvmFactory::with_fast_transfers(fast))
    }

    fn parent() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for i in 1..=8 {
            db.insert_account_info(&addr(i), AccountInfo::empty().with_balance(U256::from(FUND)));
        }
        // A poor sender: the fee, not the value.
        db.insert_account_info(
            &addr(9),
            AccountInfo::empty().with_balance(U256::from(21_000u64 * 100)),
        );
        db.insert_account_info(
            &addr(BENEFICIARY),
            AccountInfo::empty().with_balance(U256::from(1u64)),
        );
        // PUSH1 1 PUSH1 0 SSTORE STOP: writes storage when called.
        let code = Bytes::from_static(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x00]);
        db.insert_account_info(
            &addr(CONTRACT),
            AccountInfo::new(
                U256::ZERO,
                1,
                alloy_primitives::keccak256(&code),
                Some(Bytecode::new_raw(code)),
            ),
        );
        db
    }

    fn header() -> Header {
        Header {
            number: 5,
            timestamp: 1_000,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(BASEFEE),
            beneficiary: addr(BENEFICIARY),
            mix_hash: B256::repeat_byte(0x67),
            excess_blob_gas: Some(0),
            blob_gas_used: Some(0),
            parent_beacon_block_root: Some(B256::ZERO),
            ..Default::default()
        }
    }

    fn env(config: &N42EvmConfig) -> EvmEnvFor<N42EvmConfig> {
        config.evm_env(&header()).expect("env")
    }

    type Tx = Recovered<TxEnvelope>;
    type Env = EvmEnvFor<N42EvmConfig>;
    type Relax = fn(Env) -> Env;

    fn eip1559(from: u64, nonce: u64, to: Address, value: u64, gas_limit: u64, input: Bytes) -> Tx {
        let tx = TxEip1559 {
            chain_id: 1,
            nonce,
            gas_limit,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 3,
            to: TxKind::Call(to),
            value: U256::from(value),
            input,
            ..Default::default()
        };
        Recovered::new_unchecked(TxEnvelope::Eip1559(tx), addr(from))
    }

    fn transfer(from: u64, to: Address, value: u64) -> Tx {
        eip1559(from, 0, to, value, 21_000, Bytes::new())
    }

    /// [`transfer`] with `change` applied to the transaction.
    fn changed(from: u64, change: impl FnOnce(&mut TxEip1559)) -> Tx {
        let mut tx = transfer(from, addr(2), 1);
        let TxEnvelope::Eip1559(inner) = tx.inner_mut() else {
            unreachable!()
        };
        change(inner);
        tx
    }

    fn legacy(from: u64, to: Address, value: u64, gas_price: u128) -> Tx {
        let tx = TxLegacy {
            chain_id: Some(1),
            nonce: 0,
            gas_price,
            gas_limit: 21_000,
            to: TxKind::Call(to),
            value: U256::from(value),
            input: Bytes::new(),
        };
        Recovered::new_unchecked(TxEnvelope::Legacy(tx), addr(from))
    }

    fn eip2930(from: u64, to: Address, value: u64, access_list: AccessList) -> Tx {
        let tx = TxEip2930 {
            chain_id: 1,
            nonce: 0,
            gas_price: 50,
            gas_limit: 40_000,
            to: TxKind::Call(to),
            value: U256::from(value),
            access_list,
            input: Bytes::new(),
        };
        Recovered::new_unchecked(TxEnvelope::Eip2930(tx), addr(from))
    }

    /// The transaction's outcome on one EVM over `db`: the result (or the
    /// error), the change stream, each account's changes in address order,
    /// as text, and whether the handler completed it.
    fn outcome_in(
        config: &N42EvmConfig,
        db: CacheDB<EmptyDB>,
        env: EvmEnvFor<N42EvmConfig>,
        tx: &Tx,
    ) -> ((String, Vec<String>), bool) {
        let before = TAKEN.with(std::cell::Cell::get);
        let mut evm = config.evm_with_env(db, env);
        let outcome = match reth_evm::Evm::transact(&mut evm, tx) {
            Ok(out) => {
                let mut changes: Vec<String> = out
                    .pending_state
                    .changes()
                    .account_changes()
                    .map(|c| format!("{c:?}"))
                    .collect();
                changes.sort();
                (format!("{:?}", out.result), changes)
            }
            Err(err) => (format!("error: {err}"), Vec::new()),
        };
        (outcome, TAKEN.with(std::cell::Cell::get) > before)
    }

    fn outcome(fork: Fork, fast: bool, tx: &Tx) -> ((String, Vec<String>), bool) {
        let config = config(fork, fast);
        let env = env(&config);
        outcome_in(&config, parent(), env, tx)
    }

    /// Each transaction on the stock handlers and on the fast one: the same
    /// result (gas, refund, status, output, logs) or the same error, and the
    /// same change stream (balances, nonces, touched and created accounts);
    /// `fast` says whether the handler takes it rather than handing it on.
    fn check(forks: &[Fork], cases: &[(&str, Tx, bool)]) {
        for fork in forks {
            for (name, tx, fast) in cases {
                let (stock, stock_taken) = outcome(*fork, false, tx);
                let (ours, taken) = outcome(*fork, true, tx);
                assert_eq!(ours, stock, "{name} ({fork:?})");
                assert!(!stock_taken);
                assert_eq!(taken, *fast, "the path of {name} ({fork:?})");
            }
        }
    }

    const BEFORE_AMSTERDAM: [Fork; 3] = [Fork::Cancun, Fork::Prague, Fork::Osaka];

    #[test]
    fn qualifying_transfers_equal_the_stock_handler() {
        check(
            &BEFORE_AMSTERDAM,
            &[
                ("1559 to an existing account", transfer(1, addr(2), 100), true),
                ("1559 creating its recipient", transfer(1, addr(FRESH), 100), true),
                (
                    "1559 over the intrinsic gas",
                    eip1559(1, 0, addr(2), 100, 30_000, Bytes::new()),
                    true,
                ),
                (
                    "1559 with its tip capped by the fee cap",
                    changed(1, |tx| tx.max_fee_per_gas = 8),
                    true,
                ),
                ("legacy", legacy(2, addr(3), 7, 50), true),
                (
                    "2930 with an empty access list",
                    eip2930(3, addr(4), 9, AccessList::default()),
                    true,
                ),
                ("zero value to an existing account", transfer(4, addr(5), 0), true),
                (
                    "legacy at the base fee: a zero tip to the beneficiary",
                    legacy(2, addr(3), 7, BASEFEE as u128),
                    true,
                ),
            ],
        );
    }

    #[test]
    fn everything_else_is_handed_to_the_stock_handler_unchanged() {
        let access_list = AccessList(vec![AccessListItem {
            address: addr(6),
            storage_keys: vec![B256::ZERO],
        }]);
        let randomness = Address::from(crate::precompile_random::RANDOMNESS_ADDRESS);
        check(
            &BEFORE_AMSTERDAM,
            &[
                ("zero value to an empty account", transfer(1, addr(FRESH), 0), false),
                (
                    "to a contract",
                    eip1559(1, 0, addr(CONTRACT), 1, 100_000, Bytes::new()),
                    false,
                ),
                (
                    "to a precompile",
                    eip1559(1, 0, Address::with_last_byte(4), 1, 30_000, Bytes::new()),
                    false,
                ),
                (
                    "to the randomness precompile",
                    eip1559(1, 0, randomness, 1, 30_000, Bytes::new()),
                    false,
                ),
                ("to itself", transfer(2, addr(2), 5), false),
                ("to the beneficiary", transfer(3, addr(BENEFICIARY), 5), false),
                (
                    "with input",
                    eip1559(4, 0, addr(5), 1, 30_000, Bytes::from_static(&[1, 2])),
                    false,
                ),
                ("2930 with an access list", eip2930(5, addr(6), 1, access_list), false),
                ("value the sender lacks after the fee", transfer(9, addr(2), 1), false),
            ],
        );
    }

    #[test]
    fn invalid_transactions_keep_the_stock_errors() {
        let cases = [
            ("a nonce ahead", changed(6, |tx| tx.nonce = 3)),
            ("a nonce at its maximum", changed(6, |tx| tx.nonce = u64::MAX)),
            ("another chain", changed(6, |tx| tx.chain_id = 2)),
            ("an intrinsic gas too low", changed(7, |tx| tx.gas_limit = 20_999)),
            ("a gas limit over the block's", changed(7, |tx| tx.gas_limit = 30_000_001)),
            ("a gas limit of u64::MAX", changed(7, |tx| tx.gas_limit = u64::MAX)),
            ("a fee cap under the base fee", changed(7, |tx| tx.max_fee_per_gas = 6)),
            (
                "a priority fee over the fee cap",
                changed(7, |tx| tx.max_priority_fee_per_gas = 101),
            ),
            ("a value of U256::MAX", changed(7, |tx| tx.value = U256::MAX)),
            ("a sender without funds", transfer(FRESH, addr(2), 1)),
        ];
        for fork in BEFORE_AMSTERDAM {
            for (name, tx) in &cases {
                let (stock, _) = outcome(fork, false, tx);
                let (ours, taken) = outcome(fork, true, tx);
                assert!(stock.0.starts_with("error: "), "{name}: {}", stock.0);
                assert_eq!(ours, stock, "{name} ({fork:?})");
                assert!(!taken, "{name} ({fork:?})");
            }
        }
    }

    /// Accounts and environments at their edges: whichever path takes the
    /// transaction, the outcome is the stock one.
    #[test]
    fn account_and_environment_edges_equal_the_stock_handler() {
        let tx = transfer(1, addr(2), 1);
        let at_basefee = legacy(1, addr(2), 1, BASEFEE as u128);
        let rich = AccountInfo::empty().with_balance(U256::MAX);
        let mut sender_at_last_nonce = AccountInfo::empty().with_balance(U256::from(FUND));
        sender_at_last_nonce.nonce = u64::MAX - 1;
        let accounts: [(&str, Address, Option<AccountInfo>, &Tx); 5] = [
            ("a recipient at the largest balance", addr(2), Some(rich.clone()), &tx),
            ("a beneficiary at the largest balance", addr(BENEFICIARY), Some(rich), &tx),
            ("an empty beneficiary paid a tip", addr(BENEFICIARY), Some(AccountInfo::empty()), &tx),
            (
                "an empty beneficiary paid no tip",
                addr(BENEFICIARY),
                Some(AccountInfo::empty()),
                &at_basefee,
            ),
            ("a sender one nonce from the end", addr(1), Some(sender_at_last_nonce), &tx),
        ];
        for fork in BEFORE_AMSTERDAM {
            for (name, address, info, tx) in &accounts {
                let run = |fast| {
                    let config = config(fork, fast);
                    let mut db = parent();
                    if let Some(info) = info {
                        db.insert_account_info(address, info.clone());
                    }
                    let env = env(&config);
                    outcome_in(&config, db, env, tx).0
                };
                assert_eq!(run(true), run(false), "{name} ({fork:?})");
            }
            let relaxed: [(&str, Relax); 2] = [
                ("no nonce check", |env| env.with_nonce_check_disabled()),
                ("no balance check", |env| env.with_balance_check_disabled()),
            ];
            for (name, relax) in relaxed {
                for tx in [&tx, &changed(6, |tx| tx.nonce = 3), &transfer(9, addr(2), 1)] {
                    let run = |fast| {
                        let config = config(fork, fast);
                        let env = relax(env(&config));
                        outcome_in(&config, parent(), env, tx).0
                    };
                    assert_eq!(run(true), run(false), "{name} ({fork:?})");
                }
            }
            // The sender is the beneficiary.
            let run = |fast| {
                let config = config(fork, fast);
                let mut db = parent();
                db.insert_account_info(
                    &addr(BENEFICIARY),
                    AccountInfo::empty().with_balance(U256::from(FUND)),
                );
                let env = env(&config);
                outcome_in(&config, db, env, &transfer(BENEFICIARY, addr(2), 1))
            };
            let (ours, taken) = run(true);
            assert_eq!(ours, run(false).0, "the beneficiary sends ({fork:?})");
            assert!(!taken);
        }
    }

    #[test]
    fn the_handler_covers_cancun_to_amsterdam() {
        assert!(covers(SpecId::CANCUN));
        assert!(covers(SpecId::PRAGUE));
        assert!(covers(SpecId::OSAKA));
        assert!(covers(SpecId::AMSTERDAM));
        assert!(!covers(SpecId::SHANGHAI));
        let mut partial = evm2::Version::new(SpecId::AMSTERDAM);
        assert!(amsterdam_features_consistent(&partial));
        partial.features.remove(evm2::EvmFeatures::EIP7708);
        assert!(
            !amsterdam_features_consistent(&partial),
            "only some of the Amsterdam transfer rules"
        );
        assert!(amsterdam_features_consistent(&evm2::Version::new(SpecId::OSAKA)));
        // Not registered before Cancun: the stock handler runs the transfer.
        check(
            &[Fork::Shanghai],
            &[("1559 to an existing account", transfer(1, addr(2), 100), false)],
        );
    }

    #[test]
    fn qualifying_transfers_equal_the_stock_amsterdam_handler() {
        let config = config(Fork::Amsterdam, false);
        assert_eq!(env(&config).spec_id(), SpecId::AMSTERDAM);
        let creating = eip1559(1, 0, addr(FRESH), 100, 210_000, Bytes::new());
        check(
            &[Fork::Amsterdam],
            &[
                ("1559 to an existing account", transfer(1, addr(2), 100), true),
                ("1559 creating its recipient (EIP-8037)", creating.clone(), true),
                (
                    "1559 creating its recipient, the charge to the gas limit",
                    eip1559(1, 0, addr(FRESH), 100, 204_600, Bytes::new()),
                    true,
                ),
                (
                    "1559 over the intrinsic gas",
                    eip1559(1, 0, addr(2), 100, 30_000, Bytes::new()),
                    true,
                ),
                ("legacy", legacy(2, addr(3), 7, 50), true),
                (
                    "2930 with an empty access list",
                    eip2930(3, addr(4), 9, AccessList::default()),
                    true,
                ),
                ("zero value to an existing account (15,000)", transfer(4, addr(5), 0), true),
                (
                    "zero value at its 15,000 intrinsic",
                    eip1559(4, 0, addr(5), 0, 15_000, Bytes::new()),
                    true,
                ),
            ],
        );
        // The stock outcome carries what Prague's does not.
        let ((result, _), _) = outcome(Fork::Amsterdam, false, &creating);
        assert!(
            result.contains("state_gas_spent: 183600")
                && result.contains("total_gas_spent: 204600"),
            "{result}"
        );
        assert!(result.contains("ddf252ad"), "the EIP-7708 log: {result}");
    }

    #[test]
    fn everything_else_is_handed_to_the_stock_amsterdam_handler_unchanged() {
        let access_list = AccessList(vec![AccessListItem {
            address: addr(6),
            storage_keys: vec![B256::ZERO],
        }]);
        check(
            &[Fork::Amsterdam],
            &[
                (
                    "creating its recipient at 21,000 (a runtime out-of-gas halt)",
                    transfer(1, addr(FRESH), 100),
                    false,
                ),
                (
                    "creating its recipient one gas short",
                    eip1559(1, 0, addr(FRESH), 100, 204_599, Bytes::new()),
                    false,
                ),
                ("zero value to an empty account", transfer(1, addr(FRESH), 0), false),
                (
                    "to a contract",
                    eip1559(1, 0, addr(CONTRACT), 1, 200_000, Bytes::new()),
                    false,
                ),
                (
                    "to a precompile",
                    eip1559(1, 0, Address::with_last_byte(4), 1, 30_000, Bytes::new()),
                    false,
                ),
                ("to itself", transfer(2, addr(2), 5), false),
                ("to the beneficiary", transfer(3, addr(BENEFICIARY), 5), false),
                (
                    "with input",
                    eip1559(4, 0, addr(5), 1, 30_000, Bytes::from_static(&[1, 2])),
                    false,
                ),
                ("2930 with an access list", eip2930(5, addr(6), 1, access_list), false),
                ("value the sender lacks after the fee", transfer(9, addr(2), 1), false),
                ("a nonce ahead", eip1559(6, 3, addr(2), 1, 21_000, Bytes::new()), false),
                (
                    "an intrinsic gas too low",
                    eip1559(7, 0, addr(2), 1, 20_000, Bytes::new()),
                    false,
                ),
                (
                    "zero value under its intrinsic",
                    eip1559(7, 0, addr(2), 0, 14_999, Bytes::new()),
                    false,
                ),
            ],
        );
    }

    #[derive(Debug, thiserror::Error)]
    #[error("injected state read failure")]
    struct ReadFailure;

    #[derive(Debug)]
    struct FailingDatabase;

    impl evm2::evm::Database for FailingDatabase {
        type Error = ReadFailure;
        fn get_account(&mut self, _: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ReadFailure)
        }
        fn get_code_by_hash(&mut self, _: &B256) -> Result<Bytecode, Self::Error> {
            Err(ReadFailure)
        }
        fn get_storage(&mut self, _: &Address, _: &U256) -> Result<U256, Self::Error> {
            Err(ReadFailure)
        }
        fn get_block_hash(&mut self, _: &U256) -> Result<B256, Self::Error> {
            Err(ReadFailure)
        }
    }

    #[test]
    fn read_errors_are_propagated_instead_of_treated_as_missing_accounts() {
        let run = |fast| {
            let config = config(Fork::Prague, fast);
            let mut evm = config.evm_with_env(FailingDatabase, env(&config));
            let error = reth_evm::Evm::transact(&mut evm, &transfer(1, addr(2), 1))
                .map(|_| ())
                .expect_err("the read fails");
            format!("{error}")
        };
        let before = TAKEN.with(std::cell::Cell::get);
        let ours = run(true);
        assert!(ours.contains("injected state read failure"), "{ours}");
        assert_eq!(ours, run(false));
        assert_eq!(TAKEN.with(std::cell::Cell::get), before);
    }

    /// One block of transfers and contract calls through the block executor
    /// on both registries: the same receipts, gas and bundle — statuses,
    /// reverts, the touched and unchanged account, the contract's storage —
    /// and the same restored-slot record.
    #[test]
    fn a_mixed_block_equals_the_stock_block() {
        use alloy_consensus::transaction::TxHashRef;
        use n42_tx_types::{Block, N42TxEnvelope};
        use reth_ethereum_primitives::{Transaction, TransactionSigned};
        use reth_evm::execute::{BasicBlockExecutor, Executor};
        use reth_primitives_traits::RecoveredBlock;

        let targets = [
            (addr(FRESH), 5u64),
            (addr(CONTRACT), 1),
            (addr(2), 1),
            (addr(CONTRACT), 0),
            (addr(FRESH), 5),
            // Touched and left as it was.
            (addr(3), 0),
        ];
        for fork in BEFORE_AMSTERDAM {
            let run = |fast: bool| {
                let transactions: Vec<N42TxEnvelope> = targets
                    .iter()
                    .enumerate()
                    .map(|(nonce, (to, value))| {
                        TransactionSigned::new_unhashed(
                            Transaction::Eip1559(TxEip1559 {
                                chain_id: 1,
                                nonce: nonce as u64,
                                gas_limit: 100_000,
                                max_fee_per_gas: 100,
                                max_priority_fee_per_gas: 3,
                                to: TxKind::Call(*to),
                                value: U256::from(*value),
                                ..Default::default()
                            }),
                            Signature::test_signature(),
                        )
                        .into()
                    })
                    .collect();
                let hashes: Vec<B256> = transactions.iter().map(|tx| *tx.tx_hash()).collect();
                let senders = vec![addr(1); transactions.len()];
                let block = RecoveredBlock::new_unhashed(
                    Block {
                        header: Header {
                            // Distinct per fork: the restored-slot registry is process-wide.
                            parent_hash: B256::repeat_byte(0xF0 + fork as u8),
                            withdrawals_root: Some(alloy_consensus::EMPTY_ROOT_HASH),
                            ..header()
                        },
                        body: BlockBody {
                            transactions,
                            withdrawals: Some(Default::default()),
                            ..Default::default()
                        },
                    },
                    senders,
                );
                let config = config(fork, fast);
                let env_key =
                    crate::restored_slots_env_key(&config.evm_env(block.header()).unwrap().block);
                let before = TAKEN.with(std::cell::Cell::get);
                let mut output = BasicBlockExecutor::new(config, parent())
                    .execute(&block)
                    .expect("block executes");
                let taken = TAKEN.with(std::cell::Cell::get) - before;
                for reverts in output.state.reverts.iter_mut() {
                    reverts.sort_unstable_by_key(|(address, _)| *address);
                }
                let context = crate::restored_slots_execution_key(
                    block.header().parent_hash,
                    env_key,
                    block.header().parent_beacon_block_root,
                    Some(&[]),
                    [],
                );
                let restored =
                    crate::restored_slots_for(crate::restored_slots_key(context, hashes)).unwrap();
                (output, restored, taken)
            };
            let (fast, fast_restored, taken) = run(true);
            let (stock, stock_restored, stock_taken) = run(false);
            assert_eq!(taken, 4, "{fork:?}");
            assert_eq!(stock_taken, 0);
            assert!(fast.receipts.iter().all(|receipt| receipt.success));
            assert_eq!(fast.receipts, stock.receipts, "{fork:?}");
            assert_eq!(fast.gas_used, stock.gas_used, "{fork:?}");
            assert_eq!(fast.state, stock.state, "{fork:?}");
            assert_eq!(fast_restored, stock_restored, "{fork:?}");
            assert!(!fast.state.state[&addr(CONTRACT)].storage.is_empty());
            let touched = &fast.state.state[&addr(3)];
            assert_eq!(touched.info, touched.original_info);
            assert!(!touched.status.is_not_modified());
        }
    }
}
