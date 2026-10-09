//! Ethereum profile control; isolated process for the write-once QMDB provider registry.
#[path = "common/parallel_qmdb_builder.rs"]
mod fixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_cancun_builder_reads_qmdb_and_prepares_the_native_root() -> eyre::Result<()> {
    fixture::run(false).await
}
