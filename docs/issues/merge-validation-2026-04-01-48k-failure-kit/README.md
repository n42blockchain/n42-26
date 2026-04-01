# Merge Validation Failure Kit

Source artifacts:
- `/private/tmp/n42-dev2603-merge-test.gtMDEF/.artifacts/7node-48k-20260401-154711`

Context:
- Merge test tree was built from `origin/dev2603@f202932` plus local commit `5d0c052`.
- The merged tree needed one compile fix before release build: pass `snapshot.last_voted_view` into `ConsensusEngine::with_recovered_state(...)`.

Smoke result:
- `scenario 1` passed
- `100` blocks
- average block interval about `3.99s`

7-node 48k result:
- inject summary: `sent=1,258,000`, `errors=0`, `injection_tps=41,678`
- chain window `289 -> 299`: `overall_tps=45,055.6`, `avg_block_time=1.0s`, `p50_tps=48,000`, `p95_tps=48,000`, `max_block_tps=48,000`, `max_tx_in_block=48,000`
- common height `299` block hash matched on all 7 nodes
- run is not a clean pass: heights later stuck at `301/309/312/312/312/299/302`
- pending pool remained on lagging nodes: `18000=94,000`, `18005=106,500`, `18006=101,000`

Included files:
- `compile-fix-note.md`: the minimal build fix needed to compile the merged tree
- `validator-0-engine-retry.txt`: leader-side `fork_choice_updated` retry loop and timeouts
- `validator-5-invalid-ancestor.txt`: lagging follower showing invalid ancestor chain from block `300`
- `validator-6-sync-invalid.txt`: lagging follower showing `new_payload rejected`, sync with `0` blocks, and repeated invalid imports

These files are intentionally trimmed to only the error paths needed for diagnosis and follow-up fixes.
