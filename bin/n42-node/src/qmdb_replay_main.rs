//! `n42-qmdb-replay`: replay recorded gov5 blocks offline through the N42
//! executor and recompute every state root against the header.
//!
//! Inputs: a Reth datadir initialised at a gov5 applied head
//! (`n42-init-snapshot`), the QMDB leaf-form export of the same head (or a
//! base file written by the node), and a finalized-range file of the blocks
//! above it (`export-range-linux.go`). The datadir is opened read-only; the
//! state of the replayed blocks is kept as an in-memory bundle overlay, the
//! commitment as the split QMDB tree. For every block: execute, convert the
//! bundle (plus the slots gov5 rewrites although revm dropped them) to leaf
//! operations, apply, compare the root with the header's, and report time
//! and memory. The first divergence stops the run with both roots printed.

use alloy_consensus::{Block, BlockBody, Header};
use alloy_eips::{Decodable2718, Encodable2718};
use alloy_primitives::{B256, U256};
use clap::Parser;
use n42_consensus::{Gov5NativeHeader, gov5_rewards_to_withdrawals};
use n42_execution::{N42EvmConfig, restored_slots_for};
use n42_network::{decode_finalized_range_stream, decode_gov5_block_rlp};
use n42_node::N42Node;
use n42_node::qmdb_state::{gov5_qmdb_operations_with_restored, gov5_restored_slots_key};
use n42_node::qmdb_state_root::Gov5QmdbStateRootStore;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardfork, ForkCondition};
use reth_cli_commands::common::{AccessRights, Environment, EnvironmentArgs};
use reth_ethereum_cli::chainspec::EthereumChainSpecParser;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{ConfigureEvm, execute::Executor};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_provider::{BlockHashReader, BlockNumReader};
use reth_revm::database::StateProviderDatabase;
use revm::{
    bytecode::Bytecode,
    database::states::BundleState,
    database_interface::{DBErrorMarker, Database},
    primitives::StorageKey,
    state::AccountInfo,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Parser)]
#[command(
    name = "n42-qmdb-replay",
    about = "Replay recorded gov5 blocks offline and recompute every QMDB state root"
)]
struct Args {
    #[command(flatten)]
    env: EnvironmentArgs<EthereumChainSpecParser>,
    /// QMDB leaf-form (v2) export of the datadir's head, or a base file.
    #[arg(long)]
    leaf_form: PathBuf,
    /// Finalized-range file with the blocks above the head.
    #[arg(long)]
    range: PathBuf,
    /// Authenticated gov5 genesis range (block zero).
    #[arg(long)]
    genesis_range: PathBuf,
    /// The chain's genesis hash.
    #[arg(long)]
    genesis_hash: B256,
    /// Last block to replay (inclusive); default: the range's last block.
    #[arg(long)]
    to: Option<u64>,
    /// Write a base file of the tree at the last replayed block.
    #[arg(long)]
    write_base: Option<PathBuf>,
    /// Append one JSON line per block to this file.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Keep going after a root mismatch (the tree then follows the header's
    /// root only in the report; it stays on the computed one).
    #[arg(long)]
    continue_on_mismatch: bool,
    /// Activate Prague (EIP-2935 history storage, EIP-7002/7251 system calls,
    /// EIP-7702) at this timestamp: gov5's `pectraTime`, which chain 94's
    /// genesis file omits.
    #[arg(long)]
    prague_time: Option<u64>,
    /// Print every leaf operation (key, value, leaf hash, slot) of a block
    /// whose root mismatches, in the order it is appended.
    #[arg(long)]
    trace_mismatch: bool,
}

fn activate_gov5_pos_execution(chain: &mut ChainSpec) {
    chain.hardforks.insert(
        EthereumHardfork::Paris,
        ForkCondition::TTD {
            activation_block_number: 0,
            fork_block: Some(0),
            total_difficulty: U256::ZERO,
        },
    );
    chain.paris_block_and_final_difficulty = Some((0, U256::ZERO));
    chain.genesis.config.terminal_total_difficulty = Some(U256::ZERO);
    chain.genesis.config.merge_netsplit_block = Some(0);
}

/// One recorded block: its gov5 hash, native header and block RLP.
struct RecordedBlock {
    number: u64,
    hash: B256,
    header_rlp: Vec<u8>,
    block_rlp: Vec<u8>,
}

/// The N42FRNG\x01 frame: header (magic, chain id, genesis, from, to,
/// count), per block (number, five hashes, header blob, block blob, receipts
/// blob), Blake3 trailer over everything before it.
fn read_range(path: &Path, chain_id: u64, genesis_hash: B256) -> eyre::Result<Vec<RecordedBlock>> {
    let data = std::fs::read(path)?;
    if data.len() < 8 + 8 + 32 + 24 + 32 || &data[..8] != b"N42FRNG\x01" {
        return Err(eyre::eyre!("{} is not an N42FRNG v1 file", path.display()));
    }
    let digest = blake3::hash(&data[..data.len() - 32]);
    if digest.as_bytes() != &data[data.len() - 32..] {
        return Err(eyre::eyre!("{} fails its Blake3 trailer", path.display()));
    }
    let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"));
    if u64_at(8) != chain_id || data[16..48] != genesis_hash.0 {
        return Err(eyre::eyre!("{} belongs to another chain", path.display()));
    }
    let mut cursor = 48;
    let from = u64_at(cursor);
    let count = u64_at(cursor + 16);
    cursor += 24;
    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        let number = u64_at(cursor);
        if number != from + index {
            return Err(eyre::eyre!("range is not contiguous at {number}"));
        }
        cursor += 8;
        let hash = B256::from_slice(&data[cursor..cursor + 32]);
        cursor += 5 * 32;
        let blob = |cursor: &mut usize| {
            let len = u32::from_le_bytes(data[*cursor..*cursor + 4].try_into().expect("4 bytes"))
                as usize;
            let bytes = data[*cursor + 4..*cursor + 4 + len].to_vec();
            *cursor += 4 + len;
            bytes
        };
        let header_rlp = blob(&mut cursor);
        let block_rlp = blob(&mut cursor);
        let _receipts = blob(&mut cursor);
        out.push(RecordedBlock {
            number,
            hash,
            header_rlp,
            block_rlp,
        });
    }
    if cursor + 32 != data.len() {
        return Err(eyre::eyre!("range file has trailing bytes"));
    }
    Ok(out)
}

/// The datadir's state below the replay, with every replayed block's bundle
/// layered on top, and the replayed headers' hashes for `BLOCKHASH`.
struct ReplayDb<DB> {
    inner: DB,
    overlay: Arc<BundleState>,
    hashes: Arc<HashMap<u64, B256>>,
    /// Every address whose account was loaded, for the mismatch trace.
    loaded: Arc<std::sync::Mutex<Vec<alloy_primitives::Address>>>,
}

impl<DB> std::fmt::Debug for ReplayDb<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayDb")
            .field("overlay_accounts", &self.overlay.state.len())
            .field("hashes", &self.hashes.len())
            .finish_non_exhaustive()
    }
}

impl<DB: Database> Database for ReplayDb<DB>
where
    DB::Error: DBErrorMarker,
{
    type Error = DB::Error;

    fn basic(
        &mut self,
        address: alloy_primitives::Address,
    ) -> Result<Option<AccountInfo>, Self::Error> {
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.push(address);
        }
        if let Some(account) = self.overlay.account(&address) {
            return Ok(account.account_info());
        }
        self.inner.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.overlay.contracts.get(&code_hash) {
            return Ok(code.clone());
        }
        self.inner.code_by_hash(code_hash)
    }

    fn storage(
        &mut self,
        address: alloy_primitives::Address,
        index: StorageKey,
    ) -> Result<U256, Self::Error> {
        if let Some(account) = self.overlay.account(&address)
            && let Some(value) = account.storage_slot(index)
        {
            return Ok(value);
        }
        self.inner.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        if let Some(hash) = self.hashes.get(&number) {
            return Ok(*hash);
        }
        self.inner.block_hash(number)
    }
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|kb| kb.parse().ok())
        })
        .unwrap_or_default()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

fn main() -> eyre::Result<()> {
    let mut args = Args::parse();
    let runner = reth_cli_runner::CliRunner::try_default_runtime()?;
    let runtime = runner.runtime();
    let chain_id = args.env.chain.chain().id();

    // Bind block zero to gov5's authenticated genesis header, as the node does.
    let range_file = File::open(&args.genesis_range)?;
    let genesis_range =
        decode_finalized_range_stream(BufReader::new(range_file), chain_id, args.genesis_hash)
            .map_err(|error| eyre::eyre!("genesis range authentication failed: {error}"))?;
    let genesis_entry = genesis_range
        .entries()
        .first()
        .filter(|entry| entry.number() == 0 && entry.block_hash() == args.genesis_hash)
        .ok_or_else(|| eyre::eyre!("genesis range must start with authenticated block zero"))?;
    {
        let chain = Arc::make_mut(&mut args.env.chain);
        chain.genesis_header = SealedHeader::new(genesis_entry.header().clone(), args.genesis_hash);
        activate_gov5_pos_execution(chain);
        if let Some(prague_time) = args.prague_time {
            chain.hardforks.insert(
                EthereumHardfork::Prague,
                ForkCondition::Timestamp(prague_time),
            );
            chain.genesis.config.prague_time = Some(prague_time);
            println!("prague activated at timestamp {prague_time}");
        }
    }
    let chain_spec = args.env.chain.clone();

    let started = Instant::now();
    let (mut tree, leaf_header) = Gov5QmdbStateRootStore::read_base_file(&args.leaf_form)
        .map_err(|error| eyre::eyre!("leaf form: {error}"))?;
    let base_number = leaf_header.block_number;
    let base_hash = B256::from(leaf_header.block_hash);
    let base_root = B256::from(leaf_header.root);
    println!(
        "leaf form: block {base_number} {base_hash} root {base_root} twigs {} live {} next_slot {} loaded and verified in {:.1?}, RSS {} MB",
        leaf_header.twigs,
        leaf_header.live,
        leaf_header.next_slot,
        started.elapsed(),
        rss_kb() / 1024
    );
    if leaf_header.chain_id != chain_id || B256::from(leaf_header.genesis_hash) != args.genesis_hash
    {
        return Err(eyre::eyre!("leaf form belongs to another chain"));
    }

    let Environment {
        provider_factory, ..
    } = args.env.init::<N42Node>(AccessRights::RO, runtime)?;
    let provider = provider_factory.provider()?;
    let head_number = provider.last_block_number()?;
    let head_hash = provider
        .block_hash(head_number)?
        .ok_or_else(|| eyre::eyre!("datadir has no hash for its head {head_number}"))?;
    if head_number != base_number || head_hash != base_hash {
        return Err(eyre::eyre!(
            "datadir head is block {head_number} {head_hash}, leaf form is block {base_number} {base_hash}"
        ));
    }
    drop(provider);

    let blocks = read_range(&args.range, chain_id, args.genesis_hash)?;
    let last = args
        .to
        .unwrap_or(blocks.last().map_or(0, |block| block.number));
    let to_replay: Vec<&RecordedBlock> = blocks
        .iter()
        .filter(|block| block.number > base_number && block.number <= last)
        .collect();
    println!(
        "range: {} recorded blocks, replaying {} ({}..={last})",
        blocks.len(),
        to_replay.len(),
        base_number + 1
    );
    if to_replay
        .first()
        .is_some_and(|block| block.number != base_number + 1)
    {
        return Err(eyre::eyre!("range does not start right above the head"));
    }

    let evm_config = N42EvmConfig::new(chain_spec.clone());
    let mut overlay = Arc::new(BundleState::default());
    let mut hashes: HashMap<u64, B256> = HashMap::new();
    hashes.insert(base_number, base_hash);
    let mut hashes = Arc::new(hashes);
    let mut report = match &args.report {
        Some(path) => Some(std::fs::File::create(path)?),
        None => None,
    };

    let mut parent_hash = base_hash;
    let mut exec_ms: Vec<f64> = Vec::with_capacity(to_replay.len());
    let mut root_us: Vec<f64> = Vec::with_capacity(to_replay.len());
    let mut total_txs = 0usize;
    let mut total_ops = 0usize;
    let mut restored_total = 0usize;
    let mut mismatches = 0usize;
    let mut replayed = 0usize;
    let run_started = Instant::now();
    let mut peak_rss_kb = rss_kb();

    for recorded in to_replay {
        let native = Gov5NativeHeader::decode(&recorded.header_rlp)
            .map_err(|error| eyre::eyre!("block {}: header: {error}", recorded.number))?;
        let native_hash = native.hash();
        if native_hash != recorded.hash {
            return Err(eyre::eyre!(
                "block {}: header hashes to {native_hash}, range says {}",
                recorded.number,
                recorded.hash
            ));
        }
        if native.header.parent_hash != parent_hash {
            return Err(eyre::eyre!(
                "block {}: parent {} is not the previous block {parent_hash}",
                recorded.number,
                native.header.parent_hash
            ));
        }
        let gossip = decode_gov5_block_rlp(&recorded.block_rlp)
            .map_err(|error| eyre::eyre!("block {}: body: {error}", recorded.number))?;
        if gossip.block_hash != recorded.hash {
            return Err(eyre::eyre!("block {}: body hash differs", recorded.number));
        }
        let withdrawals = gov5_rewards_to_withdrawals(&gossip.rewards)
            .map_err(|error| eyre::eyre!("block {}: rewards: {error}", recorded.number))?;
        let header: Header = native.header.clone();
        // The wire carries alloy envelopes; the executor wants reth's.
        let transactions = gossip
            .transactions
            .iter()
            .map(|tx| TransactionSigned::decode_2718(&mut tx.encoded_2718().as_slice()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| eyre::eyre!("block {}: transaction: {error}", recorded.number))?;
        let body = BlockBody {
            transactions,
            ommers: Vec::new(),
            withdrawals: Some(withdrawals.into()),
        };
        let sealed = SealedBlock::new_unchecked(Block::new(header, body), recorded.hash);
        let recovered = sealed
            .try_recover()
            .map_err(|error| eyre::eyre!("block {}: sender recovery: {error}", recorded.number))?;
        let tx_count = recovered.body().transactions.len();

        let state_provider = provider_factory.latest()?;
        let loaded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let db = ReplayDb {
            inner: StateProviderDatabase::new(state_provider),
            overlay: Arc::clone(&overlay),
            hashes: Arc::clone(&hashes),
            loaded: Arc::clone(&loaded),
        };
        let exec_started = Instant::now();
        let output = evm_config
            .executor(db)
            .execute(&recovered)
            .map_err(|error| eyre::eyre!("block {}: execution: {error}", recorded.number))?;
        let exec_elapsed = exec_started.elapsed();
        if output.result.gas_used != native.header.gas_used {
            return Err(eyre::eyre!(
                "block {}: gas used {} but the header says {}",
                recorded.number,
                output.result.gas_used,
                native.header.gas_used
            ));
        }

        let restored = restored_slots_for(gov5_restored_slots_key(recovered.sealed_block()))
            .unwrap_or_default();
        let mut operations = gov5_qmdb_operations_with_restored(&output.state, &restored);
        let restored_used = operations
            .len()
            .saturating_sub(n42_node::qmdb_state::gov5_qmdb_operations(&output.state).len());
        if reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(
            chain_spec.as_ref(),
            native.header.timestamp,
        ) {
            n42_node::qmdb_state::with_gov5_prague_system_caller(&mut operations);
        }
        let slot_before = tree.next_slot();
        let root_started = Instant::now();
        let computed = B256::from(
            tree.apply_sorted_ops(operations.iter().cloned())
                .map_err(|error| eyre::eyre!("block {}: operations: {error}", recorded.number))?,
        );
        let root_elapsed = root_started.elapsed();
        let expected = native.header.state_root;
        let matched = computed == expected;
        if !matched {
            mismatches += 1;
            println!(
                "block {} {}: ROOT MISMATCH computed {computed} header {expected} (txs {tx_count}, ops {}, restored {restored_used})",
                recorded.number,
                recorded.hash,
                operations.len()
            );
            if args.trace_mismatch {
                let mut sorted: Vec<&n42_twig_core::qmdb_compat::QmdbOperation> =
                    operations.iter().collect();
                sorted.sort_unstable_by_key(|operation| operation.key);
                let mut slot = slot_before;
                println!(
                    "  tree next_slot before {slot_before} after {}",
                    tree.next_slot()
                );
                for operation in sorted {
                    match &operation.value {
                        Some(value) => {
                            println!(
                                "  op set    key {} value {} leaf {} slot {slot}",
                                hex::encode(operation.key),
                                hex::encode(value),
                                hex::encode(n42_twig_core::hash_leaf(&operation.key, value))
                            );
                            slot += 1;
                        }
                        None => println!("  op delete key {}", hex::encode(operation.key)),
                    }
                }
                for (index, (tx, sender)) in recovered
                    .body()
                    .transactions
                    .iter()
                    .zip(recovered.senders())
                    .enumerate()
                {
                    use alloy_consensus::Transaction as _;
                    let to = tx.to();
                    let key = |address: &alloy_primitives::Address| {
                        hex::encode(n42_twig_core::qmdb_compat::gov5_account_key(
                            &address.into_array(),
                        ))
                    };
                    println!(
                        "  tx {index} from {sender} (key {}) to {} value {} in_bundle from={} to={}",
                        key(sender),
                        to.map(|to| format!("{to} (key {})", key(&to)))
                            .unwrap_or_else(|| "create".into()),
                        tx.value(),
                        output.state.state.contains_key(sender),
                        to.is_none_or(|to| output.state.state.contains_key(&to))
                    );
                }
                let mut seen = std::collections::BTreeSet::new();
                for address in loaded
                    .lock()
                    .map(|loaded| loaded.clone())
                    .unwrap_or_default()
                {
                    if seen.insert(address) && !output.state.state.contains_key(&address) {
                        println!(
                            "  loaded but not in bundle {address} key {}",
                            hex::encode(n42_twig_core::qmdb_compat::gov5_account_key(
                                &address.into_array()
                            ))
                        );
                    }
                }
                for (address, account) in &output.state.state {
                    println!(
                        "  bundle {address} status {:?} info {:?} storage {}",
                        account.status,
                        account.info.as_ref().map(|info| (
                            info.nonce,
                            info.balance,
                            info.code_hash
                        )),
                        account.storage.len()
                    );
                }
            }
            if !args.continue_on_mismatch {
                break;
            }
        }
        replayed += 1;
        total_txs += tx_count;
        total_ops += operations.len();
        restored_total += restored_used;
        exec_ms.push(exec_elapsed.as_secs_f64() * 1_000.0);
        root_us.push(root_elapsed.as_secs_f64() * 1_000_000.0);
        let rss = rss_kb();
        peak_rss_kb = peak_rss_kb.max(rss);
        if let Some(report) = report.as_mut() {
            writeln!(
                report,
                "{{\"number\":{},\"hash\":\"{}\",\"txs\":{tx_count},\"ops\":{},\"restored\":{restored_used},\"exec_ms\":{:.3},\"root_us\":{:.1},\"computed\":\"{computed}\",\"header\":\"{expected}\",\"match\":{matched},\"rss_kb\":{rss}}}",
                recorded.number,
                recorded.hash,
                operations.len(),
                exec_elapsed.as_secs_f64() * 1_000.0,
                root_elapsed.as_secs_f64() * 1_000_000.0
            )?;
        }
        if replayed.is_multiple_of(250) || tx_count >= 200 {
            println!(
                "block {} ok: txs {tx_count} ops {} restored {restored_used} exec {:.2} ms root {:.0} us RSS {} MB",
                recorded.number,
                operations.len(),
                exec_elapsed.as_secs_f64() * 1_000.0,
                root_elapsed.as_secs_f64() * 1_000_000.0,
                rss / 1024
            );
        }

        // Layer the block's state over the datadir for the next block.
        let mut merged = Arc::try_unwrap(overlay).unwrap_or_else(|arc| (*arc).clone());
        merged.extend(output.state);
        overlay = Arc::new(merged);
        Arc::make_mut(&mut hashes).insert(recorded.number, recorded.hash);
        parent_hash = recorded.hash;
    }

    exec_ms.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    root_us.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    println!(
        "replayed {replayed} blocks ({total_txs} txs, {total_ops} leaf ops, {restored_total} restored-slot rewrites) in {:.1?}: {mismatches} root mismatches; exec p50 {:.2} ms p90 {:.2} ms max {:.2} ms; root p50 {:.0} us p90 {:.0} us max {:.0} us; RSS now {} MB peak {} MB; tree live {} next_slot {} twigs {}",
        run_started.elapsed(),
        percentile(&exec_ms, 0.5),
        percentile(&exec_ms, 0.9),
        exec_ms.last().copied().unwrap_or_default(),
        percentile(&root_us, 0.5),
        percentile(&root_us, 0.9),
        root_us.last().copied().unwrap_or_default(),
        rss_kb() / 1024,
        peak_rss_kb / 1024,
        tree.len(),
        tree.next_slot(),
        tree.twig_count()
    );
    if let Some(path) = &args.write_base
        && mismatches == 0
    {
        let root = B256::from(tree.root());
        let file = File::create(path)?;
        let mut writer = std::io::BufWriter::with_capacity(1 << 20, file);
        tree.write_leaf_form_v2(
            &mut writer,
            chain_id,
            &args.genesis_hash.0,
            base_number + replayed as u64,
            &parent_hash.0,
            &root.0,
        )?;
        writer.into_inner()?.sync_all()?;
        println!(
            "base file written: {} at block {} {parent_hash} root {root}",
            path.display(),
            base_number + replayed as u64
        );
    }
    if mismatches > 0 {
        std::process::exit(2);
    }
    Ok(())
}
