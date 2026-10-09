use alloy_consensus::EthereumReceipt;
use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Encodable, Header};

/// Reproduces gov5 native `hash.DeriveSha(block.Receipts)` exactly. This is intentionally not the
/// Ethereum receipt trie: replay-v2 hashes the concatenation of RLP
/// `[status, cumulativeGasUsed, logs]` receipts, and the empty list is `keccak256("")`.
pub fn gov5_native_receipts_root(receipts: &[EthereumReceipt]) -> B256 {
    let mut encoded = Vec::new();
    for receipt in receipts {
        let status = u64::from(receipt.success);
        let payload_length =
            status.length() + receipt.cumulative_gas_used.length() + receipt.logs.length();
        Header {
            list: true,
            payload_length,
        }
        .encode(&mut encoded);
        status.encode(&mut encoded);
        receipt.cumulative_gas_used.encode(&mut encoded);
        receipt.logs.encode(&mut encoded);
    }
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxType;

    #[test]
    fn matches_gov5_native_receipt_fixture() {
        let receipt = EthereumReceipt {
            tx_type: TxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs: Vec::new(),
        };
        assert_eq!(
            gov5_native_receipts_root(&[receipt]),
            "0x9ec602b25fc63e86a5feb8943d52cf66b24ed8e8021f3f74f077271ffae88c75"
                .parse::<B256>()
                .unwrap()
        );
        assert_eq!(
            gov5_native_receipts_root(&[]),
            "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
                .parse::<B256>()
                .unwrap()
        );
    }

    #[test]
    fn matches_offline_audit_log_and_failure_fixture() {
        use alloy_primitives::{Address, Bytes, Log};
        // Also checked by scripts/test-h2-tps-audit.py against its independent
        // RLP encoder (scripts/fixtures/gov5-receipt-audit.json).
        let receipts: Vec<_> = [true, false, true]
            .into_iter()
            .enumerate()
            .map(|(index, success)| EthereumReceipt {
                tx_type: TxType::Eip1559,
                success,
                cumulative_gas_used: (index as u64 + 1) * 21_000,
                logs: if index == 0 {
                    vec![Log::new_unchecked(
                        Address::repeat_byte(0x11),
                        vec![B256::repeat_byte(0x22), B256::repeat_byte(0x33)],
                        Bytes::from((0..80u8).collect::<Vec<_>>()),
                    )]
                } else {
                    Vec::new()
                },
            })
            .collect();
        assert_eq!(
            gov5_native_receipts_root(&receipts),
            "0x91e7d4d76b7fe8687ce49e001e8438a779fbbc7b59b797b47f2dc7cd9ddadd9e"
                .parse::<B256>()
                .unwrap()
        );
    }
}
