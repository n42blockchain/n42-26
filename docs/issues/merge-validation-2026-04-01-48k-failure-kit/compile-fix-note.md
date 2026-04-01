# Compile Fix Note

Merged code in the isolated validation tree did not pass release build until this call site was updated:

- file: `bin/n42-node/src/main.rs`
- function: `ConsensusEngine::with_recovered_state(...)`
- fix: add `snapshot.last_voted_view` as the final argument

Why this was needed:
- `ConsensusSnapshot` already persists `last_voted_view` in `crates/n42-node/src/persistence.rs`
- `ConsensusEngine::with_recovered_state(...)` now expects that value
- the call site in `main.rs` had not been updated yet

This fix was applied only in the isolated merge-test worktree used for validation, not in the main workspace.
