//! Where a leader's transfer batches spend their CPU, over QMDB-backed state
//! of realistic size.
//!
//! A block of `N42_EXEC_BENCH_TXS` transfers (95,238) from
//! `N42_EXEC_BENCH_SENDERS` senders (20,000) to as many distinct recipients,
//! over a QMDB read view of `N42_EXEC_BENCH_ACCOUNTS` accounts (10M), is run
//! through [`execute_batches`] as the payload builder runs it, and against
//! the same accounts held in a small in-memory map. The read path is then
//! timed on its own: key hashing, the index lookup, and the provider's whole
//! read. `N42_EXEC_BENCH_PROFILE=<path>` writes a folded CPU profile of the
//! QMDB rounds.
//!
//! `cargo test --profile profiling -p n42-node --lib exec_cost -- --ignored --nocapture`

use crate::{
    qmdb_state_reader::{QmdbReadsMode, QmdbStateAdapter},
    qmdb_state_root::Gov5QmdbStateRootStore,
};
use alloy_consensus::{Header, TxEip1559, transaction::Recovered};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256, map::AddressMap};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Database},
};
use n42_execution::{
    N42EvmConfig,
    direct_transfer::{DirectTransfers, shape_of},
    parallel_build::{self, ParallelRun, execute_batches, merge_map},
};
use n42_twig_core::{
    qmdb_compat::{QmdbOperation, encode_gov5_account_value, gov5_account_key},
    qmdb_leaf_tree::QmdbLeafTree,
};
use n42_tx_types::N42TxEnvelope;
use rayon::prelude::*;
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_evm::{ConfigureEvm, database::StateProviderDatabase};
use reth_provider::test_utils::MockEthProvider;
use reth_storage_api::{
    EvmStateProviderAdapter, StateProvider, StateProviderBox, StateProviderFactory,
    n42_state::BlockStateAdapter,
};
use std::{sync::Arc, time::Instant};

const FUND: u64 = 1_000_000_000_000_000_000;

fn var(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub(crate) fn address(domain: u8, n: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = domain;
    bytes[12..].copy_from_slice(&n.to_be_bytes());
    Address::from(bytes)
}

fn account(address: Address, balance: u64) -> QmdbOperation {
    QmdbOperation {
        key: gov5_account_key(address.as_ref()),
        value: Some(encode_gov5_account_value(
            0,
            &U256::from(balance).to_be_bytes::<32>(),
            &B256::ZERO.0,
        )),
    }
}

/// The parent's accounts in a map: the batches' work without the QMDB read.
#[derive(Clone)]
struct MemDb(Arc<AddressMap<AccountInfo>>);

impl Database for MemDb {
    type Error = std::convert::Infallible;

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.0.get(address).cloned())
    }

    fn get_code_by_hash(&mut self, _: &B256) -> Result<Bytecode, Self::Error> {
        Ok(Bytecode::default())
    }

    fn get_storage(&mut self, _: &Address, _: &U256) -> Result<U256, Self::Error> {
        Ok(U256::ZERO)
    }

    fn get_block_hash(&mut self, _: &U256) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

/// A block of transfers and the state it runs over.
pub(crate) struct Fixture {
    pub(crate) config: N42EvmConfig,
    pub(crate) env: reth_evm::EvmEnvFor<N42EvmConfig>,
    pub(crate) txs: Vec<Recovered<N42TxEnvelope>>,
    pub(crate) keys: Vec<(Address, Address)>,
    pub(crate) base_hash: B256,
    pub(crate) adapter: QmdbStateAdapter,
    pub(crate) store: Arc<Gov5QmdbStateRootStore>,
    /// The provider QMDB answers in front of; made once, since a new one
    /// derives the dev chain's genesis.
    fallback: MockEthProvider,
    mem: MemDb,
}

impl Fixture {
    /// `count` transfers, sender `i % senders` paying recipient `i`, over
    /// `accounts` accounts in all.
    pub(crate) fn new(count: u64, senders: u64, accounts: u64) -> Self {
        assert!(accounts > senders + count);
        let mut chain_spec = n42_chainspec::n42_dev_chainspec();
        {
            let chain = Arc::make_mut(&mut chain_spec);
            chain.hardforks.insert(
                reth_chainspec::EthereumHardfork::Prague,
                reth_chainspec::ForkCondition::Timestamp(0),
            );
            chain.genesis.config.prague_time = Some(0);
        }
        let chain_id = chain_spec.chain.id();
        let config = parallel_build::batch_config(&N42EvmConfig::with_evm_factory(
            chain_spec,
            n42_execution::N42EvmFactory::with_fast_transfers(true),
        ));
        let beneficiary = address(0xc0, 0);
        let header = Header {
            parent_hash: B256::repeat_byte(0x71),
            number: 1,
            timestamp: 1_700_000_001,
            gas_limit: 21_000 * count * 2,
            base_fee_per_gas: Some(7),
            beneficiary,
            parent_beacon_block_root: Some(B256::ZERO),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Header::default()
        };
        let env = config.evm_env(&header).expect("a valid environment");

        let started = Instant::now();
        let mut initial = Vec::with_capacity(accounts as usize);
        let mut mem = AddressMap::default();
        for n in 0..senders {
            initial.push(account(address(0xa1, n), FUND));
            mem.insert(address(0xa1, n), AccountInfo::empty().with_balance(U256::from(FUND)));
        }
        for n in 0..count {
            initial.push(account(address(0xa2, n), 1));
            mem.insert(address(0xa2, n), AccountInfo::empty().with_balance(U256::from(1)));
        }
        initial.push(account(beneficiary, 1));
        mem.insert(beneficiary, AccountInfo::empty().with_balance(U256::from(1)));
        let filler = accounts - senders - count - 1;
        initial.par_extend((0..filler).into_par_iter().map(|n| account(address(0xb0, n), 1)));
        initial.par_sort_unstable_by_key(|op| op.key);
        let mut tree = QmdbLeafTree::new();
        let base_root = B256::from(tree.apply_sorted_ops(initial).expect("a valid genesis"));
        let base_hash = B256::repeat_byte(0x71);
        let store = Arc::new(
            Gov5QmdbStateRootStore::from_leaf_tree(base_hash, base_root, tree, 64)
                .expect("the store"),
        );
        let adapter = QmdbStateAdapter::new(store.clone(), QmdbReadsMode::Only).expect("adapter");
        store.prepare_read_view(base_hash).expect("view").expect("the base view");
        println!(
            "exec_cost fixture: {accounts} accounts, {senders} senders, {count} transfers, built in {:.1} s",
            started.elapsed().as_secs_f64()
        );

        let txs = (0..count)
            .map(|i| {
                let tx = Transaction::Eip1559(TxEip1559 {
                    chain_id,
                    nonce: i / senders,
                    gas_limit: 21_000,
                    max_fee_per_gas: 9,
                    max_priority_fee_per_gas: 2,
                    to: TxKind::Call(address(0xa2, i)),
                    value: U256::from(1),
                    input: Bytes::new(),
                    ..Default::default()
                });
                Recovered::new_unchecked(
                    TransactionSigned::new_unhashed(tx, Signature::test_signature()).into(),
                    address(0xa1, i % senders),
                )
            })
            .collect::<Vec<_>>();
        let keys = (0..count).map(|i| (address(0xa1, i % senders), address(0xa2, i))).collect();
        Self {
            config,
            env,
            txs,
            keys,
            base_hash,
            adapter,
            store,
            fallback: MockEthProvider::default(),
            mem: MemDb(Arc::new(mem)),
        }
    }

    pub(crate) fn qmdb(&self) -> Option<StateProviderDatabase<EvmStateProviderAdapter<StateProviderBox>>> {
        let provider = self.adapter.wrap(self.base_hash, self.fallback.latest().ok()?).ok()?;
        Some(StateProviderDatabase::new(provider.into_evm_state_provider()))
    }

    pub(crate) fn run<D>(&self, open: &(dyn Fn() -> Option<D> + Sync)) -> ParallelRun<N42EvmConfig>
    where
        D: Database + Send + 'static,
    {
        let convert = |i: usize| {
            Recovered::new_unchecked(
                n42_tx_types::execution::evm_tx_of(self.txs[i].inner()),
                self.txs[i].signer(),
            )
        };
        let direct = DirectTransfers::for_env(&self.env, shape_of).expect("the direct path");
        let run = execute_batches(&self.config, &self.env, &self.keys, &convert, open, Some(&direct))
            .unwrap_or_else(|_| panic!("the batches run"));
        assert_eq!(run.executed.len(), self.txs.len());
        run
    }
}

/// User plus system CPU of the process in milliseconds, and its minor page
/// faults.
fn process_cpu_ms() -> (f64, f64) {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat.rsplit_once(')').map(|(_, rest)| rest).unwrap_or("").split_whitespace().collect();
    let field = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    // Fields 10, 14 and 15 of the whole line; the first two are before `)`.
    ((field(11) + field(12)) * 10.0, field(7))
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

struct Rounds {
    wall: Vec<f64>,
    cpu: Vec<f64>,
    faults: Vec<f64>,
    open: Vec<f64>,
    run: Vec<f64>,
    take: Vec<f64>,
    merge: Vec<f64>,
    partition: Vec<f64>,
    batches: Vec<f64>,
    collect: Vec<f64>,
}

/// `pause`: time left between rounds, as between a leader's blocks, and
/// in which a prefaulted merge table is made.
fn rounds<D>(
    fixture: &Fixture,
    rounds: usize,
    open: &(dyn Fn() -> Option<D> + Sync),
    pause: Option<std::time::Duration>,
) -> Rounds
where
    D: Database + Send + 'static,
{
    let mut out = Rounds {
        wall: vec![],
        cpu: vec![],
        faults: vec![],
        open: vec![],
        run: vec![],
        take: vec![],
        merge: vec![],
        partition: vec![],
        batches: vec![],
        collect: vec![],
    };
    let mut kept = Vec::new();
    for round in 0..rounds + 2 {
        if let Some(pause) = pause {
            std::thread::sleep(pause);
            let waited = Instant::now();
            while merge_map::enabled() && !merge_map::ready() && waited.elapsed().as_secs() < 5 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let ((cpu, faults), at) = (process_cpu_ms(), Instant::now());
        let run = fixture.run(open);
        let wall = at.elapsed().as_secs_f64() * 1e3;
        let (cpu_after, faults_after) = process_cpu_ms();
        let (cpu, faults) = (cpu_after - cpu, faults_after - faults);
        // Kept as the node keeps a block's output, while the next is built.
        kept.push(run.merged);
        if kept.len() > 4 {
            kept.remove(0);
        }
        let p = run.phases;
        if round < 2 {
            continue;
        }
        out.wall.push(wall);
        out.cpu.push(cpu);
        out.faults.push(faults);
        out.open.push(p.open_ms_sum as f64);
        out.run.push(p.run_ms_sum as f64);
        out.take.push(p.batch_merge_ms_sum as f64);
        out.merge.push(p.merge_ms as f64);
        out.partition.push(p.partition_ms as f64);
        out.batches.push(p.groups_ms as f64);
        out.collect.push(p.collect_ms as f64);
    }
    out
}

fn report(name: &str, count: usize, r: Rounds) {
    let us = |ms: f64| ms * 1e3 / count as f64;
    let (wall, cpu, run, open, take) = (median(r.wall), median(r.cpu), median(r.run), median(r.open), median(r.take));
    println!(
        "EXEC_COST {name}: wall {wall:.1} ms (partition {:.0} batches {:.0} collect {:.0}), process cpu {cpu:.0} ms = {:.2} us/tx, {:.0} minor faults; \
         batch thread sums: open {open:.0} ms, run {run:.0} ms = {:.2} us/tx, take+check bundle {take:.0} ms = {:.2} us/tx; merge (builder thread) {:.0} ms",
        median(r.partition),
        median(r.batches),
        median(r.collect),
        us(cpu),
        median(r.faults),
        us(run),
        us(take),
        median(r.merge),
    );
}

/// Thread-summed milliseconds of `work` over `items` split in `parts`.
fn thread_sum<T: Sync>(items: &[T], parts: usize, work: impl Fn(&[T]) -> u64 + Sync) -> (f64, u64) {
    let chunk = items.len().div_ceil(parts.max(1));
    let pool = n42_execution::parallel_common::build_pool();
    pool.install(|| {
        items
            .par_chunks(chunk)
            .map(|part| {
                let at = Instant::now();
                let sink = work(part);
                (at.elapsed().as_secs_f64() * 1e3, sink)
            })
            .reduce(|| (0.0, 0), |a, b| (a.0 + b.0, a.1.wrapping_add(b.1)))
    })
}

#[test]
#[ignore = "timing run over a large QMDB state, not a check"]
fn exec_cost_of_a_full_block_of_transfers() {
    let count = var("N42_EXEC_BENCH_TXS", 95_238) as u64;
    let senders = var("N42_EXEC_BENCH_SENDERS", 20_000) as u64;
    let accounts = var("N42_EXEC_BENCH_ACCOUNTS", 10_000_000) as u64;
    let n = var("N42_EXEC_BENCH_ROUNDS", 10);
    let fixture = Fixture::new(count, senders, accounts);
    let threads = n42_execution::parallel_common::build_pool().current_num_threads();
    println!("exec_cost: {threads} build threads");

    let mem = fixture.mem.clone();
    report("mem", count as usize, rounds(&fixture, n, &|| Some(mem.clone()), None));

    let profile = std::env::var("N42_EXEC_BENCH_PROFILE").ok().filter(|path| !path.is_empty());
    let guard = profile.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(1999)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("profiler")
    });
    report("qmdb", count as usize, rounds(&fixture, n, &|| fixture.qmdb(), None));
    if let (Some(path), Some(guard)) = (profile, guard) {
        let report = guard.report().build().expect("profile");
        let mut folded = String::new();
        for (frames, count) in &report.data {
            let thread = frames.thread_name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
            let names: Vec<String> = frames.frames.iter().flatten().map(|s| s.name().replace(';', ":")).collect();
            folded.push_str(&format!("{thread};{} {count}\n", names.into_iter().rev().collect::<Vec<_>>().join(";")));
        }
        std::fs::write(&path, folded).expect("profile written");
        println!("exec_cost: profile written to {path}");
    }

    // The merge into a prefaulted table (`N42_MERGE_PREFAULT`) against a
    // fresh one, with a leader's pause between blocks: the same output.
    let pause = std::time::Duration::from_millis(var("N42_EXEC_BENCH_PAUSE_MS", 200) as u64);
    merge_map::set_enabled(false);
    report("qmdb fresh table", count as usize, rounds(&fixture, n, &|| fixture.qmdb(), Some(pause)));
    let fresh = fixture.run(&|| fixture.qmdb());
    merge_map::set_enabled(true);
    let before = merge_map::counts();
    report("qmdb prefaulted table", count as usize, rounds(&fixture, n, &|| fixture.qmdb(), Some(pause)));
    std::thread::sleep(pause);
    while !merge_map::ready() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let prefaulted = fixture.run(&|| fixture.qmdb());
    let after = merge_map::counts();
    merge_map::set_enabled(false);
    assert!(fresh.merged == prefaulted.merged, "the prefaulted table merges the same accounts");
    assert_eq!(fresh.skipped, prefaulted.skipped);
    println!(
        "EXEC_COST prefault: {} merges took a prefaulted table, {} a fresh one; merged output identical ({} accounts)",
        after.0 - before.0,
        after.1 - before.1,
        fresh.merged.addresses().count(),
    );

    // The read path on its own, over the same addresses and as many parts as
    // the batches.
    let parts = threads * 2;
    let mut addresses: Vec<Address> = (0..senders).map(|n| address(0xa1, n)).collect();
    addresses.extend((0..count).map(|n| address(0xa2, n)));
    let reads = addresses.len() as f64;
    let view = fixture.store.prepare_read_view(fixture.base_hash).expect("view").expect("view");
    let keys: Vec<[u8; 32]> = addresses.iter().map(|a| gov5_account_key(a.as_ref())).collect();
    let (mut hash, mut lookup, mut provider, mut decode) = (vec![], vec![], vec![], vec![]);
    for _ in 0..n {
        hash.push(thread_sum(&addresses, parts, |part| {
            part.iter().map(|a| gov5_account_key(a.as_ref())[0] as u64).sum()
        }).0);
        lookup.push(thread_sum(&keys, parts, |part| {
            part.iter().map(|k| view.value(k).map_or(0, |v| v.len() as u64)).sum()
        }).0);
        decode.push(thread_sum(&addresses, parts, |part| {
            part.iter().map(|a| view.account(a).ok().flatten().map_or(0, |a| a.nonce + 1)).sum()
        }).0);
        provider.push(thread_sum(&addresses, parts, |part| {
            let mut db = fixture.qmdb().expect("db");
            part.iter().map(|a| db.get_account(a).ok().flatten().map_or(0, |a| a.nonce + 1)).sum()
        }).0);
    }
    let per = |ms: f64| ms * 1e6 / reads;
    let (hash, lookup, decode, provider) = (median(hash), median(lookup), median(decode), median(provider));
    println!(
        "EXEC_COST reads ({reads} accounts, thread sums): blake3 key {hash:.1} ms = {:.0} ns/read; index lookup {lookup:.1} ms = {:.0} ns/read; \
         view.account (key+lookup+decode) {decode:.1} ms = {:.0} ns/read; provider get_account {provider:.1} ms = {:.0} ns/read",
        per(hash),
        per(lookup),
        per(decode),
        per(provider),
    );
}
