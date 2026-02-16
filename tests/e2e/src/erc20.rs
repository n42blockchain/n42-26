use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{sol, SolCall, SolValue};
use tracing::info;

use crate::rpc_client::RpcClient;
use crate::tx_engine::TxEngine;

// Minimal ERC-20 ABI definitions using alloy-sol-types.
sol! {
    function name() external view returns (string);
    function symbol() external view returns (string);
    function decimals() external view returns (uint8);
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
}

/// Pre-compiled ERC-20 deployment bytecode.
///
/// Compiled from the following Solidity source using forge (solc 0.8.20, optimizer 200 runs):
///
/// ```solidity
/// // SPDX-License-Identifier: MIT
/// pragma solidity ^0.8.20;
/// contract TestUSDT {
///     string public name = "Test USDT";
///     string public symbol = "USDT";
///     uint8 public decimals = 6;
///     uint256 public totalSupply;
///     mapping(address => uint256) public balanceOf;
///     mapping(address => mapping(address => uint256)) public allowance;
///     event Transfer(address indexed from, address indexed to, uint256 value);
///     event Approval(address indexed owner, address indexed spender, uint256 value);
///     constructor() {
///         totalSupply = 1_000_000_000 * 10 ** 6; // 1 billion USDT
///         balanceOf[msg.sender] = totalSupply;
///         emit Transfer(address(0), msg.sender, totalSupply);
///     }
///     function transfer(address to, uint256 amount) public returns (bool) { ... }
///     function approve(address spender, uint256 amount) public returns (bool) { ... }
///     function transferFrom(address from, address to, uint256 amount) public returns (bool) { ... }
/// }
/// ```
///
/// Source: tests/e2e/contracts/TestUSDT.sol
pub fn erc20_deploy_bytecode() -> Vec<u8> {
    hex::decode(include_str!("../contracts/TestUSDT.hex").trim())
        .expect("valid ERC-20 deployment bytecode")
}

/// Manages ERC-20 contract interactions.
pub struct Erc20Manager {
    pub contract_address: Address,
}

impl Erc20Manager {
    /// Deploys the ERC-20 contract and returns a manager.
    pub async fn deploy(
        tx_engine: &mut TxEngine,
        rpc: &RpcClient,
        deployer_index: usize,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> eyre::Result<Self> {
        let bytecode = erc20_deploy_bytecode();
        info!(bytecode_len = bytecode.len(), "deploying ERC-20 contract");

        let (tx_hash, raw_tx) = tx_engine.build_deploy(
            deployer_index,
            bytecode,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            3_000_000, // 3M gas limit for deployment
        )?;

        rpc.send_raw_transaction(&raw_tx).await?;

        let receipt = rpc.wait_for_receipt(
            tx_hash,
            std::time::Duration::from_secs(120),
        ).await?;

        let contract_address = receipt.contract_address
            .ok_or_else(|| eyre::eyre!("deployment receipt missing contract address"))?;

        info!(%contract_address, "ERC-20 contract deployed");

        Ok(Self { contract_address })
    }

    /// Queries the balance of an address.
    pub async fn balance_of(&self, rpc: &RpcClient, account: Address) -> eyre::Result<U256> {
        let calldata = balanceOfCall { account }.abi_encode();
        let result = rpc.eth_call(self.contract_address, calldata).await?;
        let balance = U256::abi_decode(&result)?;
        Ok(balance)
    }

    /// Queries the total supply.
    pub async fn total_supply(&self, rpc: &RpcClient) -> eyre::Result<U256> {
        let calldata = totalSupplyCall {}.abi_encode();
        let result = rpc.eth_call(self.contract_address, calldata).await?;
        let supply = U256::abi_decode(&result)?;
        Ok(supply)
    }

    /// Builds a transfer transaction.
    pub fn build_transfer_tx(
        &self,
        tx_engine: &mut TxEngine,
        from_index: usize,
        to: Address,
        amount: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> eyre::Result<(B256, Vec<u8>)> {
        let calldata = transferCall { to, amount }.abi_encode();
        tx_engine.build_contract_call(
            from_index,
            self.contract_address,
            calldata,
            U256::ZERO,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            100_000, // 100K gas for ERC-20 transfer
        )
    }
}
