//! Wire tests for the v1.1 sync protocols, including gov5's golden vectors
//! (`tests/fixtures/sync-v11`): every case is replayed against a server built
//! over the vector's chain and every recorded response is read and judged by
//! the client.

use super::*;
use futures::executor::block_on;
use futures::io::Cursor;
use request_response::Codec;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

const BODIES_VECTORS: &str = include_str!("../../tests/fixtures/sync-v11/bodies_by_range_2.json");
const COMMIT_QC_VECTORS: &str = include_str!("../../tests/fixtures/sync-v11/commit_qc_1.json");
const BODIES_SHA256: &str = "27d5edfc766de64d87c1dc0a5d16ce34624ca91407fb3f2595a6f54ad60217d6";
const COMMIT_QC_SHA256: &str = "bf1ab9a872868730a809b13b2a19c6d8c5a56d063dc2d4a094a759b8108d1fe9";

/// The vector chain's committed head (`chain` field of the JSON files).
const VECTOR_HEAD: u64 = 10;

fn bytes(value: &Value) -> Vec<u8> {
    hex::decode(value.as_str().unwrap().trim_start_matches("0x")).unwrap()
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn fork_digest(vectors: &Value) -> [u8; 4] {
    bytes(&vectors["genesis_hash"])[..4].try_into().unwrap()
}

fn protocol(id: &'static str) -> StreamProtocol {
    StreamProtocol::new(id)
}

/// The vector chain as a server would hold it: bodies 7..=10 (every block the
/// vectors carry) and headers 6..=10, committed head 10. The server joined by
/// state sync at pivot 6, so nothing below 6 is held.
fn vector_reader() -> Gov5CanonicalBlockReader {
    let vectors = json(BODIES_VECTORS);
    let mut blocks = BTreeMap::new();
    let mut headers = BTreeMap::new();
    for case in vectors["cases"].as_array().unwrap() {
        let headers_only = case["request_fields"]["flags"].as_u64().unwrap() & 1 == 1;
        for chunk in case["response_chunks"].as_array().unwrap() {
            if chunk["code"] != "0x00" {
                continue;
            }
            let payload = bytes(&chunk["payload"]);
            if headers_only {
                let header = decode_sync_header(&payload).unwrap();
                headers.insert(header.number, payload);
            } else {
                let block = crate::gov5_block::decode_wire_block_rlp(&payload).unwrap();
                headers.insert(
                    block.header.number,
                    header_rlp_of_block(&payload).unwrap().to_vec(),
                );
                blocks.insert(block.header.number, payload);
            }
        }
    }
    assert_eq!(
        blocks.keys().copied().collect::<Vec<_>>(),
        vec![7, 8, 9, 10]
    );
    assert_eq!(
        headers.keys().copied().collect::<Vec<_>>(),
        vec![6, 7, 8, 9, 10]
    );
    let contiguous = |map: Arc<BTreeMap<u64, Vec<u8>>>| {
        move |numbers: std::ops::RangeInclusive<u64>| {
            Ok(numbers
                .map_while(|number| map.get(&number).cloned())
                .collect::<Vec<_>>())
        }
    };
    Gov5CanonicalBlockReader::new_ranged(|| Ok(VECTOR_HEAD), contiguous(Arc::new(blocks)))
        .with_header_reader(contiguous(Arc::new(headers)))
}

fn read_reply(wire: &[u8]) -> RangeV2Reply {
    block_on(read_range_v2_reply(
        &mut Cursor::new(wire),
        RANGE_V2_CLIENT_BUDGET,
    ))
}

fn write_range_response(response: RangeV2Response) -> Vec<u8> {
    let mut wire = Cursor::new(Vec::new());
    block_on(Gov5BodiesByRangeV2Codec.write_response(
        &protocol(GOV5_BODIES_BY_RANGE_V2_PROTOCOL),
        &mut wire,
        response,
    ))
    .unwrap();
    wire.into_inner()
}

/// Asserts that `reply` holds exactly the vector's chunks: codes, context
/// bytes, payloads, the END trailer and the error code.
fn assert_reply_matches(case: &Value, reply: &RangeV2Reply) {
    let name = case["name"].as_str().unwrap();
    assert_eq!(reply.violation, None, "{name}");
    let mut chunks = reply.chunks.iter();
    for expected in case["response_chunks"].as_array().unwrap() {
        let code = u8::from_str_radix(
            expected["code"].as_str().unwrap().trim_start_matches("0x"),
            16,
        )
        .unwrap();
        match code {
            result_code::SUCCESS => {
                let chunk = chunks
                    .next()
                    .unwrap_or_else(|| panic!("{name}: missing chunk"));
                assert_eq!(
                    chunk.fork_digest.as_slice(),
                    bytes(&expected["context_bytes"]),
                    "{name}"
                );
                assert_eq!(chunk.rlp, bytes(&expected["payload"]), "{name}");
            }
            result_code::END => {
                let end = reply.end.unwrap_or_else(|| panic!("{name}: missing END"));
                assert_eq!(end.encode().to_vec(), bytes(&expected["payload"]), "{name}");
                assert_eq!(
                    end.next,
                    expected["end"]["next"].as_u64().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    end.served_bytes,
                    expected["end"]["served_bytes"].as_u64().unwrap(),
                    "{name}"
                );
                assert_eq!(
                    u64::from(end.reason),
                    expected["end"]["reason"].as_u64().unwrap()
                );
            }
            code => {
                let (got, message) = reply.error.clone().expect("error chunk");
                assert_eq!(got, code, "{name}");
                // Messages need not match byte for byte; ours is gov5's
                // without its package prefix.
                let theirs = expected["error_message"].as_str().unwrap();
                assert!(
                    theirs.ends_with(&message),
                    "{name}: {message:?} vs {theirs:?}"
                );
            }
        }
    }
    assert!(chunks.next().is_none(), "{name}: extra chunks");
}

#[test]
fn vector_fixtures_are_the_published_files() {
    assert_eq!(hex::encode(Sha256::digest(BODIES_VECTORS)), BODIES_SHA256);
    assert_eq!(
        hex::encode(Sha256::digest(COMMIT_QC_VECTORS)),
        COMMIT_QC_SHA256
    );
}

#[test]
fn bodies_by_range_2_requests_match_the_vectors() {
    let vectors = json(BODIES_VECTORS);
    assert_eq!(vectors["protocol"], GOV5_BODIES_BY_RANGE_V2_PROTOCOL);
    for case in vectors["cases"].as_array().unwrap() {
        let fields = &case["request_fields"];
        let request = RangeV2Request {
            start: fields["start"].as_u64().unwrap(),
            count: fields["count"].as_u64().unwrap(),
            max_bytes: fields["max_bytes"].as_u64().unwrap(),
            flags: fields["flags"].as_u64().unwrap(),
        };
        assert_eq!(request.encode().to_vec(), bytes(&case["request_payload"]));
        // gov5's framed request reads back as the same request.
        let wire = bytes(&case["request_wire"]);
        let read = block_on(Gov5BodiesByRangeV2Codec.read_request(
            &protocol(GOV5_BODIES_BY_RANGE_V2_PROTOCOL),
            &mut Cursor::new(wire.as_slice()),
        ))
        .unwrap();
        assert_eq!(read, request);
        // Ours does too, whenever the request is sendable.
        if request.validate().is_ok() {
            let mut ours = Cursor::new(Vec::new());
            block_on(Gov5BodiesByRangeV2Codec.write_request(
                &protocol(GOV5_BODIES_BY_RANGE_V2_PROTOCOL),
                &mut ours,
                request,
            ))
            .unwrap();
            let ours = ours.into_inner();
            let reread = block_on(Gov5BodiesByRangeV2Codec.read_request(
                &protocol(GOV5_BODIES_BY_RANGE_V2_PROTOCOL),
                &mut Cursor::new(ours.as_slice()),
            ))
            .unwrap();
            assert_eq!(reread, request);
        }
    }
}

#[test]
fn bodies_by_range_2_server_reproduces_every_vector() {
    let vectors = json(BODIES_VECTORS);
    let digest = fork_digest(&vectors);
    let reader = vector_reader();
    let mut names = Vec::new();
    for case in vectors["cases"].as_array().unwrap() {
        let request = RangeV2Request::decode(&bytes(&case["request_payload"])).unwrap();
        let wire = write_range_response(range_v2_response(request, digest, reader.clone()));
        assert_reply_matches(case, &read_reply(&wire));
        names.push(case["name"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        names,
        [
            "complete",
            "headers_only",
            "byte_budget",
            "canonical_end",
            "empty_range",
            "not_available",
            "invalid_count_zero"
        ]
    );
}

#[test]
fn bodies_by_range_2_client_accepts_every_vector_reply() {
    let vectors = json(BODIES_VECTORS);
    let digest = fork_digest(&vectors);
    for case in vectors["cases"].as_array().unwrap() {
        let request = RangeV2Request::decode(&bytes(&case["request_payload"])).unwrap();
        let reply = read_reply(&bytes(&case["response_wire"]));
        assert_reply_matches(case, &reply);
        let payloads: Vec<Vec<u8>> = case["response_chunks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|chunk| chunk["code"] == "0x00")
            .map(|chunk| bytes(&chunk["payload"]))
            .collect();
        let verdict = check_range_v2_reply(&request, reply, digest);
        if request.validate().is_err() {
            assert!(
                matches!(
                    verdict,
                    RangeV2Verdict::Refused {
                        code: result_code::INVALID_REQUEST,
                        ..
                    }
                ),
                "{verdict:?}"
            );
        } else if payloads.is_empty() {
            // Zero blocks with END reason 3-6: neither a fault nor a back-off.
            let reason = reply_end_reason(case);
            assert!(reason >= end_reason::CANONICAL_END, "{}", case["name"]);
            assert_eq!(
                verdict,
                RangeV2Verdict::Unavailable {
                    reason,
                    retry_after: None
                },
                "{}",
                case["name"]
            );
        } else {
            assert_eq!(
                verdict,
                RangeV2Verdict::Payloads(payloads),
                "{}",
                case["name"]
            );
        }
    }
}

fn reply_end_reason(case: &Value) -> u8 {
    case["response_chunks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|chunk| chunk["code"] == "0x10")
        .map(|chunk| chunk["end"]["reason"].as_u64().unwrap() as u8)
        .unwrap()
}

#[test]
fn bodies_by_range_2_client_does_not_penalise_rate_limiting() {
    let (request, _, digest) = complete_case();
    let refused = |message: &str| {
        check_range_v2_reply(
            &request,
            RangeV2Reply {
                error: Some((result_code::RATE_LIMITED, message.to_owned())),
                ..RangeV2Reply::default()
            },
            digest,
        )
    };
    assert_eq!(
        refused("2500"),
        RangeV2Verdict::Unavailable {
            reason: end_reason::RATE_LIMITED,
            retry_after: Some(std::time::Duration::from_millis(2500)),
        }
    );
    // Unreadable: the default pause; huge: clamped.
    assert_eq!(
        refused("soon"),
        RangeV2Verdict::Unavailable {
            reason: end_reason::RATE_LIMITED,
            retry_after: Some(std::time::Duration::from_secs(1)),
        }
    );
    assert_eq!(
        refused("86400000"),
        RangeV2Verdict::Unavailable {
            reason: end_reason::RATE_LIMITED,
            retry_after: Some(std::time::Duration::from_secs(60)),
        }
    );
    // Any other refusal stays a refusal.
    assert!(matches!(
        check_range_v2_reply(
            &request,
            RangeV2Reply {
                error: Some((result_code::SERVER_ERROR, "x".to_owned())),
                ..RangeV2Reply::default()
            },
            digest,
        ),
        RangeV2Verdict::Refused { .. }
    ));
}

#[test]
fn bodies_by_range_2_client_faults_a_reply_over_its_budget() {
    let (_, reply, digest) = complete_case();
    let served: u64 = reply.chunks.iter().map(|c| c.rlp.len() as u64).sum();
    let first = reply.chunks[0].rlp.len() as u64;
    let request = |max_bytes: u64| RangeV2Request {
        start: 7,
        count: 4,
        max_bytes,
        flags: 0,
    };
    // Several blocks over the requested budget: a provable fault.
    let mut over = reply.clone();
    over.end.as_mut().unwrap().reason = end_reason::BYTE_BUDGET;
    assert_eq!(
        check_range_v2_reply(&request(served - 1), over, digest),
        RangeV2Verdict::Fault("reply exceeds the requested byte budget")
    );
    // A single first block may exceed it alone.
    let mut single = reply.clone();
    single.chunks.truncate(1);
    let end = single.end.as_mut().unwrap();
    end.next = 8;
    end.served_bytes = first;
    end.reason = end_reason::BYTE_BUDGET;
    assert!(matches!(
        check_range_v2_reply(&request(1), single, digest),
        RangeV2Verdict::Payloads(blocks) if blocks.len() == 1
    ));
    // The client reads no more than the 64 MiB it asks for.
    assert_eq!(RANGE_V2_CLIENT_BUDGET as u64, RANGE_V2_DEFAULT_BYTES);
}

/// The vector chain's block hashes by number (6..=10).
fn vector_hashes(reader: &Gov5CanonicalBlockReader) -> BTreeMap<u64, alloy_primitives::B256> {
    reader
        .header_rlps_by_range(6..=10)
        .unwrap()
        .iter()
        .map(|raw| {
            let header = decode_sync_header(raw).unwrap();
            (header.number, header.hash)
        })
        .collect()
}

/// A leader's storage head runs ahead of its committed tip: the served
/// head follows only the committed blocks, never moves down, and is not
/// moved by a hash that is not canonical.
#[test]
fn served_head_follows_the_committed_tip_not_the_storage_head() {
    let vectors = json(BODIES_VECTORS);
    let digest = fork_digest(&vectors);
    let hashes = vector_hashes(&vector_reader());
    let numbers: BTreeMap<_, _> = hashes.iter().map(|(n, h)| (*h, *n)).collect();
    let reader =
        vector_reader().with_committed_resolver(move |hash| Ok(numbers.get(&hash).copied()));
    // Nothing committed reported yet: nothing is served.
    assert!(reader.committed_head().is_err());
    reader.note_committed(hashes[&9]);
    assert_eq!(reader.best_block_number(), Ok(VECTOR_HEAD));
    assert_eq!(reader.committed_head(), Ok(9));

    // `/2` for 7..=10 stops at 9 with CANONICAL_END.
    let request = RangeV2Request {
        start: 7,
        count: 4,
        max_bytes: 0,
        flags: 0,
    };
    let reply = read_reply(&write_range_response(range_v2_response(
        request,
        digest,
        reader.clone(),
    )));
    assert_eq!(reply.chunks.len(), 3);
    let end = reply.end.unwrap();
    assert_eq!((end.next, end.reason), (10, end_reason::CANONICAL_END));
    // ...and above it serves nothing, without a fault.
    let above = RangeV2Request {
        start: 10,
        count: 1,
        ..request
    };
    let reply = read_reply(&write_range_response(range_v2_response(
        above,
        digest,
        reader.clone(),
    )));
    assert!(matches!(
        check_range_v2_reply(&above, reply, digest),
        RangeV2Verdict::Unavailable {
            reason: end_reason::CANONICAL_END,
            ..
        }
    ));

    // commit_qc: block 10 is held but not committed.
    let source = CommitQcSource {
        reader: reader.clone(),
        persisted_qc: None,
    };
    assert!(matches!(
        serve_commit_qc(
            &source,
            CommitQcRequest {
                number: 10,
                hash: alloy_primitives::B256::ZERO
            }
        ),
        CommitQcResponse::Error {
            code: result_code::RESOURCE_UNAVAILABLE,
            ..
        }
    ));

    // A lower or unknown hash never moves the head down.
    reader.note_committed(hashes[&8]);
    assert_eq!(reader.committed_head(), Ok(9));
    reader.note_committed(alloy_primitives::B256::repeat_byte(0xab));
    assert_eq!(reader.committed_head(), Ok(9));
    // The next commit moves it up; clones share the tip.
    reader.clone().note_committed(hashes[&10]);
    assert_eq!(reader.committed_head(), Ok(10));
}

fn complete_case() -> (RangeV2Request, RangeV2Reply, [u8; 4]) {
    let vectors = json(BODIES_VECTORS);
    let case = &vectors["cases"][0];
    assert_eq!(case["name"], "complete");
    (
        RangeV2Request::decode(&bytes(&case["request_payload"])).unwrap(),
        read_reply(&bytes(&case["response_wire"])),
        fork_digest(&vectors),
    )
}

#[test]
fn bodies_by_range_2_client_scores_an_inconsistent_end() {
    let (request, reply, digest) = complete_case();
    let fault = |mutate: &dyn Fn(&mut RangeV2Reply)| {
        let mut reply = reply.clone();
        mutate(&mut reply);
        check_range_v2_reply(&request, reply, digest)
    };
    assert_eq!(
        fault(&|reply| reply.end.as_mut().unwrap().next = 9),
        RangeV2Verdict::Fault("END next does not match the chunks")
    );
    assert_eq!(
        fault(&|reply| reply.end.as_mut().unwrap().served_bytes += 1),
        RangeV2Verdict::Fault("END served_bytes does not match the chunks")
    );
    assert_eq!(
        fault(&|reply| reply.end.as_mut().unwrap().reason = end_reason::BYTE_BUDGET),
        RangeV2Verdict::Fault("END reason contradicts next")
    );
    assert_eq!(
        fault(&|reply| reply.end.as_mut().unwrap().reason = 9),
        RangeV2Verdict::Fault("unknown END reason")
    );
    assert_eq!(
        fault(&|reply| {
            let extra = reply.chunks[0].clone();
            reply.chunks.push(extra);
        }),
        RangeV2Verdict::Fault("more chunks than requested")
    );
    assert_eq!(
        fault(&|reply| reply.chunks[1].fork_digest = [0; 4]),
        RangeV2Verdict::Fault("foreign context bytes")
    );
    // A missing END is a soft fault, an empty BYTE_BUDGET reply a hard one.
    assert_eq!(
        fault(&|reply| reply.end = None),
        RangeV2Verdict::Soft("missing END")
    );
    assert_eq!(
        fault(&|reply| {
            reply.chunks.clear();
            reply.end = Some(RangeEnd {
                next: 7,
                served_bytes: 0,
                reason: end_reason::BYTE_BUDGET,
            });
        }),
        RangeV2Verdict::Fault("empty reply with a non-terminal END reason")
    );
}

#[test]
fn bodies_by_range_2_reader_flags_wire_violations() {
    let (_, reply, _) = complete_case();
    let vectors = json(BODIES_VECTORS);
    let mut wire = bytes(&vectors["cases"][0]["response_wire"]);
    // Anything after END is a violation.
    wire.push(0);
    assert_eq!(read_reply(&wire).violation, Some("data after END"));
    // A chunk declaring more than the remaining budget is refused from its
    // declared length, before it is decoded.
    let truncated =
        read_range_v2_reply_with_budget(&bytes(&vectors["cases"][0]["response_wire"]), 2000);
    assert_eq!(truncated.violation, Some("declared length above the cap"));
    assert_eq!(truncated.chunks.len(), 1);
    // A stream cut inside a chunk keeps what was read and has no END.
    let full = bytes(&vectors["cases"][0]["response_wire"]);
    let cut = read_reply(&full[..full.len() / 2]);
    assert!(cut.end.is_none() && cut.violation.is_none());
    assert!(cut.chunks.len() < reply.chunks.len());
}

fn read_range_v2_reply_with_budget(wire: &[u8], budget: usize) -> RangeV2Reply {
    block_on(read_range_v2_reply(&mut Cursor::new(wire), budget))
}

#[test]
fn bodies_by_range_2_request_bounds() {
    assert!(RangeV2Request::decode(&[0; 31]).is_err());
    assert!(RangeV2Request::decode(&[0; 33]).is_err());
    let request = |count, flags| RangeV2Request {
        start: 1,
        count,
        max_bytes: 0,
        flags,
    };
    assert!(request(1, 0).validate().is_ok());
    assert!(request(1024, 1).validate().is_ok());
    assert!(request(1025, 0).validate().is_err());
    assert!(request(1, 2).validate().is_err());
    assert_eq!(request(1, 0).byte_budget(), RANGE_V2_DEFAULT_BYTES);
    let huge = RangeV2Request {
        max_bytes: u64::MAX,
        ..request(1, 0)
    };
    assert_eq!(huge.byte_budget(), RANGE_V2_SERVER_CAP);
    // A declared request above 32 bytes is refused before it is read.
    let mut oversized = Vec::new();
    crate::gov5_rpc::encode_uvarint(33, &mut oversized);
    oversized.extend_from_slice(&crate::snappy_pool::frame_encode(&[0; 33]).unwrap());
    assert!(
        block_on(Gov5BodiesByRangeV2Codec.read_request(
            &protocol(GOV5_BODIES_BY_RANGE_V2_PROTOCOL),
            &mut Cursor::new(oversized.as_slice()),
        ))
        .is_err()
    );
}

#[test]
fn bodies_by_range_2_server_reports_a_storage_failure_before_the_first_block() {
    let reader = Gov5CanonicalBlockReader::new_ranged(|| Ok(5), |_| Err("disk".to_owned()));
    let request = RangeV2Request {
        start: 1,
        count: 2,
        max_bytes: 0,
        flags: 0,
    };
    let reply = read_reply(&write_range_response(range_v2_response(
        request, [1; 4], reader,
    )));
    assert_eq!(
        reply.error.map(|(code, _)| code),
        Some(result_code::SERVER_ERROR)
    );
    assert!(reply.end.is_none());
}

// ---------------------------------------------------------------------------
// commit_qc/1
// ---------------------------------------------------------------------------

fn commit_qc_case_request(case: &Value) -> CommitQcRequest {
    let fields = &case["request_fields"];
    CommitQcRequest {
        number: fields["number"].as_u64().unwrap(),
        hash: B256::from_slice(&bytes(&fields["hash"])),
    }
}

fn write_commit_qc_response(response: CommitQcResponse) -> Vec<u8> {
    let mut wire = Cursor::new(Vec::new());
    block_on(Gov5CommitQcCodec.write_response(
        &protocol(GOV5_COMMIT_QC_PROTOCOL),
        &mut wire,
        response,
    ))
    .unwrap();
    wire.into_inner()
}

fn read_commit_qc_response(wire: &[u8]) -> CommitQcResponse {
    block_on(
        Gov5CommitQcCodec.read_response(&protocol(GOV5_COMMIT_QC_PROTOCOL), &mut Cursor::new(wire)),
    )
    .unwrap()
}

fn assert_commit_qc_matches(case: &Value, response: &CommitQcResponse) {
    let name = case["name"].as_str().unwrap();
    let chunks = case["response_chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1, "{name}");
    let expected = &chunks[0];
    assert!(expected.get("context_bytes").is_none(), "{name}");
    match response {
        CommitQcResponse::Found(payload) => {
            assert_eq!(expected["code"], "0x00", "{name}");
            assert_eq!(
                payload.encode().unwrap(),
                bytes(&expected["payload"]),
                "{name}"
            );
        }
        CommitQcResponse::Error { code, .. } => {
            assert_eq!(expected["code"], format!("0x{code:02x}"), "{name}");
        }
        CommitQcResponse::Lookup { .. } => panic!("{name}: unresolved lookup"),
    }
}

#[test]
fn commit_qc_requests_match_the_vectors() {
    let vectors = json(COMMIT_QC_VECTORS);
    assert_eq!(vectors["protocol"], GOV5_COMMIT_QC_PROTOCOL);
    for case in vectors["cases"].as_array().unwrap() {
        let request = commit_qc_case_request(case);
        assert_eq!(request.encode().to_vec(), bytes(&case["request_payload"]));
        let read = block_on(Gov5CommitQcCodec.read_request(
            &protocol(GOV5_COMMIT_QC_PROTOCOL),
            &mut Cursor::new(bytes(&case["request_wire"]).as_slice()),
        ))
        .unwrap();
        assert_eq!(read, request);
    }
    assert!(CommitQcRequest::decode(&[0; 39]).is_err());
    assert!(CommitQcRequest::decode(&[0; 41]).is_err());
}

#[test]
fn commit_qc_server_reproduces_every_vector() {
    let vectors = json(COMMIT_QC_VECTORS);
    let source = CommitQcSource {
        reader: vector_reader(),
        persisted_qc: None,
    };
    let mut names = Vec::new();
    for case in vectors["cases"].as_array().unwrap() {
        let request = commit_qc_case_request(case);
        let served = serve_commit_qc(&source, request);
        assert_commit_qc_matches(case, &served);
        // The same through the codec, resolved lazily as the swarm does.
        let wire = write_commit_qc_response(CommitQcResponse::Lookup {
            request,
            source: source.clone(),
        });
        assert_commit_qc_matches(case, &read_commit_qc_response(&wire));
        names.push(case["name"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        names,
        [
            "by_number",
            "by_hash",
            "highest",
            "unknown_hash",
            "above_head"
        ]
    );
}

#[test]
fn commit_qc_client_checks_every_vector_reply() {
    let vectors = json(COMMIT_QC_VECTORS);
    let block9 = B256::from_slice(&bytes(&vectors["cases"][1]["request_fields"]["hash"]));
    for case in vectors["cases"].as_array().unwrap() {
        let request = commit_qc_case_request(case);
        let response = read_commit_qc_response(&bytes(&case["response_wire"]));
        assert_commit_qc_matches(case, &response);
        match response {
            CommitQcResponse::Found(payload) => {
                let anchor = check_commit_qc_reply(&request, &payload).unwrap();
                assert_eq!(anchor.qc.view, 9);
                assert_eq!(anchor.qc.block_hash, block9);
                assert_eq!(anchor.qc_block, Some((9, block9)));
                assert_eq!(anchor.chain, vec![(9, block9)]);
            }
            CommitQcResponse::Error { code, .. } => {
                assert_eq!(code, result_code::RESOURCE_UNAVAILABLE);
            }
            CommitQcResponse::Lookup { .. } => unreachable!(),
        }
    }
}

fn by_number_payload() -> (CommitQcRequest, CommitQcPayload) {
    let vectors = json(COMMIT_QC_VECTORS);
    let case = &vectors["cases"][0];
    (
        commit_qc_case_request(case),
        CommitQcPayload::decode(&bytes(&case["response_chunks"][0]["payload"])).unwrap(),
    )
}

#[test]
fn commit_qc_client_rejects_a_chain_that_does_not_reach_the_request() {
    let (request, payload) = by_number_payload();
    // Another height.
    let other = CommitQcRequest {
        number: 8,
        ..request
    };
    assert_eq!(
        check_commit_qc_reply(&other, &payload),
        Err("header chain does not reach the requested number")
    );
    // Another hash at the right height.
    let other = CommitQcRequest {
        hash: B256::repeat_byte(1),
        ..request
    };
    assert_eq!(
        check_commit_qc_reply(&other, &payload),
        Err("header chain names another block at the requested number")
    );
    // A header that is not the QC's block.
    let mut wrong = payload.clone();
    let (_, mut reader_headers) = (0, vector_reader().header_rlps_by_range(8..=8).unwrap());
    wrong.headers = vec![reader_headers.remove(0)];
    assert_eq!(
        check_commit_qc_reply(&request, &wrong),
        Err("header chain does not link to the QC")
    );
    // A bare QC (anchor further than 256 headers) is structurally fine.
    let bare = CommitQcPayload {
        headers: Vec::new(),
        ..payload
    };
    assert_eq!(
        check_commit_qc_reply(&request, &bare).unwrap().qc_block,
        None
    );
}

#[test]
fn commit_qc_server_prefers_the_lowest_qc_and_reads_the_persisted_one() {
    let source = CommitQcSource {
        reader: vector_reader(),
        persisted_qc: None,
    };
    let headers = vector_reader().header_rlps_by_range(6..=10).unwrap();
    let hashes: Vec<B256> = headers.iter().map(keccak256).collect();
    // Header N embeds the QC on N-1: number 7 is anchored by header 8's QC.
    let CommitQcResponse::Found(payload) = serve_commit_qc(
        &source,
        CommitQcRequest {
            number: 7,
            hash: hashes[1],
        },
    ) else {
        panic!("expected a QC");
    };
    let anchor = check_commit_qc_reply(
        &CommitQcRequest {
            number: 7,
            hash: hashes[1],
        },
        &payload,
    )
    .unwrap();
    assert_eq!(anchor.chain, vec![(7, hashes[1])]);
    // Number 10 (the head) has no QC in any committed header; only a
    // persisted committed QC naming it reaches it.
    let head_request = CommitQcRequest {
        number: 10,
        hash: B256::ZERO,
    };
    let CommitQcResponse::Found(bare) = serve_commit_qc(&source, head_request) else {
        panic!("expected the highest QC");
    };
    assert!(bare.headers.is_empty());
    let mut persisted = decode_h2_qc(&bare.qc_wire).unwrap();
    persisted.view = 10;
    persisted.block_hash = hashes[4];
    let with_persisted = CommitQcSource {
        persisted_qc: Some(encode_h2_qc(&persisted).unwrap()),
        ..source
    };
    let CommitQcResponse::Found(reached) = serve_commit_qc(&with_persisted, head_request) else {
        panic!("expected the persisted QC");
    };
    let anchor = check_commit_qc_reply(&head_request, &reached).unwrap();
    assert_eq!(anchor.qc.view, 10);
    assert_eq!(anchor.chain, vec![(10, hashes[4])]);
    // It is also the highest QC held.
    let CommitQcResponse::Found(highest) = serve_commit_qc(
        &with_persisted,
        CommitQcRequest {
            number: 0,
            hash: B256::ZERO,
        },
    ) else {
        panic!("expected the highest QC");
    };
    assert_eq!(decode_h2_qc(&highest.qc_wire).unwrap().view, 10);
    assert_eq!(highest.headers, vec![headers[4].clone()]);
}

#[test]
fn commit_qc_payload_decoder_checks_lengths_before_allocating() {
    let (_, payload) = by_number_payload();
    let encoded = payload.encode().unwrap();
    assert_eq!(CommitQcPayload::decode(&encoded).unwrap(), payload);
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(CommitQcPayload::decode(&trailing).is_err());
    assert!(CommitQcPayload::decode(&encoded[..encoded.len() - 1]).is_err());
    // qc_len above 2048.
    let mut long_qc = encoded.clone();
    long_qc[..4].copy_from_slice(&2049u32.to_le_bytes());
    assert!(CommitQcPayload::decode(&long_qc).is_err());
    // A header count above 256 and a header above 64 KiB, declared only.
    let qc_end = 4 + payload.qc_wire.len();
    let mut many = encoded[..qc_end].to_vec();
    many.extend_from_slice(&257u16.to_le_bytes());
    assert!(CommitQcPayload::decode(&many).is_err());
    let mut huge = encoded[..qc_end].to_vec();
    huge.extend_from_slice(&1u16.to_le_bytes());
    huge.extend_from_slice(&(64u32 << 10 | 1).to_le_bytes());
    assert!(CommitQcPayload::decode(&huge).is_err());
}
