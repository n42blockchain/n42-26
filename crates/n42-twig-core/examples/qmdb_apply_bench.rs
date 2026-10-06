//! Synthetic QMDB block apply/undo benchmark; not committed network TPS.
use n42_twig_core::{qmdb_compat::QmdbOperation, qmdb_leaf_tree::QmdbLeafTree};
use std::{hint::black_box, time::Instant};

fn operations(count: usize, tag: u8) -> Vec<QmdbOperation> {
    (0..count)
        .map(|i| QmdbOperation {
            key: *blake3::hash(&i.to_le_bytes()).as_bytes(),
            value: Some(vec![tag; 72]),
        })
        .collect()
}

fn main() {
    for count in [1, 128, 25_000] {
        let mut tree = QmdbLeafTree::new();
        tree.apply_sorted_ops(operations(100_000, 1)).unwrap();
        let parent = tree.root();
        let ops = operations(count, 2);
        let rounds = if count == 25_000 { 20 } else { 1_000 };
        let start = Instant::now();
        for _ in 0..rounds {
            let (root, undo) = tree.apply_sorted_ops_recorded(ops.iter().cloned()).unwrap();
            black_box(root);
            tree.apply_undo(&undo).unwrap();
            assert_eq!(black_box(tree.root()), parent);
        }
        println!(
            "ops={count} rounds={rounds} apply_undo_root_us={}",
            start.elapsed().as_micros()
        );
    }
}
