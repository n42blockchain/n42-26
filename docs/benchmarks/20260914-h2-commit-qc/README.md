# H2 commit-QC verifier evidence — 2026-09-14

Synthetic certificates only; this is not four-node throughput evidence. At this
milestone the verifier is not yet a mandatory TPS qualification gate. The caller
must authenticate the ordered validator roster, signing profile and chain context.

The archived verifier and RPC source match this milestone. `fixtures/` contains
three signed commit requests and three prepare requests from deterministic test
keys. No production private keys were read. `cli-results.json` records every
request and actual executable result: 27 cases passed. Only H2-v4 reports a
signature explicitly bound to chain identity.

Reproduce against the pinned patched reth dependency documented in devlog 159:

```sh
cargo build --locked --offline --bin n42-verify-commit
cargo test --locked --offline --bin n42-verify-commit
python3 docs/benchmarks/20260914-h2-commit-qc/run-cli-tests.py \
  --binary "$PWD/target/debug/n42-verify-commit" \
  --fixtures docs/benchmarks/20260914-h2-commit-qc/fixtures \
  --out /tmp/n42-commit-cli-reproduction
```

The Rust suite has 3 passing tests and 1 ignored fixture exporter. The node RPC
suite has 18 passing tests; workspace all-target clippy with `-D warnings` passed.
Logs, executable hash and source/fixture hashes are included. The executable
reuses the production message construction and BLS verifier; it is an external
process, not an independently implemented cryptographic verifier.
