# rBTC task-executor acceptance fixture

This isolated fixture proves that N42's real Reth `TaskExecutor` can own an
rBTC `NodeHandle::wait` future while the host retains a `NodeController` for
graceful shutdown:

```bash
cargo test --manifest-path integration-tests/rbtc-embedding/Cargo.toml --locked
```

Keeping the fixture outside the `n42-node-bin` package avoids compiling the
unrelated EVM, RocksDB, networking, and node-assembly dependency graph. The
fixture is a technical linkage test only. Distribution of a combined binary
still requires the GPL-compatible licensing decision documented by rBTC.
