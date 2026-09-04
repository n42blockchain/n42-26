//! EOF guard for the gov5 header profile.
//!
//! gov5 activates the EVM Object Format (`eofTime`, chain 94: 1765238400,
//! long past) while revm has no EOF implementation. A contract whose code
//! starts with the EIP-3540 magic `0xEF00` would be validated, deployed and
//! run under legacy semantics here and gov5 would disagree on every state the
//! block touches, so the node must never vote for or propose such a block.
//! This module looks for EOF code in the three places it can enter a block —
//! the initcode of a create transaction, the code the block deploys, and the
//! code the block loads for execution — and refuses the block with an
//! unmistakable error and a metric.
//!
//! The import path is guarded by [`EofGuardedStateRootStrategy`], which sits
//! in front of the gov5 state-root jobs so the check runs on every block the
//! engine executes (catch-up ranges, live proposals, the member's own payloads
//! coming back through `newPayload`); the builder path is guarded by
//! [`EofFilteringTransactions`] plus [`check_built_block`] in the payload
//! builder. Both are inert outside the gov5 profile, where reth's own rules
//! apply.
//!
//! EIP-7702 delegation designators start with `0xEF01` and are not EOF.

use alloy_consensus::transaction::Transaction as _;
use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY as KECCAK_EMPTY};
use reth_engine_tree::tree::StateProviderBuilder;
use reth_engine_tree::tree::state_root_strategy::{
    LazyHashedPostState, PreparedStateRootJob, StateRootJob, StateRootJobContext,
    StateRootJobOutcome, StateRootStrategy,
};
use reth_ethereum_primitives::{Block, EthPrimitives, Receipt, TransactionSigned};
use reth_evm::ConfigureEvm;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::RecoveredBlock;
use reth_provider::{ProviderError, ProviderResult};
use reth_storage_api::{
    AccountReader, BlockReader, BytecodeReader, StateProviderFactory, StateReader,
};
use reth_transaction_pool::{
    BestTransactions, PoolTransaction, ValidPoolTransaction,
    error::{InvalidPoolTransactionError, PoolTransactionError},
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// The EIP-3540 container magic.
pub const EOF_MAGIC: [u8; 2] = [0xEF, 0x00];

/// The fixed part of every EOF refusal, so a log grep finds them all.
pub const EOF_UNSUPPORTED: &str = "EOF code is not supported by this node (revm has no EOF)";

/// Whether `code` is an EOF container (or an EOF initcode).
pub fn is_eof_code(code: &[u8]) -> bool {
    code.len() >= 2 && code[..2] == EOF_MAGIC
}

/// Where EOF code was found in a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EofSighting {
    /// A create transaction whose initcode is an EOF container.
    Initcode {
        /// Index of the transaction in the block.
        tx_index: usize,
        /// Hash of the transaction.
        tx_hash: B256,
    },
    /// Code deployed by the block that is an EOF container.
    Deployed {
        /// The account that received the code, when the bundle names one.
        address: Option<Address>,
        /// keccak of the deployed code.
        code_hash: B256,
    },
    /// Code the block loaded for execution that is an EOF container.
    Loaded {
        /// The account whose code was loaded.
        address: Address,
        /// keccak of that code.
        code_hash: B256,
    },
}

impl std::fmt::Display for EofSighting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initcode { tx_index, tx_hash } => {
                write!(f, "EOF initcode in transaction {tx_index} ({tx_hash})")
            }
            Self::Deployed {
                address: Some(address),
                code_hash,
            } => write!(f, "EOF code deployed at {address} (code hash {code_hash})"),
            Self::Deployed {
                address: None,
                code_hash,
            } => write!(f, "EOF code deployed (code hash {code_hash})"),
            Self::Loaded { address, code_hash } => {
                write!(f, "EOF code loaded from {address} (code hash {code_hash})")
            }
        }
    }
}

/// Why a block was refused by the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EofGuardError {
    /// The block carries EOF code.
    Eof {
        /// Block number.
        block_number: u64,
        /// Block hash.
        block_hash: B256,
        /// Where the code was found.
        sighting: EofSighting,
    },
    /// The code of an account could not be read, so the block cannot be
    /// cleared; the guard fails closed.
    Lookup {
        /// Block number.
        block_number: u64,
        /// What failed.
        message: String,
    },
}

impl std::fmt::Display for EofGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof {
                block_number,
                block_hash,
                sighting,
            } => write!(
                f,
                "{EOF_UNSUPPORTED}: block {block_number} ({block_hash}): {sighting}; refusing to vote or propose"
            ),
            Self::Lookup {
                block_number,
                message,
            } => write!(
                f,
                "{EOF_UNSUPPORTED}: block {block_number}: cannot read account code to clear the block: {message}"
            ),
        }
    }
}

impl std::error::Error for EofGuardError {}

/// Reads account code for the guard.
pub trait CodeLookup {
    /// The code stored under `code_hash`, if any.
    fn code_by_hash(&self, code_hash: &B256) -> Result<Option<Bytes>, String>;
    /// The code of `address`, if it has any.
    fn code_of(&self, address: &Address) -> Result<Option<Bytes>, String>;
}

impl<T: AccountReader + BytecodeReader + ?Sized> CodeLookup for T {
    fn code_by_hash(&self, code_hash: &B256) -> Result<Option<Bytes>, String> {
        self.bytecode_by_hash(code_hash)
            .map(|code| code.map(|code| code.original_bytes()))
            .map_err(|error| error.to_string())
    }

    fn code_of(&self, address: &Address) -> Result<Option<Bytes>, String> {
        let Some(account) = self
            .basic_account(address)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        match account.bytecode_hash {
            Some(hash) if hash != KECCAK_EMPTY => self.code_by_hash(&hash),
            _ => Ok(None),
        }
    }
}

static EOF_REJECTIONS: AtomicU64 = AtomicU64::new(0);

/// Blocks refused by the guard since the process started (mirrors the
/// `n42_gov5_eof_blocks_rejected_total` metric for tests and status).
pub fn eof_rejections_total() -> u64 {
    EOF_REJECTIONS.load(Ordering::Relaxed)
}

fn record_rejection(stage: &'static str, error: &EofGuardError) {
    EOF_REJECTIONS.fetch_add(1, Ordering::Relaxed);
    metrics::counter!("n42_gov5_eof_blocks_rejected_total", "stage" => stage).increment(1);
    tracing::error!(target: "n42::eof_guard", stage, %error, "block refused: EOF code");
}

/// The first create transaction whose initcode is an EOF container.
pub fn find_eof_initcode<'a>(
    transactions: impl IntoIterator<Item = &'a TransactionSigned>,
) -> Option<EofSighting> {
    for (tx_index, tx) in transactions.into_iter().enumerate() {
        if tx.kind().is_create() && is_eof_code(tx.input()) {
            return Some(EofSighting::Initcode {
                tx_index,
                tx_hash: *tx.tx_hash(),
            });
        }
    }
    None
}

fn is_code_hash(hash: &B256) -> bool {
    *hash != KECCAK_EMPTY && !hash.is_zero()
}

/// Finds EOF code anywhere the block can carry it: create initcode, code the
/// block deployed (`contracts` and the post-state of touched accounts), and
/// code the block loaded (the pre-state of touched accounts and the targets of
/// its call transactions).
pub fn find_eof_in_block(
    block: &RecoveredBlock<Block>,
    output: &BlockExecutionOutput<Receipt>,
    lookup: &(impl CodeLookup + ?Sized),
) -> Result<Option<EofSighting>, String> {
    if let Some(sighting) = find_eof_initcode(block.body().transactions()) {
        return Ok(Some(sighting));
    }

    for (code_hash, bytecode) in &output.state.contracts {
        if is_eof_code(bytecode.original_byte_slice()) {
            let address = output
                .state
                .state
                .iter()
                .find(|(_, account)| {
                    account
                        .info
                        .as_ref()
                        .is_some_and(|info| info.code_hash == *code_hash)
                })
                .map(|(address, _)| *address);
            return Ok(Some(EofSighting::Deployed {
                address,
                code_hash: *code_hash,
            }));
        }
    }

    for (address, account) in &output.state.state {
        let original_hash = account
            .original_info
            .as_ref()
            .map(|info| info.code_hash)
            .filter(is_code_hash);
        if let Some(info) = &account.info
            && is_code_hash(&info.code_hash)
        {
            let code = match &info.code {
                Some(code) => Some(code.original_bytes()),
                None => lookup.code_by_hash(&info.code_hash)?,
            };
            if code.as_deref().is_some_and(|code| is_eof_code(code)) {
                return Ok(Some(if original_hash == Some(info.code_hash) {
                    EofSighting::Loaded {
                        address: *address,
                        code_hash: info.code_hash,
                    }
                } else {
                    EofSighting::Deployed {
                        address: Some(*address),
                        code_hash: info.code_hash,
                    }
                }));
            }
        }
        if let (Some(info), Some(code_hash)) = (&account.original_info, original_hash) {
            let code = match &info.code {
                Some(code) => Some(code.original_bytes()),
                None => lookup.code_by_hash(&code_hash)?,
            };
            if code.as_deref().is_some_and(|code| is_eof_code(code)) {
                return Ok(Some(EofSighting::Loaded {
                    address: *address,
                    code_hash,
                }));
            }
        }
    }

    for tx in block.body().transactions() {
        let Some(target) = tx.to() else { continue };
        if output.state.state.contains_key(&target) {
            continue;
        }
        if let Some(code) = lookup.code_of(&target)?
            && is_eof_code(&code)
        {
            return Ok(Some(EofSighting::Loaded {
                address: target,
                code_hash: alloy_primitives::keccak256(&code),
            }));
        }
    }

    Ok(None)
}

/// Refuses `block` when it carries EOF code; records the metric and the log
/// line. `stage` labels the metric (`import` or `build`).
pub fn check_block(
    block: &RecoveredBlock<Block>,
    output: &BlockExecutionOutput<Receipt>,
    lookup: &(impl CodeLookup + ?Sized),
    stage: &'static str,
) -> Result<(), EofGuardError> {
    let result = match find_eof_in_block(block, output, lookup) {
        Ok(None) => return Ok(()),
        Ok(Some(sighting)) => Err(EofGuardError::Eof {
            block_number: block.number,
            block_hash: block.hash(),
            sighting,
        }),
        Err(message) => Err(EofGuardError::Lookup {
            block_number: block.number,
            message,
        }),
    };
    if let Err(error) = &result {
        record_rejection(stage, error);
    }
    result
}

/// Refuses a block the payload builder produced when any of its create
/// transactions carries EOF initcode. Deployed and loaded code are checked when
/// the built block comes back through `newPayload`, where its execution output
/// is available.
pub fn check_built_block(block: &RecoveredBlock<Block>) -> Result<(), EofGuardError> {
    if let Some(sighting) = find_eof_initcode(block.body().transactions()) {
        let error = EofGuardError::Eof {
            block_number: block.number,
            block_hash: block.hash(),
            sighting,
        };
        record_rejection("build", &error);
        return Err(error);
    }
    Ok(())
}

/// A [`StateRootStrategy`] that runs the EOF guard on every executed block
/// before handing it to the wrapped strategy. Only for jobs that use no
/// execution hook or update stream (the gov5 jobs); those capabilities cannot
/// be re-attached through a wrapper.
pub struct EofGuardedStateRootStrategy<P, Evm> {
    inner: Arc<dyn StateRootStrategy<EthPrimitives, P, Evm>>,
}

impl<P, Evm> std::fmt::Debug for EofGuardedStateRootStrategy<P, Evm> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EofGuardedStateRootStrategy")
            .finish_non_exhaustive()
    }
}

impl<P, Evm> EofGuardedStateRootStrategy<P, Evm> {
    /// Wraps `inner`.
    pub fn new(inner: Arc<dyn StateRootStrategy<EthPrimitives, P, Evm>>) -> Self {
        Self { inner }
    }
}

impl<P, Evm> StateRootStrategy<EthPrimitives, P, Evm> for EofGuardedStateRootStrategy<P, Evm>
where
    P: BlockReader + StateProviderFactory + StateReader + Clone + Send + Sync + 'static,
    Evm: ConfigureEvm<Primitives = EthPrimitives>,
{
    fn prepare(
        &self,
        ctx: StateRootJobContext<'_, EthPrimitives, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<EthPrimitives>> {
        let provider_builder = ctx.provider_builder();
        let mut inner = self.inner.prepare(ctx)?;
        let hashed_state_rx = inner.take_hashed_state_rx();
        Ok(PreparedStateRootJob::new(
            Box::new(EofGuardedJob {
                inner,
                provider_builder,
            }),
            hashed_state_rx,
        ))
    }
}

struct EofGuardedJob<P> {
    inner: PreparedStateRootJob<EthPrimitives>,
    provider_builder: StateProviderBuilder<EthPrimitives, P>,
}

impl<P> StateRootJob<EthPrimitives> for EofGuardedJob<P>
where
    P: BlockReader + StateProviderFactory + StateReader + Clone + Send + 'static,
{
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn finish(
        &mut self,
        block: &RecoveredBlock<Block>,
        output: Arc<BlockExecutionOutput<Receipt>>,
        hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        let provider = self.provider_builder.build()?;
        check_block(block, &output, &provider, "import").map_err(ProviderError::other)?;
        self.inner.finish(block, output, hashed_state)
    }
}

/// The pool error a skipped EOF initcode transaction is marked invalid with.
#[derive(Debug, thiserror::Error)]
#[error("{EOF_UNSUPPORTED}: create transaction carries EOF initcode")]
pub struct EofInitcodeError;

impl PoolTransactionError for EofInitcodeError {
    fn is_bad_transaction(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A [`BestTransactions`] view that skips create transactions with EOF
/// initcode so the builder never puts one into a block.
pub struct EofFilteringTransactions<T: PoolTransaction> {
    inner: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>,
}

impl<T: PoolTransaction> EofFilteringTransactions<T> {
    /// Wraps `inner`.
    pub fn new(inner: Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>) -> Self {
        Self { inner }
    }
}

impl<T: PoolTransaction> Iterator for EofFilteringTransactions<T> {
    type Item = Arc<ValidPoolTransaction<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let tx = self.inner.next()?;
            if tx.transaction.is_create() && is_eof_code(tx.transaction.input()) {
                metrics::counter!("n42_gov5_eof_transactions_skipped_total").increment(1);
                tracing::warn!(
                    target: "n42::eof_guard",
                    tx_hash = %tx.hash(),
                    sender = %tx.sender(),
                    "{EOF_UNSUPPORTED}: skipping create transaction with EOF initcode"
                );
                self.inner.mark_invalid(
                    &tx,
                    InvalidPoolTransactionError::Other(Box::new(EofInitcodeError)),
                );
                continue;
            }
            return Some(tx);
        }
    }
}

impl<T: PoolTransaction> BestTransactions for EofFilteringTransactions<T> {
    fn mark_invalid(&mut self, tx: &Self::Item, kind: InvalidPoolTransactionError) {
        self.inner.mark_invalid(tx, kind);
    }

    fn no_updates(&mut self) {
        self.inner.no_updates();
    }

    fn set_skip_blobs(&mut self, skip_blobs: bool) {
        self.inner.set_skip_blobs(skip_blobs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Header, TxLegacy};
    use alloy_primitives::{TxKind, U256, keccak256};
    use reth_ethereum_primitives::{BlockBody, Transaction};
    use reth_execution_types::BlockExecutionResult;
    use reth_primitives_traits::crypto::secp256k1::public_key_to_address;
    use reth_testing_utils::generators::sign_tx_with_key_pair;
    use revm::{
        bytecode::Bytecode,
        database::states::{AccountStatus, BundleAccount, BundleState},
        state::AccountInfo,
    };
    use secp256k1::{Keypair, Secp256k1, SecretKey};
    use std::collections::HashMap;

    #[derive(Default)]
    struct MapLookup {
        by_hash: HashMap<B256, Bytes>,
        by_address: HashMap<Address, Bytes>,
    }

    impl MapLookup {
        fn with_account(mut self, address: Address, code: Bytes) -> Self {
            self.by_hash.insert(keccak256(&code), code.clone());
            self.by_address.insert(address, code);
            self
        }
    }

    impl CodeLookup for MapLookup {
        fn code_by_hash(&self, code_hash: &B256) -> Result<Option<Bytes>, String> {
            Ok(self.by_hash.get(code_hash).cloned())
        }

        fn code_of(&self, address: &Address) -> Result<Option<Bytes>, String> {
            Ok(self.by_address.get(address).cloned())
        }
    }

    struct FailingLookup;

    impl CodeLookup for FailingLookup {
        fn code_by_hash(&self, _: &B256) -> Result<Option<Bytes>, String> {
            Err("database closed".into())
        }

        fn code_of(&self, _: &Address) -> Result<Option<Bytes>, String> {
            Err("database closed".into())
        }
    }

    fn keypair() -> Keypair {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&[0x42; 32]).unwrap();
        Keypair::from_secret_key(&secp, &secret)
    }

    fn tx(nonce: u64, to: TxKind, input: Bytes) -> (TransactionSigned, Address) {
        let key = keypair();
        let sender = public_key_to_address(key.public_key());
        let tx = sign_tx_with_key_pair(
            key,
            Transaction::Legacy(TxLegacy {
                chain_id: Some(94),
                nonce,
                gas_price: 7,
                gas_limit: 1_000_000,
                to,
                value: U256::ZERO,
                input,
            }),
        );
        (tx, sender)
    }

    fn block(txs: Vec<(TransactionSigned, Address)>) -> RecoveredBlock<Block> {
        let (transactions, senders): (Vec<_>, Vec<_>) = txs.into_iter().unzip();
        let header = Header {
            number: 13_560_376,
            gas_limit: 30_000_000,
            ..Header::default()
        };
        RecoveredBlock::new_unhashed(
            Block {
                header,
                body: BlockBody {
                    transactions,
                    ..Default::default()
                },
            },
            senders,
        )
    }

    fn output(state: BundleState) -> BlockExecutionOutput<Receipt> {
        BlockExecutionOutput {
            result: BlockExecutionResult::default(),
            state,
        }
    }

    fn eof_code() -> Bytes {
        Bytes::from_static(&[0xEF, 0x00, 0x01, 0x01, 0x00, 0x04, 0x02, 0x00])
    }

    fn legacy_code() -> Bytes {
        Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xF3])
    }

    fn account_with_code(
        original: Option<Bytes>,
        present: Option<Bytes>,
        status: AccountStatus,
    ) -> BundleAccount {
        let info = |code: Bytes| AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            ..Default::default()
        };
        BundleAccount::new(
            original.map(info),
            present.map(info),
            Default::default(),
            status,
        )
    }

    #[test]
    fn eof_magic_is_ef00_only() {
        assert!(is_eof_code(&[0xEF, 0x00]));
        assert!(is_eof_code(&[0xEF, 0x00, 0x01]));
        assert!(!is_eof_code(&[0xEF]));
        assert!(
            !is_eof_code(&[0xEF, 0x01, 0x00]),
            "EIP-7702 designators are not EOF"
        );
        assert!(!is_eof_code(&[0x60, 0x00]));
        assert!(!is_eof_code(&[]));
    }

    #[test]
    fn a_block_with_eof_initcode_is_rejected() {
        let before = eof_rejections_total();
        let (create, sender) = tx(0, TxKind::Create, eof_code());
        let hash = *create.tx_hash();
        let block = block(vec![(create, sender)]);
        let error = check_block(
            &block,
            &output(BundleState::default()),
            &MapLookup::default(),
            "import",
        )
        .unwrap_err();
        assert_eq!(
            error,
            EofGuardError::Eof {
                block_number: 13_560_376,
                block_hash: block.hash(),
                sighting: EofSighting::Initcode {
                    tx_index: 0,
                    tx_hash: hash
                },
            }
        );
        let text = error.to_string();
        assert!(text.contains(EOF_UNSUPPORTED), "{text}");
        assert!(text.contains("block 13560376"), "{text}");
        assert!(text.contains("refusing to vote or propose"), "{text}");
        // Other tests share the process-wide mirror, so only monotonicity is asserted here;
        // `the_metric_counts_every_refusal` checks the exact count on a local recorder.
        assert!(eof_rejections_total() > before, "metric mirror increments");
        assert!(
            check_built_block(&block).is_err(),
            "the builder refuses the same block"
        );
    }

    #[test]
    fn a_block_deploying_eof_runtime_code_is_rejected() {
        let (create, sender) = tx(0, TxKind::Create, legacy_code());
        let block = block(vec![(create, sender)]);
        let deployed = Address::repeat_byte(0xDE);
        let mut state = BundleState::default();
        state
            .contracts
            .insert(keccak256(eof_code()), Bytecode::new_raw(eof_code()));
        state.state.insert(
            deployed,
            account_with_code(None, Some(eof_code()), AccountStatus::InMemoryChange),
        );
        let error =
            check_block(&block, &output(state), &MapLookup::default(), "import").unwrap_err();
        assert!(
            matches!(
                error,
                EofGuardError::Eof {
                    sighting: EofSighting::Deployed {
                        address: Some(address),
                        ..
                    },
                    ..
                } if address == deployed
            ),
            "{error}"
        );

        // The same deployment seen only through the account's post-state.
        let mut state = BundleState::default();
        state.state.insert(
            deployed,
            account_with_code(None, Some(eof_code()), AccountStatus::InMemoryChange),
        );
        let error =
            check_block(&block, &output(state), &MapLookup::default(), "import").unwrap_err();
        assert!(
            matches!(
                error,
                EofGuardError::Eof {
                    sighting: EofSighting::Deployed { .. },
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn a_call_to_a_pre_existing_eof_account_is_rejected() {
        let target = Address::repeat_byte(0xE0);
        let (call, sender) = tx(0, TxKind::Call(target), Bytes::new());
        let block = block(vec![(call, sender)]);

        // Touched: the account is in the bundle with unchanged EOF code.
        let mut state = BundleState::default();
        state.state.insert(
            target,
            account_with_code(Some(eof_code()), Some(eof_code()), AccountStatus::Changed),
        );
        let error =
            check_block(&block, &output(state), &MapLookup::default(), "import").unwrap_err();
        assert!(
            matches!(
                error,
                EofGuardError::Eof {
                    sighting: EofSighting::Loaded { address, .. },
                    ..
                } if address == target
            ),
            "{error}"
        );

        // Not touched: only the call target and the state say so.
        let lookup = MapLookup::default().with_account(target, eof_code());
        let error =
            check_block(&block, &output(BundleState::default()), &lookup, "import").unwrap_err();
        assert!(
            matches!(
                error,
                EofGuardError::Eof {
                    sighting: EofSighting::Loaded { address, .. },
                    ..
                } if address == target
            ),
            "{error}"
        );

        // Touched with the code hash only (no bytes in the bundle): resolved through the lookup.
        let mut state = BundleState::default();
        let mut account =
            account_with_code(Some(eof_code()), Some(eof_code()), AccountStatus::Changed);
        account.info.as_mut().unwrap().code = None;
        account.original_info.as_mut().unwrap().code = None;
        state.state.insert(target, account);
        let error = check_block(&block, &output(state), &lookup, "import").unwrap_err();
        assert!(matches!(error, EofGuardError::Eof { .. }), "{error}");
    }

    #[test]
    fn a_normal_block_passes() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let target = Address::repeat_byte(0x11);
        let (create, sender) = tx(0, TxKind::Create, legacy_code());
        let (call, _) = tx(1, TxKind::Call(target), Bytes::from_static(&[0xEF, 0x00]));
        let block = block(vec![(create, sender), (call, sender)]);
        let deployed = Address::repeat_byte(0xDD);
        let mut state = BundleState::default();
        state
            .contracts
            .insert(keccak256(legacy_code()), Bytecode::new_raw(legacy_code()));
        state.state.insert(
            deployed,
            account_with_code(None, Some(legacy_code()), AccountStatus::InMemoryChange),
        );
        state.state.insert(
            target,
            account_with_code(
                Some(legacy_code()),
                Some(legacy_code()),
                AccountStatus::Changed,
            ),
        );
        let lookup = MapLookup::default().with_account(target, legacy_code());
        check_block(&block, &output(state), &lookup, "import").expect("legacy code passes");
        check_built_block(&block).expect("calldata starting with EF00 is not initcode");
        assert!(
            snapshotter
                .snapshot()
                .into_vec()
                .iter()
                .all(|(key, _, _, _)| key.key().name() != "n42_gov5_eof_blocks_rejected_total"),
            "a clean block records no refusal"
        );
    }

    #[test]
    fn an_unreadable_code_lookup_fails_closed() {
        let target = Address::repeat_byte(0xE0);
        let (call, sender) = tx(0, TxKind::Call(target), Bytes::new());
        let block = block(vec![(call, sender)]);
        let error = check_block(
            &block,
            &output(BundleState::default()),
            &FailingLookup,
            "import",
        )
        .unwrap_err();
        assert!(matches!(error, EofGuardError::Lookup { .. }), "{error}");
        assert!(error.to_string().contains(EOF_UNSUPPORTED));
    }

    #[test]
    fn the_metric_counts_every_refusal() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (create, sender) = tx(0, TxKind::Create, eof_code());
        let block = block(vec![(create, sender)]);
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            let _ = check_block(
                &block,
                &output(BundleState::default()),
                &MapLookup::default(),
                "import",
            );
            let _ = check_built_block(&block);
        }
        let snapshot = snapshotter.snapshot().into_vec();
        let mut import = 0;
        let mut build = 0;
        for (key, _, _, value) in snapshot {
            if key.key().name() != "n42_gov5_eof_blocks_rejected_total" {
                continue;
            }
            let DebugValue::Counter(count) = value else {
                continue;
            };
            let stage = key
                .key()
                .labels()
                .find(|label| label.key() == "stage")
                .map(|label| label.value().to_string());
            match stage.as_deref() {
                Some("import") => import = count,
                Some("build") => build = count,
                _ => {}
            }
        }
        assert_eq!(import, 1);
        assert_eq!(build, 1);
    }
}
