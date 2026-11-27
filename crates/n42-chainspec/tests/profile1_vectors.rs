//! Profile-1 genesis and header vectors (V1, V8) from gov5, the byte
//! reference of the mixed production standard. The files under
//! `testdata/profile1` are verbatim copies of
//! `/data/blockchain/wr-logs/qs-scratch/profile1/vectors/{V1,V8}`.

use alloy_primitives::{B256, U256, hex};
use n42_chainspec::profile1::{
    self, Profile1Config, Profile1Header, Profile1HeaderError, config, profile1_chain_spec,
    profile1_forbidden_env,
};
use n42_primitives::{BlsPublicKey, BlsSecretKey};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/profile1")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn v1() -> Value {
    read_json(&testdata().join("V1/header_profile_v1.json"))
}

fn genesis_8s() -> Value {
    read_json(&testdata().join("V1/genesis_8s.json"))
}

fn bytes(value: &Value) -> Vec<u8> {
    hex::decode(value.as_str().unwrap()).unwrap()
}

fn hash(value: &Value) -> B256 {
    value.as_str().unwrap().parse().unwrap()
}

fn sorted(list: &[&str]) -> Vec<String> {
    let mut list: Vec<String> = list.iter().map(|key| (*key).to_owned()).collect();
    list.sort();
    list
}

fn json_list(value: &Value) -> Vec<String> {
    let mut list: Vec<String> = value
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_owned())
        .collect();
    list.sort();
    list
}

#[test]
fn closed_key_lists_equal_v8_allowed_keys() {
    let allowed = read_json(&testdata().join("V8/allowed-keys.json"));
    let pairs: [(&str, &[&str], &[&str]); 4] = [
        (
            "config",
            config::CONFIG_MANDATORY_KEYS,
            config::CONFIG_OPTIONAL_KEYS,
        ),
        ("config.hotstuff", config::HOTSTUFF_KEYS, &[]),
        (
            "config.hotstuff.committeePool",
            config::COMMITTEE_POOL_KEYS,
            &[],
        ),
        ("config.hotstuff.validators[]", config::VALIDATOR_KEYS, &[]),
    ];
    assert_eq!(allowed.as_object().unwrap().len(), pairs.len());
    for (path, mandatory, optional) in pairs {
        assert_eq!(json_list(&allowed[path]["mandatory"]), sorted(mandatory), "{path}");
        assert_eq!(json_list(&allowed[path]["optional"]), sorted(optional), "{path}");
    }
}

#[test]
fn genesis_8s_loads_to_the_v1_genesis_header() {
    let vector = v1();
    let (spec, config, header) = profile1_chain_spec(&genesis_8s()).expect("V1 genesis loads");

    // Field by field first, so a mismatch names the field.
    let expected = Profile1Header::decode(&bytes(&vector["genesis"]["rlp"])).unwrap();
    assert_eq!(header.header.state_root, expected.header.state_root, "stateRoot (A5)");
    assert_eq!(header.header.ommers_hash, B256::ZERO, "ommersHash (B3)");
    assert_eq!(header.header.requests_hash, expected.header.requests_hash, "requestsHash (AM24)");
    assert_eq!(header.mobile_registry_root, expected.mobile_registry_root);
    assert_eq!(header, expected);

    assert_eq!(header.encode(), bytes(&vector["genesis"]["rlp"]));
    let genesis_hash = hash(&vector["genesisHash"]);
    assert_eq!(header.hash(), genesis_hash);
    assert_eq!(hash(&vector["genesis"]["hash"]), genesis_hash);
    assert_eq!(spec.genesis_hash(), genesis_hash);
    assert_eq!(spec.chain().id(), 1144);
    // AM24: EIP-7685's sha256("") in requestsHash gives this genesis hash.
    assert_eq!(
        header.header.requests_hash,
        Some(n42_chainspec::profile1::PROFILE1_REQUESTS_HASH)
    );
    assert_eq!(
        genesis_hash,
        "0x188e9d930cfd43317416b0f65e8f98240ae686407ba806b854e98428c3b490dc"
            .parse::<B256>()
            .unwrap()
    );

    // The cause of the earlier mismatch: reth's own genesis header of the
    // same file carries the Ethereum MPT root of the alloc (0xac68dc35…),
    // not the QMDB root of A5.
    let reth_header = reth_chainspec::ChainSpec::from(spec.genesis.clone()).genesis_header().clone();
    assert!(
        format!("{:x}", reth_header.state_root).starts_with("ac68dc35"),
        "MPT root {}",
        reth_header.state_root
    );
    assert_ne!(reth_header.state_root, header.header.state_root);

    // AM22 values of the 8-second genesis.
    assert_eq!(config.chain_id, 1144);
    assert_eq!(config.block_interval_ms, 8000);
    assert_eq!(config.base_timeout_ms, 24000);
    assert_eq!(config.max_timeout_ms, 96000);
    assert_eq!(config.leader_tenure, 1);
    assert_eq!(config.epoch_length, 10800);
    assert_eq!(config.gas_limit, 480_000_000);
    assert_eq!(config.deferred_execution_depth, 1);
    assert_eq!(config.max_block_bytes, 64 << 20);
    assert_eq!(config.dev_block_reward, U256::from(1_000_000_000_000_000_000_u64));
    assert_eq!(config.validators.len(), 4);
    assert_eq!(config.fault_tolerance(), 1);
    for (validator, expected) in config.validators.iter().zip(vector["validators"].as_array().unwrap()) {
        assert_eq!(validator.address, expected["address"].as_str().unwrap().parse::<alloy_primitives::Address>().unwrap());
        assert_eq!(validator.bls_key.to_bytes().to_vec(), bytes(&expected["blsPublicKey"]));
    }
    let consensus = config.consensus_config();
    consensus.validate().unwrap();
    assert_eq!(consensus.slot_time_ms, 8000);
    assert_eq!(consensus.epoch_length, 10800);
    assert_eq!(consensus.quorum_size(), 3);
}

#[test]
fn the_parsed_genesis_reads_back_the_same_config() {
    let (spec, raw_config, _) = profile1_chain_spec(&genesis_8s()).unwrap();
    assert!(profile1::genesis_declares_profile(&spec.genesis));
    let (profile, config) = profile1::resolve_genesis_profile(&spec.genesis).unwrap().unwrap();
    assert_eq!(profile, profile1::N42Profile::Profile1);
    assert_eq!(config.leader_tenure, raw_config.leader_tenure);
    assert_eq!(config.epoch_length, raw_config.epoch_length);
    assert_eq!(config.committee_pool, raw_config.committee_pool);
}

/// The base of the V8 refused cases: gov5's provisional standard genesis.
/// `committee_pool_enabled` differs from it only by `committeePool.enabled`.
fn v8_base_genesis() -> Value {
    let mut case = read_json(&testdata().join("V8/refused/committee_pool_enabled.json"));
    let genesis = case["genesis"].take();
    let mut genesis = genesis;
    genesis["config"]["hotstuff"]["committeePool"]
        .as_object_mut()
        .unwrap()
        .remove("enabled")
        .expect("the case sets enabled");
    genesis
}

#[test]
fn the_v8_base_genesis_loads() {
    let (_, config, _) = profile1_chain_spec(&v8_base_genesis()).expect("V8 base genesis loads");
    assert_eq!(config.leader_tenure, 8);
    assert_eq!(config.epoch_length, 200);
}

#[test]
fn every_v8_refused_genesis_is_refused() {
    let mut cases: Vec<PathBuf> = std::fs::read_dir(testdata().join("V8/refused"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    cases.sort();
    assert_eq!(cases.len(), 14, "V8 has {} refused cases", cases.len());
    for path in cases {
        let case = read_json(&path);
        let name = case["case"].as_str().unwrap();
        match profile1_chain_spec(&case["genesis"]) {
            Ok(_) => panic!("V8 case {name} ({}) was accepted", case["expectedError"]),
            Err(error) => eprintln!("{name}: {error}  [gov5: {}]", case["expectedError"]),
        }
    }
}

#[test]
fn dev_block_reward_is_a_decimal_string_of_whole_gwei() {
    let parse = |value: Value| config::parse_dev_block_reward(&value);
    assert_eq!(parse(Value::from("0")).unwrap(), U256::ZERO);
    assert_eq!(
        parse(Value::from("18446744073709551615000000000")).unwrap(),
        U256::from(u64::MAX) * U256::from(1_000_000_000_u64)
    );
    assert!(parse(Value::from(1_000_000_000_000_000_000_u64)).is_err(), "JSON number");
    assert!(parse(Value::from("0x0de0b6b3a7640000")).is_err(), "hex");
    assert!(parse(Value::from("")).is_err());
    assert!(parse(Value::from("-1000000000")).is_err());
    assert!(parse(Value::from("1000000001")).is_err(), "not whole gwei");
    assert!(parse(Value::from("18446744073709551616000000000")).is_err(), "> u64::MAX gwei");
    assert!(
        parse(Value::from("1".repeat(90))).is_err(),
        "larger than U256"
    );
}

#[test]
fn profile_value_and_key_shape_are_checked_on_the_v1_genesis() {
    let refuse = |mutate: &dyn Fn(&mut Value)| {
        let mut genesis = genesis_8s();
        mutate(&mut genesis);
        profile1_chain_spec(&genesis).unwrap_err().to_string()
    };
    assert!(refuse(&|g| g["config"]["n42Profile"] = Value::from(1.0)).contains("n42Profile"));
    assert!(refuse(&|g| g["config"]["hotstuff"]["leaderTenure"] = Value::from("1")).contains("leaderTenure"));
    assert!(refuse(&|g| g["config"]["hotstuff"]["epochLength"] = Value::from(0)).contains("epochLength"));
    assert!(refuse(&|g| g["config"]["stateScheme"] = Value::from("mpt")).contains("stateScheme"));
    assert!(refuse(&|g| g["config"]["consensus"] = Value::from("clique")).contains("consensus"));
    assert!(refuse(&|g| g["config"]["terminalTotalDifficultyPassed"] = Value::from(false)).contains("terminalTotalDifficultyPassed"));
    assert!(refuse(&|g| g["mixHash"] = Value::from(format!("{:#x}", B256::repeat_byte(1)))).contains("mixHash"));
    assert!(refuse(&|g| {
        let validators = g["config"]["hotstuff"]["validators"].as_array_mut().unwrap();
        validators[3] = validators[0].clone();
    })
    .contains("duplicate"));
    assert!(refuse(&|g| g["config"]["hotstuff"]["validators"][0]["blsKey"] = Value::from("0x01")).contains("blsKey"));
}

#[test]
fn forbidden_environment_names() {
    let names = [
        "N42_LEADER_TENURE",
        "N42_HOTSTUFF_ROTATE_ON_TIMEOUT",
        "N42_BASE_TIMEOUT_MS",
        "N42_MAX_TIMEOUT_MS",
        "N42_FRAME_BLOCKS",
        "N42_FRAME_REF_BLOCKS",
        "N42_GOV5_LEGACY_SIGNING",
        "N42_GOV5_PRAGUE_TIME",
        "N42_CHAIN_ID",
        "N42_INTEROP_GENESIS_HASH",
        "N42_REWARD_EPOCH_BLOCKS",
        // Permitted bench and local knobs (J4).
        "N42_DATA_DIR",
        "N42_PARALLEL_IMPORT",
        "N42_INGEST_PORT",
        "N42_VALIDATOR_KEY",
        "RUST_LOG",
    ];
    let found = profile1_forbidden_env(names);
    assert_eq!(found.len(), 11, "{found:?}");
    assert!(!found.iter().any(|name| name == "N42_DATA_DIR" || name == "N42_VALIDATOR_KEY"));
}

#[test]
fn v1_headers_hash_seal_hash_and_seal() {
    let vector = v1();
    let keys: Vec<(BlsSecretKey, BlsPublicKey)> = vector["validators"]
        .as_array()
        .unwrap()
        .iter()
        .map(|validator| {
            let secret: [u8; 32] = bytes(&validator["blsSecretKey"]).try_into().unwrap();
            let public: [u8; 48] = bytes(&validator["blsPublicKey"]).try_into().unwrap();
            let secret = BlsSecretKey::from_bytes(&secret).unwrap();
            let public = BlsPublicKey::from_bytes(&public).unwrap();
            assert_eq!(secret.public_key().to_bytes(), public.to_bytes());
            (secret, public)
        })
        .collect();
    let mut parent = hash(&vector["genesisHash"]);
    for block in vector["headers"].as_array().unwrap() {
        let name = block["name"].as_str().unwrap();
        let raw = bytes(&block["rlp"]);
        let header = Profile1Header::decode(&raw).unwrap();
        assert_eq!(header.encode(), raw, "{name} re-encodes byte for byte");
        assert_eq!(header.hash(), hash(&block["hash"]), "{name} hash");
        assert_eq!(header.header.parent_hash, parent, "{name} parent");
        assert_eq!(header.seal_hash(), hash(&block["sealHash"]), "{name} sealHash (AM4)");
        let extra = header.extra().unwrap();
        assert_eq!(extra.view, block["view"].as_u64().unwrap());
        assert_eq!(extra.seal.to_vec(), bytes(&block["seal"]), "{name} seal bytes");
        let signer = block["signerIndex"].as_u64().unwrap() as usize;
        header.verify_seal(&keys[signer].1).expect("seal verifies under the signer");
        let other = (signer + 1) % keys.len();
        assert!(header.verify_seal(&keys[other].1).is_err(), "{name} seal is not another key's");
        match block.get("qc") {
            Some(qc) => assert_eq!(extra.qc.to_vec(), bytes(&qc["ssz"]), "{name} QC bytes"),
            None => assert!(extra.qc.is_empty(), "{name} carries no QC"),
        }
        // Re-sealing with the signer's key reproduces the seal: BLS is
        // deterministic, so this pins the DST and the signed bytes.
        let mut resealed = header.clone();
        resealed.seal(&keys[signer].0).unwrap();
        assert_eq!(resealed, header, "{name} reseal");
        assert_eq!(header.header.slot_number, Some(header.header.number), "{name} B10");
        parent = header.hash();
    }
}

#[test]
fn layout_refusals_of_v1_are_decode_errors() {
    let vector = v1();
    for refusal in vector["refusals"].as_array().unwrap() {
        let name = refusal["name"].as_str().unwrap();
        let raw = bytes(&refusal["rlp"]);
        let decoded = Profile1Header::decode(&raw);
        match name {
            // rc4 V1: "requestsHash absent" (B8) drops element 21's value.
            "withdrawalsRoot absent" | "requestsHash absent" => assert!(
                matches!(decoded, Err(Profile1HeaderError::Missing { .. }) | Err(Profile1HeaderError::Field { .. })),
                "{name}: {decoded:?}"
            ),
            "element 22 present" => {
                assert_eq!(decoded, Err(Profile1HeaderError::Element22), "{name}")
            }
            _ => {
                let header = decoded.unwrap_or_else(|error| panic!("{name}: {error}"));
                assert_eq!(header.encode(), raw, "{name} re-encodes");
                assert_eq!(header.hash(), hash(&refusal["hash"]), "{name} hash");
            }
        }
    }
}

#[test]
fn a_profile1_genesis_with_alt_sig_tx_or_frame_blocks_is_refused() {
    let (spec, _, _) = profile1_chain_spec(&genesis_8s()).unwrap();
    for key in ["altSigTx", "frameBlocks"] {
        let mut genesis = spec.genesis.clone();
        genesis.config.extra_fields.insert(key.to_owned(), serde_json::json!(true));
        let error = profile1::resolve_genesis_profile(&genesis).unwrap_err();
        assert!(error.to_string().contains(key), "{key}: {error}");
        // An explicit false is the default and stays valid.
        genesis.config.extra_fields.insert(key.to_owned(), serde_json::json!(false));
        assert!(profile1::resolve_genesis_profile(&genesis).unwrap().is_some(), "{key}=false");
    }
}

#[test]
fn a_profile1_config_from_genesis_requires_the_profile_field() {
    let (spec, _, _) = profile1_chain_spec(&genesis_8s()).unwrap();
    let mut genesis = spec.genesis.clone();
    genesis.config.extra_fields.remove("n42Profile");
    assert!(Profile1Config::from_genesis(&genesis).is_err());
    assert!(!profile1::genesis_declares_profile(&genesis));
    assert!(profile1::resolve_genesis_profile(&genesis).unwrap().is_none());
}

/// AM2: on Profile 1 `hotstuff.maxBlockBytes` is the only block size bound,
/// at every fork; other chains keep the 8 MiB cap from Osaka on.
#[test]
fn profile1_block_size_bound_is_max_block_bytes() {
    use n42_chainspec::profile1::block_rlp_size_limit;
    const ETH: usize = 8 * 1024 * 1024;
    let (_, config, _) = profile1_chain_spec(&genesis_8s()).unwrap();
    assert_eq!(config.max_block_bytes, 64 * 1024 * 1024);
    let max = Some(config.max_block_bytes);
    assert_eq!(block_rlp_size_limit(max, true, ETH), Some(64 * 1024 * 1024));
    assert_eq!(block_rlp_size_limit(max, false, ETH), Some(64 * 1024 * 1024));
    assert_eq!(block_rlp_size_limit(None, true, ETH), Some(ETH));
    assert_eq!(block_rlp_size_limit(None, false, ETH), None);
    // No profile installed in this process: today's rule.
    assert_eq!(n42_chainspec::profile1::installed_block_rlp_size_limit(true, ETH), Some(ETH));
}
