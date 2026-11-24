//! The node's `--chain` parser.
//!
//! A genesis file whose `config` carries `n42Profile` is a production-profile
//! chain: it is checked as written (closed key lists, P4) and its genesis
//! header is the Profile-1 header (A6), so every subcommand (`init`, `node`,
//! `db`, …) sees the standard genesis hash. Any other value (`dev`, a named
//! chain, a genesis without the field) goes to reth's Ethereum parser
//! unchanged.

use reth_chainspec::ChainSpec;
use reth_cli::chainspec::ChainSpecParser;
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub(crate) struct N42ChainSpecParser;

/// The genesis JSON behind a `--chain` value, when it is a file path or
/// inline JSON (reth's `parse_genesis` order: a readable path first).
fn raw_genesis(value: &str) -> Option<serde_json::Value> {
    let text = match std::fs::read_to_string(value) {
        Ok(text) => text,
        Err(_) if value.contains('{') => value.to_owned(),
        Err(_) => return None,
    };
    serde_json::from_str(&text).ok()
}

impl ChainSpecParser for N42ChainSpecParser {
    type ChainSpec = ChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = EthereumChainSpecParser::SUPPORTED_CHAINS;

    fn parse(value: &str) -> eyre::Result<Arc<ChainSpec>> {
        if !EthereumChainSpecParser::SUPPORTED_CHAINS.contains(&value)
            && let Some(raw) = raw_genesis(value)
            && n42_chainspec::profile1::raw_genesis_declares_profile(&raw)
        {
            let (spec, _, _) = n42_chainspec::profile1::profile1_chain_spec(&raw)
                .map_err(|error| eyre::eyre!("refusing genesis {value}: {error}"))?;
            return Ok(Arc::new(spec));
        }
        EthereumChainSpecParser::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn v1_genesis() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/n42-chainspec/testdata/profile1/V1/genesis_8s.json")
    }

    #[test]
    fn a_profile1_file_gets_the_standard_genesis_hash() {
        let spec = N42ChainSpecParser::parse(v1_genesis().to_str().unwrap()).unwrap();
        assert_eq!(
            spec.genesis_hash(),
            "0x188e9d930cfd43317416b0f65e8f98240ae686407ba806b854e98428c3b490dc"
                .parse::<alloy_primitives::B256>()
                .unwrap()
        );
        assert_eq!(spec.chain().id(), 1144);
    }

    #[test]
    fn a_profile1_file_with_an_unknown_key_is_refused() {
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(v1_genesis()).unwrap()).unwrap();
        raw["config"]["frameBlocks"] = true.into();
        let error = N42ChainSpecParser::parse(&raw.to_string()).unwrap_err();
        assert!(error.to_string().contains("frameBlocks"), "{error}");
    }

    #[test]
    fn other_chains_parse_as_before() {
        for value in ["dev", "mainnet"] {
            assert_eq!(
                N42ChainSpecParser::parse(value).unwrap().genesis_hash(),
                EthereumChainSpecParser::parse(value).unwrap().genesis_hash()
            );
        }
        let plain = r#"{"config":{"chainId":4242,"hotstuff":{"leaderTenure":8}},"alloc":{},"gasLimit":"0x1c9c380"}"#;
        assert_eq!(
            N42ChainSpecParser::parse(plain).unwrap().genesis_hash(),
            EthereumChainSpecParser::parse(plain).unwrap().genesis_hash()
        );
    }
}
