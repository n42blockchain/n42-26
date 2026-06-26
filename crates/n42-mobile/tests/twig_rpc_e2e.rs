use std::{
    error::Error,
    io::{Read, Write},
    net::TcpStream,
    str::FromStr,
    thread,
    time::Duration,
};

use alloy_primitives::B256;
use n42_mobile::state_proof::{ShardedBmtProof, verify_state_proof};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_RPC_URL: &str = "http://127.0.0.1:18000";
const GENESIS_ACCOUNT: &str = "0xe3778939cdCa78b70fc36dE06B0E862333D6D8dc";
const MISSING_ACCOUNT: &str = "0x0000000000000000000000000000000000000042";
const ACCOUNT_VALUE_LEN: usize = 72;
const EMPTY_CODE_HASH: B256 = B256::new([
    0xc5, 0xd2, 0x46, 0x01, 0x86, 0xf7, 0x23, 0x3c, 0x92, 0x7e, 0x7d, 0xb2, 0xdc, 0xc7, 0x03, 0xc0,
    0xe5, 0x00, 0xb6, 0x53, 0xca, 0x82, 0x27, 0x3b, 0x7b, 0xfa, 0xd8, 0x04, 0x5d, 0x85, 0xa4, 0x70,
]);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmtRootResponse {
    version: u64,
    root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JmtProofResponse {
    value: Option<String>,
    proof_hex: String,
    root: String,
}

#[test]
#[ignore = "requires a running local n42 node with N42_TWIG=1 and HTTP RPC enabled"]
fn twig_rpc_proof_roundtrip() -> Result<(), Box<dyn Error>> {
    let rpc_url = std::env::var("N42_TWIG_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.to_string());

    let first_version = jmt_version(&rpc_url)?;
    let first_root = jmt_root(&rpc_url)?;
    let _first_root_hash = B256::from_str(&first_root.root)?;
    assert!(
        first_root.version >= first_version,
        "n42_jmtRoot.version should not lag n42_jmtVersion"
    );

    let later_root = wait_for_new_jmt_version(&rpc_url, first_root.version)?;
    assert!(
        later_root.version > first_root.version,
        "Twig version should advance as local blocks commit"
    );

    let inclusion = jmt_proof(&rpc_url, GENESIS_ACCOUNT)?;
    assert!(
        inclusion.value.is_some(),
        "genesis account proof should be inclusion"
    );
    assert_eq!(
        inclusion_code_hash(&inclusion)?,
        EMPTY_CODE_HASH,
        "genesis EOA leaf must use reth/revm empty-code hash, not zero"
    );
    verify_rpc_proof("inclusion", &inclusion)?;

    let exclusion = jmt_proof(&rpc_url, MISSING_ACCOUNT)?;
    assert!(
        exclusion.value.is_none(),
        "missing account proof should be exclusion"
    );
    verify_rpc_proof("exclusion", &exclusion)?;

    Ok(())
}

fn inclusion_code_hash(response: &JmtProofResponse) -> Result<B256, Box<dyn Error>> {
    let value_hex = response
        .value
        .as_deref()
        .ok_or("inclusion proof response missing value")?;
    let value = hex::decode(value_hex.trim_start_matches("0x"))?;
    if value.len() != ACCOUNT_VALUE_LEN {
        return Err(format!("unexpected account leaf value length: {}", value.len()).into());
    }
    Ok(B256::from_slice(&value[40..72]))
}

fn wait_for_new_jmt_version(
    rpc_url: &str,
    previous_version: u64,
) -> Result<JmtRootResponse, Box<dyn Error>> {
    let mut last = jmt_root(rpc_url)?;
    for _ in 0..10 {
        if last.version > previous_version {
            return Ok(last);
        }
        thread::sleep(Duration::from_secs(2));
        last = jmt_root(rpc_url)?;
    }
    Ok(last)
}

fn verify_rpc_proof(label: &str, response: &JmtProofResponse) -> Result<(), Box<dyn Error>> {
    let root = B256::from_str(&response.root)?;
    let proof_bytes = hex::decode(response.proof_hex.trim_start_matches("0x"))?;
    let proof: ShardedBmtProof = bincode::deserialize(&proof_bytes)?;

    verify_state_proof(&proof, root)?;

    let mut bad_shard_root = proof.clone();
    bad_shard_root.shard_root[0] ^= 0xFF;
    assert!(
        verify_state_proof(&bad_shard_root, root).is_err(),
        "{label} proof with tampered shard_root must fail"
    );

    let mut bad_value = proof.clone();
    match bad_value.value.as_mut() {
        Some(value) if !value.is_empty() => value[0] ^= 0xFF,
        _ => bad_value.value = Some(vec![0x42]),
    }
    assert!(
        verify_state_proof(&bad_value, root).is_err(),
        "{label} proof with tampered value must fail"
    );

    if let Some(first_path_hash) = proof.shard_path.first().copied().map(|mut hash| {
        hash[0] ^= 0xFF;
        hash
    }) {
        let mut bad_path = proof.clone();
        bad_path.shard_path[0] = first_path_hash;
        assert!(
            verify_state_proof(&bad_path, root).is_err(),
            "{label} proof with tampered shard_path must fail"
        );
    }

    eprintln!("{label} proof size: {} bytes", proof_bytes.len());
    assert!(
        (512..=4096).contains(&proof_bytes.len()),
        "{label} proof size should stay in the expected compact range"
    );

    Ok(())
}

fn jmt_version(rpc_url: &str) -> Result<u64, Box<dyn Error>> {
    let result = rpc_call(rpc_url, "n42_jmtVersion", json!([]))?;
    result
        .as_u64()
        .ok_or_else(|| format!("invalid n42_jmtVersion result: {result:?}").into())
}

fn jmt_root(rpc_url: &str) -> Result<JmtRootResponse, Box<dyn Error>> {
    let result = rpc_call(rpc_url, "n42_jmtRoot", json!([]))?;
    Ok(serde_json::from_value(result)?)
}

fn jmt_proof(rpc_url: &str, address: &str) -> Result<JmtProofResponse, Box<dyn Error>> {
    let result = rpc_call(rpc_url, "n42_jmtProof", json!([address, null]))?;
    Ok(serde_json::from_value(result)?)
}

fn rpc_call(rpc_url: &str, method: &str, params: Value) -> Result<Value, Box<dyn Error>> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let response_body = post_json(rpc_url, &payload.to_string())?;
    let response: Value = serde_json::from_str(&response_body)?;

    if let Some(error) = response.get("error") {
        return Err(format!("JSON-RPC {method} error: {error}").into());
    }

    response
        .get("result")
        .cloned()
        .ok_or_else(|| format!("JSON-RPC {method} missing result: {response}").into())
}

fn post_json(rpc_url: &str, body: &str) -> Result<String, Box<dyn Error>> {
    let target = rpc_url
        .strip_prefix("http://")
        .ok_or("only http:// RPC URLs are supported")?;
    let (host_port, path) = target.split_once('/').unwrap_or((target, ""));
    let path = format!("/{path}");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>()?),
        None => (host_port, 80),
    };

    let mut stream = TcpStream::connect((host, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    let status = response
        .lines()
        .next()
        .ok_or("empty HTTP response from RPC")?;
    if !status.contains(" 200 ") {
        return Err(format!("unexpected HTTP status: {status}").into());
    }

    let (_, response_body) = response
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP response from RPC")?;
    Ok(response_body.to_string())
}
