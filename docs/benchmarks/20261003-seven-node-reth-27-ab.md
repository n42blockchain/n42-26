# Seven-node Reth 2.7 performance comparison

Status: harness ready; A/B/A run pending.

The approved upgrade plan compares one pre-upgrade N42 binary (A) with the
Reth 2.7.0 N42 binary (B), then repeats A. Each leg uses the same seven-node
genesis, validator and account configuration, signed 48M workload, 220,000
transaction block cap, RPC response cap, Gov5 builder reuse setting, and
`--parallel-build` option. The runner warms the fleet for 60 seconds before a
60-second scored window, then verifies the seven-node result and restarts the
same fleet data before scoring the leg as complete.

`scripts/run-native-seven-reth-27-ab.sh` consumes immutable binaries and JSON
source manifests through `N42_RETH_OLD_BINARY`, `N42_RETH_NEW_BINARY`,
`N42_RETH_OLD_MANIFEST`, and `N42_RETH_NEW_MANIFEST`. The manifests identify
the Reth revision, Reth source digest, lockfile digest, and N42 source-tree
digest. The summarizer checks workload and runtime configuration hashes,
seven-validator audits, zero failed committed transactions, complete 15-second
score windows, post-run roots and hashes, post-restart agreement, and A-bookend
drift. It reports both total throughput and sustained-window throughput; an
improvement is accepted only when sustained uplift exceeds A-bookend drift.

No performance result is recorded yet. This environment currently has neither
shared claim file (`/data/blockchain/.box-claim-codex` and
`/data/blockchain/wr-logs/.box-claim-codex`) and does not contain either
executable `n42-node` build artifact. The runner was invoked to check its
precondition and exited 2 before starting any node. Acquire the shared claim
through the established controller, provide both immutable binaries and their
source manifests, and run the script to produce `reth-ab-summary.json`.

The earlier fast-transfer seven-node A/B/A is not a candidate for repetition:
its audited 60-second results were 29,647.75 TPS for A1, 24,108.74 TPS for B,
and 29,464.67 TPS for A2. Parallel import also regressed in the recent 220k
comparison. Neither result is evidence about the Reth version change.
