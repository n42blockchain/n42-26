//! Prints one signed 0x50 (Ed25519) value transfer as raw hex, for
//! `eth_sendRawTransaction`. Offline: no RPC client, no nonce lookup.
//!
//!   alt_sig_raw --seed <32-byte hex> --chain-id <id> --nonce <n> --to <address> [--value <wei>]
//!   alt_sig_raw --seed <32-byte hex> --address
//!
//! The sender account is keccak256(0x01 || pubkey)[12..]; `--address` prints it
//! so a genesis can fund it.
//!
//! `--secp256k1-key <hex>` in place of `--seed` signs the same transfer as an
//! ordinary EIP-1559 transaction, for mixed 0x02/0x50 checks.
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, crypto::secp256k1};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, hex};
use ed25519_dalek::SigningKey;
use n42_tx_types::{ALG_ED25519, TxAltSig, alt_sig::sender_of};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut seed = None;
    let mut chain_id = None;
    let mut nonce = 0u64;
    let mut to = None;
    let mut value = U256::from(1_000_000_000_000_000u64);
    let mut address_only = false;
    let mut secp_key = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seed" => seed = Some(args.next().ok_or("--seed needs hex")?),
            "--chain-id" => chain_id = Some(args.next().ok_or("--chain-id needs a number")?.parse()?),
            "--nonce" => nonce = args.next().ok_or("--nonce needs a number")?.parse()?,
            "--to" => to = Some(args.next().ok_or("--to needs an address")?.parse::<Address>()?),
            "--value" => value = args.next().ok_or("--value needs wei")?.parse()?,
            "--address" => address_only = true,
            "--secp256k1-key" => {
                secp_key = Some(args.next().ok_or("--secp256k1-key needs hex")?.parse::<B256>()?)
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if let Some(secret) = secp_key {
        let tx = TxEip1559 {
            chain_id: chain_id.ok_or("--chain-id is required")?,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 5_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(to.ok_or("--to is required")?),
            value,
            ..Default::default()
        };
        let signature = secp256k1::sign_message(secret, tx.signature_hash())?;
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        eprintln!("hash {}", envelope.tx_hash());
        println!("{}", hex::encode_prefixed(envelope.encoded_2718()));
        return Ok(());
    }
    let seed: [u8; 32] = hex::decode(seed.ok_or("--seed is required")?.trim_start_matches("0x"))?
        .try_into()
        .map_err(|_| "--seed must be 32 bytes")?;
    let key = SigningKey::from_bytes(&seed);
    let pubkey = Bytes::copy_from_slice(key.verifying_key().as_bytes());
    let from = sender_of(ALG_ED25519, &pubkey);
    if address_only {
        println!("{from}");
        return Ok(());
    }
    let tx = TxAltSig {
        chain_id: chain_id.ok_or("--chain-id is required")?,
        nonce,
        max_priority_fee_per_gas: 1_000_000_000,
        max_fee_per_gas: 5_000_000_000,
        gas_limit: 21_000,
        to: to.ok_or("--to is required")?,
        value,
        input: Bytes::new(),
        access_list: Default::default(),
        alg_type: ALG_ED25519,
        pubkey,
    }
    .sign_ed25519(&key);
    eprintln!("from {from} hash {}", tx.hash());
    println!("{}", hex::encode_prefixed(tx.encoded_2718()));
    Ok(())
}
