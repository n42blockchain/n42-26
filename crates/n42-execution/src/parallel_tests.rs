//! What the tests of the parallel transfer modules share.

use crate::{N42EvmConfig, N42EvmFactory};
use std::sync::Arc;

/// The chain id of [`config`]'s chain.
pub(crate) const CHAIN_ID: u64 = n42_chainspec::N42_CHAIN_ID;

/// The dev chain with Prague on (or Cancun only), without the transfer fast
/// path: what the batches' results are compared against.
pub(crate) fn config_on(prague: bool) -> N42EvmConfig {
    let mut chain = n42_chainspec::n42_dev_chainspec();
    if prague {
        let chain_mut = Arc::make_mut(&mut chain);
        chain_mut
            .hardforks
            .insert(reth_chainspec::EthereumHardfork::Prague, reth_chainspec::ForkCondition::Timestamp(0));
        chain_mut.genesis.config.prague_time = Some(0);
    }
    N42EvmConfig::with_evm_factory(chain, N42EvmFactory::with_fast_transfers(false))
}

/// [`config_on`] with Prague.
pub(crate) fn config() -> N42EvmConfig {
    config_on(true)
}
