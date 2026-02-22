use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use tracing::{debug, info, warn};

use crate::rpc_client::RpcClient;

/// Configuration for launching an N42 node.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Path to the n42-node binary.
    pub binary_path: PathBuf,
    /// Path to the genesis JSON file.
    pub genesis_path: PathBuf,
    /// Validator index for deterministic key generation (used with N42_VALIDATOR_KEY).
    pub validator_index: usize,
    /// Total number of validators.
    pub validator_count: usize,
    /// Block build interval in milliseconds (0 = immediate).
    pub block_interval_ms: u64,
    /// Port offset for this node (avoids conflicts when running multiple nodes).
    pub port_offset: u16,
    /// Additional trusted peer multiaddrs for peer discovery.
    pub trusted_peers: Vec<String>,
    /// Pacemaker base timeout override in milliseconds.
    pub base_timeout_ms: Option<u64>,
    /// Pacemaker max timeout override in milliseconds.
    pub max_timeout_ms: Option<u64>,
    /// Startup delay override in milliseconds before first proposal.
    pub startup_delay_ms: Option<u64>,
}

impl NodeConfig {
    /// Creates a config for a single-node dev setup.
    pub fn single_node(binary_path: PathBuf, genesis_path: PathBuf, block_interval_ms: u64) -> Self {
        Self {
            binary_path,
            genesis_path,
            validator_index: 0,
            validator_count: 1,
            block_interval_ms,
            port_offset: 0,
            trusted_peers: vec![],
            base_timeout_ms: None,
            max_timeout_ms: None,
            startup_delay_ms: None,
        }
    }

    pub fn http_port(&self) -> u16 {
        8545 + self.port_offset
    }

    pub fn ws_port(&self) -> u16 {
        8546 + self.port_offset
    }

    pub fn auth_port(&self) -> u16 {
        8551 + self.port_offset
    }

    pub fn p2p_port(&self) -> u16 {
        30303 + self.port_offset
    }

    pub fn consensus_port(&self) -> u16 {
        9400 + self.port_offset
    }

    pub fn starhub_port(&self) -> u16 {
        9443 + self.port_offset
    }
}

/// A running N42 node process.
#[allow(dead_code)]
pub struct NodeProcess {
    child: Child,
    pub http_port: u16,
    pub ws_port: u16,
    pub p2p_port: u16,
    pub consensus_port: u16,
    pub starhub_port: u16,
    /// Wrapped in Option so it can be extracted for restart tests via `stop_keep_data`.
    data_dir: Option<TempDir>,
    pub rpc: RpcClient,
}

impl NodeProcess {
    /// Starts an N42 node with the given configuration (new temp data directory).
    pub async fn start(config: &NodeConfig) -> eyre::Result<Self> {
        let data_dir = TempDir::new()?;
        Self::start_inner(config, data_dir).await
    }

    /// Starts an N42 node reusing an existing data directory (for restart tests).
    pub async fn start_with_datadir(config: &NodeConfig, data_dir: TempDir) -> eyre::Result<Self> {
        Self::start_inner(config, data_dir).await
    }

    async fn start_inner(config: &NodeConfig, data_dir: TempDir) -> eyre::Result<Self> {
        // Generate the deterministic validator key for this index.
        let key_bytes = n42_chainspec::ConsensusConfig::deterministic_key_bytes(config.validator_index);
        let key_hex = hex::encode(key_bytes);

        let http_port = config.http_port();
        let ws_port = config.ws_port();
        let auth_port = config.auth_port();
        let p2p_port = config.p2p_port();
        let consensus_port = config.consensus_port();
        let starhub_port = config.starhub_port();

        let mut cmd = Command::new(&config.binary_path);

        cmd.arg("node")
            .arg("--chain").arg(&config.genesis_path)
            .arg("--datadir").arg(data_dir.path())
            .arg("--http")
            .arg("--http.port").arg(http_port.to_string())
            .arg("--ws")
            .arg("--ws.port").arg(ws_port.to_string())
            .arg("--authrpc.port").arg(auth_port.to_string())
            .arg("--port").arg(p2p_port.to_string())
            .arg("--ipcdisable")
            .arg("--disable-discovery");

        cmd.env("N42_VALIDATOR_KEY", &key_hex)
            .env("N42_VALIDATOR_COUNT", config.validator_count.to_string())
            .env("N42_CONSENSUS_PORT", consensus_port.to_string())
            .env("N42_STARHUB_PORT", starhub_port.to_string())
            .env("N42_DATA_DIR", data_dir.path());

        if config.block_interval_ms > 0 {
            cmd.env("N42_BLOCK_INTERVAL_MS", config.block_interval_ms.to_string());
        }

        if let Some(base_timeout) = config.base_timeout_ms {
            cmd.env("N42_BASE_TIMEOUT_MS", base_timeout.to_string());
        }
        if let Some(max_timeout) = config.max_timeout_ms {
            cmd.env("N42_MAX_TIMEOUT_MS", max_timeout.to_string());
        }
        if let Some(startup_delay) = config.startup_delay_ms {
            cmd.env("N42_STARTUP_DELAY_MS", startup_delay.to_string());
        }

        if !config.trusted_peers.is_empty() {
            cmd.env("N42_TRUSTED_PEERS", config.trusted_peers.join(","));
        }

        let stdout_log = std::fs::File::create(format!("/tmp/n42-node-{}.log", config.validator_index))
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
        let stderr_log = std::fs::File::create(format!("/tmp/n42-node-{}.err.log", config.validator_index))
            .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap());
        cmd.stdout(Stdio::from(stdout_log)).stderr(Stdio::from(stderr_log));

        info!(
            binary = %config.binary_path.display(),
            http_port,
            validator_index = config.validator_index,
            "starting N42 node"
        );

        let child = cmd.spawn()?;

        let rpc = RpcClient::new(format!("http://127.0.0.1:{http_port}"));

        let node = NodeProcess {
            child,
            http_port,
            ws_port,
            p2p_port,
            consensus_port,
            starhub_port,
            data_dir: Some(data_dir),
            rpc,
        };

        // Wait for the node to become ready.
        node.wait_for_rpc_ready(Duration::from_secs(60)).await?;

        Ok(node)
    }

    async fn wait_for_rpc_ready(&self, timeout: Duration) -> eyre::Result<()> {
        let start = tokio::time::Instant::now();
        let poll_interval = Duration::from_millis(500);

        loop {
            if start.elapsed() > timeout {
                return Err(eyre::eyre!(
                    "node did not become ready within {} seconds",
                    timeout.as_secs()
                ));
            }

            match self.rpc.block_number().await {
                Ok(n) => {
                    info!(block_number = n, http_port = self.http_port, "node is ready");
                    return Ok(());
                }
                Err(_) => {
                    debug!(http_port = self.http_port, "waiting for node to become ready...");
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    }

    /// Stops the node but returns the data directory for reuse in restart tests.
    pub fn stop_keep_data(mut self) -> eyre::Result<TempDir> {
        info!(http_port = self.http_port, "stopping N42 node (keeping data)");

        #[cfg(unix)]
        {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
        }

        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }

        let _ = self.child.wait();
        self.data_dir.take().ok_or_else(|| eyre::eyre!("data dir already taken"))
    }

    /// Sends SIGTERM and waits for the node process to exit.
    pub fn stop(mut self) -> eyre::Result<()> {
        info!(http_port = self.http_port, "stopping N42 node");

        // Send SIGTERM on Unix.
        #[cfg(unix)]
        {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
        }

        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }

        match self.child.wait() {
            Ok(status) => {
                info!(http_port = self.http_port, ?status, "node process exited");
                Ok(())
            }
            Err(e) => {
                warn!(http_port = self.http_port, error = %e, "error waiting for node to exit");
                Ok(())
            }
        }
    }

    /// Returns the WebSocket URL for this node.
    #[allow(dead_code)]
    pub fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.ws_port)
    }

    /// Returns the HTTP URL for this node.
    pub fn http_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.http_port)
    }
}

impl Drop for NodeProcess {
    fn drop(&mut self) {
        // Best-effort cleanup: kill the child process if it's still running.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Finds the n42-node binary in the cargo build output.
pub fn find_n42_binary() -> eyre::Result<PathBuf> {
    // Try to find in the cargo target directory.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| ".".to_string());

    // Walk up to find the workspace root. Prefer release over debug for performance.
    let mut dir = PathBuf::from(&manifest_dir);
    loop {
        let target = dir.join("target/release/n42-node");
        if target.exists() {
            return Ok(target);
        }
        let target = dir.join("target/debug/n42-node");
        if target.exists() {
            return Ok(target);
        }
        if !dir.pop() {
            break;
        }
    }

    // Fallback: try PATH.
    if let Ok(output) = Command::new("which").arg("n42-node").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    Err(eyre::eyre!(
        "n42-node binary not found. Build it first with: cargo build -p n42-node-bin"
    ))
}
