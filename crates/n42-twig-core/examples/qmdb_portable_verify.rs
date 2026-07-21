//! Verify a gov5 replay-v2 QMDB portable snapshot without modifying any local
//! node database.

use std::{env, fs::File, io::BufReader, process};

use n42_twig_core::{Hash, qmdb_compat::verify_portable_stream};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut input = None;
    let mut chain_id = None;
    let mut genesis_hash = None;
    let mut expected_block = None;
    let mut expected_block_hash = None;
    let mut expected_root = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--input" => input = Some(value),
            "--chain-id" => {
                chain_id = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| format!("invalid --chain-id: {error}"))?,
                );
            }
            "--genesis" => genesis_hash = Some(parse_hash("--genesis", &value)?),
            "--expect-block" => {
                expected_block = Some(
                    value
                        .parse::<u64>()
                        .map_err(|error| format!("invalid --expect-block: {error}"))?,
                );
            }
            "--expect-block-hash" => {
                expected_block_hash = Some(parse_hash("--expect-block-hash", &value)?);
            }
            "--expect-root" => {
                expected_root = Some(parse_hash("--expect-root", &value)?);
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let input = input.ok_or_else(usage)?;
    let chain_id = chain_id.ok_or_else(usage)?;
    let genesis_hash = genesis_hash.ok_or_else(usage)?;
    let file = File::open(&input).map_err(|error| format!("open {input}: {error}"))?;
    let bytes = file
        .metadata()
        .map_err(|error| format!("stat {input}: {error}"))?
        .len();
    let verified = verify_portable_stream(BufReader::new(file), chain_id, &genesis_hash)
        .map_err(|error| error.to_string())?;
    if let Some(expected) = expected_block
        && verified.block_number != expected
    {
        return Err(format!(
            "checkpoint block {} does not equal expected {expected}",
            verified.block_number
        ));
    }
    if let Some(expected) = expected_block_hash
        && verified.block_hash != expected
    {
        return Err("checkpoint block hash does not equal expected hash".to_owned());
    }
    // Cross-client equality gate: without this the tool only proves the snapshot
    // is self-consistent for the target chain, not that its root equals gov5's
    // canonical replay root. `--expect-root <gov5-root>` makes the cross-check a
    // machine-checkable assertion (CI-friendly) instead of an eyeball comparison.
    if let Some(expected) = expected_root
        && verified.root != expected
    {
        return Err(format!(
            "QMDB root {} does not equal expected gov5 root {}",
            hex::encode(verified.root),
            hex::encode(expected)
        ));
    }
    println!(
        "verified chain_id={} block={} block_hash={} root={} slots={} live={} bytes={}",
        verified.chain_id,
        verified.block_number,
        hex::encode(verified.block_hash),
        hex::encode(verified.root),
        verified.next_slot,
        verified.live_count,
        bytes
    );
    Ok(())
}

fn parse_hash(name: &str, value: &str) -> Result<Hash, String> {
    let decoded = hex::decode(value.strip_prefix("0x").unwrap_or(value))
        .map_err(|error| format!("invalid {name}: {error}"))?;
    decoded
        .try_into()
        .map_err(|value: Vec<u8>| format!("invalid {name}: expected 32 bytes, got {}", value.len()))
}

fn usage() -> String {
    "usage: qmdb_portable_verify --input <file> --chain-id <id> --genesis <32-byte-hex> [--expect-block <n>] [--expect-block-hash <hex>] [--expect-root <32-byte-hex>]".to_owned()
}
