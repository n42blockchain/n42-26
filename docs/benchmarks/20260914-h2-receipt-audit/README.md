# H2 successful-receipt audit evidence

All example reports here are **synthetic offline fixtures**, not fleet results.
The threshold examples use 0.1 TPS only to test pass/fail behavior. Production
qualification defaults to 1,000,000 successful common committed TPS.

From the repository root with the fixed patched dependencies prepared:

```sh
cargo build --locked --offline --bin n42-keccak
N42_KECCAK_BIN="$PWD/target/debug/n42-keccak" python3 scripts/test-h2-tps-audit.py
cargo test --locked --offline -p n42-consensus receipt::tests
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
```

The archived Python tests can also run with an absolute `N42_KECCAK_BIN`.
`fixtures/gov5-receipt-audit.json` is shared with the production Rust root test.
The Python encoder and production Rust function agree on its native receipt
root, including a failed receipt and a long log payload.

The helper uses streaming Keccak-256, not NIST SHA3-256. The audit verifies
receipt roots against RPC-provided headers. It does not verify BLS certificates,
header hashes, transaction trie roots, EVM reexecution or hardware durability.
Full-block receipt JSON is materialized one node/block at a time; target-scale
RPC volume, response limits and memory remain to be measured.
