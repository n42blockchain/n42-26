# P2-3: 0x07 BLS aggregate batch transaction assessment

Date: 2026-07-18  
Scope: `docs/codex-task-sync-from-gov5-2026H1.md` P2-3

## Decision

**NO-GO for production implementation.** A 96-byte aggregate signature is a
useful wire-size primitive, but n42-26's current fast lane already amortizes one
secp256k1 signature over all transfers from a sender. At the measured 10,000
transfers/sender shape, replacing those signatures saves at most 0.04% of the
block, while introducing a new account key registry, custom transaction type,
wallet incompatibility, and txpool atomicity surface. The local CPU proxy also
shows no verification win over the existing per-sender recovery path.

Revisit only for a native BLS-account cohort or a rollup/bundler protocol whose
users already possess registered BLS keys. Do not impose BLS keys on ordinary
Ethereum/secp256k1 accounts.

## What the gov5 PoC is

gov5 commit `69d755332` defines transaction type `0x07`:

- N distinct senders, nonces, recipients, values, gas limits, and calldata;
- each sender signs its own distinct `SubHash` with BLS12-381;
- N signatures aggregate into one 96-byte signature;
- verification resolves every sender's public key and calls distinct-message
  `AggregateVerify`;
- after successful verification, `Expand()` produces N dynamic-fee sub-txs.

The PoC explicitly leaves the on-chain BLS registry and txpool/RLP wiring as
follow-ups. Its 10,000-sender codec test reports 50.97 raw bytes/tx while still
carrying 20-byte sender and recipient addresses. That number is not directly
comparable to n42-26's 12-byte indexed-transfer benchmark.

## Bandwidth calculation against n42-26

The existing fast-lane encoding is:

- transfer record: `recipient_index u32 + amount u64` = 12 bytes;
- sender-batch header: `start_nonce u64 + count u32 + signature[65]` = 77 bytes;
- total for `N` senders and `T` transfers: `12T + 77N`.

BLS cannot recover an Ethereum sender. Even with an on-chain key registry, each
batch must identify the account. Using a 20-byte address gives:

`12T + (start_nonce 8 + count 4 + address 20)N + aggregate_signature 96`

or `12T + 32N + 96`. If the public key is carried instead, the header is 60N
(8 + 4 + 48) plus 96 bytes.

### Measured 512 x 10,000 shape

`N=512`, `T=5,120,000`:

| Encoding | Bytes | Saving vs current |
|---|---:|---:|
| Current secp sender batches | 61,479,424 | baseline |
| BLS + 20B registry address | 61,456,480 | 22,944 bytes (0.0373%) |
| BLS + embedded 48B pubkey | 61,470,816 | 8,608 bytes (0.0140%) |

The 61.44MB of transfer records dominates. The signature scheme cannot change
the 12-byte record payload, so the absolute saving is only tens of kilobytes.

### One transfer per sender

At the opposite extreme (`T=N`), current size approaches 89 bytes/sender and
BLS+registry approaches 44 bytes/sender plus 96 bytes. At 512 senders this saves
22,944 of 45,568 bytes (50.35%). This is the workload where aggregation is a
real bandwidth feature—but it is also the workload requiring every retail
sender/wallet to support BLS or delegate signing to a new account system.

## CPU calculation

The 0x07 messages are distinct, so it cannot use the same-message
`FastAggregateVerify` used for HotStuff/mobile receipts. Verification remains
O(N): resolve N public keys, hash N sub-messages to the curve, and run a
multi-pairing. “One signature” means one 96-byte object, not constant-time
authentication of N accounts.

Existing n42-26 data:

- devlog-81 estimates 512 secp recoveries at 28.5ms for 512 x 10K;
- the full fast-lane verify/hash/apply measurement is 106.8ms;
- devlog-83's 512 x 10K seven-node run observes roughly 125ms average follower
  signature work and concludes ECDSA is no longer the dominant limiter.

Local release baseline on 2026-07-18:

```text
BLS_BENCH_N=512 cargo run --release --example bls_receipt_bench -p n42-primitives
batch_verify (random coefficients): 30.5 ms
same-message aggregate total:       23.7 ms
```

The 30.5ms random-coefficient multi-pairing is the closest existing local proxy,
but it uses the same message repeatedly; distinct 0x07 sub-hashes can only add
hash-to-curve work. It is already in the same range as the 28.5ms secp estimate,
before registry/state lookups and txpool expansion. The 23.7ms same-message
number is **not applicable** to 0x07 and is shown only to prevent accidental use
of the wrong BLS verification mode.

Therefore the honest CPU conclusion is “no demonstrated gain,” not a projected
speedup. A production proposal would need an end-to-end benchmark of distinct
messages, registry reads, admission, expansion, execution, and invalid-batch
fallback.

## Compatibility and security cost

1. **Key ownership:** Ethereum wallets and hardware signers use secp256k1. The
   validator/mobile BLS keys already in n42 are different identities and must
   not authorize user funds.
2. **Registry:** address-to-BLS-key registration needs proof of possession,
   rotation, revocation, replay domains, duplicate-key rules, storage/gas
   pricing, and historical lookup semantics. Without proof of possession,
   aggregate schemes are exposed to rogue-key constructions.
3. **Typed transaction plumbing:** Alloy/reth decoding, transaction/receipt
   roots, Engine API, P2P propagation, RPC, signing, fee accounting, explorer
   indexing, and hardware wallets must agree on 0x07.
4. **Accept-and-expand atomicity:** the pool must verify the aggregate before
   expansion and define all-or-nothing rules for nonce gaps, replacement,
   insufficient balance, fee changes, eviction, reorg reinsertion, and partial
   invalidity. The original batch and expanded set cannot have ambiguous hashes
   or commitment order.
5. **DoS pricing:** one small outer object can cause N registry reads, N message
   hashes, an O(N) pairing check, N pool insertions, and expensive individual
   diagnosis after failure. Count, byte, gas, and CPU caps are mandatory.
6. **Aggregator trust/availability:** every sender must learn and sign the exact
   sub-transaction domain. A common batch-root signature could enable the
   same-message fast path but creates an interactive all-signer coordination
   protocol and different atomic semantics; it is not the gov5 design.

## Revisit gate

Only reopen 0x07 after all are true:

- a real workload has many one-transfer BLS-native senders, rather than the
  existing 10K-transfers-per-sender shape;
- account/key registry and proof-of-possession rules are specified;
- distinct-message aggregate verification beats or materially reduces the
  end-to-end current path on target hardware;
- invalid/adversarial batches have bounded admission and fallback cost;
- wallet, RPC, P2P, transaction-root, receipt, reorg, and fee semantics pass
  cross-node compatibility tests.

Until then, retain the 12-byte transfer format and one ECDSA signature per sender
batch.
