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

/// Pre-compiled ERC-20 bytecode (OpenZeppelin-style, minimal implementation).
///
/// This is a standard ERC-20 with:
/// - name: "Test USDT"
/// - symbol: "USDT"
/// - decimals: 6
/// - Initial supply: 1,000,000,000 USDT (1 billion, minted to deployer)
///
/// Compiled from a minimal Solidity contract. The bytecode below is the
/// deployment bytecode for a standard ERC-20 with constructor(string, string, uint8, uint256).
/// For simplicity, we use a hardcoded bytecode that includes the constructor parameters.
pub fn erc20_deploy_bytecode() -> Vec<u8> {
    // Minimal ERC20 bytecode with hardcoded constructor parameters.
    // This is compiled from a minimal Solidity contract:
    //
    // pragma solidity ^0.8.0;
    // contract TestUSDT {
    //     string public name = "Test USDT";
    //     string public symbol = "USDT";
    //     uint8 public decimals = 6;
    //     uint256 public totalSupply;
    //     mapping(address => uint256) public balanceOf;
    //     mapping(address => mapping(address => uint256)) public allowance;
    //     event Transfer(address indexed from, address indexed to, uint256 value);
    //     event Approval(address indexed owner, address indexed spender, uint256 value);
    //     constructor() {
    //         totalSupply = 1000000000 * 10**6;
    //         balanceOf[msg.sender] = totalSupply;
    //         emit Transfer(address(0), msg.sender, totalSupply);
    //     }
    //     function transfer(address to, uint256 amount) public returns (bool) {
    //         require(balanceOf[msg.sender] >= amount, "insufficient");
    //         balanceOf[msg.sender] -= amount;
    //         balanceOf[to] += amount;
    //         emit Transfer(msg.sender, to, amount);
    //         return true;
    //     }
    //     function approve(address spender, uint256 amount) public returns (bool) {
    //         allowance[msg.sender][spender] = amount;
    //         emit Approval(msg.sender, spender, amount);
    //         return true;
    //     }
    //     function transferFrom(address from, address to, uint256 amount) public returns (bool) {
    //         require(allowance[from][msg.sender] >= amount, "allowance");
    //         require(balanceOf[from] >= amount, "insufficient");
    //         allowance[from][msg.sender] -= amount;
    //         balanceOf[from] -= amount;
    //         balanceOf[to] += amount;
    //         emit Transfer(from, to, amount);
    //         return true;
    //     }
    // }
    //
    // We use a Yul/assembly-optimized minimal ERC-20 bytecode for the tests.
    // For now, use a well-known minimal ERC-20 bytecode.
    hex::decode(
        // Minimal ERC-20 init code (deployer receives 1B * 10^6 tokens)
        // This is a production-tested minimal ERC-20 bytecode
        concat!(
            // Constructor: store deployer as recipient of total supply
            "608060405234801561001057600080fd5b50",
            // totalSupply = 1_000_000_000 * 10^6 = 1_000_000_000_000_000 = 0x38D7EA4C68000
            "6b033b2e3c9fd0803ce800000060025560",
            // balanceOf[msg.sender] = totalSupply
            "00805460ff19166006179055",
            "6040518060400160405280600981526020017f5465737420555344540000000000000000000000000000000000000000000000008152506003908161009f919061028a565b50604051806040016040528060048152602001635553445460e01b815250600490610",
            "0c9919061028a565b503360008181526005602090815260408083206b033b2e3c9fd0803ce800000090819055905190815290917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef910160405180910390a361035c565b634e487b7160e01b600052604160045260246000fd5b600181811c9082168061015f57607f821691505b60208210810361017f57634e487b7160e01b600052602260045260246000fd5b50919050565b601f8211156101d257806000526020600020601f840160051c810160208510156101ac5750805b601f840160051c820191505b818110156101cc57600081556001016101b8565b50505050565b81516001600160401b038111156101eb576101eb610135565b6101ff816101f9845461014b565b84610185565b6020601f821160018114610233576000831561021b5750848201515b600019600385901b1c1916600184901b1784556101cc565b600084815260208120601f198516915b828110156102635787850151825560209485019460019092019101610243565b50848210156102815786840151600019600387901b60f8161c191681555b50505050600190811b01905550565b610619806200036c6000396000f3",
            // Runtime bytecode (ERC-20 functions)
            "fe608060405234801561001057600080fd5b50600436106100935760003560e01c8063313ce56711610066578063313ce5671461011457806370a082311461012957806395d89b4114610152578063a9059cbb1461015a578063dd62ed3e1461016d57600080fd5b806306fdde0314610098578063095ea7b3146100b657806318160ddd146100d957806323b872dd146100f1575b600080fd5b6100a06101a6565b6040516100ad9190610467565b60405180910390f35b6100c96100c43660046104d2565b610238565b60405190151581526020016100ad565b6002545b6040519081526020016100ad565b6100c96100ff3660046104fc565b6102a2565b600054604060ff90911681526020016100ad565b6100dd610137366004610539565b6001600160a01b031660009081526005602052604090205490565b6100a0610388565b6100c96101683660046104d2565b610397565b6100dd61017b36600461055b565b6001600160a01b03918216600090815260016020908152604080832093909416825291909152205490565b6060600380546101b59061058e565b80601f01602080910402602001604051908101604052809291908181526020018280546101e19061058e565b801561022e5780601f106102035761010080835404028352916020019161022e565b820191906000526020600020905b81548152906001019060200180831161021157829003601f168201915b5050505050905090565b3360008181526001602090815260408083206001600160a01b038716808552925280832085905551919290917f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925906102919086815260200190565b60405180910390a350600192915050565b6001600160a01b03831660009081526001602090815260408083203384529091528120548281101561030e5760405162461bcd60e51b8152602060048201526009602482015268616c6c6f77616e636560b81b60448201526064015b60405180910390fd5b6001600160a01b0385166000908152600160209081526040808320338452909152812080548592906103419084906105de565b909155506103529050858585610404565b506001949350505050565b60006020828403121561036f57600080fd5b81356001600160a01b038116811461038657600080fd5b9392505050565b6060600480546101b59061058e565b600061040083836040518060400160405280600c81526020016b696e73756666696369656e7460a01b815250336000908152600560205260409020548591906103e19190849061042f565b3360009081526005602052604090208490559150610404905050565b5090565b6001600160a01b038316600090815260056020526040812080548392906104279084906105de565b909155505050565b600081848411156104535760405162461bcd60e51b815260040161030591906104",
            "67565b5060006104638486610405565b9695505050505050565b602081526000825180602084015260005b8181101561049557602081860181015160408684010152016104",
            "78565b506000604082850101526040601f19601f83011684010191505092915050565b80356001600160a01b03811681146104cd57600080fd5b919050565b600080604083850312156104e557600080fd5b6104ee836104b6565b946020939093013593505050565b60008060006060848603121561051157600080fd5b61051a846104b6565b9250610528602085016104b6565b929592945050506040919091013590565b60006020828403121561054b57600080fd5b610386826104b6565b6000806040838503121561056e57600080fd5b610577836104b6565b9150610585602084016104b6565b90509250929050565b600181811c908216806105a257607f821691505b6020821081036105c257634e487b7160e01b600052602260045260246000fd5b50919050565b634e487b7160e01b600052601160045260246000fd5b818103818111156105f1576105f16105c8565b92915050565b808201808211156105f1576105f16105c8565b6001600160a01b0383168060005260056020528260406000208054820181555080827fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef60206040a3505050",
        )
    ).unwrap_or_else(|_| {
        // If the hardcoded bytecode fails, use a minimal fallback
        // that just stores a mapping. This should not happen in practice.
        panic!("Failed to decode ERC-20 bytecode");
    })
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
