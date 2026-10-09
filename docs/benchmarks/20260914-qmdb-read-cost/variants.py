"""Recreate the read-index experiments from the frozen production source."""
import argparse
from pathlib import Path

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--check", action="store_true")
a = p.parse_args()
out = Path(__file__).resolve().parent
base = (out / "production-read-view.rs").read_text()
base = base.replace("let operations: Vec<_> = (0..keys.min(147_000))", "let mut operations: Vec<_> = (0..keys.min(147_000))")
base = base.replace("        let root = B256::from(tree.apply_sorted_ops(operations.clone()).unwrap());\n        let start", "        operations.sort_unstable_by_key(|operation| operation.key);\n        let root = B256::from(tree.apply_sorted_ops(operations.clone()).unwrap());\n        let start")
fold = base.replace("imbl::HashMap<[u8; 32], ReadValue>", "imbl::GenericHashMap<[u8; 32], ReadValue, alloy_primitives::map::DefaultHashBuilder, imbl::shared_ptr::DefaultSharedPtr>")
ordered = base.replace("imbl::HashMap", "imbl::OrdMap")
variants = {"qmdb": base, "fold": fold, "ordered": ordered,
            "ordered256": ordered.replace("READ_SHARDS: usize = 64", "READ_SHARDS: usize = 256")}
for n in (256, 1024, 4096):
    variants[f"hash{n}"] = base.replace("READ_SHARDS: usize = 64", f"READ_SHARDS: usize = {n}").replace("usize::from(key[0]) % READ_SHARDS", "usize::from(u16::from_le_bytes([key[0], key[1]])) % READ_SHARDS")
for name, source in (("paths", base), ("foldpaths", fold)):
    variants[name] = source.replace("use rayon::prelude::*;", "use rayon::prelude::*;\nuse std::hash::BuildHasher;").replace("|(shard, group)| {", "|(shard, mut group)| {\n                        group.sort_by_cached_key(|operation| shard.values.hasher().hash_one(&operation.key).reverse_bits());")
for name in ("qmdb", "hash4096"):
    variants[name] = variants[name].replace("let mut operations: Vec<_> = (0..keys.min(147_000))", '''let updates = std::env::var("N42_READ_BENCH_UPDATES")
            .ok().map(|v| v.parse::<u64>().expect("valid update count"))
            .unwrap_or(147_000).min(keys);
        assert!(updates > 0);
        let mut operations: Vec<_> = (0..updates)''')
for name, source in variants.items():
    target = out / f"{name}-read-view.rs"
    if a.check:
        assert source == target.read_text(), f"unexpected variant difference: {name}"
    else:
        target.write_text(source)
