//! N42 custom EVM factory with randomness precompile at address `0x0302`.

use alloy_evm::{
    eth::EthEvmContext,
    precompiles::PrecompilesMap,
    EthEvm, EvmFactory,
};
use alloy_primitives::address;
use revm::{
    context::{BlockEnv, TxEnv},
    context_interface::result::{EVMError, HaltReason},
    inspector::{Inspector, NoOpInspector},
    precompile::{Precompile, PrecompileId, PrecompileSpecId, Precompiles},
    primitives::hardfork::SpecId,
    Context, MainBuilder, MainContext,
};

use crate::precompile_random;

/// Address of the N42 randomness precompile: `0x0302`.
const RANDOMNESS_PRECOMPILE_ADDR: alloy_primitives::Address =
    address!("0000000000000000000000000000000000000302");

/// Build precompiles for a given spec, adding the N42 randomness precompile.
/// Cached per SpecId — leaked once, reused for all subsequent calls with the same spec.
fn n42_precompiles(spec: SpecId) -> &'static Precompiles {
    use std::sync::Mutex;

    // Bounded cache: SpecId has ~20 variants, so at most ~20 leaked entries.
    static CACHE: Mutex<Vec<(SpecId, &'static Precompiles)>> = Mutex::new(Vec::new());

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, precompiles)) = cache.iter().find(|(s, _)| *s == spec) {
        return precompiles;
    }

    let mut precompiles = Precompiles::new(PrecompileSpecId::from(spec)).clone();
    precompiles.extend([Precompile::new(
        PrecompileId::custom("n42-randomness"),
        RANDOMNESS_PRECOMPILE_ADDR,
        precompile_random::revm_precompile_fn,
    )]);
    let leaked: &'static Precompiles = Box::leak(Box::new(precompiles));
    cache.push((spec, leaked));
    leaked
}

/// N42 EVM factory — produces EVMs with Ethereum precompiles + randomness at `0x0302`.
#[derive(Debug, Clone, Default)]
pub struct N42EvmFactory;

impl EvmFactory for N42EvmFactory {
    type Evm<DB: alloy_evm::Database, I: Inspector<EthEvmContext<DB>>> =
        EthEvm<DB, I, Self::Precompiles>;
    type Context<DB: alloy_evm::Database> = EthEvmContext<DB>;
    type Tx = TxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = SpecId;
    type BlockEnv = BlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: alloy_evm::Database>(
        &self,
        db: DB,
        input: alloy_evm::EvmEnv,
    ) -> Self::Evm<DB, NoOpInspector> {
        let spec = input.cfg_env.spec;

        // Inject prevrandao into thread-local for the randomness precompile.
        if let Some(prevrandao) = input.block_env.prevrandao {
            precompile_random::set_block_randomness(prevrandao);
        }

        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(n42_precompiles(spec)));

        EthEvm::new(evm, false)
    }

    fn create_evm_with_inspector<DB: alloy_evm::Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: alloy_evm::EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        EthEvm::new(
            self.create_evm(db, input).into_inner().with_inspector(inspector),
            true,
        )
    }
}
