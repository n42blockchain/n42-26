//! N42 EVM benchmark
//!
//! Measures pure EVM execution time for:
//! 1. Simple ETH transfer (21000 gas) — single-core
//! 2. ERC-20 style transfer (SLOAD/SSTORE contract call) — single-core
//! 3. Parallel ETH transfers — Block-STM multi-core

use alloy_primitives::{address, Address, TxKind, U256};
use revm::{
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
    Context, ExecuteCommitEvm, MainBuilder, MainContext,
};
use std::time::Instant;
use std::net::UdpSocket;

const SENDER: Address = address!("1000000000000000000000000000000000000001");
const RECEIVER: Address = address!("2000000000000000000000000000000000000002");
const ERC20_ADDR: Address = address!("3000000000000000000000000000000000000003");

fn main() {
    println!("=== N42 Pipeline Benchmark (100k tx baseline) ===\n");

    bench_eth_transfer();
    println!();
    bench_erc20_transfer();
    println!();
    bench_mixed_block();
    println!();
    bench_serialization_compression();
    println!();
    bench_network_transfer_simulation();
    println!();
    bench_tx_pool_submission();
    println!();
    bench_full_pipeline_timeline();
    println!();
    bench_parallel_transfers();
}

/// Simulate a realistic block: 90% ETH transfers + 10% ERC-20 contract calls.
/// Test various block sizes to understand how EVM execution scales.
fn bench_mixed_block() {
    println!("--- Mixed Block Simulation (90% transfer + 10% ERC-20) ---");

    // ERC-20 contract bytecode (same as bench_erc20_transfer)
    let bytecode: Vec<u8> = vec![
        0x60, 0x00, 0x54,       // PUSH1 0, SLOAD (sender bal)
        0x60, 0x01, 0x90, 0x03, // PUSH1 1, SWAP1, SUB
        0x60, 0x00, 0x55,       // PUSH1 0, SSTORE
        0x60, 0x01, 0x54,       // PUSH1 1, SLOAD (receiver bal)
        0x60, 0x01, 0x01,       // PUSH1 1, ADD
        0x60, 0x01, 0x55,       // PUSH1 1, SSTORE
        0x60, 0x01,             // PUSH1 1 (true)
        0x60, 0x00,             // PUSH1 0 (memory offset)
        0x52,                   // MSTORE
        0x60, 0x20,             // PUSH1 32
        0x60, 0x00,             // PUSH1 0
        0xf3,                   // RETURN
    ];

    let block_sizes = [1_000, 5_000, 10_000, 23_809, 50_000, 100_000];

    for &total_txs in &block_sizes {
        let erc20_count = total_txs / 10;
        let transfer_count = total_txs - erc20_count;

        let mut db = CacheDB::<EmptyDB>::default();

        // Deploy ERC-20 contract
        let mut contract_info = AccountInfo::default();
        contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.clone().into()));
        contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
        db.insert_account_info(ERC20_ADDR, contract_info);
        db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();

        // Create unique senders for all txs
        for i in 0..total_txs {
            let sender = Address::from_word(U256::from(0x10000 + i as u64).into());
            db.insert_account_info(sender, AccountInfo {
                balance: U256::from(u128::MAX),
                nonce: 0,
                ..Default::default()
            });
        }

        let ctx = Context::mainnet().with_db(&mut db);
        let mut evm = ctx.build_mainnet();

        let start = Instant::now();

        // Execute ETH transfers (90%)
        for i in 0..transfer_count {
            let sender = Address::from_word(U256::from(0x10000 + i as u64).into());
            let receiver = Address::from_word(U256::from(0x90000 + i as u64).into());
            let tx = TxEnv::builder()
                .caller(sender)
                .kind(TxKind::Call(receiver))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(0)
                .nonce(0)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }

        // Execute ERC-20 transfers (10%)
        for i in 0..erc20_count {
            let sender = Address::from_word(U256::from(0x10000 + transfer_count as u64 + i as u64).into());
            let tx = TxEnv::builder()
                .caller(sender)
                .kind(TxKind::Call(ERC20_ADDR))
                .gas_limit(100_000)
                .gas_price(0)
                .nonce(0)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }

        let elapsed = start.elapsed();
        let ms = elapsed.as_secs_f64() * 1000.0;
        let ns_per_tx = elapsed.as_nanos() as f64 / total_txs as f64;
        let tps = 1_000_000_000.0 / ns_per_tx;
        let total_gas = transfer_count as u64 * 21_000 + erc20_count as u64 * 100_000;
        let gas_m = total_gas as f64 / 1_000_000.0;

        println!(
            "  {total_txs:>6} txs ({transfer_count} transfer + {erc20_count} erc20): \
             {ms:>8.1}ms  {tps:>8.0} tx/s  gas={gas_m:.0}M  \
             slot_pct_4s={pct4:.1}%  slot_pct_2s={pct2:.1}%  slot_pct_1s={pct1:.1}%",
            pct4 = ms / 4000.0 * 100.0,
            pct2 = ms / 2000.0 * 100.0,
            pct1 = ms / 1000.0 * 100.0,
        );
    }
}

fn bench_eth_transfer() {
    println!("--- ETH Transfer (21000 gas) ---");

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        ..Default::default()
    });

    let warmup = 1_000;
    let iterations = 100_000;

    // Warmup
    {
        let ctx = Context::mainnet().with_db(&mut db);
        let mut evm = ctx.build_mainnet();
        for i in 0..warmup {
            let tx = TxEnv::builder()
                .caller(SENDER)
                .kind(TxKind::Call(RECEIVER))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(0)
                .nonce(i)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }
    }

    // Reset state for clean measurement
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        nonce: 0,
        ..Default::default()
    });

    let ctx = Context::mainnet().with_db(&mut db);
    let mut evm = ctx.build_mainnet();

    let start = Instant::now();
    for i in 0..iterations {
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(RECEIVER))
            .value(U256::from(1))
            .gas_limit(21_000)
            .gas_price(0)
            .nonce(i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    let elapsed = start.elapsed();

    let ns_per_tx = elapsed.as_nanos() as f64 / iterations as f64;
    let tps = 1_000_000_000.0 / ns_per_tx;

    println!("  Iterations: {iterations}");
    println!("  Total time: {elapsed:.2?}");
    println!("  Per tx:     {ns_per_tx:.0} ns");
    println!("  Theoretical single-core TPS: {tps:.0}");
}

fn bench_erc20_transfer() {
    println!("--- ERC-20 Transfer (2x SLOAD + 2x SSTORE contract call) ---");

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        ..Default::default()
    });

    // Hand-crafted bytecode: SLOAD slot 0 (sender), SUB 1, SSTORE;
    // SLOAD slot 1 (receiver), ADD 1, SSTORE; RETURN(32, 0)
    let bytecode: Vec<u8> = vec![
        0x60, 0x00, 0x54,       // PUSH1 0, SLOAD (sender bal)
        0x60, 0x01, 0x90, 0x03, // PUSH1 1, SWAP1, SUB
        0x60, 0x00, 0x55,       // PUSH1 0, SSTORE
        0x60, 0x01, 0x54,       // PUSH1 1, SLOAD (receiver bal)
        0x60, 0x01, 0x01,       // PUSH1 1, ADD
        0x60, 0x01, 0x55,       // PUSH1 1, SSTORE
        0x60, 0x01,             // PUSH1 1 (true)
        0x60, 0x00,             // PUSH1 0 (memory offset)
        0x52,                   // MSTORE
        0x60, 0x20,             // PUSH1 32
        0x60, 0x00,             // PUSH1 0
        0xf3,                   // RETURN
    ];

    let mut contract_info = AccountInfo::default();
    contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.clone().into()));
    contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
    db.insert_account_info(ERC20_ADDR, contract_info);

    // Initial sender balance in contract storage slot 0
    db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();

    let iterations: u64 = 100_000;

    // Warmup
    {
        let ctx = Context::mainnet().with_db(&mut db);
        let mut evm = ctx.build_mainnet();
        for i in 0..1000u64 {
            let tx = TxEnv::builder()
                .caller(SENDER)
                .kind(TxKind::Call(ERC20_ADDR))
                .gas_limit(100_000)
                .gas_price(0)
                .nonce(i)
                .build()
                .unwrap();
            let _ = evm.transact_commit(tx);
        }
    }

    // Reset
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(SENDER, AccountInfo {
        balance: U256::from(u128::MAX),
        nonce: 0,
        ..Default::default()
    });

    let mut contract_info = AccountInfo::default();
    contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.into()));
    contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
    db.insert_account_info(ERC20_ADDR, contract_info);
    db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();

    let ctx = Context::mainnet().with_db(&mut db);
    let mut evm = ctx.build_mainnet();

    let start = Instant::now();
    for i in 0..iterations {
        let tx = TxEnv::builder()
            .caller(SENDER)
            .kind(TxKind::Call(ERC20_ADDR))
            .gas_limit(100_000)
            .gas_price(0)
            .nonce(i)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    let elapsed = start.elapsed();

    let ns_per_tx = elapsed.as_nanos() as f64 / iterations as f64;
    let tps = 1_000_000_000.0 / ns_per_tx;

    println!("  Iterations: {iterations}");
    println!("  Total time: {elapsed:.2?}");
    println!("  Per tx:     {ns_per_tx:.0} ns");
    println!("  Theoretical single-core TPS: {tps:.0}");
}

fn bench_parallel_transfers() {
    println!("--- Parallel ETH Transfers (Block-STM) ---");

    let num_threads = rayon::current_num_threads();
    println!("  Rayon threads: {num_threads}");

    let num_txs_list = [100, 500, 1000, 2000, 5000];

    for &num_txs in &num_txs_list {
        let mut db = CacheDB::<EmptyDB>::default();

        // Create unique sender/receiver pairs (no conflicts).
        let mut txs = Vec::with_capacity(num_txs);
        for i in 0..num_txs {
            let sender = Address::from_word(U256::from(0x1000 + i as u64).into());
            let receiver = Address::from_word(U256::from(0x2000 + i as u64).into());

            db.insert_account_info(
                sender,
                AccountInfo {
                    balance: U256::from(u128::MAX),
                    nonce: 0,
                    ..Default::default()
                },
            );

            let tx = TxEnv::builder()
                .caller(sender)
                .kind(TxKind::Call(receiver))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(0)
                .nonce(0)
                .build()
                .unwrap();
            txs.push(tx);
        }

        let cfg_env = CfgEnv::default();
        let block_env = BlockEnv::default();

        // Sequential: force sequential path with high threshold.
        // SAFETY: benchmark is single-threaded at this point.
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", &format!("{}", num_txs + 1)); }
        let seq_start = Instant::now();
        let seq_result =
            n42_parallel_evm::parallel_execute(&txs, &db, cfg_env.clone(), block_env.clone());
        let seq_elapsed = seq_start.elapsed();

        // Parallel: force parallel path with threshold=1.
        unsafe { std::env::set_var("N42_PARALLEL_THRESHOLD", "1"); }
        let par_start = Instant::now();
        let par_result = n42_parallel_evm::parallel_execute(&txs, &db, cfg_env, block_env);
        let par_elapsed = par_start.elapsed();
        unsafe { std::env::remove_var("N42_PARALLEL_THRESHOLD"); }

        let seq_ok = seq_result.is_ok();
        let par_ok = par_result.is_ok();
        let speedup = if par_elapsed.as_nanos() > 0 {
            seq_elapsed.as_nanos() as f64 / par_elapsed.as_nanos() as f64
        } else {
            0.0
        };

        println!(
            "  {num_txs:>5} txs: seq={seq_ms:>6.1}ms  par={par_ms:>6.1}ms  speedup={speedup:.2}x  seq_ok={seq_ok}  par_ok={par_ok}",
            seq_ms = seq_elapsed.as_secs_f64() * 1000.0,
            par_ms = par_elapsed.as_secs_f64() * 1000.0,
        );
    }
}

/// Benchmark: serialize 100k tx block to JSON, compress, decompress, deserialize.
/// Simulates the full block data lifecycle:
///   Leader: ExecutionData → JSON → zstd compress → bincode wrap
///   Follower: bincode unwrap → zstd decompress → JSON → ExecutionData
fn bench_serialization_compression() {
    println!("--- Serialization + Compression Benchmark ---");

    // Generate fake payload data of various sizes (simulating ExecutionData JSON)
    // A typical tx in JSON is ~200 bytes; 100k txs ≈ 20MB raw JSON
    let sizes: &[(usize, &str)] = &[
        (1_000, "1k txs (~200KB)"),
        (5_000, "5k txs (~1MB)"),
        (10_000, "10k txs (~2MB)"),
        (23_809, "23.8k txs (~4.8MB, current max)"),
        (50_000, "50k txs (~10MB)"),
        (100_000, "100k txs (~20MB)"),
    ];

    for &(num_txs, label) in sizes {
        // Generate realistic-looking JSON payload
        // Each tx entry is ~200 bytes of JSON
        let tx_template = r#"{"hash":"0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890","from":"0x1234567890123456789012345678901234567890","to":"0x0987654321098765432109876543210987654321","value":"0x1","gas":"0x5208","nonce":"0x0"}"#;
        let raw_json: Vec<u8> = {
            let mut buf = Vec::with_capacity(num_txs * 220);
            buf.push(b'[');
            for i in 0..num_txs {
                if i > 0 { buf.push(b','); }
                buf.extend_from_slice(tx_template.as_bytes());
            }
            buf.push(b']');
            buf
        };
        let raw_len = raw_json.len();

        // 1. zstd compress
        let compress_start = Instant::now();
        let compressed = zstd::bulk::compress(&raw_json, 3).unwrap();
        let compress_time = compress_start.elapsed();
        let compressed_len = compressed.len();

        // 2. bincode serialize (wrapping as BlockDataBroadcast-like struct)
        #[derive(serde::Serialize, serde::Deserialize)]
        struct FakeBroadcast {
            hash: [u8; 32],
            view: u64,
            payload: Vec<u8>,
            timestamp: u64,
        }
        let broadcast = FakeBroadcast {
            hash: [0xab; 32],
            view: 100,
            payload: compressed.clone(),
            timestamp: 1234567890,
        };
        let bincode_start = Instant::now();
        let encoded = bincode::serialize(&broadcast).unwrap();
        let bincode_ser_time = bincode_start.elapsed();
        let wire_len = encoded.len();

        // 3. bincode deserialize
        let bincode_de_start = Instant::now();
        let decoded: FakeBroadcast = bincode::deserialize(&encoded).unwrap();
        let bincode_de_time = bincode_de_start.elapsed();

        // 4. zstd decompress
        let decompress_start = Instant::now();
        let decompressed = zstd::bulk::decompress(&decoded.payload, 64 * 1024 * 1024).unwrap();
        let decompress_time = decompress_start.elapsed();

        assert_eq!(decompressed.len(), raw_len);

        let total_leader = compress_time + bincode_ser_time;
        let total_follower = bincode_de_time + decompress_time;

        println!(
            "  {label:>30}: raw={raw_mb:.1}MB  wire={wire_mb:.1}MB  ratio={ratio:.0}%",
            raw_mb = raw_len as f64 / 1_048_576.0,
            wire_mb = wire_len as f64 / 1_048_576.0,
            ratio = compressed_len as f64 / raw_len as f64 * 100.0,
        );
        println!(
            "    Leader:   compress={comp:.1}ms  bincode_ser={bs:.1}ms  total={lt:.1}ms",
            comp = compress_time.as_secs_f64() * 1000.0,
            bs = bincode_ser_time.as_secs_f64() * 1000.0,
            lt = total_leader.as_secs_f64() * 1000.0,
        );
        println!(
            "    Follower: bincode_de={bd:.1}ms  decompress={dec:.1}ms  total={ft:.1}ms",
            bd = bincode_de_time.as_secs_f64() * 1000.0,
            dec = decompress_time.as_secs_f64() * 1000.0,
            ft = total_follower.as_secs_f64() * 1000.0,
        );
    }
}

/// Benchmark: simulated network transfer of block data to 6 followers.
/// Measures localhost loopback transfer time for realistic block sizes.
/// This approximates QUIC direct push latency on local network.
fn bench_network_transfer_simulation() {
    println!("--- Network Transfer Simulation (localhost, 6 followers) ---");

    let sizes: &[(usize, &str)] = &[
        (1_000, "1KB (empty block)"),
        (100_000, "100KB (small block)"),
        (500_000, "500KB (compressed full block)"),
        (1_000_000, "1MB"),
        (2_000_000, "2MB"),
        (5_000_000, "5MB"),
        (10_000_000, "10MB (100k tx compressed)"),
        (20_000_000, "20MB (100k tx raw)"),
    ];

    let num_followers = 6;

    for &(size, label) in sizes {
        let data: Vec<u8> = vec![0xAB; size];

        // Measure: sequential send to 6 followers via TCP localhost
        let listeners: Vec<_> = (0..num_followers)
            .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        let addrs: Vec<_> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();

        // Spawn receiver threads
        let handles: Vec<_> = listeners
            .into_iter()
            .map(|listener| {
                std::thread::spawn(move || {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut buf = vec![0u8; 65536];
                    let mut total = 0;
                    loop {
                        use std::io::Read;
                        match stream.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => total += n,
                            Err(_) => break,
                        }
                    }
                    total
                })
            })
            .collect();

        let send_start = Instant::now();
        for addr in &addrs {
            use std::io::Write;
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream.write_all(&data).unwrap();
            // Drop stream to signal EOF
        }
        drop(data);
        for h in handles {
            let _ = h.join();
        }
        let send_elapsed = send_start.elapsed();

        let total_bytes = size * num_followers;
        let throughput_gbps =
            total_bytes as f64 * 8.0 / send_elapsed.as_secs_f64() / 1_000_000_000.0;

        println!(
            "  {label:>30} × {num_followers} followers: {ms:>7.1}ms  total={total_mb:.1}MB  throughput={gbps:.1}Gbps",
            ms = send_elapsed.as_secs_f64() * 1000.0,
            total_mb = total_bytes as f64 / 1_048_576.0,
            gbps = throughput_gbps,
        );
    }
}

/// Benchmark: simulated tx submission to pool.
/// Measures how fast we can generate and enqueue transactions.
fn bench_tx_pool_submission() {
    println!("--- Tx Pool Submission Simulation ---");

    // Simulate: generate TxEnv objects (like stress tool does)
    let counts = [1_000, 10_000, 50_000, 100_000];

    for &count in &counts {
        let start = Instant::now();
        let mut txs = Vec::with_capacity(count);
        for i in 0..count {
            let sender = Address::from_word(U256::from(0x10000 + i as u64).into());
            let receiver = Address::from_word(U256::from(0x90000 + i as u64).into());
            let tx = TxEnv::builder()
                .caller(sender)
                .kind(TxKind::Call(receiver))
                .value(U256::from(1))
                .gas_limit(21_000)
                .gas_price(0)
                .nonce(0)
                .build()
                .unwrap();
            txs.push(tx);
        }
        let gen_elapsed = start.elapsed();

        // Simulate enqueue: push to Vec (approximates channel send)
        let enqueue_start = Instant::now();
        let mut pool: Vec<TxEnv> = Vec::with_capacity(count);
        for tx in txs {
            pool.push(tx);
        }
        let enqueue_elapsed = enqueue_start.elapsed();

        println!(
            "  {count:>6} txs: generate={gen:.1}ms  enqueue={enq:.1}ms  total={tot:.1}ms  rate={rate:.0}k tx/s",
            gen = gen_elapsed.as_secs_f64() * 1000.0,
            enq = enqueue_elapsed.as_secs_f64() * 1000.0,
            tot = (gen_elapsed + enqueue_elapsed).as_secs_f64() * 1000.0,
            rate = count as f64 / (gen_elapsed + enqueue_elapsed).as_secs_f64() / 1000.0,
        );
    }
}

/// Full pipeline timeline: all stages for a 100k tx block.
/// Measures each stage independently and shows which can overlap.
fn bench_full_pipeline_timeline() {
    println!("--- Full Pipeline Timeline (100k tx block, 90% transfer + 10% ERC-20) ---");
    println!();

    let total_txs: usize = 100_000;
    let erc20_count = total_txs / 10;
    let transfer_count = total_txs - erc20_count;

    // ERC-20 bytecode
    let bytecode: Vec<u8> = vec![
        0x60, 0x00, 0x54, 0x60, 0x01, 0x90, 0x03, 0x60, 0x00, 0x55,
        0x60, 0x01, 0x54, 0x60, 0x01, 0x01, 0x60, 0x01, 0x55,
        0x60, 0x01, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
    ];

    // ============ Stage 1: EVM Execution ============
    let mut db = CacheDB::<EmptyDB>::default();
    let mut contract_info = AccountInfo::default();
    contract_info.code = Some(revm::bytecode::Bytecode::new_legacy(bytecode.clone().into()));
    contract_info.code_hash = contract_info.code.as_ref().unwrap().hash_slow();
    db.insert_account_info(ERC20_ADDR, contract_info);
    db.insert_account_storage(ERC20_ADDR, U256::ZERO, U256::from(u128::MAX)).unwrap();
    for i in 0..total_txs {
        let sender = Address::from_word(U256::from(0x10000 + i as u64).into());
        db.insert_account_info(sender, AccountInfo {
            balance: U256::from(u128::MAX),
            nonce: 0,
            ..Default::default()
        });
    }

    let ctx = Context::mainnet().with_db(&mut db);
    let mut evm = ctx.build_mainnet();

    let evm_start = Instant::now();
    for i in 0..transfer_count {
        let sender = Address::from_word(U256::from(0x10000 + i as u64).into());
        let receiver = Address::from_word(U256::from(0x90000 + i as u64).into());
        let tx = TxEnv::builder()
            .caller(sender)
            .kind(TxKind::Call(receiver))
            .value(U256::from(1))
            .gas_limit(21_000)
            .gas_price(0)
            .nonce(0)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    for i in 0..erc20_count {
        let sender = Address::from_word(U256::from(0x10000 + transfer_count as u64 + i as u64).into());
        let tx = TxEnv::builder()
            .caller(sender)
            .kind(TxKind::Call(ERC20_ADDR))
            .gas_limit(100_000)
            .gas_price(0)
            .nonce(0)
            .build()
            .unwrap();
        let _ = evm.transact_commit(tx);
    }
    let evm_time = evm_start.elapsed();
    let total_gas = transfer_count as u64 * 21_000 + erc20_count as u64 * 100_000;

    // ============ Stage 2: Serialize to JSON ============
    // Simulate payload JSON (~200 bytes per tx)
    let tx_template = r#"{"hash":"0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890","from":"0x1234567890123456789012345678901234567890","to":"0x0987654321098765432109876543210987654321","value":"0x1","gas":"0x5208","nonce":"0x0"}"#;
    let serialize_start = Instant::now();
    let mut payload_json = Vec::with_capacity(total_txs * 220);
    payload_json.push(b'[');
    for i in 0..total_txs {
        if i > 0 { payload_json.push(b','); }
        payload_json.extend_from_slice(tx_template.as_bytes());
    }
    payload_json.push(b']');
    let serialize_time = serialize_start.elapsed();
    let raw_size = payload_json.len();

    // ============ Stage 3: Compress ============
    let compress_start = Instant::now();
    let compressed = zstd::bulk::compress(&payload_json, 3).unwrap();
    let compress_time = compress_start.elapsed();
    let compressed_size = compressed.len();

    // ============ Stage 4: Network transfer (simulated 6 followers) ============
    let num_followers = 6;
    let listeners: Vec<_> = (0..num_followers)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let addrs: Vec<_> = listeners.iter().map(|l| l.local_addr().unwrap()).collect();

    let handles: Vec<_> = listeners
        .into_iter()
        .map(|listener| {
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 65536];
                let mut total = 0;
                loop {
                    use std::io::Read;
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(_) => break,
                    }
                }
                total
            })
        })
        .collect();

    let net_start = Instant::now();
    for addr in &addrs {
        use std::io::Write;
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(&compressed).unwrap();
    }
    for h in handles {
        let _ = h.join();
    }
    let net_time = net_start.elapsed();

    // ============ Stage 5: Decompress (follower side) ============
    let decompress_start = Instant::now();
    let _decompressed = zstd::bulk::decompress(&compressed, 64 * 1024 * 1024).unwrap();
    let decompress_time = decompress_start.elapsed();

    // ============ Stage 6: Consensus voting (from empty block test: ~20-36ms) ============
    let consensus_time_ms = 30.0; // measured from live testnet

    // ============ Stage 7: Finalize FCU ============
    let finalize_time_ms = 5.0; // measured from live testnet

    // ============ Print Timeline ============
    let evm_ms = evm_time.as_secs_f64() * 1000.0;
    let ser_ms = serialize_time.as_secs_f64() * 1000.0;
    let comp_ms = compress_time.as_secs_f64() * 1000.0;
    let net_ms = net_time.as_secs_f64() * 1000.0;
    let dec_ms = decompress_time.as_secs_f64() * 1000.0;

    let leader_serial = evm_ms + ser_ms + comp_ms + net_ms;
    let follower_serial = dec_ms + evm_ms; // follower re-executes
    let total_serial = leader_serial + consensus_time_ms + finalize_time_ms;

    println!("  Block: {total_txs} txs ({transfer_count} transfer + {erc20_count} ERC-20)");
    println!("  Total gas: {:.0}M ({:.1}% of 500M)", total_gas as f64 / 1e6, total_gas as f64 / 5e8 * 100.0);
    println!("  Raw payload: {:.1}MB  Compressed: {:.1}MB  Ratio: {:.0}%",
        raw_size as f64 / 1_048_576.0,
        compressed_size as f64 / 1_048_576.0,
        compressed_size as f64 / raw_size as f64 * 100.0);
    println!();
    println!("  ┌─────────────────────────────────────────────────────────────┐");
    println!("  │ Stage                          │  Time (ms) │  % of total  │");
    println!("  ├─────────────────────────────────────────────────────────────┤");
    println!("  │ 1. EVM execution (leader)      │ {:>9.1}  │ {:>9.1}%   │", evm_ms, evm_ms / total_serial * 100.0);
    println!("  │ 2. Serialize to JSON           │ {:>9.1}  │ {:>9.1}%   │", ser_ms, ser_ms / total_serial * 100.0);
    println!("  │ 3. zstd compress               │ {:>9.1}  │ {:>9.1}%   │", comp_ms, comp_ms / total_serial * 100.0);
    println!("  │ 4. Network send (6 followers)  │ {:>9.1}  │ {:>9.1}%   │", net_ms, net_ms / total_serial * 100.0);
    println!("  │ 5. zstd decompress (follower)  │ {:>9.1}  │ {:>9.1}%   │", dec_ms, dec_ms / total_serial * 100.0);
    println!("  │ 6. Consensus voting            │ {:>9.1}  │ {:>9.1}%   │", consensus_time_ms, consensus_time_ms / total_serial * 100.0);
    println!("  │ 7. Finalize FCU                │ {:>9.1}  │ {:>9.1}%   │", finalize_time_ms, finalize_time_ms / total_serial * 100.0);
    println!("  ├─────────────────────────────────────────────────────────────┤");
    println!("  │ Total (serial, no overlap)     │ {:>9.1}  │  100.0%     │", total_serial);
    println!("  └─────────────────────────────────────────────────────────────┘");
    println!();
    println!("  *** Pipeline with parallelism (actual expected timeline) ***");
    println!();

    // Leader critical path: EVM → serialize → compress → broadcast(start)
    // Consensus starts after broadcast begins (voting can start as followers receive data)
    // Follower import (EVM re-exec) runs in parallel with consensus voting
    //
    // Leader:   [-----EVM-----][ser][comp][---net send---]
    // Follower:                            [dec][--EVM re-exec--]  (parallel with consensus)
    // Consensus:                           [-----voting-----]
    // Finalize:                                              [FCU]
    //
    // Critical path = EVM + ser + comp + max(net, consensus+follower_import) + finalize

    let leader_build = evm_ms + ser_ms + comp_ms;
    // After broadcast starts, consensus and follower import run in parallel
    // Consensus needs: net propagation + voting
    // Follower needs: net propagation + decompress + EVM re-exec (for eager import)
    let consensus_path = net_ms + consensus_time_ms;
    let follower_path = net_ms + dec_ms + evm_ms; // follower eager import (parallel with consensus)
    let parallel_mid = consensus_path.max(follower_path);
    let pipeline_total = leader_build + parallel_mid + finalize_time_ms;

    println!("  Leader build (EVM+ser+comp):    {:>7.1}ms", leader_build);
    println!("  ├─ Consensus path (net+vote):   {:>7.1}ms  (parallel)", consensus_path);
    println!("  ├─ Follower path (net+dec+EVM): {:>7.1}ms  (parallel)", follower_path);
    println!("  └─ Bottleneck of parallel:      {:>7.1}ms", parallel_mid);
    println!("  Finalize FCU:                   {:>7.1}ms", finalize_time_ms);
    println!("  ─────────────────────────────────────────");
    println!("  Pipeline total:                 {:>7.1}ms", pipeline_total);
    println!();

    // TPS projections at different slot times
    let tps_per_ms = total_txs as f64 / pipeline_total;
    println!("  *** TPS projections ***");
    println!("  If slot = pipeline_total ({:.0}ms):  {:.0} TPS", pipeline_total, tps_per_ms * 1000.0);
    for slot_ms in [500.0, 1000.0, 2000.0, 4000.0] {
        if slot_ms >= pipeline_total {
            let idle = slot_ms - pipeline_total;
            println!("  If slot = {slot_ms:.0}ms:  {:.0} TPS  (idle {idle:.0}ms = {:.0}%)",
                total_txs as f64 / (slot_ms / 1000.0),
                idle / slot_ms * 100.0);
        } else {
            println!("  If slot = {slot_ms:.0}ms:  *** CANNOT FIT — need {pipeline_total:.0}ms ***");
        }
    }

    println!();
    println!("  *** 时序图 (100k tx block) ***");
    println!();
    let scale = 200.0 / pipeline_total; // scale to ~200 chars width
    let bar = |start: f64, end: f64, label: &str| {
        let s = (start * scale) as usize;
        let e = (end * scale) as usize;
        let width = e.saturating_sub(s).max(1);
        format!("  {:>width_pad$}{}{} {label} ({:.0}ms)",
            "", "█".repeat(width), "",
            end - start,
            width_pad = s)
    };

    // Timeline bars
    let t0 = 0.0;
    let t1 = evm_ms; // EVM done
    let t2 = t1 + ser_ms + comp_ms; // serialize + compress done = broadcast starts
    let t3 = t2 + net_ms; // network send complete
    let t4 = t2 + parallel_mid; // parallel section done
    let t5 = t4 + finalize_time_ms; // finalize done

    let to_w = |v: f64| -> usize { (v * scale).max(1.0) as usize };
    let to_pad = |v: f64| -> usize { (v * scale) as usize };

    println!("  Timeline (ms): 0{:>w$}{:.0}", pipeline_total, w = to_pad(pipeline_total));
    println!("  Leader EVM:    {:pad$}{}", "", "█".repeat(to_w(t1 - t0)), pad = to_pad(t0));
    println!("  Ser+Compress:  {:pad$}{}", "", "█".repeat(to_w(t2 - t1)), pad = to_pad(t1));
    println!("  Net send(×6):  {:pad$}{}", "", "▓".repeat(to_w(t3 - t2)), pad = to_pad(t2));
    println!("  Consensus:     {:pad$}{}", "", "░".repeat(to_w(consensus_path)), pad = to_pad(t2));
    println!("  F.EagerImport: {:pad$}{}", "", "▒".repeat(to_w(follower_path)), pad = to_pad(t2));
    println!("  Finalize:      {:pad$}{}", "", "█".repeat(to_w(t5 - t4)), pad = to_pad(t4));
    println!();
    println!("  Legend: █=sequential(blocking)  ▓=network  ░=consensus(parallel)  ▒=follower(parallel)");
}
