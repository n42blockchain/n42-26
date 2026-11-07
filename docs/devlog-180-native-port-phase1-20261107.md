# Native-path port, phase 1: N42 primitives and 0x50 over RPC (2026-11-07)

Phase 1 of the port from n42-rs tag `fleet7-x22-5.2M-20261024` (plan and difference list:
[devlog-179](devlog-179-native-port-phase0-20261106.md)). Route: the extreme-TPS route
(0x50 Ed25519 transactions). The ETH-ECDSA H2/QMDB mainline keeps its behaviour when the
genesis does not set `altSigTx`; the one exception is the storage format (section 3).

## 1. Design decisions

- **One node type for both routes.** The node now runs on `N42Primitives` everywhere
  (`N42TxEnvelope = Eth(TransactionSigned) | AltSig(AltSigTx)`), as n42-rs does. The alternative was
  to keep the Ethereum node and add a second node type for 0x50 chains. That would double the
  orchestrator, payload, engine and QMDB generic surface for no runtime gain: every Ethereum
  transaction is the `Eth` variant and is handled by the same code as before.
- **0x50 is gated by genesis.** `config.altSigTx` (read by `n42_chainspec::native::alt_sig_tx_enabled`)
  turns on pool admission of type 0x50 (`with_custom_tx_type(0x50)`) and the process-wide
  `n42_tx_types::set_alt_sig_enabled`. Without the flag the pool stays Ethereum-only.
- **No new reth fork.** reth's ready-made pieces that are bound to `EthPrimitives` are replaced
  by N42 copies inside n42-26 rather than by patching reth:
  - `EthereumAddOns` → `N42AddOns` (`node.rs`): the same `RpcAddOns` wrapper without the
    Ethereum-only extra modules this node never enabled.
  - `default_ethereum_payload` → `native_payload::default_n42_payload`: a copy of reth v2.8.1's
    function with only the types swapped. Keep it in step with upstream on reth rebases.
  - `EthereumEngineValidator::convert_payload_to_block` → `EthereumExecutionPayloadValidator::
    ensure_well_formed_payload::<N42TxEnvelope>`. The inner Ethereum validator is kept for the
    attribute and version checks.
- **Ported unchanged from the tag** (port headers in each file):
  - `engine_types.rs`: `N42BuiltPayload`, `N42EngineTypes`, Engine API v1–v6 conversions.
    These are needed because orphan rules block the conversions for `EthBuiltPayload<N42Primitives>`.
  - `native_rpc.rs`: `N42RpcTypes`, the 0x50 receipt converter, `N42EthApiBuilder`.
  - `native_pool.rs`: `N42PooledTransaction`.
  - In `n42-tx-types`: `rpc.rs` and `sender_cache.rs`. The sender cache replaces n42-26's
    `key_cache.rs`; its SHA256s are in `crates/n42-tx-types/PORT.md`.

## 2. Implementation details

- **Execution (`n42-execution`):**
  - `N42ReceiptBuilder` builds `Receipt<N42TxType>`.
  - `N42BlockExecutorFactory = EthBlockExecutorFactory<N42ReceiptBuilder, Arc<ChainSpec>, N42EvmFactory>`.
  - `N42EvmConfig` assembles blocks with `EthBlockAssembler` and builds the execution context by hand.
  - Payload sender recovery uses `AltSigSenderCache` for 0x50 and the existing secp256k1 cache for Ethereum.
  - `try_execute_import_batch` and `commit_transfer_batch` take `Recovered<N42TxEnvelope>`.
  - 0x50 executes as a type-2 `TxEnv`.
- **Consensus:** `adapter.rs` is typed to `N42Block` / `N42Primitives`.
  `gov5_native_receipts_root` is generic over the receipt's transaction type.
- **Node:**
  - Pool, ingest and tx_bridge use `N42PooledTransaction` / `N42TxEnvelope`.
  - Payload and parallel_payload produce `N42BuiltPayload`.
  - Updated to N42 types: el/reth_engine, eof_guard, qmdb_state_root / qmdb_state strategies, exec_cache, mobile_packet, packet_builder.
  - The orchestrator's `CompactBlockExecution` carries `n42_tx_types::Receipt`.
- **Mobile:** the verifier decodes and executes `N42Primitives` blocks, so a phone can verify
  blocks that hold 0x50.
- **Tooling:**
  - `crates/n42-tx-types/examples/alt_sig_raw.rs` signs one 0x50 transfer offline. `--address`
    prints the sender (keccak256(0x01‖pubkey)[12..]); `--secp256k1-key` signs an EIP-1559
    transfer instead.
  - `scripts/testnet.sh` has an opt-in: `N42_ALT_SIG_TX=1` sets `altSigTx` in the genesis, and
    `N42_ALT_SIG_FUND=<addresses>` funds the Ed25519 accounts.

## 3. Problems and resolutions

- **Storage format changes (datadir break).** `N42TxEnvelope`'s `Compact` encoding
  (`n42-tx-types/src/compact.rs`, unchanged from the tag) stores an Ethereum transaction behind
  one tag byte (0). Existing n42-26 datadirs (transaction tables, static files) cannot be read
  by this binary; they must be re-synced from genesis or replayed. Receipts are unaffected
  (`N42TxType::Eth` delegates to `TxType`). RPC and the network wire format are unchanged.
- **Trait methods are no longer inherent.** `tx_hash()`, `tx_type()` and `genesis()` were inherent
  on reth's Ethereum types but are trait methods on the envelope. Fixed by importing
  `TxHashRef` / `TransactionEnvelope` / `EthChainSpec`.
- **Test mocks default to Ethereum.** `MockEthProvider`, `NoopTransactionPool` and
  `create_test_provider_factory*` default to Ethereum types. Tests now use
  `MockEthProvider<N42Primitives>`, `NoopTransactionPool::<N42PooledTransaction>::new()` and
  `create_test_provider_factory_with_node_types::<N42Node>`.
- **`Receipt root task dropped sender` errors.** Followers log this error on blocks whose execution comes from the leader's cache.
  - Cause: the existing N42 reth patch (`N42_PAYLOAD_CACHE_HIT` in `payload_validator.rs`) closes
    the receipt-root channel on purpose, and reth then derives the root from the cached receipts.
  - It is not caused by this port and does not affect validation, but the message is misleading at error level.
  - It belongs in the reth patch series (downgrade or skip the log on that branch).

## 4. Status

| Item | Result |
|------|--------|
| `cargo check --all-targets` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo test --workspace` | 1851 passed, 0 failed |
| 7 nodes with `altSigTx`, 0x50 over `eth_sendRawTransaction` | Three 0x50 transfers were included in block 6. Receipts read `type 0x50`, `status 1`, `gasUsed 21000` and the right `from` on v0, v3 and v6. `eth_getTransactionByHash` returns the 0x50 fields (algType, pubkey, signature). |
| Mixed block | One 0x02 and one 0x50 were included in the same block (0x1d4); receipts agree on v0 and v6. |
| Bad Ed25519 signature | rejected: `invalid transaction signature` |
| Cross-node agreement | Block 6 and block 0x91 have the same hash, stateRoot and receiptsRoot on all 7 nodes. |
| Ethereum-only regression (E2E 1,3,4, no `altSigTx`) | 3/3 PASS: single node, ERC-20, and 7- and 21-node consensus with height, hash and rotation checks |
| 0x50 on a chain without `altSigTx` (1 node) | rejected: `transaction type not supported` (-32003) |

Logs and data are in `.artifacts/native-port-p0/` (ignored): `test-p1.log`, `tn7.log`, `tn7/`,
`e2e-134.log`, `tn1.log`.

## 5. Next

Phase 2 (after acceptance):
- Port tx-ingest and gateway frame attestation (`N42_FRAME_GATEWAYS`, `N42_FRAME_ATTEST_MIN`).
- Port an Ed25519 load source (n42-rs `tx_flood --alg ed25519 --ingest --gateway-key`).
- Make the trusted ingest admit 0x50 frames.
- Take the first 0x50 profile on the box.

Open items carried over:
- The receipt-root log noise (reth patch).
- Datadir migration, if any long-lived n42-26 datadir must survive the format change.
