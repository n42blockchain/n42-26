//! A follower's parallel import (`N42_PARALLEL_IMPORT=1`) of a native H2
//! block over QMDB-only state, validated by the actual Engine import, with
//! the refusal of a wrong cached output.
#[path = "common/parallel_qmdb_builder.rs"]
mod fixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn follower_parallel_import_is_validated_by_the_engine_and_matches_serial() -> eyre::Result<()> {
    fixture::run_follower().await
}
