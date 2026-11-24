//! Static header validity on a Profile-1 chain (mixed production standard
//! B1-B4, B8-B14, A23, AM4, AM24), checked against the parent header.
//!
//! [`validate_profile1_header`] is the import-time check: it depends only on
//! the header, its parent, the genesis validator set and leader schedule, and
//! an ancestry oracle for the embedded QC, never on the clock. The vote-time
//! clock bound of B4 is [`check_profile1_vote_time`]. The body checks (B6
//! rewards against `withdrawalsRoot`, transactions root) and the deferred
//! execution fields (B13 values, A10) belong to execution and are not here.

use alloy_eips::eip1559::BaseFeeParams;
use alloy_primitives::{Address, B256, U256};
use n42_chainspec::profile1::{
    Profile1ExtraError, Profile1Header, Profile1SealError,
};
use n42_consensus::{LeaderSchedule, ValidatorSet, leader_index};
use n42_network::h2_v4::H2V4ChainIdentity;
use n42_network::h2_wire::decode_h2_qc;

use crate::h2_finality::{H2V4AnchorVerdict, verify_h2_v4_sync_anchor};

/// What a Profile-1 header is checked against.
#[derive(Clone, Copy, Debug)]
pub struct Profile1HeaderRules<'a> {
    /// `(chainId, genesisHash)`: the H2-v4 signing identity (A1).
    pub identity: H2V4ChainIdentity,
    /// The genesis validator set (A3, AM5).
    pub validators: &'a ValidatorSet,
    /// Tenure and rotate-on-timeout (A15, A22).
    pub schedule: LeaderSchedule,
    /// `config.hotstuff.gasLimit` (A23).
    pub gas_limit: u64,
}

/// Why a Profile-1 header is invalid. The rule id is the standard's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Profile1HeaderInvalid {
    #[error("profile 1 (B1): invalid header extraData: {0}")]
    Extra(Profile1ExtraError),
    #[error("profile 1 (B1): invalid header QC: {0}")]
    Qc(String),
    #[error("profile 1 (B2): invalid seal: {0}")]
    Seal(Profile1SealError),
    #[error("profile 1 (B3): coinbase {coinbase} is not the leader of view {view}, {leader}")]
    Proposer {
        coinbase: Address,
        view: u64,
        leader: Address,
    },
    #[error("profile 1 (B3): difficulty is not zero: {0}")]
    Difficulty(U256),
    #[error("profile 1 (B3): nonce is not zero")]
    Nonce,
    #[error("profile 1 (B3): mixHash is not zero: {0}")]
    MixHash(B256),
    #[error("profile 1 (B3): ommersHash is not zero: {0}")]
    OmmersHash(B256),
    #[error("profile 1 (B4): timestamp before parent: {time} < parent {parent}")]
    TimeBeforeParent { time: u64, parent: u64 },
    #[error("profile 1 (B4): timestamp too far ahead of the local clock: {time} > floor(now) + 2 = {bound}")]
    TimeFuture { time: u64, bound: u64 },
    #[error("profile 1 (B8, AM24): requestsHash is not sha256(\"\") of EIP-7685: {0:?}")]
    RequestsHash(Option<B256>),
    #[error("profile 1 (B9): blob gas fields are not zero: blobGasUsed {used:?}, excessBlobGas {excess:?}")]
    BlobGas {
        used: Option<u64>,
        excess: Option<u64>,
    },
    #[error("profile 1 (B10): slot number {slot:?} is not the block number {number}")]
    SlotNumber { slot: Option<u64>, number: u64 },
    #[error("profile 1 (B13): gasUsed {used} exceeds the parent gasLimit {limit}")]
    GasUsed { used: u64, limit: u64 },
    #[error("profile 1 (B14): baseFee {got:?} is not the EIP-1559 value of the parent, want {want}")]
    BaseFee { got: Option<u64>, want: u64 },
    #[error("profile 1 (A23): gasLimit is not hotstuff.gasLimit: {got}, want {want}")]
    GasLimit { got: u64, want: u64 },
    #[error("profile 1: header does not extend its parent: {0}")]
    Parent(String),
}

/// What a valid header says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile1HeaderView {
    pub view: u64,
    /// The embedded CommitQC: `(view, block)`, when present.
    pub qc: Option<(u64, B256)>,
    /// The validator that proposed and sealed the header.
    pub leader_index: u32,
}

/// The view a Profile-1 header carries in its extra data; genesis (empty
/// extra) is view 0.
pub fn profile1_header_view(header: &Profile1Header) -> Result<u64, Profile1HeaderInvalid> {
    if header.header.number == 0 && header.header.extra_data.is_empty() {
        return Ok(0);
    }
    header
        .extra()
        .map(|extra| extra.view)
        .map_err(Profile1HeaderInvalid::Extra)
}

/// The import-time checks of a Profile-1 header against its parent.
/// `is_ancestor(hash)` answers whether `hash` is the parent or one of its
/// ancestors (B1: the embedded QC names an ancestor of the block).
pub fn validate_profile1_header(
    header: &Profile1Header,
    parent: &Profile1Header,
    rules: &Profile1HeaderRules<'_>,
    is_ancestor: impl Fn(B256) -> bool,
) -> Result<Profile1HeaderView, Profile1HeaderInvalid> {
    let h = &header.header;
    let p = &parent.header;
    let parent_hash = parent.hash();
    if h.parent_hash != parent_hash || Some(h.number) != p.number.checked_add(1) {
        return Err(Profile1HeaderInvalid::Parent(format!(
            "block {} names parent {}, given parent is block {} {}",
            h.number, h.parent_hash, p.number, parent_hash
        )));
    }

    // B1: one extra layout.
    let extra = header.extra().map_err(Profile1HeaderInvalid::Extra)?;
    let parent_view = profile1_header_view(parent)?;
    if extra.view <= parent_view {
        return Err(Profile1HeaderInvalid::Parent(format!(
            "view {} is not above the parent's view {parent_view}",
            extra.view
        )));
    }

    // B3: constant fields and the proposer.
    if h.difficulty != U256::ZERO {
        return Err(Profile1HeaderInvalid::Difficulty(h.difficulty));
    }
    if !h.nonce.is_zero() {
        return Err(Profile1HeaderInvalid::Nonce);
    }
    if h.mix_hash != B256::ZERO {
        return Err(Profile1HeaderInvalid::MixHash(h.mix_hash));
    }
    if h.ommers_hash != B256::ZERO {
        return Err(Profile1HeaderInvalid::OmmersHash(h.ommers_hash));
    }
    let n = u64::from(rules.validators.len());
    let leader = leader_index(extra.view, n, rules.schedule.tenure);
    let leader_address = *rules
        .validators
        .get_address(leader)
        .map_err(|error| Profile1HeaderInvalid::Qc(error.to_string()))?;
    if h.beneficiary != leader_address {
        return Err(Profile1HeaderInvalid::Proposer {
            coinbase: h.beneficiary,
            view: extra.view,
            leader: leader_address,
        });
    }

    // B4 (import half): equal timestamps are allowed.
    if h.timestamp < p.timestamp {
        return Err(Profile1HeaderInvalid::TimeBeforeParent {
            time: h.timestamp,
            parent: p.timestamp,
        });
    }
    // B8 (AM24): EIP-7685's empty requests hash; the empty-trie root is refused.
    if h.requests_hash != Some(n42_chainspec::profile1::PROFILE1_REQUESTS_HASH) {
        return Err(Profile1HeaderInvalid::RequestsHash(h.requests_hash));
    }
    // B9.
    if h.blob_gas_used != Some(0) || h.excess_blob_gas != Some(0) {
        return Err(Profile1HeaderInvalid::BlobGas {
            used: h.blob_gas_used,
            excess: h.excess_blob_gas,
        });
    }
    // B10.
    if h.slot_number != Some(h.number) {
        return Err(Profile1HeaderInvalid::SlotNumber {
            slot: h.slot_number,
            number: h.number,
        });
    }
    // A23 and B13's bound.
    if h.gas_limit != rules.gas_limit {
        return Err(Profile1HeaderInvalid::GasLimit {
            got: h.gas_limit,
            want: rules.gas_limit,
        });
    }
    if h.gas_used > p.gas_limit {
        return Err(Profile1HeaderInvalid::GasUsed {
            used: h.gas_used,
            limit: p.gas_limit,
        });
    }
    // B14: from the parent header as written.
    let want = p
        .next_block_base_fee(BaseFeeParams::ethereum())
        .ok_or_else(|| Profile1HeaderInvalid::Parent("parent has no base fee".to_owned()))?;
    if h.base_fee_per_gas != Some(want) {
        return Err(Profile1HeaderInvalid::BaseFee {
            got: h.base_fee_per_gas,
            want,
        });
    }

    // B1: the embedded CommitQC, zero bytes when absent.
    let qc = if extra.qc.is_empty() {
        None
    } else {
        let qc = decode_h2_qc(extra.qc)
            .map_err(|error| Profile1HeaderInvalid::Qc(format!("QC does not decode: {error}")))?;
        if qc.view == 0 || qc.block_hash == B256::ZERO {
            return Err(Profile1HeaderInvalid::Qc(
                "an absent QC is zero bytes, not an encoded empty QC".to_owned(),
            ));
        }
        if qc.view >= extra.view {
            return Err(Profile1HeaderInvalid::Qc(format!(
                "QC view {} is not below header view {}",
                qc.view, extra.view
            )));
        }
        match verify_h2_v4_sync_anchor(&qc, rules.identity, rules.validators) {
            H2V4AnchorVerdict::Verified => {}
            H2V4AnchorVerdict::PrepareDomain => {
                return Err(Profile1HeaderInvalid::Qc(format!(
                    "QC for view {} is signed in the vote domain, not the commit domain",
                    qc.view
                )));
            }
            H2V4AnchorVerdict::Invalid => {
                return Err(Profile1HeaderInvalid::Qc(format!(
                    "QC for view {} is not a commit-domain quorum of the genesis set",
                    qc.view
                )));
            }
        }
        if !is_ancestor(qc.block_hash) {
            return Err(Profile1HeaderInvalid::Qc(format!(
                "QC block {} is not an ancestor",
                qc.block_hash
            )));
        }
        Some((qc.view, qc.block_hash))
    };

    // B2 (AM4): the leader's seal over sealHash, verified on import.
    let key = rules
        .validators
        .get_public_key(leader)
        .map_err(|error| Profile1HeaderInvalid::Qc(error.to_string()))?;
    header
        .verify_seal(key)
        .map_err(Profile1HeaderInvalid::Seal)?;

    Ok(Profile1HeaderView {
        view: extra.view,
        qc,
        leader_index: leader,
    })
}

/// B4, vote half: `time <= floor(now) + 2`, checked before the Round-1 vote
/// only (never on import of a committed block).
pub fn check_profile1_vote_time(
    header: &Profile1Header,
    now_unix: u64,
) -> Result<(), Profile1HeaderInvalid> {
    let bound = now_unix.saturating_add(2);
    if header.header.timestamp > bound {
        return Err(Profile1HeaderInvalid::TimeFuture {
            time: header.header.timestamp,
            bound,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    use n42_chainspec::profile1::profile1_chain_spec;
    use serde_json::Value;
    use std::path::Path;

    fn testdata(file: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../n42-chainspec/testdata/profile1/V1")
            .join(file);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn header(value: &Value) -> Profile1Header {
        Profile1Header::decode(&hex::decode(value.as_str().unwrap()).unwrap()).unwrap()
    }

    struct Fixture {
        vector: Value,
        genesis: Profile1Header,
        block1: Profile1Header,
        block2: Profile1Header,
        validators: ValidatorSet,
        identity: H2V4ChainIdentity,
        gas_limit: u64,
        schedule: LeaderSchedule,
    }

    impl Fixture {
        fn load() -> Self {
            let vector = testdata("header_profile_v1.json");
            let (spec, config, genesis) = profile1_chain_spec(&testdata("genesis_8s.json")).unwrap();
            let validators =
                ValidatorSet::try_new(&config.validator_infos(), config.fault_tolerance()).unwrap();
            let schedule = LeaderSchedule::resolve_profile1(&config, None, None)
                .unwrap()
                .schedule;
            Self {
                block1: header(&vector["headers"][0]["rlp"]),
                block2: header(&vector["headers"][1]["rlp"]),
                vector,
                genesis,
                validators,
                identity: H2V4ChainIdentity {
                    chain_id: config.chain_id,
                    genesis_hash: spec.genesis_hash(),
                },
                gas_limit: config.gas_limit,
                schedule,
            }
        }

        fn rules(&self) -> Profile1HeaderRules<'_> {
            Profile1HeaderRules {
                identity: self.identity,
                validators: &self.validators,
                schedule: self.schedule,
                gas_limit: self.gas_limit,
            }
        }

        /// Block 2's ancestors: block 1 and genesis.
        fn is_ancestor(&self) -> impl Fn(B256) -> bool + '_ {
            move |hash| hash == self.block1.hash() || hash == self.genesis.hash()
        }
    }

    #[test]
    fn v1_blocks_are_valid() {
        let f = Fixture::load();
        let rules = f.rules();
        let view1 = validate_profile1_header(&f.block1, &f.genesis, &rules, |hash| {
            hash == f.genesis.hash()
        })
        .unwrap();
        assert_eq!(view1, Profile1HeaderView { view: 1, qc: None, leader_index: 1 });
        let view2 = validate_profile1_header(&f.block2, &f.block1, &rules, f.is_ancestor()).unwrap();
        assert_eq!(view2.view, 2);
        assert_eq!(view2.leader_index, 2);
        assert_eq!(view2.qc, Some((1, f.block1.hash())));
        assert_eq!(f.vector["headers"][1]["qc"]["view"].as_u64(), Some(1));
        // Committed blocks import whatever the clock says; the vote bound
        // holds at the block's own time.
        check_profile1_vote_time(&f.block2, f.block2.header.timestamp).unwrap();
    }

    /// Every V1 refusal: the import ones by [`validate_profile1_header`] (or,
    /// for a layout the decoder cannot read, by [`Profile1Header::decode`]),
    /// the vote one by [`check_profile1_vote_time`]. The execution one (body
    /// rewards against withdrawalsRoot) is not a header check.
    #[test]
    fn every_v1_header_refusal_is_refused() {
        let f = Fixture::load();
        let rules = f.rules();
        let mut checked = 0;
        for refusal in f.vector["refusals"].as_array().unwrap() {
            let name = refusal["name"].as_str().unwrap();
            let raw = hex::decode(refusal["rlp"].as_str().unwrap()).unwrap();
            match refusal["check"].as_str().unwrap() {
                "import" => {
                    let result = Profile1Header::decode(&raw)
                        .map_err(|error| error.to_string())
                        .and_then(|header| {
                            validate_profile1_header(&header, &f.block1, &rules, f.is_ancestor())
                                .map_err(|error| error.to_string())
                        });
                    let error = result.expect_err(name);
                    eprintln!("{name}: {error}  [gov5: {}]", refusal["gov5Message"]);
                    if name.starts_with("requestsHash") {
                        // AM24: the empty-trie root is refused by B8 itself.
                        assert!(error.contains("(B8, AM24)"), "{name}: {error}");
                    }
                    checked += 1;
                }
                "vote" => {
                    let header = Profile1Header::decode(&raw).unwrap();
                    validate_profile1_header(&header, &f.block1, &rules, f.is_ancestor())
                        .expect("a vote-time refusal imports");
                    let now = refusal["nowUnix"].as_u64().unwrap();
                    let error = check_profile1_vote_time(&header, now).expect_err(name);
                    eprintln!("{name}: {error}");
                    checked += 1;
                }
                "execution" => {
                    // The header is block 2 itself and valid on import.
                    let header = Profile1Header::decode(&raw).unwrap();
                    assert_eq!(header, f.block2, "{name}");
                }
                other => panic!("{name}: unknown check {other}"),
            }
        }
        assert_eq!(checked, 24);
    }

    /// B6 / A8: block N's withdrawalsRoot is the rewards root of
    /// `devBlockReward` to its coinbase and to the faucet.
    #[test]
    fn v1_withdrawals_roots_are_the_dev_rewards() {
        let f = Fixture::load();
        let faucet: Address = "0x42e9819036f61bF665D5f727E8C03121f12f586e".parse().unwrap();
        let reward = U256::from(1_000_000_000_000_000_000_u64);
        for block in [&f.block1, &f.block2] {
            let rewards = [
                n42_consensus::Gov5Reward { address: block.header.beneficiary, amount: reward },
                n42_consensus::Gov5Reward { address: faucet, amount: reward },
            ];
            assert_eq!(
                Some(n42_consensus::gov5_native_rewards_root(&rewards)),
                block.header.withdrawals_root
            );
        }
    }
}
