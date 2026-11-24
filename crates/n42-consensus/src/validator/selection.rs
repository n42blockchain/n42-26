use super::set::ValidatorSet;
use n42_primitives::consensus::ViewNumber;

/// Leader selection strategy based on round-robin rotation.
///
/// The leader for view `v` is `(v / T) % n` where `n` is the validator set
/// size and `T` the chain's leader tenure ([`leader_tenure`]). This is
/// deterministic and requires no communication: every honest node can
/// independently compute who the leader is for any view.
#[derive(Debug, Clone)]
pub struct LeaderSelector;

/// Bench override of the genesis `config.hotstuff.leaderTenure`.
pub const LEADER_TENURE_ENV: &str = "N42_LEADER_TENURE";
/// Bench override of the genesis `config.hotstuff.rotateOnTimeoutView`:
/// `1|true|on` turns the rule on from view 0, `0|false|off` forces it off,
/// unset defers to the genesis (sync standard section 10.3).
pub const ROTATE_ON_TIMEOUT_ENV: &str = "N42_HOTSTUFF_ROTATE_ON_TIMEOUT";

/// The chain's leader rules, resolved once at start-up: the tenure and the
/// rotate-on-timeout activation (sync standard section 10).
///
/// Both are consensus-visible: every validator must run with the same values,
/// or they disagree about who may propose and which view follows a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaderSchedule {
    /// Consecutive views one validator leads; at least 1.
    pub tenure: u64,
    /// The first TC view rotate-on-timeout applies to; `None` = disabled.
    pub rotate_on_timeout_from: Option<ViewNumber>,
}

impl Default for LeaderSchedule {
    /// Tenure 1, no rotate-on-timeout: the rules of a genesis that sets
    /// neither field.
    fn default() -> Self {
        Self {
            tenure: 1,
            rotate_on_timeout_from: None,
        }
    }
}

/// Why the genesis leader rules cannot be read. Start-up fails closed on any
/// of these rather than guessing a consensus-visible value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaderScheduleError {
    /// `config.hotstuff` is present but not an object.
    #[error("genesis config.hotstuff is not an object")]
    HotstuffShape,
    /// A field is present but not an unsigned 64-bit integer.
    #[error("genesis config.hotstuff.{0} is not an unsigned 64-bit integer")]
    Field(&'static str),
    /// A Profile-1 genesis whose leader rules cannot be used.
    #[error("{0}")]
    Profile1(String),
}

/// The schedule together with what the environment changed, for the caller
/// to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLeaderSchedule {
    pub schedule: LeaderSchedule,
    /// The genesis values before any override.
    pub genesis: LeaderSchedule,
    /// Bench-override notes: an env value that replaced a different genesis
    /// value, or one that was ignored because it does not parse.
    pub notes: Vec<String>,
}

impl LeaderSchedule {
    /// Reads `leaderTenure` (absent or 0 = 1) and `rotateOnTimeoutView`
    /// (absent = disabled) from a genesis `config.hotstuff` value.
    pub fn from_hotstuff_config(
        hotstuff: Option<&serde_json::Value>,
    ) -> Result<Self, LeaderScheduleError> {
        let Some(hotstuff) = hotstuff else {
            return Ok(Self::default());
        };
        let hotstuff = hotstuff
            .as_object()
            .ok_or(LeaderScheduleError::HotstuffShape)?;
        let field = |name: &'static str| -> Result<Option<u64>, LeaderScheduleError> {
            match hotstuff.get(name) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(value) => value
                    .as_u64()
                    .map(Some)
                    .ok_or(LeaderScheduleError::Field(name)),
            }
        };
        Ok(Self {
            tenure: field("leaderTenure")?.unwrap_or(1).max(1),
            rotate_on_timeout_from: field("rotateOnTimeoutView")?,
        })
    }

    /// The genesis rules with the bench overrides applied. `env_tenure` and
    /// `env_rotate` are the raw values of [`LEADER_TENURE_ENV`] and
    /// [`ROTATE_ON_TIMEOUT_ENV`].
    pub fn resolve(
        genesis_hotstuff: Option<&serde_json::Value>,
        env_tenure: Option<&str>,
        env_rotate: Option<&str>,
    ) -> Result<ResolvedLeaderSchedule, LeaderScheduleError> {
        let genesis = Self::from_hotstuff_config(genesis_hotstuff)?;
        let mut schedule = genesis;
        let mut notes = Vec::new();
        if let Some(raw) = env_tenure {
            match raw.trim().parse::<u64>() {
                Ok(tenure) if tenure > 0 => {
                    if tenure != genesis.tenure {
                        notes.push(format!(
                            "{LEADER_TENURE_ENV}={tenure} overrides the genesis leaderTenure {}",
                            genesis.tenure
                        ));
                    }
                    schedule.tenure = tenure;
                }
                _ => notes.push(format!(
                    "{LEADER_TENURE_ENV}={raw:?} is not a positive integer; ignored"
                )),
            }
        }
        if let Some(raw) = env_rotate {
            let forced = match raw {
                "1" | "true" | "TRUE" | "on" => Some(Some(0)),
                "0" | "false" | "FALSE" | "off" => Some(None),
                _ => {
                    notes.push(format!(
                        "{ROTATE_ON_TIMEOUT_ENV}={raw:?} is neither on nor off; ignored"
                    ));
                    None
                }
            };
            if let Some(from) = forced {
                if from != genesis.rotate_on_timeout_from {
                    notes.push(format!(
                        "{ROTATE_ON_TIMEOUT_ENV}={raw} overrides the genesis rotateOnTimeoutView {:?}",
                        genesis.rotate_on_timeout_from
                    ));
                }
                schedule.rotate_on_timeout_from = from;
            }
        }
        Ok(ResolvedLeaderSchedule {
            schedule,
            genesis,
            notes,
        })
    }

    /// The rules of a Profile-1 chain (mixed production standard A15, A22,
    /// AM12): the mandatory `leaderTenure` (>= 1) and rotate-on-timeout from
    /// view 0, with no field and no override. A bench override is refused,
    /// not applied (AM10).
    pub fn resolve_profile1(
        config: &n42_chainspec::profile1::Profile1Config,
        env_tenure: Option<&str>,
        env_rotate: Option<&str>,
    ) -> Result<ResolvedLeaderSchedule, LeaderScheduleError> {
        for (name, value) in [
            (LEADER_TENURE_ENV, env_tenure),
            (ROTATE_ON_TIMEOUT_ENV, env_rotate),
        ] {
            if value.is_some() {
                return Err(LeaderScheduleError::Profile1(format!(
                    "{name} is set; a Profile 1 chain reads its leader rules only from the genesis"
                )));
            }
        }
        if config.leader_tenure == 0 {
            return Err(LeaderScheduleError::Profile1(
                "leaderTenure must be >= 1".to_owned(),
            ));
        }
        let schedule = Self {
            tenure: config.leader_tenure,
            rotate_on_timeout_from: Some(0),
        };
        Ok(ResolvedLeaderSchedule {
            schedule,
            genesis: schedule,
            notes: Vec::new(),
        })
    }

    /// [`Self::resolve`] for a genesis and this process's environment; on a
    /// Profile-1 genesis, [`Self::resolve_profile1`].
    pub fn resolve_for_genesis(
        genesis: &alloy_genesis::Genesis,
    ) -> Result<ResolvedLeaderSchedule, LeaderScheduleError> {
        if let Some((_, config)) = n42_chainspec::profile1::resolve_genesis_profile(genesis)
            .map_err(|error| LeaderScheduleError::Profile1(error.to_string()))?
        {
            return Self::resolve_profile1(
                &config,
                std::env::var(LEADER_TENURE_ENV).ok().as_deref(),
                std::env::var(ROTATE_ON_TIMEOUT_ENV).ok().as_deref(),
            );
        }
        Self::resolve(
            genesis.config.extra_fields.get("hotstuff"),
            std::env::var(LEADER_TENURE_ENV).ok().as_deref(),
            std::env::var(ROTATE_ON_TIMEOUT_ENV).ok().as_deref(),
        )
    }

    /// Whether rotate-on-timeout applies to a TC for `tc_view`.
    pub fn rotates_at(&self, tc_view: ViewNumber) -> bool {
        self.rotate_on_timeout_from
            .is_some_and(|from| tc_view >= from)
    }

    /// The view entered on a TC for `tc_view` under these rules, with
    /// `epoch_length` views per epoch (0 = a single epoch).
    pub fn view_after_tc(&self, tc_view: ViewNumber, epoch_length: u64) -> ViewNumber {
        view_after_tc(tc_view, self.tenure, epoch_length, self.rotates_at(tc_view))
    }
}

/// The view a validator enters once it holds a timeout certificate for
/// `tc_view` (sync standard section 10.2; gov5 `ViewAfterTimeout`).
///
/// The leader schedule `(v / T) % n` is unchanged; the rule moves the view.
/// Without rotation, or with a tenure of one, it is `tc_view + 1`. With
/// rotation it is the first view of the next tenure window, so a timed-out
/// leader loses the rest of its window and the next validator leads a fresh
/// one; the jump is clamped to the first epoch-boundary view above `tc_view`
/// (boundary `b`: `(b - 1) % E == 0`), so every client enters that view. A
/// window start that overflows falls back to `tc_view + 1`.
pub fn view_after_tc(
    tc_view: ViewNumber,
    tenure: u64,
    epoch_length: u64,
    rotate: bool,
) -> ViewNumber {
    let next = tc_view.wrapping_add(1);
    if !rotate || tenure <= 1 || next == 0 {
        return next;
    }
    let Some(target) = (tc_view / tenure + 1)
        .checked_mul(tenure)
        .filter(|target| *target >= next)
    else {
        return next;
    };
    if epoch_length > 0 && tc_view >= 1 {
        let boundary = ((tc_view - 1) / epoch_length + 1)
            .checked_mul(epoch_length)
            .and_then(|view| view.checked_add(1));
        if let Some(boundary) = boundary
            && boundary > tc_view
            && boundary < target
        {
            return boundary;
        }
    }
    target
}

static SCHEDULE: std::sync::OnceLock<LeaderSchedule> = std::sync::OnceLock::new();

/// Installs the chain's leader rules for this process. The node does this
/// once at start-up, from the genesis, before any engine or verifier exists.
///
/// Returns the schedule already in force when another one was installed or
/// read first; the caller must then refuse to start, as two values in one
/// process would let components disagree about the leader.
pub fn install_leader_schedule(schedule: LeaderSchedule) -> Result<(), LeaderSchedule> {
    let installed = *SCHEDULE.get_or_init(|| schedule);
    if installed == schedule {
        Ok(())
    } else {
        Err(installed)
    }
}

/// The leader rules in force in this process: the installed ones, or, in a
/// process that installs none (tools, tests), the environment alone over a
/// genesis without the fields.
pub fn leader_schedule() -> LeaderSchedule {
    *SCHEDULE.get_or_init(|| {
        LeaderSchedule::resolve(
            None,
            std::env::var(LEADER_TENURE_ENV).ok().as_deref(),
            std::env::var(ROTATE_ON_TIMEOUT_ENV).ok().as_deref(),
        )
        .map(|resolved| resolved.schedule)
        .unwrap_or_default()
    })
}

/// Consecutive views one validator leads before the role moves on: the
/// genesis `config.hotstuff.leaderTenure` (absent = 1), or the
/// `N42_LEADER_TENURE` bench override; see [`leader_schedule`].
pub fn leader_tenure() -> u64 {
    leader_schedule().tenure
}

/// The leader of `view` among `validators` validators when each leads
/// `tenure` consecutive views. `validators` must not be zero; a zero
/// `tenure` is read as 1.
pub fn leader_index(view: ViewNumber, validators: u64, tenure: u64) -> u32 {
    ((view / tenure.max(1)) % validators) as u32
}

impl LeaderSelector {
    /// Returns the leader's validator index for the given view under this
    /// process's tenure.
    ///
    /// # Panics
    /// Panics if the validator set is empty. Returning a synthetic validator
    /// index would let a malformed release configuration appear to have a
    /// leader and fail open.
    pub fn leader_for_view(view: ViewNumber, validator_set: &ValidatorSet) -> u32 {
        Self::leader_for_view_with_tenure(view, validator_set, leader_tenure())
    }

    /// [`Self::leader_for_view`] with an explicit tenure.
    ///
    /// # Panics
    /// Panics if the validator set is empty.
    pub fn leader_for_view_with_tenure(
        view: ViewNumber,
        validator_set: &ValidatorSet,
        tenure: u64,
    ) -> u32 {
        assert!(
            !validator_set.is_empty(),
            "leader_for_view called with empty validator set"
        );
        leader_index(view, validator_set.len() as u64, tenure)
    }

    /// Checks if the given validator is the leader for the given view.
    pub fn is_leader(validator_index: u32, view: ViewNumber, validator_set: &ValidatorSet) -> bool {
        Self::leader_for_view(view, validator_set) == validator_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenure_keeps_the_leader_for_that_many_views_then_rotates() {
        assert_eq!(
            (0..8)
                .map(|view| leader_index(view, 3, 1))
                .collect::<Vec<_>>(),
            [0, 1, 2, 0, 1, 2, 0, 1]
        );
        assert_eq!(
            (0..10)
                .map(|view| leader_index(view, 3, 4))
                .collect::<Vec<_>>(),
            [0, 0, 0, 0, 1, 1, 1, 1, 2, 2]
        );
        assert_eq!(leader_index(4 * 3, 3, 4), 0);
    }

    /// Sync standard section 10.4, `view_after_tc(x)` for (T, E, active).
    #[test]
    fn view_after_tc_matches_the_standard_vectors() {
        const MAX: u64 = u64::MAX;
        let vectors: [(u64, u64, u64, bool, u64); 16] = [
            (8, 8, 0, false, 9),
            (8, 8, 0, true, 16),
            (9, 8, 0, true, 16),
            (15, 8, 0, true, 16),
            (16, 8, 0, true, 24),
            (7, 8, 0, true, 8),
            (0, 8, 0, true, 8),
            (5, 1, 0, true, 6),
            (5, 0, 0, true, 6),
            (8, 8, 10, true, 11),
            (10, 8, 10, true, 11),
            (11, 8, 10, true, 16),
            (17, 8, 10, true, 21),
            (100, 32, 0, true, 128),
            (MAX - 1, 8, 0, true, MAX),     // 2^64-2 -> 2^64-1
            (MAX - 3, 8, 0, true, MAX - 2), // 2^64-4 -> 2^64-3
        ];
        for (x, tenure, epoch, active, expected) in vectors {
            assert_eq!(
                view_after_tc(x, tenure, epoch, active),
                expected,
                "x={x} T={tenure} E={epoch} active={active}"
            );
        }
    }

    /// Sync standard section 10.4 sequences S1-S11: n = 7, T = 8, E = 0,
    /// active from view 0 unless stated.
    #[test]
    fn view_sequences_match_the_standard_scenarios() {
        let on = LeaderSchedule {
            tenure: 8,
            rotate_on_timeout_from: Some(0),
        };
        let off = LeaderSchedule {
            rotate_on_timeout_from: None,
            ..on
        };
        let leader = |view| leader_index(view, 7, 8);
        let tc = |schedule: LeaderSchedule, view| schedule.view_after_tc(view, 0);
        // A QC for view q always moves to q + 1.
        let qc = |view: u64| view + 1;

        // S1: leader 1 dead.
        assert_eq!((qc(7), leader(qc(7))), (8, 1));
        assert_eq!((tc(on, 8), leader(tc(on, 8))), (16, 2));
        assert_eq!((qc(16), leader(17)), (17, 2));
        assert_eq!((qc(23), leader(24)), (24, 3));
        // S2: leaders 1 and 2 dead.
        assert_eq!((tc(on, 16), leader(24)), (24, 3));
        assert_eq!((qc(24), leader(25)), (25, 3));
        // S3: leader 1 dies mid-tenure.
        assert_eq!((qc(10), leader(11)), (11, 1));
        assert_eq!((tc(on, 11), leader(16)), (16, 2));
        // S4: timeout on the window's last view, as classic.
        assert_eq!(tc(on, 15), 16);
        assert_eq!(tc(off, 15), 16);
        // S5: wrap-around.
        assert_eq!(leader(48), 6);
        assert_eq!((tc(on, 48), leader(56)), (56, 0));
        // S6: switched off.
        for view in 8..15 {
            assert_eq!((tc(off, view), leader(view + 1)), (view + 1, 1));
        }
        assert_eq!((tc(off, 15), leader(16)), (16, 2));
        // S7: epoch clamp, E = 10.
        assert_eq!((on.view_after_tc(8, 10), leader(11)), (11, 1));
        assert_eq!((on.view_after_tc(11, 10), leader(16)), (16, 2));
        // S8: activation A = 100.
        let from_100 = LeaderSchedule {
            rotate_on_timeout_from: Some(100),
            ..on
        };
        assert_eq!((tc(from_100, 99), leader(100)), (100, 5));
        assert_eq!((tc(from_100, 100), leader(104)), (104, 6));
        // S9: tenure 1 is identical to off.
        let tenure_one = LeaderSchedule {
            tenure: 1,
            rotate_on_timeout_from: Some(0),
        };
        for view in 0..50 {
            assert_eq!(tc(tenure_one, view), view + 1);
            assert_eq!(leader_index(view + 1, 7, 1), ((view + 1) % 7) as u32);
        }
        // S10: a node persisted at view 16 after TC(8) needs only the view.
        assert_eq!(leader(16), 2);
        // S11: QC(8) and TC(8) race; the view alone names the leader.
        assert_eq!((qc(8), leader(9)), (9, 1));
        assert_eq!((tc(on, 8), leader(16)), (16, 2));
    }

    #[test]
    fn genesis_leader_rules_and_their_defaults() {
        let read = |hotstuff: serde_json::Value| LeaderSchedule::from_hotstuff_config(Some(&hotstuff));
        assert_eq!(
            LeaderSchedule::from_hotstuff_config(None).unwrap(),
            LeaderSchedule::default()
        );
        assert_eq!(read(serde_json::json!({})).unwrap(), LeaderSchedule::default());
        assert_eq!(
            read(serde_json::json!({"leaderTenure": 0})).unwrap(),
            LeaderSchedule::default(),
            "a zero tenure is 1"
        );
        assert_eq!(
            read(serde_json::json!({"leaderTenure": 8, "rotateOnTimeoutView": 100, "epochLength": 10}))
                .unwrap(),
            LeaderSchedule {
                tenure: 8,
                rotate_on_timeout_from: Some(100),
            }
        );
        assert_eq!(
            read(serde_json::json!({"rotateOnTimeoutView": 0})).unwrap(),
            LeaderSchedule {
                tenure: 1,
                rotate_on_timeout_from: Some(0),
            }
        );
        assert_eq!(
            read(serde_json::json!({"leaderTenure": "8"})),
            Err(LeaderScheduleError::Field("leaderTenure"))
        );
        assert_eq!(
            read(serde_json::json!({"rotateOnTimeoutView": -1})),
            Err(LeaderScheduleError::Field("rotateOnTimeoutView"))
        );
        assert_eq!(read(serde_json::json!(5)), Err(LeaderScheduleError::HotstuffShape));

        // Read from a whole genesis, as the node does.
        let genesis: alloy_genesis::Genesis = serde_json::from_value(serde_json::json!({
            "config": {"chainId": 94, "hotstuff": {"leaderTenure": 32}},
            "alloc": {}
        }))
        .unwrap();
        assert_eq!(
            LeaderSchedule::from_hotstuff_config(genesis.config.extra_fields.get("hotstuff"))
                .unwrap()
                .tenure,
            32
        );
        let plain: alloy_genesis::Genesis =
            serde_json::from_value(serde_json::json!({"config": {"chainId": 1}, "alloc": {}}))
                .unwrap();
        assert_eq!(
            LeaderSchedule::from_hotstuff_config(plain.config.extra_fields.get("hotstuff"))
                .unwrap(),
            LeaderSchedule::default()
        );
    }

    #[test]
    fn bench_env_overrides_the_genesis_and_says_so() {
        let genesis = serde_json::json!({"leaderTenure": 8, "rotateOnTimeoutView": 100});
        let resolve = |tenure: Option<&str>, rotate: Option<&str>| {
            LeaderSchedule::resolve(Some(&genesis), tenure, rotate).unwrap()
        };

        let plain = resolve(None, None);
        assert_eq!(plain.schedule, plain.genesis);
        assert!(plain.notes.is_empty());

        // The fleets' tenure override still works and is reported.
        let tenure = resolve(Some("32"), None);
        assert_eq!(tenure.schedule.tenure, 32);
        assert_eq!(tenure.genesis.tenure, 8);
        assert_eq!(tenure.notes.len(), 1);
        assert!(resolve(Some("8"), None).notes.is_empty(), "same value, no note");
        let ignored = resolve(Some("0"), None);
        assert_eq!(ignored.schedule.tenure, 8);
        assert_eq!(ignored.notes.len(), 1);

        for on in ["1", "true", "TRUE", "on"] {
            assert_eq!(resolve(None, Some(on)).schedule.rotate_on_timeout_from, Some(0));
        }
        for off in ["0", "false", "FALSE", "off"] {
            assert_eq!(resolve(None, Some(off)).schedule.rotate_on_timeout_from, None);
        }
        let unknown = resolve(None, Some("maybe"));
        assert_eq!(unknown.schedule.rotate_on_timeout_from, Some(100));
        assert_eq!(unknown.notes.len(), 1);

        // Without genesis fields: tenure 1 and no rotation, as before.
        let none = LeaderSchedule::resolve(None, None, None).unwrap();
        assert_eq!(none.schedule, LeaderSchedule::default());
        let fleet = LeaderSchedule::resolve(None, Some("1000000"), None).unwrap();
        assert_eq!(fleet.schedule.tenure, 1_000_000);
        assert_eq!(fleet.schedule.rotate_on_timeout_from, None);
    }

    fn profile1_genesis(tenure: u64) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../n42-chainspec/testdata/profile1/V1/genesis_8s.json");
        let mut genesis: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        genesis["config"]["hotstuff"]["leaderTenure"] = tenure.into();
        genesis
    }

    #[test]
    fn profile1_rotates_on_timeout_from_view_zero_and_refuses_overrides() {
        let (spec, config, _) =
            n42_chainspec::profile1::profile1_chain_spec(&profile1_genesis(8)).unwrap();
        let resolved = LeaderSchedule::resolve_profile1(&config, None, None).unwrap();
        assert_eq!(
            resolved.schedule,
            LeaderSchedule {
                tenure: 8,
                rotate_on_timeout_from: Some(0),
            }
        );
        assert!(resolved.schedule.rotates_at(0));
        assert_eq!(resolved.schedule.view_after_tc(3, config.epoch_length), 8);
        // The parsed genesis takes the same path.
        if std::env::var_os(LEADER_TENURE_ENV).is_none()
            && std::env::var_os(ROTATE_ON_TIMEOUT_ENV).is_none()
        {
            assert_eq!(
                LeaderSchedule::resolve_for_genesis(&spec.genesis)
                    .unwrap()
                    .schedule,
                resolved.schedule
            );
        }
        for (tenure, rotate) in [(Some("8"), None), (None, Some("0")), (None, Some("1"))] {
            assert!(matches!(
                LeaderSchedule::resolve_profile1(&config, tenure, rotate),
                Err(LeaderScheduleError::Profile1(_))
            ));
        }
        // A non-profile genesis keeps today's rules and overrides.
        let plain: alloy_genesis::Genesis = serde_json::from_value(serde_json::json!({
            "config": {"chainId": 94, "hotstuff": {"leaderTenure": 8}},
            "alloc": {}
        }))
        .unwrap();
        assert!(!n42_chainspec::profile1::genesis_declares_profile(&plain));
        let mut broken = spec.genesis.clone();
        broken.config.extra_fields.insert("n42Profile".into(), 2.into());
        assert!(matches!(
            LeaderSchedule::resolve_for_genesis(&broken),
            Err(LeaderScheduleError::Profile1(_))
        ));
    }
    use alloy_primitives::Address;
    use n42_chainspec::ValidatorInfo;
    use n42_primitives::BlsSecretKey;

    fn test_key(seed: u8) -> BlsSecretKey {
        BlsSecretKey::key_gen(&[seed; 32]).expect("deterministic test key should be valid")
    }

    /// Helper: create a ValidatorSet with `n` deterministic validators.
    fn make_validator_set(n: usize) -> ValidatorSet {
        let infos: Vec<_> = (0..n)
            .map(|i| {
                let sk = test_key(0x30 + i as u8);
                ValidatorInfo {
                    address: Address::with_last_byte(i as u8),
                    bls_public_key: sk.public_key(),
                    p2p_peer_id: None,
                }
            })
            .collect();
        let f = ((n as u32).saturating_sub(1)) / 3;
        ValidatorSet::new(&infos, f)
    }

    #[test]
    fn test_round_robin() {
        let vs = make_validator_set(4);

        let expected = [0u32, 1, 2, 3, 0, 1, 2, 3];
        for (view, &expected_leader) in expected.iter().enumerate() {
            let leader = LeaderSelector::leader_for_view(view as u64, &vs);
            assert_eq!(leader, expected_leader);
        }
    }

    #[test]
    fn test_is_leader() {
        let vs = make_validator_set(4);

        // View 0: validator 0 is leader
        assert!(LeaderSelector::is_leader(0, 0, &vs));
        assert!(!LeaderSelector::is_leader(1, 0, &vs));
        assert!(!LeaderSelector::is_leader(2, 0, &vs));
        assert!(!LeaderSelector::is_leader(3, 0, &vs));

        // View 1: validator 1 is leader
        assert!(!LeaderSelector::is_leader(0, 1, &vs));
        assert!(LeaderSelector::is_leader(1, 1, &vs));

        // View 5: validator 1 is leader (5 % 4 = 1)
        assert!(LeaderSelector::is_leader(1, 5, &vs));
        assert!(!LeaderSelector::is_leader(0, 5, &vs));
    }

    #[test]
    #[should_panic(expected = "leader_for_view called with empty validator set")]
    fn test_leader_for_view_empty_set() {
        let vs = ValidatorSet::new(&[], 0);
        let _leader = LeaderSelector::leader_for_view(5, &vs);
    }

    #[test]
    fn test_leader_for_view_single_validator() {
        let vs = make_validator_set(1);
        for view in 0..10u64 {
            let leader = LeaderSelector::leader_for_view(view, &vs);
            assert_eq!(leader, 0);
        }
    }
}
