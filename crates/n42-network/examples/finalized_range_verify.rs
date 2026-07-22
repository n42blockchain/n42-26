use alloy_primitives::B256;
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
    let materialized =
        decode_finalized_range_stream(BufReader::new(File::open(&path)?), chain_id, genesis)?;
    let verified = materialized.verification();
    let withdrawals = materialized
        .entries()
        .iter()
        .filter(|entry| entry.header().withdrawals_root.is_some())
        .count();
    let beacon_roots = materialized
        .entries()
        .iter()
        .filter(|entry| entry.header().parent_beacon_block_root.is_some())
        .count();
    let requests = materialized
        .entries()
        .iter()
        .filter(|entry| entry.header().requests_hash.is_some())
        .count();
    let block_access_lists = materialized
        .entries()
        .iter()
        .filter(|entry| entry.header().block_access_list_hash.is_some())
        .count();
    println!(
        "verified chain={} blocks={}-{} count={} txs={} materialized={} parent={:#x} head={:#x} state_root={:#x} receipts_root={:#x}",
        verified.chain_id,
        verified.from_block,
        verified.to_block,
        verified.block_count,
        verified.transaction_count,
        materialized.entries().len(),
        verified.first_parent_hash,
        verified.last_block_hash,
        verified.last_state_root,
        verified.last_receipts_root
    );
    println!(
        "payload_profile ommers_hash={:#x} withdrawals={} beacon_roots={} requests={} block_access_lists={}",
        materialized.entries()[0].header().ommers_hash,
        withdrawals,
        beacon_roots,
        requests,
        block_access_lists
    );
    Ok(())
}
