use metrics::{counter, gauge, histogram};

/// Record a JMT batch update operation.
pub fn record_update(version: u64, leaf_count: usize, duration_ms: f64) {
    histogram!("n42_jmt_update_ms").record(duration_ms);
    gauge!("n42_jmt_version").set(version as f64);
    gauge!("n42_jmt_leaf_count").set(leaf_count as f64);
    counter!("n42_jmt_updates_total").increment(1);
}

/// Record JMT root computation timing.
pub fn record_root_computation(duration_ms: f64) {
    histogram!("n42_jmt_root_ms").record(duration_ms);
}

/// Record a proof generation operation.
pub fn record_proof_generation(proof_size_bytes: usize, duration_ms: f64) {
    histogram!("n42_jmt_proof_ms").record(duration_ms);
    histogram!("n42_jmt_proof_size_bytes").record(proof_size_bytes as f64);
    counter!("n42_jmt_proofs_generated_total").increment(1);
}

/// Record a proof verification operation.
pub fn record_proof_verification(success: bool, duration_ms: f64) {
    histogram!("n42_jmt_verify_ms").record(duration_ms);
    if success {
        counter!("n42_jmt_verify_success_total").increment(1);
    } else {
        counter!("n42_jmt_verify_failure_total").increment(1);
    }
}

/// Record shard-level parallel update timing.
pub fn record_sharded_update(shard_count: usize, total_keys: usize, duration_ms: f64) {
    histogram!("n42_jmt_sharded_update_ms").record(duration_ms);
    gauge!("n42_jmt_shard_count").set(shard_count as f64);
    gauge!("n42_jmt_sharded_total_keys").set(total_keys as f64);
}
