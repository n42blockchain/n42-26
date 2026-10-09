//! Native H2 header/wire/payload-validator path through the actual Engine import.
//! This is execution integration, not a connected four-validator consensus test.
#[path = "common/parallel_qmdb_builder.rs"]
mod fixture;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_cancun_first_child_survives_wire_and_engine_import() -> eyre::Result<()> {
    fixture::run(true).await
}
