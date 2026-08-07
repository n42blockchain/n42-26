use alloy_primitives::B256;
use clap::Parser;
use n42_twig_core::qmdb_compat::QmdbProof;

#[derive(Debug, Parser)]
#[command(
    name = "n42-qmdb-proof-verify",
    about = "Verify an untrusted replay-v2 QMDB archive proof offline"
)]
struct Args {
    /// Expected QMDB root (0x-prefixed 32-byte hex).
    #[arg(long)]
    root: B256,
    /// Expected QMDB key (0x-prefixed 32-byte hex).
    #[arg(long)]
    key: B256,
    /// QMDB proof returned by n42_qmdbArchiveProof (hex, with or without 0x).
    #[arg(long)]
    proof_hex: String,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let encoded = hex::decode(args.proof_hex.strip_prefix("0x").unwrap_or(&args.proof_hex))
        .map_err(|error| eyre::eyre!("invalid proof hex: {error}"))?;
    let proof = QmdbProof::decode(&encoded)
        .map_err(|error| eyre::eyre!("invalid QMDB proof encoding: {error}"))?;
    if !proof.verify_for_key(args.root.as_ref(), args.key.as_ref()) {
        return Err(eyre::eyre!(
            "QMDB proof does not authenticate the expected root and key"
        ));
    }

    println!(
        "{{\"ok\":true,\"root\":\"{}\",\"key\":\"{}\",\"slot\":{},\"value\":\"{}\"}}",
        args.root,
        args.key,
        proof.slot,
        hex::encode(&proof.value)
    );
    Ok(())
}
