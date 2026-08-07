use serde_json::{Value, json};
use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;

fn post_json(url: &str, body: &Value) -> Value {
    let (host, port) = parse_http_url(url);
    let body = body.to_string();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let mut stream = TcpStream::connect((host.as_str(), port)).expect("connect rpc");
    stream
        .write_all(request.as_bytes())
        .expect("write rpc request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read rpc response");
    let (_, json_body) = response
        .split_once("\r\n\r\n")
        .expect("http response has headers");
    serde_json::from_str(json_body).expect("valid rpc json")
}

fn parse_http_url(url: &str) -> (String, u16) {
    let without_scheme = url
        .strip_prefix("http://")
        .expect("N42_TWIG_RPC_URL must be http://host:port");
    let host_port = without_scheme.trim_end_matches('/');
    let (host, port) = host_port.rsplit_once(':').expect("url includes port");
    let port = port.parse().expect("valid rpc port");
    (host.to_string(), port)
}

fn decode_fixed<const N: usize>(hex_value: &str) -> [u8; N] {
    let value = hex_value.strip_prefix("0x").unwrap_or(hex_value);
    let bytes = hex::decode(value).expect("valid hex");
    bytes.try_into().unwrap_or_else(|bytes: Vec<u8>| {
        panic!("expected {N} bytes, got {}", bytes.len());
    })
}

#[test]
#[ignore = "requires a live n42 node with N42_TWIG=1"]
fn verifies_live_rpc_twig_account_proof_with_ffi() {
    let rpc_url =
        env::var("N42_TWIG_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:18000".to_string());
    let account = env::var("N42_TWIG_ACCOUNT")
        .expect("N42_TWIG_ACCOUNT must be set to an account present in Twig");

    let response = post_json(
        &rpc_url,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "n42_twigProof",
            "params": [account, null],
        }),
    );
    if let Some(error) = response.get("error") {
        panic!("n42_twigProof failed: {error}");
    }

    let result = response.get("result").expect("proof result");
    let proof_hex = result
        .get("proofHex")
        .and_then(Value::as_str)
        .expect("proofHex");
    let root = result.get("root").and_then(Value::as_str).expect("root");

    let proof = hex::decode(proof_hex).expect("valid proof hex");
    let root = decode_fixed::<32>(root);
    let address = decode_fixed::<20>(&account);

    let code = unsafe {
        n42_mobile_ffi::n42_verify_twig_account_proof(
            proof.as_ptr(),
            proof.len(),
            root.as_ptr(),
            address.as_ptr(),
        )
    };
    assert_eq!(code, 0, "live Twig account proof must verify");
}
