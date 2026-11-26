//! gov5's transaction gossip: `/n42/<fork digest>/transaction_v2/ssz_snappy`.
//!
//! Wire form (gov5 `internal/sync/tx_batch_wire.go`, mirrored by n42-rs
//! `h2-net/src/tx_gossip.rs`): raw snappy over either one EIP-2718
//! transaction as it stands, or the marker byte `0x7e` followed by the RLP
//! list of EIP-2718 transactions. The marker cannot open a transaction: typed
//! envelopes start at a type byte no chain uses for `0x7e`, legacy RLP lists
//! at `0xc0` or above. A batch carries at most 256 transactions and 256 KiB.
//!
//! What an n42-26 member does with the topic: every transaction it hears goes
//! into its local reth pool, so an n42-26 leader seals the fleet's
//! transactions; every transaction its pool admits from a local submission
//! goes out on the topic, so a gov5 leader seals the ones submitted here.
//! Neither direction is consensus: a transaction that fails to arrive waits
//! for the next leader that has it.
//!
//! Inside n42-26 transactions travel in their RLP *network* form (what
//! `alloy_rlp::encode(N42TxEnvelope)` produces and the pool bridge decodes);
//! the topic carries the EIP-2718 form. The conversions live here.

use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::Bytes;
use alloy_rlp::{Decodable, Encodable};
use n42_tx_types::N42TxEnvelope;

/// The byte a batch starts with.
pub const TX_BATCH_MARKER: u8 = 0x7e;
/// Transactions per batch, gov5's `txBatchMaxTxs`.
pub const TX_BATCH_MAX_TXS: usize = 256;
/// Bytes per encoded (uncompressed) batch, gov5's `txBatchMaxBytes`.
pub const TX_BATCH_MAX_BYTES: usize = 256 * 1024;

/// Why a transaction message could not be framed or read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TxGossipError {
    /// The message is empty.
    #[error("empty transaction message")]
    Empty,
    /// The batch does not decode as an RLP list of byte strings.
    #[error("transaction batch is not an RLP list of transactions")]
    InvalidBatch,
    /// More transactions than gov5 accepts in one message.
    #[error("transaction batch of {0} exceeds the {TX_BATCH_MAX_TXS} cap")]
    TooMany(usize),
    /// A larger message than gov5 sends.
    #[error("transaction batch of {0} bytes exceeds the {TX_BATCH_MAX_BYTES} cap")]
    TooLarge(usize),
    /// The snappy frame is malformed.
    #[error("invalid snappy frame: {0}")]
    Snappy(String),
}

/// Frames `transactions` (each an EIP-2718 encoding) as one batch message,
/// before compression. Byte-identical to n42-rs `encode_tx_batch`.
pub fn encode_tx_batch(transactions: &[Bytes]) -> Result<Vec<u8>, TxGossipError> {
    if transactions.len() > TX_BATCH_MAX_TXS {
        return Err(TxGossipError::TooMany(transactions.len()));
    }
    let mut out = vec![TX_BATCH_MARKER];
    alloy_rlp::encode_list::<Bytes, Bytes>(transactions, &mut out);
    if out.len() > TX_BATCH_MAX_BYTES {
        return Err(TxGossipError::TooLarge(out.len()));
    }
    Ok(out)
}

/// The transactions of a decompressed message: a batch, or one bare
/// transaction as gov5's older nodes sent them. The transactions themselves
/// are not decoded here; the pool judges them.
pub fn decode_tx_batch(payload: &[u8]) -> Result<Vec<Bytes>, TxGossipError> {
    match payload.first() {
        None => Err(TxGossipError::Empty),
        Some(&TX_BATCH_MARKER) => {
            if payload.len() > TX_BATCH_MAX_BYTES {
                return Err(TxGossipError::TooLarge(payload.len()));
            }
            let mut cursor = &payload[1..];
            let header =
                alloy_rlp::Header::decode(&mut cursor).map_err(|_| TxGossipError::InvalidBatch)?;
            if !header.list || header.payload_length != cursor.len() {
                return Err(TxGossipError::InvalidBatch);
            }
            let mut transactions = Vec::new();
            while !cursor.is_empty() {
                if transactions.len() == TX_BATCH_MAX_TXS {
                    return Err(TxGossipError::TooMany(TX_BATCH_MAX_TXS + 1));
                }
                transactions
                    .push(Bytes::decode(&mut cursor).map_err(|_| TxGossipError::InvalidBatch)?);
            }
            if transactions.is_empty() {
                return Err(TxGossipError::InvalidBatch);
            }
            Ok(transactions)
        }
        Some(_) => Ok(vec![Bytes::copy_from_slice(payload)]),
    }
}

/// Splits EIP-2718 `transactions` into topic messages, each a compressed
/// batch within both caps. A transaction that could not fit even alone is
/// left out (the pool's own size limit is far below the cap).
pub fn encode_gossip_messages(transactions: &[Bytes]) -> Vec<Vec<u8>> {
    // Marker + the longest list header a 256 KiB payload needs.
    const FRAME_OVERHEAD: usize = 1 + 9;
    let mut messages = Vec::new();
    let mut start = 0;
    while start < transactions.len() {
        let mut end = start;
        let mut size = FRAME_OVERHEAD;
        while end < transactions.len() && end - start < TX_BATCH_MAX_TXS {
            let item = transactions[end].length();
            if size + item > TX_BATCH_MAX_BYTES {
                break;
            }
            size += item;
            end += 1;
        }
        if end == start {
            tracing::debug!(
                target: "n42::interop::tx",
                bytes = transactions[start].len(),
                "transaction too large for a gov5 batch; not gossiped"
            );
            start += 1;
            continue;
        }
        match encode_tx_batch(&transactions[start..end]).and_then(|payload| {
            crate::snappy_pool::raw_compress(&payload)
                .map_err(|error| TxGossipError::Snappy(error.to_string()))
        }) {
            Ok(message) => messages.push(message),
            Err(error) => {
                tracing::debug!(target: "n42::interop::tx", %error, "could not encode a transaction batch")
            }
        }
        start = end;
    }
    messages
}

/// Reads one topic message: raw snappy, bounded before decompression, then
/// the batch framing.
pub fn decode_gossip_message(data: &[u8]) -> Result<Vec<Bytes>, TxGossipError> {
    let len = snap::raw::decompress_len(data)
        .map_err(|error| TxGossipError::Snappy(error.to_string()))?;
    if len > TX_BATCH_MAX_BYTES {
        return Err(TxGossipError::TooLarge(len));
    }
    let payload = crate::snappy_pool::raw_decompress(data)
        .map_err(|error| TxGossipError::Snappy(error.to_string()))?;
    decode_tx_batch(&payload)
}

/// The EIP-2718 form of a transaction held in n42-26's RLP network form.
pub fn network_rlp_to_eip2718(rlp: &[u8]) -> Option<Bytes> {
    let tx = N42TxEnvelope::decode(&mut &rlp[..]).ok()?;
    Some(Bytes::from(tx.encoded_2718()))
}

/// The RLP network form of an EIP-2718 transaction, or `None` if it does not
/// decode as one this chain knows.
pub fn eip2718_to_network_rlp(raw: &[u8]) -> Option<Vec<u8>> {
    let tx = N42TxEnvelope::decode_2718_exact(raw).ok()?;
    Some(alloy_rlp::encode(&tx))
}

/// One topic message in the pool bridge's RLP network form, plus how many of
/// its entries did not decode as transactions this chain knows.
pub fn decode_gossip_to_network_rlp(data: &[u8]) -> Result<(Vec<Vec<u8>>, usize), TxGossipError> {
    let raws = decode_gossip_message(data)?;
    let total = raws.len();
    let txs: Vec<Vec<u8>> = raws
        .iter()
        .filter_map(|raw| eip2718_to_network_rlp(raw))
        .collect();
    let dropped = total - txs.len();
    Ok((txs, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
    use alloy_primitives::{Address, Signature, TxKind, U256, hex};

    fn eip1559(nonce: u64) -> TxEnvelope {
        let tx = TxEip1559 {
            chain_id: 4242,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            value: U256::from(1u64),
            ..Default::default()
        };
        let sig = Signature::new(U256::from(1u64), U256::from(2u64), false);
        TxEnvelope::Eip1559(tx.into_signed(sig))
    }

    fn legacy(nonce: u64) -> TxEnvelope {
        let tx = TxLegacy {
            chain_id: Some(4242),
            nonce,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::repeat_byte(0x22)),
            value: U256::from(7u64),
            ..Default::default()
        };
        let sig = Signature::new(U256::from(3u64), U256::from(4u64), true);
        TxEnvelope::Legacy(tx.into_signed(sig))
    }

    #[test]
    fn a_batch_round_trips_and_a_bare_transaction_is_one() {
        let txs = vec![
            Bytes::from_static(&[0x02, 0xf8, 0x01]),
            Bytes::from_static(&[0xf8, 0x02, 0x03]),
        ];
        let encoded = encode_tx_batch(&txs).unwrap();
        assert_eq!(encoded[0], TX_BATCH_MARKER);
        assert_eq!(decode_tx_batch(&encoded).unwrap(), txs);
        assert_eq!(
            decode_tx_batch(&[0xf8, 0x02, 0x03]).unwrap(),
            vec![Bytes::from_static(&[0xf8, 0x02, 0x03])]
        );
        assert_eq!(decode_tx_batch(&[]), Err(TxGossipError::Empty));
        assert_eq!(
            decode_tx_batch(&[TX_BATCH_MARKER, 0xc0]),
            Err(TxGossipError::InvalidBatch)
        );
    }

    /// Fixed vector shared with n42-rs `encode_tx_batch`: the marker, then a
    /// plain RLP list of byte strings.
    #[test]
    fn the_batch_bytes_match_n42_rs() {
        let txs = vec![
            Bytes::from_static(&[0x02, 0xf8, 0x01]),
            Bytes::from_static(&[0xf8, 0x02, 0x03]),
        ];
        assert_eq!(
            encode_tx_batch(&txs).unwrap(),
            hex!("7e c8 83 02f801 83 f80203").to_vec()
        );
        // A single-byte item below 0x80 is its own RLP encoding.
        assert_eq!(
            encode_tx_batch(&[Bytes::from_static(&[0x05])]).unwrap(),
            hex!("7e c1 05").to_vec()
        );
    }

    #[test]
    fn malformed_batches_are_refused() {
        // Trailing bytes after the list.
        assert_eq!(
            decode_tx_batch(&hex!("7e c2 01 02 03")),
            Err(TxGossipError::InvalidBatch)
        );
        // A string where the list should be.
        assert_eq!(
            decode_tx_batch(&hex!("7e 82 01 02")),
            Err(TxGossipError::InvalidBatch)
        );
        // A list item that is itself a list.
        assert_eq!(
            decode_tx_batch(&hex!("7e c2 c1 01")),
            Err(TxGossipError::InvalidBatch)
        );
        // Truncated list.
        assert_eq!(
            decode_tx_batch(&hex!("7e c5 83 01")),
            Err(TxGossipError::InvalidBatch)
        );
        assert_eq!(
            decode_tx_batch(&[TX_BATCH_MARKER]),
            Err(TxGossipError::InvalidBatch)
        );
        assert!(matches!(
            decode_gossip_message(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            Err(TxGossipError::Snappy(_))
        ));
    }

    #[test]
    fn an_incoming_batch_obeys_the_encoder_limits() {
        let mut oversized = vec![TX_BATCH_MARKER];
        vec![Bytes::from(vec![0; TX_BATCH_MAX_BYTES])].encode(&mut oversized);
        assert_eq!(
            decode_tx_batch(&oversized),
            Err(TxGossipError::TooLarge(oversized.len()))
        );
        let mut crowded = vec![TX_BATCH_MARKER];
        vec![Bytes::new(); TX_BATCH_MAX_TXS + 1].encode(&mut crowded);
        assert_eq!(
            decode_tx_batch(&crowded),
            Err(TxGossipError::TooMany(TX_BATCH_MAX_TXS + 1))
        );
        let at_limit = vec![Bytes::new(); TX_BATCH_MAX_TXS];
        assert_eq!(
            decode_tx_batch(&encode_tx_batch(&at_limit).unwrap()).unwrap(),
            at_limit
        );
        // A compressed message that would expand past the cap is refused
        // before it is decompressed.
        let big = crate::snappy_pool::raw_compress(&oversized).unwrap();
        assert_eq!(
            decode_gossip_message(&big),
            Err(TxGossipError::TooLarge(oversized.len()))
        );
    }

    #[test]
    fn the_caps_are_gov5s() {
        let many: Vec<Bytes> = (0..TX_BATCH_MAX_TXS + 1)
            .map(|_| Bytes::from_static(&[0x02]))
            .collect();
        assert_eq!(
            encode_tx_batch(&many),
            Err(TxGossipError::TooMany(TX_BATCH_MAX_TXS + 1))
        );
        let big = vec![Bytes::from(vec![0u8; TX_BATCH_MAX_BYTES])];
        assert!(matches!(
            encode_tx_batch(&big),
            Err(TxGossipError::TooLarge(_))
        ));
    }

    #[test]
    fn messages_split_on_both_caps_and_round_trip() {
        // 600 small transactions: three messages by count.
        let small: Vec<Bytes> = (0..600u32)
            .map(|i| Bytes::from(i.to_be_bytes().to_vec()))
            .collect();
        let messages = encode_gossip_messages(&small);
        assert_eq!(messages.len(), 3);
        let back: Vec<Bytes> = messages
            .iter()
            .flat_map(|m| decode_gossip_message(m).unwrap())
            .collect();
        assert_eq!(back, small);

        // 100 KiB transactions: two per message by size; an oversized one is
        // dropped rather than breaking the stream.
        let mut large: Vec<Bytes> = (0..5u8).map(|i| Bytes::from(vec![i; 100 * 1024])).collect();
        large.insert(2, Bytes::from(vec![9u8; TX_BATCH_MAX_BYTES]));
        let messages = encode_gossip_messages(&large);
        assert_eq!(messages.len(), 3);
        let back: Vec<Bytes> = messages
            .iter()
            .flat_map(|m| decode_gossip_message(m).unwrap())
            .collect();
        large.remove(2);
        assert_eq!(back, large);
        assert!(encode_gossip_messages(&[]).is_empty());
    }

    #[test]
    fn network_and_2718_forms_convert_both_ways() {
        for tx in [eip1559(3), legacy(4)] {
            let raw = tx.encoded_2718();
            let n42 = N42TxEnvelope::decode_2718_exact(&raw).unwrap();
            let network = alloy_rlp::encode(&n42);
            assert_eq!(network_rlp_to_eip2718(&network).unwrap().as_ref(), &raw[..]);
            assert_eq!(eip2718_to_network_rlp(&raw).unwrap(), network);
        }
        // A typed transaction's network form is the RLP string around it.
        let raw = eip1559(5).encoded_2718();
        let network = eip2718_to_network_rlp(&raw).unwrap();
        assert_ne!(network, raw);
        assert_eq!(
            Bytes::decode(&mut network.as_slice()).unwrap().as_ref(),
            &raw[..]
        );
        assert!(eip2718_to_network_rlp(&[0x02, 0x01]).is_none());
        assert!(network_rlp_to_eip2718(&[0x80]).is_none());
    }

    /// A gov5-style message of real transactions decodes into the network
    /// form the pool bridge imports.
    #[test]
    fn a_gossiped_batch_reaches_the_pool_form() {
        let raws: Vec<Bytes> = [eip1559(0), legacy(1)]
            .iter()
            .map(|tx| Bytes::from(tx.encoded_2718()))
            .collect();
        let message = crate::snappy_pool::raw_compress(&encode_tx_batch(&raws).unwrap()).unwrap();
        let (decoded, dropped) = decode_gossip_to_network_rlp(&message).unwrap();
        assert_eq!(dropped, 0);
        let pool_form: Vec<N42TxEnvelope> = decoded
            .iter()
            .map(|rlp| N42TxEnvelope::decode(&mut rlp.as_slice()).unwrap())
            .collect();
        assert_eq!(pool_form.len(), 2);
        assert_eq!(pool_form[0].encoded_2718(), raws[0].to_vec());
        assert_eq!(pool_form[1].encoded_2718(), raws[1].to_vec());

        // Garbage beside real transactions is counted, not fatal.
        let mixed = vec![raws[0].clone(), Bytes::from_static(&[0x02, 0x01])];
        let message = crate::snappy_pool::raw_compress(&encode_tx_batch(&mixed).unwrap()).unwrap();
        let (decoded, dropped) = decode_gossip_to_network_rlp(&message).unwrap();
        assert_eq!((decoded.len(), dropped), (1, 1));
    }
}
