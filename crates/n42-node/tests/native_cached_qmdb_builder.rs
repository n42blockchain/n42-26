//! Native Gov5 normalization reuses the builder output while the Engine keeps
//! the authenticated QMDB state-root strategy on the cache-hit path.
#[path = "common/parallel_qmdb_builder.rs"]
mod fixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancun_cached_builder_output_uses_qmdb_root() -> eyre::Result<()> {
    fixture::run_with_cache(true, true).await
}
