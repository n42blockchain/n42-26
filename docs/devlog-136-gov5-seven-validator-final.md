# Gov5 5.7.905 + Rust/Reth seven-validator interop closeout

Date: 2026-08-08

## Result

The mixed-client runtime using Gov5 main `8d7f57db2539b323cc863e5a1274bc1b451439e1` completed the formal 24-hour acceptance window.

- Genesis hash: `0xb71c28109836f120453d097c38819a55b14c49abcc92713037fb9b11201392ec`
- Topology: five Gov5 validators plus Rust validators in slots 0 and 6
- Endpoints: `28501`–`28505`, `29545`, `29546`
- Head soak: 86,400 seconds, 8,119 samples, maximum gap 13 seconds, maximum lag 0, 100,725 blocks produced
- Rust0 resources: 86,443 seconds, 289 samples, 162 threads, 93–96 file descriptors, monotonic counters
- Rust6 resources: 86,450 seconds, 290 samples, 162 threads, 93–96 file descriptors, monotonic counters
- Both Rust leader audits: stride 7, parent chains continuous, expected leader slots exact, all seven endpoint identities exact
- Consensus: validator count 7, committed QC present, zero equivocation evidence
- Critical logs: zero structured errors, panics, fatals, or equivocation records

The final verification artifact is:

`runtime-42-gov5-8d7f57db-reth-sync-seven-validator/evidence/runtime42-seven-validator-final-verification.json`

## Fix included

The final verifier now slurps the line-delimited Gov5 upstream evidence before applying `all(...)`. Previously, applying the predicate directly to JSONL caused jq to iterate object field values and made the finalizer exit without producing its result artifact.

## Reproduction

Run `scripts/gov5-seven-validator-final-verifier.sh` with the runtime, Gov5 repository, expected binary hashes, and the two final leader audit paths supplied through its documented environment variables.
