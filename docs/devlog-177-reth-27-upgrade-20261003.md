# Reth 2.7.0 Upgrade and N42 Compatibility

Date: 2026-10-03

## Source and dependency pin

The N42 workspace now targets upstream Reth v2.7.0 at `3d592ece6de8c4559987416a544fc215fd6d6921`, plus six ordered N42 integration patches. The adapted source revision is `3452d7d0217095446b0f2632a283e314951da273`. `reth-source.lock` records the source and direct dependency versions. The original `../reth` checkout remains untouched; all patch application and Rust verification used the isolated `.artifacts/reth-upgrade/reth-worktree` clone.

The N42 compatibility port updates the direct Reth, Alloy, alloy-evm and revm pins, QMDB provider/read and state-root integration, native witness extraction, Engine API validation, execution cache behavior, and replay/packet interfaces. Node startup and fleet manifests explicitly keep persistence immediate: state masking, persistence backlog and memory-buffer targets are all zero.

## Correctness fix found during migration

The native parallel import integration exposed a fee-recipient edge case. The transfer workers had read the recipient's parent balance, but the block executor's database view did not have it in cache. Applying only the worker fee delta produced a QMDB root mismatch. The graft now carries the worker-observed parent account value and restores it into Revm's cache before applying the credit. This retains the parent value and correct rollback metadata. The native parallel import test now agrees with serial execution.

The network fallback unit fixture also listed an alternate connected peer while expecting the last peer to be selected as a final fallback. The fixture now contains only the fallback peer, matching the behavior the test names.

## Verification

- `python3 -m unittest discover -s scripts -p 'test_*.py' -v`: 64 passed.
- Focused QMDB provider/builder tests (`qmdb_provider_hook`, `native_cached_qmdb_builder`, `native_parallel_qmdb_builder`, `parallel_qmdb_builder`): all 4 passed.
- `cargo check --workspace --all-targets --locked`: passed against the isolated patched Reth v2.7 source.
- Workspace tests with seven socket-binding cases skipped: 1,754 passed, 0 failed, 26 ignored. The seven omitted tests fail in this sandbox because localhost TCP/UDP binds return `PermissionDenied`; the unrestricted workspace run was attempted and confirmed this boundary.
- `scripts/check-reth-source.sh .artifacts/reth-upgrade/reth-worktree`: passed; all six patches are present and idempotent.
- The actual isolated Reth source passed `git diff --check`. The N42 repository-wide `git diff --check` still reports whitespace in blank context lines inside the pre-existing unified patch `patches/reth-payload-transactions-root.patch`; those lines are patch context, not changed Rust source.

## Remaining gates

No full workspace run with socket tests, seven-validator A/B/A, or post-restart fleet verification is claimed. The seven-node scripts require the shared quiet-hardware claim files and `N42_QUIET_CLAIM=1`; the current session does not hold that claim. Existing October 1 evidence rejected parallel import under its current configuration (A1 41,222.50 TPS, B 37,536.38, A2 41,370.65); this migration has no new fleet performance result.

Small commits and pushes remain unperformed because the N42 Git metadata is read-only (`.git/index.lock` cannot be created). The implementation is left in the current workspace for review; no changes were written to the original dirty Reth checkout.
