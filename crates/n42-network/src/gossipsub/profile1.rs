//! GossipSub on a standard-profile-1 chain: gov5's router values (G4, vector
//! V9), the G2 topic list (AM34 items 4-5) and the message-size cap after
//! `maxBlockBytes` (B15).
//!
//! The wire contract is the topic strings, the message id (G3,
//! [`super::message_id_fn`]) and the size cap; those must equal gov5's
//! exactly. The mesh, gossip and history values are local router behaviour
//! (a client with others still interoperates) but are set to gov5's so a
//! Rust member grafts and prunes like a Go one. Every value below is V9's
//! `gossipSubParams` / `router` (gov5 `pubsubGossipParam`, go-libp2p-pubsub
//! v0.18.0 defaults where gov5 sets nothing).
//!
//! Peer scoring is not part of the contract (AM34: "a local choice"); this
//! node keeps its own.

use std::time::Duration;

use alloy_primitives::B256;
use libp2p::gossipsub::{self, IdentTopic};

use super::message_id_fn;
use super::topics::{gov5_block_topic, gov5_h2_topic, gov5_tx_topic, h2_v4_topic};

/// gov5 `gossipSubD`: target mesh degree.
pub const D: usize = 8;
/// gov5 `gossipSubDlo`: low watermark.
pub const D_LO: usize = 6;
/// go-libp2p-pubsub `GossipSubDhi` (gov5 keeps the default).
pub const D_HI: usize = 12;
/// go-libp2p-pubsub `GossipSubDlazy`.
pub const D_LAZY: usize = 6;
/// go-libp2p-pubsub `GossipSubDout`.
pub const D_OUT: usize = 2;
/// go-libp2p-pubsub `GossipSubDscore`.
pub const D_SCORE: usize = 4;
/// gov5 `gossipSubHeartbeatInterval`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(700);
/// go-libp2p-pubsub `GossipSubHeartbeatInitialDelay` (rust-libp2p: 5 s).
pub const HEARTBEAT_INITIAL_DELAY: Duration = Duration::from_millis(100);
/// gov5 `gossipSubMcacheLen` (rust-libp2p: 5).
pub const HISTORY_LENGTH: usize = 6;
/// gov5 `gossipSubMcacheGossip`.
pub const HISTORY_GOSSIP: usize = 3;
/// gov5 `seenMessagesTTL` (rust-libp2p: 60 s).
pub const SEEN_MESSAGES_TTL: Duration = Duration::from_secs(30);
/// go-libp2p-pubsub `GossipSubIDontWantMessageThreshold` (rust-libp2p: 1000).
pub const IDONTWANT_MESSAGE_THRESHOLD: usize = 1024;
/// go-libp2p-pubsub `GossipSubMaxIHaveLength`.
pub const MAX_IHAVE_LENGTH: usize = 5000;
/// go-libp2p-pubsub `GossipSubMaxIHaveMessages`.
pub const MAX_IHAVE_MESSAGES: usize = 10;
/// go-libp2p-pubsub `GossipSubOpportunisticGraftTicks`.
pub const OPPORTUNISTIC_GRAFT_TICKS: u64 = 60;
/// go-libp2p-pubsub `GossipSubOpportunisticGraftPeers`.
pub const OPPORTUNISTIC_GRAFT_PEERS: usize = 2;
/// go-libp2p-pubsub `GossipSubGossipRetransmission`.
pub const GOSSIP_RETRANSMISSION: u32 = 3;
/// go-libp2p-pubsub `GossipSubGossipFactor`.
pub const GOSSIP_FACTOR: f64 = 0.25;
/// go-libp2p-pubsub `GossipSubDirectConnectTicks`.
pub const DIRECT_CONNECT_TICKS: u64 = 300;
/// go-libp2p-pubsub `GossipSubFanoutTTL`.
pub const FANOUT_TTL: Duration = Duration::from_secs(60);
/// go-libp2p-pubsub `GossipSubPruneBackoff`.
pub const PRUNE_BACKOFF: Duration = Duration::from_secs(60);
/// go-libp2p-pubsub `GossipSubUnsubscribeBackoff`.
pub const UNSUBSCRIBE_BACKOFF: Duration = Duration::from_secs(10);
/// go-libp2p-pubsub `GossipSubGraftFloodThreshold`.
pub const GRAFT_FLOOD_THRESHOLD: Duration = Duration::from_secs(10);
/// go-libp2p-pubsub `GossipSubIWantFollowupTime`.
pub const IWANT_FOLLOWUP_TIME: Duration = Duration::from_secs(3);

/// The router's message-size cap for a chain whose blocks are at most
/// `max_block_bytes` decoded (B15): gov5's `GossipMaxSize`, Go's
/// `snappy.MaxEncodedLen(n) = 32 + n + n/6`. 78 293 706 B for 64 MiB.
pub const fn max_gossip_wire_size(max_block_bytes: usize) -> usize {
    32 + max_block_bytes + max_block_bytes / 6
}

/// GossipSub as a profile-1 node runs it.
///
/// [`gossipsub::ValidationMode::Anonymous`] is gov5's `StrictNoSign` plus
/// `WithNoAuthor`: messages carry no author, sequence number or signature,
/// and one that does is refused. It pairs with
/// [`gossipsub::MessageAuthenticity::Anonymous`] on the behaviour.
pub fn gossipsub_config(
    genesis_hash: B256,
    max_block_bytes: u64,
) -> Result<gossipsub::Config, gossipsub::ConfigBuilderError> {
    let max_block_bytes = usize::try_from(max_block_bytes).unwrap_or(usize::MAX / 2);
    gossipsub::ConfigBuilder::default()
        .mesh_n(D)
        .mesh_n_low(D_LO)
        .mesh_n_high(D_HI)
        .gossip_lazy(D_LAZY)
        .mesh_outbound_min(D_OUT)
        .retain_scores(D_SCORE)
        .heartbeat_interval(HEARTBEAT_INTERVAL)
        .heartbeat_initial_delay(HEARTBEAT_INITIAL_DELAY)
        .history_length(HISTORY_LENGTH)
        .history_gossip(HISTORY_GOSSIP)
        .duplicate_cache_time(SEEN_MESSAGES_TTL)
        .idontwant_message_size_threshold(IDONTWANT_MESSAGE_THRESHOLD)
        .max_control_messages_sent(MAX_IHAVE_LENGTH)
        .max_ihave_messages_heartbeat(MAX_IHAVE_MESSAGES)
        .opportunistic_graft_ticks(OPPORTUNISTIC_GRAFT_TICKS)
        .opportunistic_graft_peers(OPPORTUNISTIC_GRAFT_PEERS)
        .gossip_retransimission(GOSSIP_RETRANSMISSION)
        .gossip_factor(GOSSIP_FACTOR)
        .check_explicit_peers_ticks(DIRECT_CONNECT_TICKS)
        .fanout_ttl(FANOUT_TTL)
        .prune_backoff(PRUNE_BACKOFF)
        .unsubscribe_backoff(UNSUBSCRIBE_BACKOFF)
        .graft_flood_threshold(GRAFT_FLOOD_THRESHOLD)
        .iwant_followup_time(IWANT_FOLLOWUP_TIME)
        .max_transmit_size(max_gossip_wire_size(max_block_bytes))
        .validation_mode(gossipsub::ValidationMode::Anonymous)
        // AM36: forward only what the node reported Accept for
        // (`NetworkService::handle_gossipsub_message` reports every message).
        .validate_messages()
        .message_id_fn(message_id_fn(genesis_hash))
        .build()
}

/// The G2 topics a profile-1 node subscribes to (AM34 item 4): gov5's
/// `hotstuff_consensus`, `block` and H2-v4, plus `transaction_v2` when the
/// node takes transactions (a participant). Nothing else: not the native
/// `/n42/*/1` topics, not `blob_sidecar` (no blobs, B9), not `zk_proof`, and
/// not the mobileverify cohort topics (AM34 item 5).
///
/// G2's `mobileverify_{packet,registration}` are not joined: this node has no
/// mobileverify service yet (I1), and joining a topic it cannot validate
/// would only make it a relay for traffic it does not read.
pub fn g2_topics(genesis_hash: B256, takes_transactions: bool) -> Vec<IdentTopic> {
    let mut topics = vec![
        gov5_block_topic(genesis_hash),
        gov5_h2_topic(genesis_hash),
        h2_v4_topic(),
    ];
    if takes_transactions {
        topics.push(gov5_tx_topic(genesis_hash));
    }
    topics
}

/// What the router cap means for one decoded gov5 block on gossip: at most
/// `max_block_bytes` (B15).
pub const fn gov5_block_fits(max_block_bytes: usize, decoded_bytes: usize) -> bool {
    decoded_bytes <= max_block_bytes
}

/// What a profile-1 member does with a gossip message before forwarding it
/// (AM36): [`gossipsub::MessageAcceptance::Accept`] forwards it,
/// `Reject` drops and penalises the sender, `Ignore` drops without penalty.
pub type Verdict = gossipsub::MessageAcceptance;

/// AM36 rule 1 / item 5 for an H2 message (gov5 `hotstuff_consensus` and
/// H2-v4): every view it carries -- its own and those of the certificates
/// inside it -- is a view a certificate may exist for (AM34 item 3: not
/// 2^64-1); a proposal's justify QC view is below the proposal's view; a
/// Decide's commit QC view equals its view. Signer bitmap format is checked
/// by the decoder.
///
/// Stateless: it needs no committee, no current view and no signature check.
/// Signer bitmaps and BLS signatures are checked by the consensus handler
/// after Accept (AM36 rule 2 leaves them optional before forwarding).
pub fn h2_fields_in_range(message: &crate::h2_wire::H2Message) -> Result<(), &'static str> {
    use crate::h2_wire::{H2Message, H2QuorumCertificate, H2TimeoutCertificate};

    fn qc(qc: &H2QuorumCertificate) -> bool {
        n42_consensus::certifiable_view(qc.view)
    }
    fn tc(tc: &H2TimeoutCertificate) -> bool {
        n42_consensus::certifiable_view(tc.view) && qc(&tc.high_qc)
    }
    // AM36 item 5: a proposal's justify QC is for an earlier view, a
    // Decide's commit QC for its own view.
    match message {
        H2Message::Proposal(p) if p.justify_qc.view >= p.view => {
            return Err("a proposal's justify QC view is not below the proposal view (AM36)");
        }
        H2Message::Decide(d) if d.commit_qc.view != d.view => {
            return Err("a Decide's commit QC view differs from its view (AM36)");
        }
        _ => {}
    }
    let in_range = match message {
        H2Message::Proposal(p) => {
            n42_consensus::certifiable_view(p.view)
                && qc(&p.justify_qc)
                && p.prepare_qc.as_ref().is_none_or(qc)
        }
        H2Message::Vote(v) | H2Message::CommitVote(v) => {
            n42_consensus::certifiable_view(v.view) && v.high_tc.as_ref().is_none_or(tc)
        }
        H2Message::PrepareQc(p) => n42_consensus::certifiable_view(p.view) && qc(&p.qc),
        H2Message::Timeout(t) => {
            n42_consensus::certifiable_view(t.view)
                && qc(&t.high_qc)
                && t.high_tc.as_ref().is_none_or(tc)
        }
        H2Message::NewView(n) => {
            n42_consensus::certifiable_view(n.view) && tc(&n.timeout_certificate)
        }
        H2Message::Decide(d) => n42_consensus::certifiable_view(d.view) && qc(&d.commit_qc),
    };
    if in_range {
        Ok(())
    } else {
        Err("a view of 2^64-1 (AM34 item 3)")
    }
}

/// The view an H2 message belongs to, for AM36 rule 3 (stale messages).
pub const fn h2_message_view(message: &crate::h2_wire::H2Message) -> u64 {
    use crate::h2_wire::H2Message;
    match message {
        H2Message::Proposal(p) => p.view,
        H2Message::Vote(v) | H2Message::CommitVote(v) => v.view,
        H2Message::PrepareQc(p) => p.view,
        H2Message::Timeout(t) => t.view,
        H2Message::NewView(n) => n.view,
        H2Message::Decide(d) => d.view,
    }
}

/// AM36 rule 3: a well-formed H2 message is stale when its view is below the
/// view of the newest CommitQC this member holds; the member has left every
/// such view. Messages for the committed view itself and later (including
/// future views) are forwarded: a peer one view behind still needs the
/// Decide and votes of the committed view.
pub const fn h2_message_is_stale(view: u64, committed_view: Option<u64>) -> bool {
    match committed_view {
        Some(committed) => view < committed,
        None => false,
    }
}

/// AM36 items 5 and 8 for one `transaction_v2` batch, entries in EIP-2718 form:
/// Reject if any entry's bytes do not decode as a transaction; Ignore if all
/// decode but one names a type outside profile 1's set
/// ([`n42_chainspec::profile1::check_profile1_tx_type`]: legacy, 2930, 1559,
/// 7702; profile 1 has no `altSigTx`); Accept otherwise. An empty batch is
/// Ignore.
///
/// A typed entry whose type byte this client has no decoder for counts as a
/// type outside the set (Ignore), not as undecodable bytes: a client that
/// knows one more type is not penalised (item 8's intent).
pub fn tx_batch_verdict(entries: &[alloy_primitives::Bytes]) -> Verdict {
    use alloy_eips::eip2718::Decodable2718;
    if entries.is_empty() {
        return gossipsub::MessageAcceptance::Ignore;
    }
    let mut outside = false;
    for raw in entries {
        let Some(&first) = raw.first() else {
            return gossipsub::MessageAcceptance::Reject;
        };
        // EIP-2718: a first byte up to 0x7f is a type; 0xc0.. a legacy list.
        let ty = if first <= 0x7f { first } else { 0x00 };
        if n42_chainspec::profile1::check_profile1_tx_type(ty).is_err() {
            if first <= 0x7f && n42_tx_types::N42TxEnvelope::decode_2718_exact(raw).is_err() {
                let known = matches!(first, 0x00..=0x04 | 0x50);
                if known {
                    // A type this client decodes, but these bytes do not.
                    return gossipsub::MessageAcceptance::Reject;
                }
            }
            outside = true;
            continue;
        }
        if n42_tx_types::N42TxEnvelope::decode_2718_exact(raw).is_err() {
            return gossipsub::MessageAcceptance::Reject;
        }
    }
    if outside {
        gossipsub::MessageAcceptance::Ignore
    } else {
        gossipsub::MessageAcceptance::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2_wire::{H2Decide, H2Message, H2Proposal, H2QuorumCertificate};
    use alloy_primitives::Bytes;
    use gossipsub::MessageAcceptance::{Accept, Ignore, Reject};

    fn qc(view: u64) -> H2QuorumCertificate {
        H2QuorumCertificate {
            view,
            block_hash: B256::repeat_byte(1),
            aggregate_signature: vec![0; 96],
            signers_bitmap: vec![4, 0, 0x0f],
        }
    }

    #[test]
    fn am36_proposal_and_decide_view_relations() {
        let signature = n42_primitives::BlsSecretKey::random().unwrap().sign(b"x");
        let proposal = |view, justify| {
            H2Message::Proposal(H2Proposal {
                view,
                block_hash: B256::ZERO,
                justify_qc: qc(justify),
                proposer: 0,
                signature: signature.clone(),
                prepare_qc: None,
                tx_root_hash: B256::ZERO,
            })
        };
        assert!(h2_fields_in_range(&proposal(10, 9)).is_ok());
        assert!(h2_fields_in_range(&proposal(10, 0)).is_ok());
        assert!(h2_fields_in_range(&proposal(10, 10)).is_err());
        assert!(h2_fields_in_range(&proposal(10, 11)).is_err());
        let decide = |view, commit| {
            H2Message::Decide(H2Decide {
                view,
                block_hash: B256::ZERO,
                commit_qc: qc(commit),
            })
        };
        assert!(h2_fields_in_range(&decide(10, 10)).is_ok());
        assert!(h2_fields_in_range(&decide(10, 9)).is_err());
        assert!(h2_fields_in_range(&decide(10, 11)).is_err());
        assert!(h2_fields_in_range(&decide(u64::MAX, u64::MAX)).is_err());
    }

    #[test]
    fn am36_stale_is_strictly_below_the_committed_view() {
        assert!(!h2_message_is_stale(5, None));
        assert!(h2_message_is_stale(49, Some(50)));
        assert!(!h2_message_is_stale(50, Some(50)));
        assert!(!h2_message_is_stale(u64::MAX - 1, Some(50)));
    }

    #[test]
    fn am36_transaction_batch_verdicts() {
        use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
        use alloy_eips::eip2718::Encodable2718;
        let sig = alloy_primitives::Signature::new(
            alloy_primitives::U256::from(1u64),
            alloy_primitives::U256::from(2u64),
            false,
        );
        let eip1559 =
            Bytes::from(TxEnvelope::Eip1559(TxEip1559::default().into_signed(sig)).encoded_2718());
        let legacy =
            Bytes::from(TxEnvelope::Legacy(TxLegacy::default().into_signed(sig)).encoded_2718());
        let blob = Bytes::from(
            TxEnvelope::from(
                alloy_consensus::TxEip4844 {
                    blob_versioned_hashes: vec![B256::repeat_byte(1)],
                    ..Default::default()
                }
                .into_signed(sig),
            )
            .encoded_2718(),
        );
        assert_eq!(tx_batch_verdict(&[eip1559.clone(), legacy.clone()]), Accept);
        // Undecodable bytes: Reject, whatever else the batch holds.
        assert_eq!(tx_batch_verdict(&[Bytes::new()]), Reject);
        assert_eq!(
            tx_batch_verdict(&[eip1559.clone(), Bytes::from_static(&[0x02, 1])]),
            Reject
        );
        assert_eq!(tx_batch_verdict(&[Bytes::from_static(&[0x90, 1])]), Reject);
        assert_eq!(
            tx_batch_verdict(&[blob.clone(), Bytes::from_static(&[0xc1])]),
            Reject
        );
        assert_eq!(
            tx_batch_verdict(&[Bytes::from_static(&[0x03, 1, 2])]),
            Reject
        );
        // Decodes, but outside profile 1: Ignore.
        assert_eq!(tx_batch_verdict(std::slice::from_ref(&blob)), Ignore);
        assert_eq!(tx_batch_verdict(&[eip1559, blob]), Ignore);
        assert_eq!(
            tx_batch_verdict(&[legacy, Bytes::from_static(&[0x05, 0xc0])]),
            Ignore
        );
        assert_eq!(tx_batch_verdict(&[]), Ignore);
    }
}
