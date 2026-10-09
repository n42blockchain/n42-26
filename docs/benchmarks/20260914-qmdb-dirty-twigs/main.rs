use std::time::Instant;
macro_rules! apply_recorded {
    ($mode:ident, $tree:expr, $input:expr) => {{ $input.sort_unstable_by_key(|op| op.key); $tree.apply_sorted_ops_recorded_borrowed(&$input) }};
}
macro_rules! measurement {
    ($core:ident, $version:expr, $keys:expr, $blocks:expr, $mode:ident) => {{
        use $core::qmdb_compat::{QmdbOperation, gov5_account_key, encode_gov5_account_value};
        use $core::qmdb_leaf_tree::QmdbLeafTree;
        let ops = |count: u64, generation: u64| -> Vec<QmdbOperation> {
            (0..count).map(|n| {
                let mut address = [0; 20];
                address[12..].copy_from_slice(&n.to_be_bytes());
                let mut balance = [0; 32];
                balance[24..].copy_from_slice(&(generation * 1_000_000_000).to_be_bytes());
                QmdbOperation { key: gov5_account_key(&address), value: Some(encode_gov5_account_value(generation, &balance, &[0; 32])) }
            }).collect()
        };
        let mut tree = QmdbLeafTree::new();
        tree.apply_sorted_ops(ops($keys, 1)).unwrap();
        for generation in 2..$blocks+2 {
            let mut input = ops($keys.min(147_000), generation);
            if std::env::var("N42_UPDATE_SORTED").as_deref() == Ok("1") { input.sort_unstable_by_key(|op| op.key); }
            let parent = tree.root();
            let cursor = tree.next_slot();
            // Include caller sorting and tree updates, undo and root folding.
            // Every variant includes identical caller sorting and borrowed operation application.
            let started = Instant::now();
            let (root, undo) = apply_recorded!($mode, tree, input).unwrap();
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let undo_digest = blake3::hash(&bincode::serialize(&undo).unwrap());
            tree.apply_undo(&undo).unwrap();
            assert_eq!(tree.root(), parent);
            assert_eq!(tree.next_slot(), cursor);
            assert_eq!(tree.apply_sorted_ops(input.clone()).unwrap(), root);
            for op in &input { assert_eq!(tree.get(&op.key), op.value.as_deref()); }
            println!("dirty_twigs version={} keys={} generation={} updates={} elapsed_ms={:.3} root={} undo={} rollback_verified=true", $version, $keys, generation, input.len(), elapsed_ms, blake3::Hash::from_bytes(root).to_hex(), undo_digest.to_hex());
        }
    }};
}
fn main() {
    let version = std::env::var("N42_UPDATE_VERSION").unwrap();
    let keys = std::env::var("N42_UPDATE_KEYS").unwrap().parse::<u64>().unwrap();
    let blocks = std::env::var("N42_UPDATE_BLOCKS").unwrap().parse::<u64>().unwrap();
    assert!(keys > 0 && blocks > 0);
    match version.as_str() {
        "before" => measurement!(n42_twig_core_before, version, keys, blocks, before),
        "batch" => measurement!(n42_twig_core_batch, version, keys, blocks, batch),
        "after" => measurement!(n42_twig_core_after, version, keys, blocks, after),
        _ => panic!("unknown version"),
    }
}
