//! Isolated acceptance fixture for hosting rBTC on N42's Reth task executor.

#[cfg(test)]
mod tests {
    use bitcoin::Network;
    use rbtc::node::{NodeBuilder, NodeLifecycle};
    use reth_tasks::TaskExecutor;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::{net::TcpListener, sync::oneshot, time::timeout};

    #[tokio::test]
    async fn reth_critical_task_executor_owns_rbtc_wait_future() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let _connection = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let directory = TempDir::new().unwrap();
        let handle = NodeBuilder::new(Network::Regtest, directory.path())
            .connect(remote)
            .launch()
            .unwrap();
        let controller = handle.controller();
        let (finished, finished_rx) = oneshot::channel();
        let executor = TaskExecutor::test();

        executor.spawn_critical_task("rbtc-regtest-fixture", async move {
            let result = handle.wait().await;
            let _ = finished.send(result);
        });
        controller.request_shutdown();

        timeout(Duration::from_secs(2), finished_rx)
            .await
            .expect("Reth critical-task executor must poll the rBTC wait future")
            .expect("fixture completion sender must remain alive")
            .expect("rBTC must stop cleanly through its host controller");
        assert_eq!(controller.lifecycle(), NodeLifecycle::Stopped);
        peer.abort();
    }
}
