use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use tracing::{debug, info};

use crate::genesis::TestAccount;
use crate::rpc_client::RpcClient;

/// Manages transaction signing and nonce tracking for multiple test wallets.
pub struct TxEngine {
    wallets: Vec<Wallet>,
    chain_id: u64,
}

struct Wallet {
    signer: PrivateKeySigner,
    address: Address,
    nonce: u64,
}

impl TxEngine {
    /// Creates a new TxEngine from test accounts.
    pub fn new(accounts: &[TestAccount], chain_id: u64) -> Self {
        let wallets = accounts
            .iter()
            .map(|acc| {
                let signer = PrivateKeySigner::from_bytes(&acc.private_key)
                    .expect("valid private key");
                Wallet {
                    address: signer.address(),
                    signer,
                    nonce: 0,
                }
            })
            .collect();

        Self { wallets, chain_id }
    }

    /// Syncs nonces from the chain for all wallets.
    pub async fn sync_nonces(&mut self, rpc: &RpcClient) -> eyre::Result<()> {
        for wallet in &mut self.wallets {
            let nonce = rpc.get_nonce(wallet.address).await?;
            wallet.nonce = nonce;
            debug!(address = %wallet.address, nonce, "synced nonce");
        }
        Ok(())
    }

    /// Returns the number of wallets.
    pub fn wallet_count(&self) -> usize {
        self.wallets.len()
    }

    /// Returns the address of a wallet by index.
    pub fn address(&self, index: usize) -> Address {
        self.wallets[index].address
    }

    pub fn build_transfer(
        &mut self,
        from_index: usize,
        to: Address,
        value: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> eyre::Result<(B256, Vec<u8>)> {
        let wallet = &mut self.wallets[from_index];
        let nonce = wallet.nonce;
        wallet.nonce += 1;

        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(to),
            value,
            input: Bytes::default(),
            access_list: Default::default(),
        };

        let sig = wallet.signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(sig);
        let mut encoded = Vec::new();
        signed.encode_2718(&mut encoded);

        Ok((*signed.hash(), encoded))
    }

    pub fn build_deploy(
        &mut self,
        from_index: usize,
        bytecode: Vec<u8>,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> eyre::Result<(B256, Vec<u8>)> {
        let wallet = &mut self.wallets[from_index];
        let nonce = wallet.nonce;
        wallet.nonce += 1;

        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::from(bytecode),
            access_list: Default::default(),
        };

        let sig = wallet.signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(sig);
        let mut encoded = Vec::new();
        signed.encode_2718(&mut encoded);

        Ok((*signed.hash(), encoded))
    }

    pub fn build_contract_call(
        &mut self,
        from_index: usize,
        to: Address,
        calldata: Vec<u8>,
        value: U256,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> eyre::Result<(B256, Vec<u8>)> {
        let wallet = &mut self.wallets[from_index];
        let nonce = wallet.nonce;
        wallet.nonce += 1;

        let tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: TxKind::Call(to),
            value,
            input: Bytes::from(calldata),
            access_list: Default::default(),
        };

        let sig = wallet.signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(sig);
        let mut encoded = Vec::new();
        signed.encode_2718(&mut encoded);

        Ok((*signed.hash(), encoded))
    }

    /// Sends a batch of transfer transactions at a given rate.
    pub async fn send_transfers_at_rate(
        &mut self,
        rpc: &RpcClient,
        tx_per_second: u32,
        duration_secs: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    ) -> eyre::Result<Vec<B256>> {
        let total_tx = (tx_per_second as u64) * duration_secs;
        let interval = std::time::Duration::from_micros(1_000_000 / tx_per_second as u64);
        let mut tx_hashes = Vec::with_capacity(total_tx as usize);
        let wallet_count = self.wallets.len();

        info!(total_tx, tx_per_second, duration_secs, "starting transfer batch");

        for i in 0..total_tx {
            let from_idx = (i as usize) % wallet_count;
            let to_idx = ((i as usize) + 1) % wallet_count;
            let to_addr = self.wallets[to_idx].address;

            // Random transfer amount: 0.01 to 1 N42 (in wei).
            let amount = U256::from(rand::random::<u64>() % 990_000_000_000_000_000 + 10_000_000_000_000_000);

            let (tx_hash, raw_tx) = self.build_transfer(
                from_idx,
                to_addr,
                amount,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                21000,
            )?;

            rpc.send_raw_transaction(&raw_tx).await?;
            tx_hashes.push(tx_hash);

            if (i + 1) % 100 == 0 {
                info!(sent = i + 1, total = total_tx, "transfer progress");
            }

            tokio::time::sleep(interval).await;
        }

        info!(total = tx_hashes.len(), "all transfers sent");
        Ok(tx_hashes)
    }

    /// Waits for all transaction receipts and returns them.
    pub async fn wait_for_all_receipts(
        rpc: &RpcClient,
        tx_hashes: &[B256],
        timeout: std::time::Duration,
    ) -> eyre::Result<Vec<crate::rpc_client::TransactionReceipt>> {
        let mut receipts = Vec::with_capacity(tx_hashes.len());
        let start = tokio::time::Instant::now();

        for (i, hash) in tx_hashes.iter().enumerate() {
            let remaining = timeout.checked_sub(start.elapsed())
                .ok_or_else(|| eyre::eyre!("timeout waiting for receipts ({}/{} received)", i, tx_hashes.len()))?;

            let receipt = rpc.wait_for_receipt(*hash, remaining).await?;
            receipts.push(receipt);

            if (i + 1) % 100 == 0 {
                info!(confirmed = i + 1, total = tx_hashes.len(), "receipt progress");
            }
        }

        Ok(receipts)
    }
}
