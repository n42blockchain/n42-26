use alloy_primitives::{B256, keccak256};
use clap::Parser;
use n42_chainspec::ConsensusConfig;
use n42_consensus_service::{
    bootstrap::{BOOTSTRAP_BUNDLE_VERSION, Gov5BootstrapBundle, Gov5BootstrapPayload},
    persistence::{ConsensusSnapshot, load_consensus_state},
};
use n42_network::{
    h2_wire::decode_h2_qc, quorum_certificate_from_h2, verify_finalized_range_stream,
};
use n42_primitives::consensus::H2V4ChainIdentity;
use n42_twig_core::qmdb_compat::verify_portable_stream;
use std::{
    fs::OpenOptions,
    io::{BufReader, Cursor, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Parser)]
#[command(
    name = "n42-bootstrap",
    about = "Build a self-contained authenticated Gov5 participant bootstrap bundle"
)]
struct Args {
    #[arg(long)]
    consensus_config: PathBuf,
    /// Native Rust consensus snapshot. Mutually exclusive with
    /// --gov5-hotstuff-state-hex.
    #[arg(long, conflicts_with = "gov5_hotstuff_state_hex")]
    consensus_state: Option<PathBuf>,
    /// Hex value stored at Gov5 HotStuffState["state"]:
    /// view(8 LE) + consecutiveTimeouts(4 LE) + lockedQC_len(4 LE) +
    /// lockedQC(SSZ) + committedQC(SSZ).
    #[arg(long, conflicts_with = "consensus_state")]
    gov5_hotstuff_state_hex: Option<String>,
    #[arg(long)]
    genesis_range: PathBuf,
    #[arg(long)]
    finalized_range: PathBuf,
    #[arg(long)]
    qmdb_checkpoint: PathBuf,
    #[arg(long)]
    chain_id: u64,
    #[arg(long)]
    genesis_hash: B256,
    #[arg(long)]
    sequence: u64,
    #[arg(long)]
    output: PathBuf,
}

fn read_u32_le(bytes: &[u8], offset: usize, label: &str) -> eyre::Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| eyre::eyre!("Gov5 HotStuff state is truncated before {label}"))?;
    Ok(u32::from_le_bytes(value.try_into().expect("fixed width")))
}

fn read_u64_le(bytes: &[u8], offset: usize, label: &str) -> eyre::Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| eyre::eyre!("Gov5 HotStuff state is truncated before {label}"))?;
    Ok(u64::from_le_bytes(value.try_into().expect("fixed width")))
}

fn snapshot_from_gov5_state(
    encoded_hex: &str,
    committed_block_count: u64,
    validators: &[n42_chainspec::ValidatorInfo],
    fault_tolerance: u32,
) -> eyre::Result<ConsensusSnapshot> {
    let encoded_hex = encoded_hex.strip_prefix("0x").unwrap_or(encoded_hex);
    let bytes =
        hex::decode(encoded_hex).map_err(|error| eyre::eyre!("invalid Gov5 state hex: {error}"))?;
    if bytes.len() < 16 {
        return Err(eyre::eyre!("Gov5 HotStuff state is shorter than 16 bytes"));
    }
    let current_view = read_u64_le(&bytes, 0, "view")?;
    let consecutive_timeouts = read_u32_le(&bytes, 8, "consecutive timeout count")?;
    let locked_len = usize::try_from(read_u32_le(&bytes, 12, "locked QC length")?)?;
    let locked_end = 16usize
        .checked_add(locked_len)
        .ok_or_else(|| eyre::eyre!("Gov5 locked QC length overflows"))?;
    let locked_bytes = bytes
        .get(16..locked_end)
        .ok_or_else(|| eyre::eyre!("Gov5 HotStuff state has a truncated locked QC"))?;
    let committed_bytes = bytes
        .get(locked_end..)
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| eyre::eyre!("Gov5 HotStuff state has no committed QC"))?;
    let locked_qc = quorum_certificate_from_h2(
        decode_h2_qc(locked_bytes)
            .map_err(|error| eyre::eyre!("invalid Gov5 locked QC: {error}"))?,
    )
    .map_err(|error| eyre::eyre!("invalid Gov5 locked QC: {error}"))?;
    let last_committed_qc = quorum_certificate_from_h2(
        decode_h2_qc(committed_bytes)
            .map_err(|error| eyre::eyre!("invalid Gov5 committed QC: {error}"))?,
    )
    .map_err(|error| eyre::eyre!("invalid Gov5 committed QC: {error}"))?;
    if current_view <= last_committed_qc.view {
        return Err(eyre::eyre!(
            "Gov5 current view {current_view} does not follow committed QC view {}",
            last_committed_qc.view
        ));
    }
    Ok(ConsensusSnapshot {
        version: 5,
        current_view,
        locked_qc,
        last_committed_qc: last_committed_qc.clone(),
        consecutive_timeouts,
        scheduled_epoch_transition: None,
        authorized_verifiers: Vec::new(),
        committed_block_count,
        // This validator was absent while Gov5 formed the checkpoint. Setting
        // both clocks to the last committed view prevents any historical vote
        // after bootstrap without claiming it signed one.
        last_voted_view: last_committed_qc.view,
        last_commit_voted_view: last_committed_qc.view,
        current_epoch_validators: Some((0, validators.to_vec(), fault_tolerance)),
        execution_validated_head_view: last_committed_qc.view,
        execution_validated_head_hash: last_committed_qc.block_hash,
    })
}

fn read(path: &Path) -> eyre::Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| eyre::eyre!("failed to read {}: {error}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    if args.sequence == 0 {
        return Err(eyre::eyre!("--sequence must be greater than zero"));
    }
    let config = ConsensusConfig::from_file(&args.consensus_config)
        .map_err(|error| eyre::eyre!("invalid consensus config: {error}"))?;
    if config.initial_validators.is_empty() {
        return Err(eyre::eyre!(
            "bootstrap creation requires an explicit validator set"
        ));
    }
    let genesis_range = read(&args.genesis_range)?;
    let finalized_range = read(&args.finalized_range)?;
    let qmdb_checkpoint = read(&args.qmdb_checkpoint)?;

    let qmdb = verify_portable_stream(
        Cursor::new(&qmdb_checkpoint),
        args.chain_id,
        &args.genesis_hash.into(),
    )
    .map_err(|error| eyre::eyre!("QMDB checkpoint authentication failed: {error}"))?;
    let range = verify_finalized_range_stream(
        BufReader::new(Cursor::new(&finalized_range)),
        args.chain_id,
        args.genesis_hash,
    )
    .map_err(|error| eyre::eyre!("finalized range authentication failed: {error}"))?;
    let genesis = verify_finalized_range_stream(
        BufReader::new(Cursor::new(&genesis_range)),
        args.chain_id,
        args.genesis_hash,
    )
    .map_err(|error| eyre::eyre!("genesis range authentication failed: {error}"))?;
    if genesis.from_block != 0 || genesis.to_block != 0 {
        return Err(eyre::eyre!(
            "genesis range must contain exactly authenticated block zero"
        ));
    }
    if range.from_block != 1
        || range.to_block != qmdb.block_number
        || range.last_block_hash != B256::from(qmdb.block_hash)
        || range.last_state_root != B256::from(qmdb.root)
    {
        return Err(eyre::eyre!(
            "finalized range and QMDB checkpoint do not identify the same execution head"
        ));
    }
    let snapshot = if let Some(path) = &args.consensus_state {
        load_consensus_state(path)?
            .ok_or_else(|| eyre::eyre!("consensus snapshot does not exist"))?
    } else if let Some(state) = &args.gov5_hotstuff_state_hex {
        snapshot_from_gov5_state(
            state,
            qmdb.block_number,
            &config.initial_validators,
            config.fault_tolerance,
        )?
    } else {
        return Err(eyre::eyre!(
            "one of --consensus-state or --gov5-hotstuff-state-hex is required"
        ));
    };
    if snapshot.committed_block_count != qmdb.block_number
        || snapshot.last_committed_qc.block_hash != B256::from(qmdb.block_hash)
        || snapshot.execution_validated_head_hash != B256::from(qmdb.block_hash)
        || snapshot.execution_validated_head_view != snapshot.last_committed_qc.view
    {
        return Err(eyre::eyre!(
            "consensus snapshot, execution-validity guard, and QMDB checkpoint are mixed or regressing"
        ));
    }

    let issued_at_unix_secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let replay_nonce = keccak256(
        [
            args.chain_id.to_le_bytes().as_slice(),
            args.sequence.to_le_bytes().as_slice(),
            issued_at_unix_secs.to_le_bytes().as_slice(),
            args.genesis_hash.as_slice(),
            args.output.as_os_str().as_encoded_bytes(),
        ]
        .concat(),
    );
    let payload = Gov5BootstrapPayload {
        format_version: BOOTSTRAP_BUNDLE_VERSION,
        sequence: args.sequence,
        replay_nonce,
        issued_at_unix_secs,
        chain_id: args.chain_id,
        genesis_hash: args.genesis_hash,
        checkpoint_block: qmdb.block_number,
        checkpoint_block_hash: B256::from(qmdb.block_hash),
        checkpoint_qmdb_root: B256::from(qmdb.root),
        commit_qc: snapshot.last_committed_qc,
        locked_qc: snapshot.locked_qc,
        next_view: snapshot.current_view,
        validators: config.initial_validators.clone(),
        fault_tolerance: config.fault_tolerance,
        last_execution_validated_view: snapshot.execution_validated_head_view,
        genesis_range,
        finalized_range,
        qmdb_checkpoint,
    };
    let bundle = Gov5BootstrapBundle::seal(payload)?;
    bundle.verify(
        H2V4ChainIdentity {
            chain_id: args.chain_id,
            genesis_hash: args.genesis_hash,
        },
        &config.initial_validators,
        config.fault_tolerance,
    )?;
    atomic_write(&args.output, &serde_json::to_vec_pretty(&bundle)?)?;
    println!(
        "wrote {} (sequence {}, digest {}, checkpoint {} {})",
        args.output.display(),
        bundle.payload.sequence,
        bundle.content_digest,
        bundle.payload.checkpoint_block,
        bundle.payload.checkpoint_block_hash
    );
    Ok(())
}
