# P2-4: quarterly geth/reth/Erigon borrow audit

Date: 2026-07-18

Audit window: 2026-04-18 through 2026-07-18

Next due: 2026-10-18

Scope: `docs/codex-task-sync-from-gov5-2026H1.md` P2-4

## Outcome

The first quarterly run is complete. It found one applicable local correctness
bug in the experimental Block-STM path and fixed it on a separate branch:

- branch: `fix/gov5-sync-p2-selfdestruct-shadow`;
- commit: `e1c6303 fix(parallel-evm): preserve selfdestruct storage wipes`;
- prerequisite: `fix/gov5-sync-parallel-validation-order` at `1e5a8ea`;
- validation: 14/14 package tests, 30 consecutive full-package rounds, and
  clippy with warnings denied.

The other Stage-2 candidates are inherited from the pinned reth fork, covered by
n42's different architecture, or remain deployment hardening rather than a code
port. No geth/Erigon code was copied by title alone.

The task book calls this a continuation of “devlog-93”. That filename is not
present in this Rust branch. The auditable source material is the task book plus
gov5's `docs/ethel/{reth,geth,erigon}-borrow-audit.md`; this document becomes the
canonical Rust quarterly continuation.

## Source freeze and method

The local upstream repositories were fetched before review. No working branch
was advanced or rewritten.

| Client | Reviewed ref | Head at audit | Commits in window |
|---|---|---|---:|
| geth | `origin/master` | `bb6401ee5f91cb5138aed19177603891a6f5ea60` | 296 |
| Erigon | `origin/main` | `475d38428be442b1927fd88df418a94748b9858d` | 993 |
| n42 reth fork | `origin/chore/reth-upstream-20260714` | `ccce351451770fa9213c4e115828f757299a845b` | 642 |

The n42-26 workspace uses local path dependencies to that reth pin. It also
requires Rust 1.97 at the reviewed head.

Method:

1. enumerate the entire three-month commit window;
2. filter subjects and paths for fixes/security/DoS/races/panics, execution,
   txpool, RPC/WS, network, state, trie/commitment, pruning, and import;
3. open each selected diff rather than infer behavior from its title;
4. find the corresponding n42-26 or pinned-reth path;
5. classify as applicable gap, inherited/covered, architecture-different,
   forward-fork, or feature absent;
6. for an applicable gap, require a separate branch, adversarial regression,
   and package-level validation.

## 2026-Q3 candidate disposition

| Source / defect family | n42-26 mapping | Verdict / action |
|---|---|---|
| reth EIP-7702 authority reservation and in-flight delegation limits | pinned `reth-transaction-pool` tracks `authority_ids` and rejects out-of-order delegated txs, excess in-flight txs, and reserved authorities | **Covered by dependency**; n42 does not replace the pool |
| geth `2133e014a`: reject `gasPrice` with `authorizationList` | Alloy selects EIP-7702 whenever `authorization_list` is present, requires both EIP-1559 fee fields, and builds `TxEip7702`; the legacy-precedence path that dropped geth's authorization list is absent | **Exact defect absent**. Alloy currently ignores the extra `gas_price` while building 7702 instead of reporting a field conflict; retain an interop regression if unsigned RPC signing is exposed |
| reth `eth_simulateV1` fixes (`f2d2bd2`, `b8116b4`, later nonce/fork fixes) | pinned reth caps simulated blocks at 256, acquires the shared blocking-I/O semaphore, and contains the later fork/nonce fixes | **Covered by dependency** |
| geth `7c1959b30`: configurable HTTP body limit | reth applies `rpc.max-request-size` to both HTTP and WS; default is 15MiB | **Covered, stricter transport-wide limit** |
| WS upgrade/request flooding | reth caps combined HTTP/WS connections at 500, subscriptions/connection at 1,024, request size at 15MiB, and expensive EVM calls through semaphores | **Partially covered**. There is no default per-IP requests/second policy; public endpoints must use an ingress/reverse-proxy limit or install RPC middleware before exposure |
| Erigon `2409a7a208`: EIP-7702 delegation code read omitted from OCC version map | n42 Block-STM versions the whole `AccountInfo` including `code_hash`; a changed delegation hash invalidates the recorded account snapshot, and code bytes are immutable by hash | **Architecture covered**; add a dedicated 7702 differential test before production wiring |
| Erigon `455449f0e5`: delayed coinbase tip after same-tx SELFDESTRUCT | local executor had a broader bug: revm selfdestruct status was never translated, storage had no account-wide wipe, and deferred-beneficiary deltas could cross a wipe | **Applicable and fixed** in `e1c6303` |
| Erigon `a01ede33dd`: parallel log index / missing WS notifications | n42 Block-STM is not wired to the production reth receipt/subscription path | **Feature absent now**; mandatory differential gate before wiring |
| Erigon RPC batch deadlock/gzip race (`c9d9061`, `63cc7de`) | Go handler ownership and gzip flush implementation are not used by jsonrpsee | **Architecture different** |
| Erigon parallel commitment wrong-root cluster | n42 uses its own QMDB binary twig sidecar and reth state-root path, not Erigon's branch-cache/deep-fold implementation | **No direct port**; defect classes remain covered by QMDB cross-root, restart, ordering, and poison tests |
| reth July import/static/prune fixes | the exact pinned reth branch already contains incomplete-EOF rejection, prune-distance-derived receipt file sizing, pending-import wiring, and related fixes | **Inherited**; validate through dependency upgrade gates, not duplicate Rust code |
| geth state/snapshot/downloader fixes | n42 production state/import paths are reth, not geth's StateDB/pathdb/snap stack | **Architecture different**; only wire-format/interoperability cases remain candidates |

## The fixed Block-STM defect

The local review did not stop at Erigon's exact coinbase symptom. In
`n42-parallel-evm`:

- `execute_single_tx` emitted `AccountWrite::Updated` for every touched account
  and never read `Account::is_selfdestructed()`;
- therefore `AccountWrite::Destroyed` had consumers but no producer;
- MVCC per-slot versions could expose a slot written before an account-wide
  destruction;
- output merge did not define storage cleanup and live-state restoration for a
  later recreation;
- blind-read beneficiary deltas could survive a non-commutative destruction.

The fix translates the revm status, adds versioned storage-wipe markers, clears
old slots on merge, unmarks destruction on later recreation, and falls back to
sequential execution if the deferred beneficiary changes dynamically. Tests
cover pre-Cancun deletion, Cancun EIP-6780 preservation, Cancun constructor
selfdestruct, old-slot wipe, and destroy-then-recreate.

The first 30-round attempt was deliberately run on the P0 base and reproduced
the previously identified hot-recipient ordering race in round 7. The fix branch
was rebased onto the already-pushed validation-order prerequisite and then ran
30/30 green. This dependency is part of the result, not a flaky-test retry.

## Remaining Stage-2 actions

These are gates, not findings that justify immediate code changes:

1. **Public WS:** document and deploy per-IP connection/request/bandwidth limits
   at ingress, or explicitly install a shared RPC middleware. The built-in
   connection/payload/subscription and heavy-call caps remain enabled.
2. **Parallel EVM production wiring:** before enabling it as a reth block
   executor, add EIP-7702 delegation races, receipt/log-index equality, WS
   subscription equality, withdrawals/system calls, and full state-root
   differential tests. Current code remains experimental/opt-in.
3. **Quarterly dependency audit:** repeat the frozen-ref scan and rerun existing
   dependency-upgrade gates whenever the reth pin advances; a reth-inherited fix
   is not considered shipped until n42 compiles and its relevant tests pass.

## Reusable quarterly template

Copy this section into `docs/quarterly-cross-client-borrow-audit-YYYYqN.md`.

### 1. Header

```text
Quarter:
Audit window (exact timestamps):
Auditor:
geth ref/head:
reth ref/head:
Erigon ref/head:
n42 base/head:
Rust/toolchain pin:
Previous audit:
Next due:
```

### 2. Required scan

- fetch without changing checked-out working branches;
- record `rev-list --count` for the window;
- scan fix/security/perf commits in txpool, EVM/execution, Engine API, RPC/WS,
  P2P, state/cache, trie/commitment, database/static files, prune/import, and
  crash recovery;
- read actual diffs for selected candidates;
- search n42 and its pinned reth dependency for the corresponding behavior;
- include reverted/superseded commits and final measured numbers, not stale
  estimates.

### 3. Candidate table

| Field | Required content |
|---|---|
| Upstream | client, commit, date, defect invariant |
| Exposure | exact n42 entry point and activation/configuration |
| Evidence | source paths, tests, flags, and dependency pin |
| Classification | applicable / inherited / covered / different / absent / forward-fork |
| Severity | consensus, data loss, security/DoS, liveness, interoperability, performance |
| Action | none with reason, or owner/branch/test/PR |

### 4. Fix acceptance

For every applicable gap:

- one topic per branch and conventional commit;
- smallest fail-before/pass-after regression using the real boundary;
- adversarial and fork/restart/reorg cases where relevant;
- package tests plus clippy with warnings denied;
- repeated/race/chaos test proportional to concurrency risk;
- explicit prerequisite commits;
- no AI authorship footer; author remains Nyxen.

### 5. Closeout

Record:

- accepted fixes and PR/merge commit;
- ruled-out candidates with evidence;
- operator-only controls and owner;
- untested assumptions;
- dependency/toolchain heads;
- next-quarter carryover and due date.

An audit is incomplete if it only lists upstream commits. Completion requires an
applicability decision for every selected candidate and a tested branch for
every applicable correctness/security gap.
