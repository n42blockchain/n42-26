//! Fresh four- or seven-validator native H2/QMDB bootstrap, using the existing wire codecs.
use alloy_consensus::TxEnvelope;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, keccak256};
use alloy_signer_local::PrivateKeySigner;
use clap::Parser;
use n42_chainspec::{ConsensusConfig, ValidatorInfo};
use n42_consensus::Gov5NativeHeader;
use n42_network::decode_finalized_range_stream;
use n42_node::qmdb_state::gov5_qmdb_genesis_tree;
use n42_primitives::BlsSecretKey;
use n42_twig_core::qmdb_compat::{QmdbPortableSnapshot, QmdbSlotSnapshot};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    name = "n42-native-fleet",
    about = "Generate a new four- or seven-node native H2/QMDB test fleet; refuses to overwrite"
)]
struct Args {
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 4)]
    validators: usize,
    #[arg(long, default_value_t = 941004)]
    chain_id: u64,
    #[arg(long, default_value_t = 5000)]
    senders: usize,
    #[arg(long, default_value_t = 147000)]
    recipients: usize,
    #[arg(long, default_value_t = 200)]
    slot_ms: u64,
    #[arg(long, default_value_t = 5_000_000_000)]
    gas_limit: u64,
    #[arg(long, default_value_t = 220_000)]
    max_txs_per_block: usize,
}

fn write_new(path: &Path, data: &[u8]) -> eyre::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn blob(frame: &mut Vec<u8>, bytes: &[u8]) -> eyre::Result<()> {
    frame.extend_from_slice(&u32::try_from(bytes.len())?.to_le_bytes());
    frame.extend_from_slice(bytes);
    Ok(())
}

fn genesis_range(chain_id: u64, native: &Gov5NativeHeader) -> eyre::Result<Vec<u8>> {
    let header = native.encode();
    let hash = native.hash();
    // Gov5 block body: header, transactions, verifiers, rewards. All lists empty at genesis.
    let mut block = Vec::new();
    alloy_rlp::Header {
        list: true,
        payload_length: header.len() + 3,
    }
    .encode(&mut block);
    block.extend_from_slice(&header);
    block.extend_from_slice(&[0xc0; 3]);
    let mut frame = b"N42FRNG\x01".to_vec();
    frame.extend_from_slice(&chain_id.to_le_bytes());
    frame.extend_from_slice(hash.as_slice());
    for value in [0u64, 0, 1, 0] {
        frame.extend_from_slice(&value.to_le_bytes());
    }
    for value in [
        hash,
        B256::ZERO,
        native.header.state_root,
        native.header.receipts_root,
        native.header.transactions_root,
    ] {
        frame.extend_from_slice(value.as_slice());
    }
    blob(&mut frame, &header)?;
    blob(&mut frame, &block)?;
    blob(&mut frame, &[])?;
    let digest = blake3::hash(&frame);
    frame.extend_from_slice(digest.as_bytes());
    let checked = decode_finalized_range_stream(frame.as_slice(), chain_id, hash)?;
    eyre::ensure!(
        checked.entries().len() == 1
            && checked.entries()[0].state_root() == native.header.state_root,
        "genesis range round-trip mismatch"
    );
    Ok(frame)
}

fn generate(args: Args) -> eyre::Result<()> {
    eyre::ensure!(
        matches!(args.validators, 4 | 7),
        "validator count must be four or seven"
    );
    eyre::ensure!(
        args.chain_id != 0 && args.chain_id != 94,
        "use a new test chain identity, not live chain 94"
    );
    eyre::ensure!(
        (1..=100_000).contains(&args.senders) && args.recipients <= 1_000_000,
        "account count outside generator bounds"
    );
    eyre::ensure!(
        args.slot_ms > 0 && args.gas_limit >= 21_000,
        "invalid block capacity or timing"
    );
    eyre::ensure!(
        args.max_txs_per_block > 0
            && args.max_txs_per_block as u128 * 21_000 <= args.gas_limit as u128,
        "transaction cap exceeds genesis gas capacity"
    );
    eyre::ensure!(
        !args.output.exists(),
        "output already exists; choose a fresh fleet directory"
    );
    fs::create_dir_all(args.output.parent().unwrap_or(Path::new(".")))?;
    fs::create_dir(&args.output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&args.output, fs::Permissions::from_mode(0o700))?;
    }
    for dir in ["artifacts", "logs", "pids"] {
        fs::create_dir(args.output.join(dir))?;
    }
    let mut consensus = ConsensusConfig::dev_multi(args.validators);
    consensus.initial_validators.clear();
    consensus.slot_time_ms = args.slot_ms;
    consensus.base_timeout_ms = args.slot_ms.saturating_mul(10).max(2000);
    consensus.max_timeout_ms = consensus.base_timeout_ms.saturating_mul(4);
    consensus.epoch_length = 1000;
    let mut alloc = BTreeMap::new();
    let mut peers = Vec::new();
    let mut hotstuff_validators = Vec::new();
    for index in 0..args.validators {
        let node = args.output.join(format!("node{index}"));
        fs::create_dir(&node)?;
        fs::create_dir(node.join("consensus"))?;
        let secret = BlsSecretKey::random()?;
        let public = secret.public_key();
        // Same address derivation as Gov5 cmd/hotstuff-testnet: first 20 BLS public-key bytes.
        let address = Address::from_slice(&public.to_bytes()[..20]);
        let identity = libp2p::identity::Keypair::generate_ed25519();
        let peer = identity.public().to_peer_id().to_string();
        let ed = identity.try_into_ed25519()?;
        write_new(
            &node.join("bls.key"),
            hex::encode(secret.to_bytes()).as_bytes(),
        )?;
        write_new(
            &node.join("p2p.key"),
            hex::encode(ed.secret().as_ref()).as_bytes(),
        )?;
        hotstuff_validators.push(
            json!({"address":address,"blsKey":format!("0x{}",hex::encode(public.to_bytes()))}),
        );
        consensus.initial_validators.push(ValidatorInfo {
            address,
            bls_public_key: public,
            p2p_peer_id: Some(peer.clone()),
        });
        peers.push(peer);
        // A non-empty beneficiary is required by the checked transfer path.
        alloc.insert(address, json!({"balance":"0x1"}));
    }
    let mut accounts = Vec::with_capacity(args.senders);
    for index in 0..args.senders {
        let key = keccak256(format!("n42-test-key-{index}"));
        let signer = PrivateKeySigner::from_bytes(&key)?;
        eyre::ensure!(
            alloc
                .insert(
                    signer.address(),
                    json!({"balance":"0x4B3B4CA85A86C47A098A224000000"})
                )
                .is_none(),
            "sender collides with validator"
        );
        accounts.push(json!({"address":signer.address(),"key":hex::encode(key)}));
    }
    let mut recipients = Vec::with_capacity(args.recipients);
    for index in 0..args.recipients {
        let hash = keccak256(format!("n42-recipient-{index}"));
        let address = Address::from_slice(&hash[12..]);
        eyre::ensure!(
            alloc.insert(address, json!({"balance":"0x1"})).is_none(),
            "recipient collision"
        );
        recipients.push(address);
    }
    let genesis_value = json!({
        "config": {
            "chainId":args.chain_id,"homesteadBlock":0,"eip150Block":0,"eip155Block":0,"eip158Block":0,
            "byzantiumBlock":0,"constantinopleBlock":0,"petersburgBlock":0,"istanbulBlock":0,"muirGlacierBlock":0,
            "berlinBlock":0,"londonBlock":0,"arrowGlacierBlock":0,"grayGlacierBlock":0,"mergeNetsplitBlock":0,
            "shanghaiTime":0,"cancunTime":0,"mobileAnchorTime":0,"terminalTotalDifficulty":"0x0",
            "terminalTotalDifficultyPassed":true,"consensus":"hotstuff","stateScheme":"qmdb",
            "hotstuff":{"period":1,"baseTimeout":consensus.base_timeout_ms,"maxTimeout":consensus.max_timeout_ms,
                "epochLength":1000,"minProposeDelayMs":args.slot_ms,"validators":hotstuff_validators,
                "devBlockReward":0,"devFaucetAddress":Address::ZERO}
        },
        "nonce":"0x0","timestamp":"0x0","extraData":"0x","gasLimit":format!("0x{:x}",args.gas_limit),
        "difficulty":"0x0","mixHash":B256::ZERO,"coinbase":consensus.initial_validators[0].address,
        "alloc":alloc,"baseFeePerGas":"0x7","excessBlobGas":"0x0","blobGasUsed":"0x0"
    });
    let genesis: Genesis = serde_json::from_value(genesis_value.clone())?;
    let tree = gov5_qmdb_genesis_tree(&genesis)?;
    let chain = reth_chainspec::ChainSpec::from_genesis(genesis);
    let mut header = chain.genesis_header().clone();
    header.state_root = B256::from(tree.root());
    // Gov5 hotstuff genesis uses Ethereum empty trie/ommer roots and Cancun fields;
    // mobileRegistryRoot is first stamped by its live miner, not genesis seeding.
    eyre::ensure!(
        header.transactions_root
            == alloy_consensus::proofs::calculate_transaction_root::<TxEnvelope>(&[]),
        "unexpected genesis transaction root"
    );
    let native = Gov5NativeHeader {
        header,
        mobile_registry_root: None,
    };
    let hash = native.hash();
    let portable = QmdbPortableSnapshot {
        chain_id: args.chain_id,
        genesis_hash: hash.0,
        block_number: 0,
        block_hash: hash.0,
        root: tree.root(),
        slots: QmdbSlotSnapshot {
            next_slot: tree.next_slot(),
            entries: Vec::new(),
        },
        leaf_form: Some(tree.leaf_form()),
    };
    let snapshot = portable.encode()?;
    let decoded = QmdbPortableSnapshot::decode(&snapshot)?;
    eyre::ensure!(
        decoded.verify_and_build(args.chain_id, &hash.0)?.root() == tree.root(),
        "QMDB snapshot round-trip mismatch"
    );
    consensus.validate().map_err(eyre::Error::msg)?;
    eyre::ensure!(
        consensus.quorum_size() == (args.validators - (args.validators - 1) / 3) as u32,
        "H2 quorum does not match the generated validator roster"
    );
    let trusted = json!({"schema":1,"profile":"h2v4","chainId":args.chain_id,"genesisHash":hash,
        "faultTolerance":consensus.fault_tolerance,"validatorChangesHash":B256::ZERO,
        "validators":consensus.initial_validators.iter().map(|v| hex::encode(v.bls_public_key.to_bytes())).collect::<Vec<_>>()});
    let mut artifacts = BTreeMap::new();
    for (name, bytes) in [
        (
            "artifacts/genesis.json",
            serde_json::to_vec_pretty(&genesis_value)?,
        ),
        (
            "artifacts/genesis-range.n42frng",
            genesis_range(args.chain_id, &native)?,
        ),
        ("artifacts/genesis.header.rlp", native.encode()),
        ("artifacts/snapshot.qmdb", snapshot),
        ("consensus.json", serde_json::to_vec_pretty(&consensus)?),
        ("trusted-config.json", serde_json::to_vec_pretty(&trusted)?),
        ("test-accounts.json", serde_json::to_vec(&accounts)?),
        ("recipients.json", serde_json::to_vec(&recipients)?),
    ] {
        artifacts.insert(name, hex::encode(Sha256::digest(&bytes)));
        write_new(&args.output.join(name), &bytes)?;
    }
    let artifact_hashes: BTreeMap<_, _> = artifacts
        .iter()
        .filter_map(|(name, hash)| name.strip_prefix("artifacts/").map(|name| (name, hash)))
        .collect();
    let runtime_hashes: BTreeMap<_, _> = artifacts
        .iter()
        .filter(|(name, _)| !name.starts_with("artifacts/"))
        .collect();
    let manifest = json!({
        "profile":"fresh-native-h2-qmdb-cancun", "node_count":args.validators,"chain_id":args.chain_id,
        "genesis_hash":hash,"peers":peers,"prague_time":null,
        "snapshot":{"block_number":0,"block_hash":hash,"state_root":native.header.state_root},
        "ports":{"http":23400,"auth":23500,"metrics":23600,"consensus":33400,"p2p":31400,"mobile":9840,"ingest":34400},
        "senders":args.senders,"recipients":args.recipients,"gas_limit":args.gas_limit,
        "max_txs_per_block":args.max_txs_per_block,"build_budget_ms":args.slot_ms,"artifacts_sha256":artifact_hashes,"runtime_sha256":runtime_hashes,
        "source":"new genesis; no existing chain snapshot or signed roster was altered",
        "recipient_rule":"last20(keccak256(UTF8(n42-recipient-{decimal index})))",
        "sender_rule":"secp256k1(keccak256(UTF8(n42-test-key-{decimal index})))"
    });
    write_new(
        &args.output.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!(
        "Prepared {} H2 validators (f={}, quorum={}), {} senders, {} recipients; genesis {} QMDB {}",
        args.validators,
        consensus.fault_tolerance,
        consensus.quorum_size(),
        args.senders,
        args.recipients,
        hash,
        native.header.state_root
    );
    Ok(())
}

fn main() -> eyre::Result<()> {
    generate(Args::parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_seven_validator_fleet() {
        let args = Args::try_parse_from([
            "n42-native-fleet",
            "--output",
            "/tmp/unused",
            "--validators",
            "7",
        ])
        .expect("seven validators should be a supported fleet size");
        assert_eq!(args.validators, 7);
    }

    #[test]
    fn generated_seven_node_genesis_satisfies_native_reward_source() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("seven");
        generate(Args {
            output: output.clone(),
            validators: 7,
            chain_id: 941007,
            senders: 1,
            recipients: 0,
            slot_ms: 200,
            gas_limit: 5_000_000_000,
            max_txs_per_block: 220_000,
        })
        .unwrap();
        let genesis: Genesis =
            serde_json::from_slice(&fs::read(output.join("artifacts/genesis.json")).unwrap())
                .unwrap();
        n42_node::sinks::Gov5WithdrawalSource::from_genesis(&genesis, Address::ZERO).unwrap();
    }
}
