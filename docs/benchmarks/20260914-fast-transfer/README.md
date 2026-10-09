# Fast-transfer adaptation verification

See [devlog166](../../devlog-166-fast-transfer-adaptation-20260914.md).
These are correctness and compile checks, not four-node TPS results.
The QMDB debug log contains diagnostic timings, which are not a performance comparison.

`changed-source.tar.gz` preserves the listed changed inputs; it is not a standalone workspace.
The pinned upstream file and local input hashes are in `source-manifest.json`.
The original workspace and dependency source baseline are documented in the adjacent
`20260914-canonical-qmdb` artifacts; the local read/commit code is unchanged by this adaptation.

Checks, from a checkout with the fixed reth patches applied:

```sh
cargo test --locked --offline -p n42-execution --lib
N42_FAST_TRANSFER=1 cargo test --locked --offline -p n42-execution --lib
cargo test --locked --offline -p n42-node --lib canonical_qmdb_fast_transfer -- --nocapture
cargo test --locked --offline --release -p n42-node --lib canonical_qmdb_ -- --test-threads=1
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
python3 scripts/test_chain94_fleet.py
```
