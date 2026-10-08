# N42 native primitives port

Reference: `n42-rs` at `52a1df393ea3ca37518edfd428651202f76fb3a7`.

Copied transaction/frame/envelope/storage/primitive rules and specification vectors.
Retained original test suite; added checked layout-length summation to reject
overflow before slicing (valid frame roots unchanged); copied only the bounded decompressed-key cache from
`sender_cache.rs`. `alt_sig.rs` calls the private `key_cache` module.
The native traits/transaction versions follow the validated Reth 2.7 / Alloy 2.5
source profile. Test with the adapted Reth checkout; older Reth sources are not
the supported native integration base. Existing root dependency migration stays
separate from this port.

Execution-environment conversion is ported as `execution.rs`; authenticated admission
exposes `execution_env()` without signer recovery. RPC, frame queue and node wiring follow separately.
The node has not yet switched its primitive types or admitted native frames.

Reference source SHA256:

- `alt_sig.rs`: `024ed1894cb03c13e560f0032da6ba2d3909f1428c4583cfb35a853d3368ab8b`
- `frame.rs`: `38ccc0f814d4d3d0b44ea45a98fc99a9496acad516f446d3eb9e0dd66515561c`
- `envelope.rs`: `73c5ef16492eacd3bf7a0dce002b37da5ecc514ea5e9f7af2e7916038ae7ca45`
- `primitives.rs`: `aebe10aaaf4fe8e0ae780587381155976be4da73ffa328819af99a2637d15644`
- `compact.rs`: `92205c883c7068eb0e54396f6d95672647c8e9e9b8fabc607cfff36e5e3276d2`
- `sender_cache.rs`: `792445bae54bf3e01a64ea353322ea04c731f8f89c7d7e8c46e7d3649d570db6`
- `lib.rs`: `a362795aa67b88725b1c1b387109d952d1e8e77b1783fb5a28fc26e4e459cf1b`

Execution adapter reference: `b03e71c8f307074df388caaae43c26b764b9417d`, `evm.rs` SHA256 `5bf5ff039a7f6b52b3162e5f489448966ca9028b9e368799b29adc9f46d490a7`.
Native wire/hash/receipt identity remains 0x50; internal TxEnv type 2 carries fee semantics.
