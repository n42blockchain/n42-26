use alloy_primitives::{Address, B256, Signature, keccak256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, bail, eyre};
use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Instant;

const RECORD_BYTES: usize = 12;
const HEADER_BYTES: usize = 8 + 4 + 65;
const DOMAIN: &[u8] = b"N42_BATCH_TRANSFER_V0";
const DEFAULT_SENDERS: usize = 128;
const DEFAULT_TRANSFERS_PER_SENDER: usize = 10_000;
const DEFAULT_ACCOUNT_CACHE: usize = 4096;

#[derive(Debug, Clone)]
struct BatchAccount {
    signer: PrivateKeySigner,
    address: Address,
}

#[derive(Debug, Clone)]
pub(crate) struct BatchTransferConfig {
    pub(crate) senders: usize,
    pub(crate) transfers_per_sender: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct BatchTransferStats {
    pub(crate) block_number: u64,
    pub(crate) senders: usize,
    pub(crate) transfers_per_sender: usize,
    pub(crate) transfers: u64,
    pub(crate) encoded_bytes: usize,
    pub(crate) bytes_per_transfer: f64,
    pub(crate) build_ms: u64,
    pub(crate) block_hash_ms: u64,
    pub(crate) decode_ms: u64,
    pub(crate) record_hash_ms: u64,
    pub(crate) recover_ms: u64,
    pub(crate) apply_ms: u64,
    pub(crate) total_verify_apply_ms: u64,
}

pub(crate) fn fastlane_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("N42_BATCH_TRANSFER_FASTLANE")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            > 0
    })
}

pub(crate) fn config() -> BatchTransferConfig {
    static CONFIG: OnceLock<BatchTransferConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| BatchTransferConfig {
            senders: env_usize("N42_BATCH_TRANSFER_SENDERS", DEFAULT_SENDERS).max(1),
            transfers_per_sender: env_usize(
                "N42_BATCH_TRANSFER_PER_SENDER",
                DEFAULT_TRANSFERS_PER_SENDER,
            )
            .max(1),
        })
        .clone()
}

pub(crate) fn build_block(block_number: u64) -> Result<(B256, Vec<u8>, BatchTransferStats)> {
    let cfg = config();
    let accounts = cached_accounts(cfg.senders)?;
    let transfers = cfg.senders.saturating_mul(cfg.transfers_per_sender);
    let capacity = 8 + 4 + cfg.senders * (HEADER_BYTES + cfg.transfers_per_sender * RECORD_BYTES);
    let build_start = Instant::now();
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(&block_number.to_le_bytes());
    encoded.extend_from_slice(&(cfg.senders as u32).to_le_bytes());

    for sender_idx in 0..cfg.senders {
        let start_nonce = block_number
            .saturating_mul(cfg.transfers_per_sender as u64)
            .saturating_add(sender_idx as u64 * cfg.transfers_per_sender as u64);
        let count = cfg.transfers_per_sender as u32;
        let header_pos = encoded.len();
        let records_start = header_pos + HEADER_BYTES;
        encoded.extend_from_slice(&start_nonce.to_le_bytes());
        encoded.extend_from_slice(&count.to_le_bytes());
        encoded.resize(encoded.len() + 65, 0);
        push_records(&mut encoded, sender_idx, cfg.transfers_per_sender);
        let records =
            &encoded[records_start..records_start + cfg.transfers_per_sender * RECORD_BYTES];
        let prehash = prehash(chain_id(), block_number, start_nonce, count, records);
        let sig = accounts[sender_idx]
            .signer
            .sign_hash_sync(&prehash)
            .map_err(|e| eyre!("batch sign failed: {e}"))?;
        encoded[header_pos + 12..header_pos + HEADER_BYTES].copy_from_slice(&sig.as_bytes());
    }
    let build_ms = build_start.elapsed().as_millis() as u64;

    let hash_start = Instant::now();
    let block_hash = B256::from_slice(blake3::hash(&encoded).as_bytes());
    let block_hash_ms = hash_start.elapsed().as_millis() as u64;
    let encoded_bytes = encoded.len();
    let stats = BatchTransferStats {
        block_number,
        senders: cfg.senders,
        transfers_per_sender: cfg.transfers_per_sender,
        transfers: transfers as u64,
        encoded_bytes,
        bytes_per_transfer: encoded_bytes as f64 / transfers.max(1) as f64,
        build_ms,
        block_hash_ms,
        decode_ms: 0,
        record_hash_ms: 0,
        recover_ms: 0,
        apply_ms: 0,
        total_verify_apply_ms: 0,
    };
    Ok((block_hash, encoded, stats))
}

pub(crate) fn verify_apply_block(
    encoded: &[u8],
    expected_hash: B256,
) -> Result<BatchTransferStats> {
    let total_start = Instant::now();
    let hash_start = Instant::now();
    let actual_hash = B256::from_slice(blake3::hash(encoded).as_bytes());
    let block_hash_ms = hash_start.elapsed().as_millis() as u64;
    if actual_hash != expected_hash {
        bail!("batch transfer block hash mismatch: expected {expected_hash}, got {actual_hash}");
    }

    let decode_start = Instant::now();
    let mut offset = 0usize;
    let block_number = read_u64(encoded, &mut offset)?;
    let senders = read_u32(encoded, &mut offset)? as usize;
    let decode_ms = decode_start.elapsed().as_millis() as u64;
    let accounts = cached_accounts(senders)?;

    let mut total_transfers = 0usize;
    let mut sender_balances = vec![u128::MAX / 4; senders.max(1)];
    let mut sender_nonces = vec![0u64; senders.max(1)];
    let recipient_count = estimated_recipient_count(encoded, senders)
        .min(2_000_000)
        .max(1);
    let mut recipient_balances = vec![0u128; recipient_count];
    let mut record_hash_ms = 0u64;
    let mut recover_ms = 0u64;
    let mut apply_ms = 0u64;
    let mut checksum = 0u128;

    for sender_idx in 0..senders {
        let start_nonce = read_u64(encoded, &mut offset)?;
        let count = read_u32(encoded, &mut offset)? as usize;
        let sig_end = offset + 65;
        if sig_end > encoded.len() {
            bail!("batch transfer truncated signature");
        }
        let sig = Signature::from_raw(&encoded[offset..sig_end])?;
        offset = sig_end;
        let records_len = count
            .checked_mul(RECORD_BYTES)
            .ok_or_else(|| eyre!("batch transfer records length overflow"))?;
        let records_end = offset + records_len;
        if records_end > encoded.len() {
            bail!("batch transfer truncated records");
        }
        let records = &encoded[offset..records_end];

        let hash_start = Instant::now();
        let prehash = prehash(chain_id(), block_number, start_nonce, count as u32, records);
        record_hash_ms += hash_start.elapsed().as_millis() as u64;

        let recover_start = Instant::now();
        let recovered = sig.recover_address_from_prehash(&prehash)?;
        if recovered != accounts[sender_idx].address {
            bail!("batch transfer sender mismatch at batch {sender_idx}");
        }
        recover_ms += recover_start.elapsed().as_millis() as u64;

        let apply_start = Instant::now();
        sender_nonces[sender_idx] = start_nonce;
        for record in records.chunks_exact(RECORD_BYTES) {
            let recipient_index = u32::from_le_bytes(record[0..4].try_into().unwrap()) as usize;
            let amount = u64::from_le_bytes(record[4..12].try_into().unwrap()) as u128;
            let recipient = recipient_index % recipient_count;
            sender_balances[sender_idx] = sender_balances[sender_idx].wrapping_sub(amount);
            recipient_balances[recipient] = recipient_balances[recipient].wrapping_add(amount);
            sender_nonces[sender_idx] = sender_nonces[sender_idx].wrapping_add(1);
            checksum ^= recipient_balances[recipient];
        }
        apply_ms += apply_start.elapsed().as_millis() as u64;
        total_transfers += count;
        offset = records_end;
    }
    if offset != encoded.len() {
        bail!("batch transfer trailing bytes: {}", encoded.len() - offset);
    }
    black_box((checksum, sender_balances, recipient_balances, sender_nonces));

    let total_verify_apply_ms = total_start.elapsed().as_millis() as u64;
    let transfers_per_sender = if senders == 0 {
        0
    } else {
        total_transfers / senders
    };
    Ok(BatchTransferStats {
        block_number,
        senders,
        transfers_per_sender,
        transfers: total_transfers as u64,
        encoded_bytes: encoded.len(),
        bytes_per_transfer: encoded.len() as f64 / total_transfers.max(1) as f64,
        build_ms: 0,
        block_hash_ms,
        decode_ms,
        record_hash_ms,
        recover_ms,
        apply_ms,
        total_verify_apply_ms,
    })
}

fn prehash(chain_id: u64, block_number: u64, start_nonce: u64, count: u32, records: &[u8]) -> B256 {
    let records_hash = blake3::hash(records);
    let mut hasher = blake3::Hasher::new();
    hasher.update(DOMAIN);
    hasher.update(&chain_id.to_le_bytes());
    hasher.update(&block_number.to_le_bytes());
    hasher.update(&start_nonce.to_le_bytes());
    hasher.update(&count.to_le_bytes());
    hasher.update(records_hash.as_bytes());
    B256::from_slice(hasher.finalize().as_bytes())
}

fn push_records(buf: &mut Vec<u8>, sender_idx: usize, transfers: usize) {
    for i in 0..transfers {
        let recipient_index = ((sender_idx * transfers + i) as u32).wrapping_mul(2_654_435_761);
        let amount = 1u64;
        buf.extend_from_slice(&recipient_index.to_le_bytes());
        buf.extend_from_slice(&amount.to_le_bytes());
    }
}

fn cached_accounts(required: usize) -> Result<&'static [BatchAccount]> {
    static ACCOUNTS: OnceLock<Vec<BatchAccount>> = OnceLock::new();
    let accounts = ACCOUNTS.get_or_init(|| {
        let count = env_usize("N42_BATCH_TRANSFER_ACCOUNT_CACHE", DEFAULT_ACCOUNT_CACHE)
            .max(DEFAULT_ACCOUNT_CACHE)
            .max(required);
        (0..count)
            .map(|idx| {
                let pk = derive_private_key(idx);
                let signer = PrivateKeySigner::from_bytes(&B256::from(pk)).expect("valid key");
                let address = signer.address();
                BatchAccount { signer, address }
            })
            .collect()
    });
    if accounts.len() < required {
        bail!(
            "batch transfer account cache too small: have {}, need {}",
            accounts.len(),
            required
        );
    }
    Ok(&accounts[..required])
}

fn derive_private_key(index: usize) -> [u8; 32] {
    let seed = format!("n42-test-key-{index}");
    let hash = keccak256(seed.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(hash.as_slice());
    out
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn chain_id() -> u64 {
    std::env::var("N42_CHAIN_ID")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(n42_chainspec::N42_CHAIN_ID)
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32> {
    let end = *offset + 4;
    if end > bytes.len() {
        bail!("batch transfer truncated u32");
    }
    let value = u32::from_le_bytes(bytes[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> Result<u64> {
    let end = *offset + 8;
    if end > bytes.len() {
        bail!("batch transfer truncated u64");
    }
    let value = u64::from_le_bytes(bytes[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

fn estimated_recipient_count(encoded: &[u8], senders: usize) -> usize {
    let body = encoded.len().saturating_sub(8 + 4);
    let records = body.saturating_sub(senders.saturating_mul(HEADER_BYTES));
    records / RECORD_BYTES
}
