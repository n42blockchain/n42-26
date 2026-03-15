mod erc20;
mod genesis;
mod mobile_sim;
mod node_manager;
mod rpc_client;
mod scenarios;
mod test_helpers;
mod tx_engine;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Parses a comma-separated list of scenario numbers from `E2E_SCENARIO_FILTER`.
///
/// Invalid entries are silently ignored. Returns an empty vec if the string is empty
/// or contains no valid numbers.
fn parse_scenario_filter(filter: &str) -> Vec<u32> {
    filter
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect()
}

#[derive(Parser)]
#[command(name = "e2e-test", about = "N42 End-to-End Test Suite")]
struct Cli {
    /// Run a specific scenario (1-13), or omit to run all.
    #[arg(long)]
    scenario: Option<u32>,

    /// Run all scenarios sequentially.
    #[arg(long)]
    all: bool,

    /// Path to the n42-node binary. Auto-detected if not specified.
    #[arg(long)]
    binary: Option<String>,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Find the n42-node binary.
    let binary_path = match cli.binary {
        Some(path) => std::path::PathBuf::from(path),
        None => node_manager::find_n42_binary()?,
    };

    info!(binary = %binary_path.display(), "using n42-node binary");

    // Determine which scenarios to run.
    //
    // Priority: --scenario CLI flag > --all flag > E2E_SCENARIO_FILTER env var > default (1).
    //
    // E2E_SCENARIO_FILTER accepts a comma-separated list of scenario numbers, e.g. "1,2,3".
    // This is used by CI workflows to limit which scenarios run in a given job.
    let scenarios: Vec<u32> = if let Some(s) = cli.scenario {
        vec![s]
    } else if cli.all {
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
    } else if let Ok(filter) = std::env::var("E2E_SCENARIO_FILTER") {
        parse_scenario_filter(&filter)
    } else {
        // Default: run scenario 1 (smoke test).
        vec![1]
    };

    let mut passed = 0u32;
    let mut failed = 0u32;

    for scenario in &scenarios {
        info!(scenario, "running scenario");

        let result = match scenario {
            1 => scenarios::scenario1_single_node::run(binary_path.clone()).await,
            2 => scenarios::scenario2_rpc_load::run(binary_path.clone()).await,
            3 => scenarios::scenario3_erc20::run(binary_path.clone()).await,
            4 => scenarios::scenario4_multi_node::run(binary_path.clone()).await,
            5 => scenarios::scenario5_mobile::run(binary_path.clone()).await,
            6 => scenarios::scenario6_stress::run(binary_path.clone()).await,
            7 => scenarios::scenario7_21x21::run(binary_path.clone()).await,
            8 => scenarios::scenario8_mobile_evm::run(binary_path.clone()).await,
            9 => scenarios::scenario9_long_run::run(binary_path.clone()).await,
            10 => scenarios::scenario10_chaos::run(binary_path.clone()).await,
            11 => scenarios::scenario11_quic_10k::run(binary_path.clone()).await,
            12 => scenarios::scenario12_blockscout_rpc::run(binary_path.clone()).await,
            13 => scenarios::scenario13_rewards::run(binary_path.clone()).await,
            _ => {
                info!(scenario, "unknown scenario, skipping");
                continue;
            }
        };

        match result {
            Ok(()) => {
                info!(scenario, "PASSED");
                passed += 1;
            }
            Err(e) => {
                tracing::error!(scenario, error = %e, "FAILED");
                failed += 1;
            }
        }
    }

    info!(
        passed,
        failed,
        total = passed + failed,
        "test suite complete"
    );

    if failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}
