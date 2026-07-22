use alloy_primitives::B256;
use n42_consensus::N42HeaderProfile;
use n42_consensus_service::build_replay_execution_plan_with_profile;
use n42_network::decode_finalized_range_stream;
use std::{fs::File, io::BufReader, path::PathBuf};

fn main() -> eyre::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(
        args.next()
            .ok_or_else(|| eyre::eyre!("missing bundle path"))?,
    );
    let chain_id = args
        .next()
        .ok_or_else(|| eyre::eyre!("missing chain id"))?
        .to_string_lossy()
        .parse::<u64>()?;
    let genesis = args
        .next()
        .ok_or_else(|| eyre::eyre!("missing genesis hash"))?
        .to_string_lossy()
        .parse::<B256>()?;
    let header_profile = match args.next().as_deref() {
        None => N42HeaderProfile::Ethereum,
        Some(value) if value == "gov5-h2" => N42HeaderProfile::Gov5H2,
        Some(value) => {
            return Err(eyre::eyre!(
                "unsupported header profile {}",
                value.to_string_lossy()
            ));
        }
    };

    let range =
        decode_finalized_range_stream(BufReader::new(File::open(path)?), chain_id, genesis)?;
    let plan = build_replay_execution_plan_with_profile(&range, header_profile)?;
    let verification = plan.verification();
    println!(
        "planned blocks={}-{} payloads={} txs={} parent={:#x} head={:#x}",
        verification.from_block,
        verification.to_block,
        plan.payloads().len(),
        verification.transaction_count,
        verification.first_parent_hash,
        verification.last_block_hash
    );
    Ok(())
}
