# Native-header qualification evidence

Synthetic reports here are offline fixtures, not real H2 fleet results. The
chain-94 header fixture is copied from the existing production codec test.

From the repository root with the fixed patched dependencies prepared:

```sh
cargo build --locked --offline --bin n42-keccak
N42_KECCAK_BIN="$PWD/target/debug/n42-keccak" python3 scripts/test-h2-tps-audit.py
cargo test --locked --offline -p n42-node --lib rpc::tests
cargo test --locked --offline -p n42-consensus --lib gov5_native_header::tests
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
```

The archived Python tests also run with an absolute `N42_KECCAK_BIN`. The helper
source/build contract is unchanged from the preceding receipt-audit milestone.
Every synthetic measured block includes exact native RLP and its computed hash.
The forged-root report demonstrates rejection when RPC root fields no longer
match the header, even though endpoint identities and H2 reports agree.

The RPC serves the existing 8,192-entry memory registry; it is not a persistent
archive. Missing evidence fails verification. BLS commit-proof verification,
transaction-root reconstruction, EVM/business-state checking and real target
throughput/restart/durability qualification remain unfinished.
