//! gov5's profile-1 vector V7 (leader schedule, v1.2;
//! `testdata/profile1/gov5/V7`, a byte-exact copy of gov5's
//! `internal/testdata/profile1/vectors/V7` at gov5 08ecf671, the republished
//! set without the `tcView = 2^64-1` row) against this node's schedule code.
//!
//! - A15 leader tables and tenure windows (`leader_index`,
//!   `LeaderSelector::leader_for_view_with_tenure`).
//! - A18 epochs and boundaries (`EpochManager`).
//! - A22 / SYNC §10.2 `view_after_tc`: the pure rule and the
//!   `LeaderSchedule` the engine delegates to (rotate from view 0 on
//!   profile 1, the genesis epoch length).
//! - SYNC §10.4's sequences, AM32's rehearsal and the standard genesis's
//!   window-holds-a-boundary case, replayed step by step.
//!
//! AM34: `view_after_tc` is defined only below 2^64-1 and a certificate for
//! that view is invalid; the vector has no row for it. The refusal is tested
//! in `protocol::quorum`, `protocol::state_machine` and `validator::selection`.

use alloy_primitives::Address;
use n42_chainspec::ValidatorInfo;
use n42_consensus::{
    EpochManager, LeaderSchedule, LeaderSelector, ValidatorSet, leader_index, view_after_tc,
};
use n42_primitives::BlsSecretKey;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// SHA-256 of `leader_schedule.json` as gov5 published it (08ecf671).
const SCHEDULE_SHA256: &str = "a08180b1fc9625c0b6cd6128b775e40dcdf04a900b9ba3bf45aa9210340fd445";

fn vector() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/profile1/gov5/V7/leader_schedule.json");
    let raw = std::fs::read(path).expect("V7 vector");
    assert_eq!(
        hex::encode(Sha256::digest(&raw)),
        SCHEDULE_SHA256,
        "V7/leader_schedule.json is not gov5's byte-exact copy"
    );
    serde_json::from_slice(&raw).expect("json")
}

fn number(value: &Value) -> u64 {
    value.as_u64().expect("a number")
}

fn set(n: usize) -> ValidatorSet {
    let infos: Vec<_> = (0..n)
        .map(|i| ValidatorInfo {
            address: Address::with_last_byte(i as u8),
            bls_public_key: BlsSecretKey::key_gen(&[i as u8 + 1; 32])
                .expect("a valid deterministic key")
                .public_key(),
            p2p_peer_id: None,
        })
        .collect();
    let vs = ValidatorSet::new(&infos, (n as u32).saturating_sub(1) / 3);
    assert_eq!(vs.len() as usize, n);
    vs
}

/// The schedule a profile-1 engine runs: tenure `T`, rotate-on-timeout from
/// view 0 (A22) or off.
fn schedule(tenure: u64, rotate: bool) -> LeaderSchedule {
    LeaderSchedule {
        tenure,
        rotate_on_timeout_from: rotate.then_some(0),
    }
}

#[test]
fn v7_profiles_are_profile_1s() {
    let v = vector();
    let profiles = v["profiles"].as_array().expect("profiles");
    assert_eq!(profiles.len(), 2);
    for p in profiles {
        assert_eq!(number(&p["validators"]), 4);
        assert!(p["rotateOnTimeoutActive"].as_bool().expect("bool"));
        assert_eq!(number(&p["rotateOnTimeoutFromView"]), 0);
    }
    assert_eq!(
        (
            number(&profiles[0]["leaderTenure"]),
            number(&profiles[0]["epochLength"])
        ),
        (8, 200)
    );
    assert_eq!(
        (
            number(&profiles[1]["leaderTenure"]),
            number(&profiles[1]["epochLength"])
        ),
        (1, 10_800)
    );
    // V1's genesis is the one this repository parses as profile 1: the
    // schedule it resolves is the vector's second profile.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json");
    let genesis: alloy_genesis::Genesis =
        serde_json::from_slice(&std::fs::read(path).expect("genesis_8s.json")).expect("genesis");
    let (_, config) = n42_chainspec::profile1::resolve_genesis_profile(&genesis)
        .expect("a valid genesis")
        .expect("profile 1");
    let resolved = LeaderSchedule::resolve_profile1(&config, None, None).expect("schedule");
    assert_eq!(resolved.schedule, schedule(1, true));
    assert_eq!(config.epoch_length, number(&profiles[1]["epochLength"]));
    assert_eq!(
        config.validators.len() as u64,
        number(&profiles[1]["validators"])
    );
}

#[test]
fn v7_leader_tables_match_gov5() {
    let v = vector();
    let tables = v["leaderSchedule"].as_array().expect("leaderSchedule");
    assert_eq!(tables.len(), 6);
    let mut rows = 0;
    for table in tables {
        let n = number(&table["n"]);
        let vs = set(n as usize);
        let tenure = number(&table["tenure"]);
        for row in table["views"].as_array().expect("views") {
            let view = number(&row["view"]);
            let leader = number(&row["leader"]);
            let what = format!("{}: view {view}", table["name"]);
            assert_eq!(u64::from(leader_index(view, n, tenure)), leader, "{what}");
            assert_eq!(
                u64::from(LeaderSelector::leader_for_view_with_tenure(view, &vs, tenure)),
                leader,
                "{what}"
            );
            rows += 1;
        }
    }
    assert!(rows > 50, "{rows} rows");
}

#[test]
fn v7_tenure_windows_match_gov5() {
    let v = vector();
    for row in v["tenureWindows"].as_array().expect("tenureWindows") {
        let n = number(&row["n"]);
        let (tenure, view) = (number(&row["tenure"]), number(&row["view"]));
        let t = tenure.max(1);
        let window = view / t;
        assert_eq!(window, number(&row["window"]), "view {view}: window");
        assert_eq!(window * t, number(&row["firstView"]), "view {view}: first view");
        assert_eq!(
            window * t + t - 1,
            number(&row["lastView"]),
            "view {view}: last view"
        );
        let leader = leader_index(view, n, tenure);
        assert_eq!(u64::from(leader), number(&row["leader"]), "view {view}: leader");
        for w in window * t..=window * t + t - 1 {
            assert_eq!(
                leader_index(w, n, tenure),
                leader,
                "view {w} in {view}'s window"
            );
        }
    }
}

#[test]
fn v7_epochs_match_gov5() {
    let v = vector();
    let vs = set(4);
    let rows = v["epochs"].as_array().expect("epochs");
    assert!(!rows.is_empty());
    for row in rows {
        let (length, view) = (number(&row["epochLength"]), number(&row["view"]));
        let manager = EpochManager::with_epoch_length(vs.clone(), length);
        assert_eq!(
            manager.epoch_for_view(view),
            number(&row["epoch"]),
            "E = {length}, view {view}: epoch"
        );
        assert_eq!(
            manager.is_epoch_boundary(view),
            row["isBoundary"].as_bool().expect("bool"),
            "E = {length}, view {view}: boundary"
        );
    }
}

#[test]
fn v7_view_after_tc_matches_gov5() {
    let v = vector();
    let rows = v["viewAfterTC"].as_array().expect("viewAfterTC");
    assert_eq!(rows.len(), 51);
    for row in rows {
        let (x, tenure, length) = (
            number(&row["tcView"]),
            number(&row["tenure"]),
            number(&row["epochLength"]),
        );
        let active = row["active"].as_bool().expect("bool");
        let result = number(&row["result"]);
        let what = format!(
            "{} TC({x}) T = {tenure} E = {length} active = {active}",
            row["source"]
        );
        assert!(x < u64::MAX, "{what}: AM34 leaves the last view undefined");
        assert_eq!(view_after_tc(x, tenure, length, active), result, "{what}");
        // The engine's rule (`ConsensusEngine::view_after_tc` delegates here).
        assert_eq!(
            schedule(tenure, active).view_after_tc(x, length),
            result,
            "{what}: schedule"
        );
        // Clamped = the result is the epoch boundary, not the window start.
        let window = (x / tenure.max(1))
            .checked_add(1)
            .and_then(|w| w.checked_mul(tenure.max(1)));
        let boundary = (length > 0 && x >= 1)
            .then(|| {
                ((x - 1) / length + 1)
                    .checked_mul(length)
                    .and_then(|b| b.checked_add(1))
            })
            .flatten();
        let clamped = active
            && tenure > 1
            && boundary == Some(result)
            && window.is_some_and(|w| result < w);
        assert_eq!(
            clamped,
            row["clampedToEpochBoundary"].as_bool().expect("bool"),
            "{what}: clamp flag"
        );
    }
}

#[test]
fn v7_sequences_match_gov5() {
    let v = vector();
    let sequences = v["sequences"].as_array().expect("sequences");
    assert_eq!(sequences.len(), 11);
    for seq in sequences {
        let id = seq["id"].as_str().expect("id");
        let (n, tenure, length) = (
            number(&seq["n"]),
            number(&seq["tenure"]),
            number(&seq["epochLength"]),
        );
        let rules = schedule(tenure, true);
        let leader = |view: u64| u64::from(leader_index(view, n, tenure));
        let mut last_tc = None;
        let steps = seq["steps"].as_array().expect("steps");
        for (i, step) in steps.iter().enumerate() {
            let view = number(&step["view"]);
            let what = format!("{id} step {i} ({} at {view})", step["event"]);
            assert_eq!(leader(view), number(&step["leaderOfView"]), "{what}: leader of view");
            let next = match step["event"].as_str().expect("event") {
                "QC" => view + 1,
                "TC" => {
                    last_tc = Some(view);
                    rules.view_after_tc(view, length)
                }
                // A piggybacked TC (the latest TC of the sequence) moves a
                // node at `view` only if the view after it is higher.
                "adoptTC" | "ignoreTC" => {
                    let target = rules.view_after_tc(last_tc.expect("a TC before"), length);
                    let adopt = target > view;
                    assert_eq!(adopt, step["event"] == "adoptTC", "{what}: adopt");
                    if adopt { target } else { view }
                }
                other => panic!("{what}: unknown event {other}"),
            };
            assert_eq!(next, number(&step["nextView"]), "{what}: next view");
            assert_eq!(leader(next), number(&step["nextLeader"]), "{what}: next leader");
            // A sequence is one node's path, except S11's race rows.
            if let Some(following) = steps.get(i + 1)
                && !matches!(following["event"].as_str(), Some("adoptTC" | "ignoreTC"))
                && id != "S11"
            {
                assert_eq!(
                    number(&following["view"]),
                    next,
                    "{what}: the next step starts where this one ends"
                );
            }
        }
    }
}
