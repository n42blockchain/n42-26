//! Presigned 0x50 (Ed25519) load over the node's binary ingest, as native frames.
//!
//! Every transaction is signed before the clock starts. Frames are built so
//! that frame `j` carries nonce `j` of `--frame-size` distinct senders; with
//! `--gateway-seed` each frame root is signed by that gateway key (the node
//! then admits the frame on `N42_FRAME_GATEWAYS` without verifying its
//! transactions), otherwise the node batch-verifies every signature.
//!
//!   native-flood --print-addresses --senders 4000
//!   native-flood --ingest 127.0.0.1:19900,127.0.0.1:19901 --chain-id 4242 \
//!       --senders 4000 --frames-per-sender 50 --frame-size 2000 --gateway-seed <hex>
//!
//! Sender `i`'s Ed25519 seed is keccak256("n42-native-flood" || u64 i BE), so
//! `--print-addresses` lists the accounts a genesis has to fund.
//!
//! The wire is `crates/n42-node/src/ingest.rs` (native frame); the ack is the
//! default 8-byte one: accepted, pool pending. A frame the node deferred (pool
//! gate, accepted = 0) is sent again after a short pause.
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, hex, keccak256};
use clap::Parser;
use ed25519_dalek::SigningKey;
use n42_tx_types::{ALG_ED25519, FrameAttestation, TxAltSig, alt_sig::sender_of, frame_root};
use rayon::prelude::*;
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpStream,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Header bit of a native frame (`ingest.rs`).
const NATIVE_FRAME: u32 = 0x4000_0000;
/// Sends of one frame before it is given up on (20 ms apart).
const MAX_FRAME_ATTEMPTS: u32 = 500;

#[derive(Parser, Debug)]
#[command(about = "Presigned 0x50 frames over the binary ingest")]
struct Args {
    /// Comma-separated ingest endpoints (host:port); frames are spread round-robin.
    #[arg(long)]
    ingest: Option<String>,
    #[arg(long, default_value_t = 4242)]
    chain_id: u64,
    /// Number of Ed25519 sender accounts.
    #[arg(long, default_value_t = 2000)]
    senders: usize,
    /// Transactions per sender (= nonces 0..n).
    #[arg(long, default_value_t = 10)]
    frames_per_sender: u64,
    /// First nonce of every sender (the accounts keep their nonces across runs).
    #[arg(long, default_value_t = 0)]
    start_nonce: u64,
    /// Transactions per frame; must divide `--senders`.
    #[arg(long, default_value_t = 1000)]
    frame_size: usize,
    /// 32-byte hex seed of the gateway key that attests frames; none = unattested.
    #[arg(long)]
    gateway_seed: Option<String>,
    /// Frames in flight per connection before waiting for an ack.
    #[arg(long, default_value_t = 4)]
    window: usize,
    /// Connections per endpoint.
    #[arg(long, default_value_t = 1)]
    connections: usize,
    /// Distinct recipients, spread as n42-rs `tx_flood` / `n42-stress` do; 1 = one sink.
    #[arg(long, default_value_t = 1)]
    recipients: u32,
    /// Print the sender addresses (comma-separated) and the gateway public key, then exit.
    #[arg(long)]
    print_addresses: bool,
}

/// Knuth's multiplicative constant, as `n42-stress` and n42-rs `tx_flood` use it.
const RECIPIENT_SPREAD: u32 = 2_654_435_761;

/// The recipient of transfer `index`, the same scatter as n42-rs `tx_flood`.
fn recipient(spread: u32, index: u64) -> Address {
    if spread <= 1 {
        return Address::repeat_byte(0x42);
    }
    let slot = (index as u32).wrapping_mul(RECIPIENT_SPREAD) % spread;
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&slot.to_be_bytes());
    bytes[4] = 0x42;
    Address::new(bytes)
}

fn sender_key(index: usize) -> SigningKey {
    let mut preimage = b"n42-native-flood".to_vec();
    preimage.extend_from_slice(&(index as u64).to_be_bytes());
    SigningKey::from_bytes(&keccak256(preimage).0)
}

fn sender_address(key: &SigningKey) -> Address {
    sender_of(ALG_ED25519, key.verifying_key().as_bytes())
}

/// One frame on the wire, ready to write: header, entries, attestations.
fn encode_frame(transactions: &[Vec<u8>], attestation: Option<&FrameAttestation>) -> Vec<u8> {
    let body: usize = transactions.iter().map(|tx| 4 + tx.len()).sum();
    let mut out = Vec::with_capacity(4 + body + 1 + 96);
    out.extend_from_slice(&(transactions.len() as u32 | NATIVE_FRAME).to_le_bytes());
    for tx in transactions {
        out.extend_from_slice(&(tx.len() as u32).to_le_bytes());
        out.extend_from_slice(tx);
    }
    match attestation {
        Some(attestation) => {
            out.push(1);
            out.extend_from_slice(&attestation.to_bytes());
        }
        None => out.push(0),
    }
    out
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.frame_size == 0 || !args.senders.is_multiple_of(args.frame_size) {
        return Err("--frame-size must be nonzero and divide --senders".into());
    }
    let gateway = args
        .gateway_seed
        .as_deref()
        .map(|seed| -> Result<SigningKey, Box<dyn std::error::Error>> {
            let seed: [u8; 32] = hex::decode(seed.trim_start_matches("0x"))?
                .try_into()
                .map_err(|_| "--gateway-seed must be 32 bytes")?;
            Ok(SigningKey::from_bytes(&seed))
        })
        .transpose()?;

    let keys: Vec<SigningKey> = (0..args.senders).into_par_iter().map(sender_key).collect();
    if args.print_addresses {
        let addresses: Vec<String> = keys
            .iter()
            .map(|key| sender_address(key).to_string())
            .collect();
        println!("{}", addresses.join(","));
        if let Some(gateway) = &gateway {
            eprintln!(
                "gateway public key {}",
                hex::encode(gateway.verifying_key().as_bytes())
            );
        }
        return Ok(());
    }
    let endpoints: Vec<String> = args
        .ingest
        .as_deref()
        .ok_or("--ingest is required")?
        .split(',')
        .map(str::to_string)
        .collect();

    // Presign: frame (round j, group g) holds nonce j of senders g*frame_size..(g+1)*frame_size.
    let groups = args.senders / args.frame_size;
    let started = Instant::now();
    let frames: Vec<Vec<u8>> = (args.start_nonce..args.start_nonce + args.frames_per_sender)
        .flat_map(|nonce| (0..groups).map(move |group| (nonce, group)))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(nonce, group)| {
            let signed: Vec<(Vec<u8>, B256)> = keys
                [group * args.frame_size..(group + 1) * args.frame_size]
                .iter()
                .enumerate()
                .map(|(offset, key)| {
                    let sender = (group * args.frame_size + offset) as u64;
                    let tx = TxAltSig {
                        chain_id: args.chain_id,
                        nonce,
                        max_priority_fee_per_gas: 1_000_000_000,
                        max_fee_per_gas: 5_000_000_000,
                        gas_limit: 21_000,
                        to: recipient(args.recipients, nonce * args.senders as u64 + sender),
                        value: U256::from(1),
                        input: Bytes::new(),
                        access_list: Default::default(),
                        alg_type: ALG_ED25519,
                        pubkey: Bytes::copy_from_slice(key.verifying_key().as_bytes()),
                    }
                    .sign_ed25519(key);
                    (tx.encoded_2718(), *tx.hash())
                })
                .collect();
            let hashes: Vec<B256> = signed.iter().map(|(_, hash)| *hash).collect();
            let attestation = gateway
                .as_ref()
                .map(|gateway| FrameAttestation::sign(gateway, args.chain_id, frame_root(&hashes)));
            let encoded: Vec<Vec<u8>> = signed.into_iter().map(|(tx, _)| tx).collect();
            encode_frame(&encoded, attestation.as_ref())
        })
        .collect();
    let total_txs = frames.len() as u64 * args.frame_size as u64;
    eprintln!(
        "presigned {} frames ({total_txs} txs, attested: {}) in {:.1}s",
        frames.len(),
        gateway.is_some(),
        started.elapsed().as_secs_f64()
    );

    // A sender group is pinned to one connection (group g -> connection g mod C),
    // as n42-rs `tx_flood` pins a worker to one ingest: every nonce of a sender
    // lands in the same pool, in order. Each connection walks its groups nonce
    // round by nonce round.
    let frames = Arc::new(frames);
    let connections: Vec<String> = endpoints
        .iter()
        .flat_map(|endpoint| std::iter::repeat_n(endpoint.clone(), args.connections))
        .collect();
    let count = connections.len();
    let accepted = Arc::new(AtomicU64::new(0));
    let deferred = Arc::new(AtomicU64::new(0));
    let send_started = Instant::now();
    let workers: Vec<_> = connections
        .into_iter()
        .enumerate()
        .map(|(connection, endpoint)| {
            let (frames, accepted, deferred) = (frames.clone(), accepted.clone(), deferred.clone());
            let mut queue: VecDeque<usize> = (0..args.frames_per_sender as usize)
                .flat_map(|nonce| {
                    (connection..groups)
                        .step_by(count)
                        .map(move |group| nonce * groups + group)
                })
                .collect();
            let window = args.window.max(1);
            std::thread::spawn(move || -> std::io::Result<()> {
                let mut stream = TcpStream::connect(&endpoint)?;
                stream.set_nodelay(true)?;
                let mut in_flight = VecDeque::new();
                let mut retry = VecDeque::new();
                // A frame refused for anything but the pool gate would come back
                // refused forever; give up on it after ~10 s of retries.
                let mut attempts = std::collections::HashMap::<usize, u32>::new();
                loop {
                    while in_flight.len() < window {
                        let Some(index) = retry.pop_front().or_else(|| queue.pop_front()) else {
                            break;
                        };
                        stream.write_all(&frames[index])?;
                        in_flight.push_back(index);
                    }
                    let Some(index) = in_flight.pop_front() else {
                        break;
                    };
                    let mut ack = [0u8; 8];
                    stream.read_exact(&mut ack)?;
                    let count = u32::from_le_bytes(ack[..4].try_into().unwrap()) as u64;
                    if count == 0 {
                        deferred.fetch_add(1, Ordering::Relaxed);
                        let tries = attempts.entry(index).or_default();
                        *tries += 1;
                        if *tries < MAX_FRAME_ATTEMPTS {
                            retry.push_back(index);
                        } else {
                            eprintln!("frame {index} refused {tries} times; dropped");
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    } else {
                        accepted.fetch_add(count, Ordering::Relaxed);
                    }
                }
                stream.write_all(&0u32.to_le_bytes())?;
                Ok(())
            })
        })
        .collect();

    let reporter_accepted = accepted.clone();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reporter_done = done.clone();
    let reporter = std::thread::spawn(move || {
        let mut last = 0;
        while !reporter_done.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            let now = reporter_accepted.load(Ordering::Relaxed);
            eprintln!("accepted {now} (+{}/s)", now - last);
            last = now;
        }
    });
    for worker in workers {
        if let Err(error) = worker.join().expect("worker thread panicked") {
            eprintln!("connection error: {error}");
        }
    }
    done.store(true, Ordering::Relaxed);
    let _ = reporter.join();
    let elapsed = send_started.elapsed().as_secs_f64();
    let accepted = accepted.load(Ordering::Relaxed);
    println!(
        "accepted {accepted}/{total_txs} txs in {elapsed:.2}s ({:.0} tx/s ingest), deferred frames {}",
        accepted as f64 / elapsed,
        deferred.load(Ordering::Relaxed)
    );
    Ok(())
}
