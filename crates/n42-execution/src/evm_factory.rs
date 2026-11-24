//! N42 custom EVM factory with randomness precompile at address `0x0302`.

use alloy_primitives::address;
use evm2::{
    BaseEvmTypes, ExecutionConfig, Precompiles, SpecId, TxResult, Version,
    precompiles::{Precompile, PrecompileId, PrecompileMap},
    registry::TxRegistry,
};
use reth_evm_ethereum::{EvmFactory, RethEvmFactory};
use std::borrow::Cow;

use crate::precompile_random;

/// Address of the N42 randomness precompile: `0x0302`.
pub(crate) const RANDOMNESS_PRECOMPILE_ADDR: alloy_primitives::Address =
    address!("0000000000000000000000000000000000000302");

/// Build precompiles for a given spec, adding the N42 randomness precompile.
/// Cached per SpecId — leaked once, reused for all subsequent calls with the same spec.
fn n42_precompiles(spec: SpecId) -> &'static PrecompileMap {
    use std::sync::Mutex;

    // Bounded cache: SpecId has ~20 variants, so at most ~20 leaked entries.
    static CACHE: Mutex<Vec<(SpecId, &'static PrecompileMap)>> = Mutex::new(Vec::new());

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, precompiles)) = cache.iter().find(|(s, _)| *s == spec) {
        return precompiles;
    }

    let mut precompiles = Precompiles::<BaseEvmTypes>::base(spec).as_map().clone();
    precompiles.extend([Precompile::new(
        RANDOMNESS_PRECOMPILE_ADDR,
        PrecompileId::custom("n42-randomness"),
        precompile_random::evm2_precompile_fn,
    )]);
    let leaked: &'static PrecompileMap = Box::leak(Box::new(precompiles));
    cache.push((spec, leaked));
    leaked
}

/// N42 EVM factory — produces EVMs with Ethereum precompiles + randomness at `0x0302`.
///
/// Everything else is reth's own evm2 factory, but for the SSTORE instruction,
/// which also reports the slots a transaction wrote (see `dirty_storage`).
#[derive(Debug, Clone, Default)]
pub struct N42EvmFactory {
    inner: RethEvmFactory,
    fast_transfers: bool,
}

impl N42EvmFactory {
    /// The factory a node is configured with: the transfer fast path is on
    /// with `N42_FAST_TRANSFER=1`.
    pub fn from_env() -> Self {
        Self::with_fast_transfers(crate::fast_transfer::enabled())
    }

    /// Select the same factory for execution comparisons without mutating process environment.
    pub fn with_fast_transfers(enabled: bool) -> Self {
        Self {
            inner: RethEvmFactory::default(),
            fast_transfers: enabled,
        }
    }

    /// Whether the transfer fast path was asked for.
    pub const fn fast_transfers(&self) -> bool {
        self.fast_transfers
    }

    /// The transaction registry N42 EVMs execute with.
    ///
    /// Ethereum's handlers, with the transfer fast path over types 0, 1 and
    /// 2 when [`Self::fast_transfers`] is set (see `fast_transfer`).
    pub fn n42_tx_registry(
        &self,
        spec: SpecId,
    ) -> TxRegistry<BaseEvmTypes, TxResult<BaseEvmTypes>> {
        let base = self.inner.tx_registry(spec);
        if self.fast_transfers {
            crate::fast_transfer::registry(spec, base)
        } else {
            base
        }
    }
}

impl EvmFactory for N42EvmFactory {
    type Types = BaseEvmTypes;
    type SpecId = SpecId;

    fn spec_id(&self, spec: SpecId) -> SpecId {
        self.inner.spec_id(spec)
    }

    fn version(&self, spec: SpecId, chain_id: u64) -> Version {
        self.inner.version(spec, chain_id)
    }

    fn execution_config(&self, spec: SpecId, version: Version) -> ExecutionConfig<BaseEvmTypes> {
        crate::dirty_storage::execution_config(spec, version)
    }

    fn tx_registry(&self, spec: SpecId) -> TxRegistry<BaseEvmTypes, TxResult<BaseEvmTypes>> {
        self.n42_tx_registry(spec)
    }

    fn precompiles(&self, spec: SpecId) -> Precompiles<BaseEvmTypes> {
        Precompiles::new(Cow::Borrowed(n42_precompiles(spec)))
    }

    fn configure_evm(&self, evm: &mut evm2::Evm<'_, BaseEvmTypes>) {
        EvmFactory::configure_evm(&self.inner, evm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evm2::evm::precompile::PrecompileProvider;

    #[test]
    fn every_spec_has_its_base_precompiles_and_the_randomness_precompile() {
        let factory = N42EvmFactory::default();
        for spec in [
            SpecId::FRONTIER,
            SpecId::BERLIN,
            SpecId::CANCUN,
            SpecId::OSAKA,
        ] {
            let base = Precompiles::<BaseEvmTypes>::base(spec);
            let n42 = factory.precompiles(spec);
            let mut want = PrecompileProvider::<BaseEvmTypes>::addresses(&base);
            want.push(RANDOMNESS_PRECOMPILE_ADDR);
            want.sort_unstable();
            let mut got = PrecompileProvider::<BaseEvmTypes>::addresses(&n42);
            got.sort_unstable();
            assert_eq!(got, want, "{spec:?}");
            assert_eq!(
                RANDOMNESS_PRECOMPILE_ADDR.as_slice(),
                precompile_random::RANDOMNESS_ADDRESS.as_slice()
            );
        }
    }
}
