//! `n42-init-snapshot`: initialise an empty Reth datadir at a gov5 chain's
//! applied head from gov5's `n42-reth-state-dump` output.
//!
//! A gov5 chain that was seeded by replay-v2 (chain 94: 13.5 million blocks of
//! folded mainnet state, empty bodies) cannot be re-executed from block zero,
//! so a Rust member starts from a state snapshot instead. Reth's own
//! `init-state --without-evm` almost serves: it writes dummy headers below the
//! head, the head header, and the state, but it recomputes an Ethereum Merkle
//! root and refuses a header whose `stateRoot` is a QMDB root, and it decodes
//! the head header with alloy, which rejects gov5's native shape. This tool
//! does the same work with the gov5 header codec and the QMDB root taken from
//! the dump's own `{"root": …}` line, checked against the header and, when the
//! caller supplies the authenticated genesis range, against block zero.

use alloy_genesis::GenesisAccount;
use alloy_primitives::{Address, B256, U256, keccak256};
use clap::Parser;
use n42_consensus::{Gov5NativeHeader, remember_gov5_native_header};
use n42_network::decode_finalized_range_stream;
use n42_node::N42Node;
use n42_node::qmdb_state::{gov5_qmdb_genesis_tree, gov5_replay_execution_genesis};
use reth_chainspec::EthChainSpec;
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_cli_commands::init_state::without_evm::setup_without_evm;
use reth_db_api::{
    cursor::DbCursorRW,
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_primitives_traits::SealedHeader;
use reth_primitives_traits::{Account, Bytecode, StorageEntry};
use reth_provider::{
    BlockHashReader, BlockNumReader, DBProvider, DatabaseProviderFactory, StageCheckpointWriter,
    StaticFileProviderFactory, StaticFileWriter,
};
use reth_stages_types::{StageCheckpoint, StageId};
use reth_static_file_types::StaticFileSegment;
use reth_storage_api::StorageSettingsCache;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Parser)]
#[command(
    name = "n42-init-snapshot",
    about = "Initialise an empty Reth datadir at a gov5 chain's applied head from a state dump"
)]
struct Args {
    #[command(flatten)]
    env: EnvironmentArgs<EthereumChainSpecParser>,
    /// gov5's canonical header RLP for the applied head (raw or hex).
    #[arg(long)]
    header: PathBuf,
    /// gov5's reth init-state JSONL: a `{"root": …}` line followed by one
    /// account per line.
    #[arg(long)]
    state: PathBuf,
    /// The head's block hash as gov5 records it; refuses a header that does
    /// not hash to it.
    #[arg(long)]
    expected_hash: B256,
    /// Authenticated gov5 genesis range (block zero). Binds Reth's block zero
    /// to gov5's genesis header and checks the configured alloc against its
    /// QMDB state root.
    #[arg(long)]
    genesis_range: PathBuf,
    /// The chain's genesis hash, which the genesis range must authenticate.
    #[arg(long)]
    genesis_hash: B256,
    /// Accounts written per database transaction.
    #[arg(long, default_value_t = 50_000)]
    chunk: usize,
}

#[derive(Deserialize)]
struct RootLine {
    root: B256,
}

#[derive(Deserialize)]
struct AccountLine {
    address: Address,
    #[serde(flatten)]
    account: GenesisAccount,
}

fn read_header(path: &Path) -> eyre::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    if let Ok(text) = std::str::from_utf8(&bytes)
        && let Ok(decoded) = hex::decode(text.trim().trim_start_matches("0x"))
    {
        return Ok(decoded);
    }
    Ok(bytes)
}

fn main() -> eyre::Result<()> {
    let mut args = Args::parse();
    let runner = reth_cli_runner::CliRunner::try_default_runtime()?;
    let runtime = runner.runtime();

    let chain_id = args.env.chain.chain().id();
    let range_file = File::open(&args.genesis_range)?;
    let range =
        decode_finalized_range_stream(BufReader::new(range_file), chain_id, args.genesis_hash)
            .map_err(|error| eyre::eyre!("genesis range authentication failed: {error}"))?;
    let genesis_entry = range
        .entries()
        .first()
        .filter(|entry| entry.number() == 0 && entry.block_hash() == args.genesis_hash)
        .ok_or_else(|| eyre::eyre!("genesis range must start with authenticated block zero"))?;
    let native_root = B256::from(gov5_qmdb_genesis_tree(&args.env.chain.genesis)?.root());
    let replay_root = B256::from(
        gov5_qmdb_genesis_tree(&gov5_replay_execution_genesis(&args.env.chain.genesis))?.root(),
    );
    let genesis_state_root = genesis_entry.state_root();
    let genesis_profile = if native_root == genesis_state_root {
        "native"
    } else if replay_root == genesis_state_root {
        "replay-v2"
    } else {
        return Err(eyre::eyre!(
            "configured alloc produces QMDB roots {native_root} (native) / {replay_root} (replay-v2), but gov5 block zero commits {genesis_state_root}"
        ));
    };
    println!(
        "genesis: block 0 {} state root {genesis_state_root} reproduced by the {genesis_profile} alloc profile",
        args.genesis_hash
    );
    // Reth writes block zero from the chain spec; gov5's block zero carries a
    // QMDB state root and gov5's own field shape, so bind the spec to the
    // authenticated header and hash before the datadir is created.
    {
        let chain = Arc::make_mut(&mut args.env.chain);
        chain.genesis_header = SealedHeader::new(genesis_entry.header().clone(), args.genesis_hash);
    }

    let raw_header = read_header(&args.header)?;
    let native = Gov5NativeHeader::decode(&raw_header)
        .map_err(|error| eyre::eyre!("head header does not decode as a gov5 header: {error}"))?;
    if native.encode() != raw_header {
        return Err(eyre::eyre!(
            "head header re-encodes differently; refusing to seal it"
        ));
    }
    let hash = native.hash();
    if hash != args.expected_hash {
        return Err(eyre::eyre!(
            "head header hashes to {hash}, expected {}",
            args.expected_hash
        ));
    }
    remember_gov5_native_header(&raw_header);
    let number = native.header.number;
    let state_root = native.header.state_root;
    println!(
        "head: block {number} {hash} state root {state_root} mobile registry root {:?} timestamp {}",
        native.mobile_registry_root, native.header.timestamp
    );

    let Environment {
        provider_factory, ..
    } = args.env.init::<N42Node>(AccessRights::RW, runtime)?;

    {
        let provider_rw = provider_factory.database_provider_rw()?;
        let last = provider_rw.last_block_number()?;
        if last == 0 && number > 0 {
            setup_without_evm(
                &provider_rw,
                SealedHeader::new(native.header.clone(), hash),
                |n| alloy_consensus::Header {
                    number: n,
                    ..Default::default()
                },
            )?;
            provider_factory.static_file_provider().commit()?;
        } else if last != number {
            return Err(eyre::eyre!(
                "the datadir is at block {last}, the snapshot at {number}; init needs an empty datadir"
            ));
        }
        provider_rw.commit()?;
    }
    // The dummy chain fills only the body segments. The changeset segments
    // must reach the head as well: the history invariants compare their tip
    // with the index-history stage checkpoints and demand an unwind to block
    // zero otherwise. A snapshot has no changes below its head, so every
    // block up to and including the head gets an empty changeset.
    {
        let static_file_provider = provider_factory.static_file_provider();
        for segment in [
            StaticFileSegment::AccountChangeSets,
            StaticFileSegment::StorageChangeSets,
        ] {
            let mut writer = static_file_provider.latest_writer(segment)?;
            let from = writer.next_block_number();
            for block in from..=number {
                match segment {
                    StaticFileSegment::AccountChangeSets => {
                        writer.append_account_changeset(Vec::new(), block)?
                    }
                    _ => writer.append_storage_changeset(Vec::new(), block)?,
                }
            }
            println!("static files: {segment:?} filled with empty changesets {from}..={number}");
        }
        static_file_provider.commit()?;
    }
    {
        let provider = provider_factory.database_provider_ro()?;
        let stored = provider.block_hash(number)?;
        if stored != Some(hash) {
            return Err(eyre::eyre!(
                "canonical hash for block {number} is {stored:?} after setup, expected {hash}"
            ));
        }
    }

    let reader = BufReader::new(File::open(&args.state)?);
    let mut lines = reader.lines();
    let first = lines
        .next()
        .ok_or_else(|| eyre::eyre!("state file is empty"))??;
    let dump_root: RootLine = serde_json::from_str(&first)
        .map_err(|error| eyre::eyre!("first state line must be {{\"root\": …}}: {error}"))?;
    if dump_root.root != state_root {
        return Err(eyre::eyre!(
            "state dump root {} is not the head header's state root {state_root}",
            dump_root.root
        ));
    }

    // Reth's `insert_state` also writes changesets and history for the
    // block, which the static-file changeset segments refuse for a block
    // that is not the next one after their (empty) dummy range. A snapshot
    // has no history below its head, so write only the state tables the
    // layout reads: hashed accounts/storage (both layouts) and the plain
    // tables (legacy layout), plus bytecodes.
    let storage_v2 = provider_factory
        .database_provider_ro()?
        .cached_storage_settings()
        .storage_v2;
    println!(
        "state layout: {}",
        if storage_v2 {
            "v2 (hashed only)"
        } else {
            "v1 (plain + hashed)"
        }
    );
    let started = std::time::Instant::now();
    let mut accounts = 0_u64;
    let mut slots = 0_u64;
    let mut chunk: Vec<(Address, GenesisAccount)> = Vec::with_capacity(args.chunk);
    let flush = |chunk: &mut Vec<(Address, GenesisAccount)>| -> eyre::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let provider_rw = provider_factory.database_provider_rw()?;
        let tx = provider_rw.tx_ref();
        let mut hashed_storage = tx.cursor_dup_write::<tables::HashedStorages>()?;
        let mut plain_storage = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        for (address, genesis_account) in chunk.iter() {
            let bytecode_hash = match &genesis_account.code {
                Some(code) => {
                    let bytecode = Bytecode::new_raw_checked(code.clone())
                        .map_err(|error| eyre::eyre!("invalid bytecode for {address}: {error}"))?;
                    let hash = bytecode.hash_slow();
                    if tx.get::<tables::Bytecodes>(hash)?.is_none() {
                        tx.put::<tables::Bytecodes>(hash, bytecode)?;
                    }
                    Some(hash)
                }
                None => None,
            };
            let account = Account {
                nonce: genesis_account.nonce.unwrap_or_default(),
                balance: genesis_account.balance,
                bytecode_hash,
            };
            let hashed_address = keccak256(address);
            tx.put::<tables::HashedAccounts>(hashed_address, account)?;
            if !storage_v2 {
                tx.put::<tables::PlainAccountState>(*address, account)?;
            }
            if let Some(storage) = &genesis_account.storage {
                for (&key, &value) in storage {
                    let value = U256::from_be_bytes(value.0);
                    if value.is_zero() {
                        continue;
                    }
                    hashed_storage.upsert(
                        hashed_address,
                        &StorageEntry {
                            key: keccak256(key),
                            value,
                        },
                    )?;
                    if !storage_v2 {
                        plain_storage.upsert(*address, &StorageEntry { key, value })?;
                    }
                }
            }
        }
        drop(hashed_storage);
        drop(plain_storage);
        provider_rw.commit()?;
        chunk.clear();
        Ok(())
    };
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: AccountLine = serde_json::from_str(&line)
            .map_err(|error| eyre::eyre!("account line {}: {error}", accounts + 1))?;
        slots += parsed
            .account
            .storage
            .as_ref()
            .map_or(0, |storage| storage.len() as u64);
        accounts += 1;
        chunk.push((parsed.address, parsed.account));
        if chunk.len() >= args.chunk {
            flush(&mut chunk)?;
            println!(
                "state: {accounts} accounts, {slots} slots written ({:.0?})",
                started.elapsed()
            );
        }
    }
    flush(&mut chunk)?;

    {
        let provider_rw = provider_factory.database_provider_rw()?;
        for stage in StageId::ALL {
            provider_rw.save_stage_checkpoint(stage, StageCheckpoint::new(number))?;
        }
        provider_rw.commit()?;
        provider_factory.static_file_provider().commit()?;
    }
    println!(
        "state: {accounts} accounts, {slots} slots; datadir is at block {number} {hash} ({:.0?})",
        started.elapsed()
    );
    Ok(())
}
