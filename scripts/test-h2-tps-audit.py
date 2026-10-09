#!/usr/bin/env python3
"""Offline regressions for H2 throughput accounting, with no network required."""

import copy
import io
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("h2_audit", Path(__file__).with_name("h2-tps-audit.py"))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)
if not audit.keccak_binary().is_file():
    raise SystemExit("Build cargo build --release --bin n42-keccak, or set N42_KECCAK_BIN")


SIGNED_REQUESTS = json.loads(Path(__file__).with_name("fixtures").joinpath("h2-tps-commit-qcs.json").read_text())
SIGNED_QCS = {request["expectedBlockHash"]: request["qc"] for request in SIGNED_REQUESTS}
TRUSTED = audit.trusted_config(dict(SIGNED_REQUESTS[0], schema=1))
if not audit.commit_binary().is_file():
    raise SystemExit("Build n42-verify-commit, or set N42_COMMIT_VERIFY_BIN")


def digest(number):
    return f"0x{number:064x}"


class Fleet:
    urls = [f"node{i}" for i in range(4)]

    def __init__(self):
        self.trusted = copy.deepcopy(TRUSTED)
        self.heads = [10] * 4
        self.counter_epoch = 0
        self.read_overrides = {}
        receipt_root = audit.keccak_chunks([audit.rlp([1, gas, []]) for gas in (21000, 42000, 63000)])
        self.blocks = {0: dict(number="0x0", hash=digest(42), parentHash=digest(0),
                              stateRoot=digest(142), receiptsRoot=digest(242),
                              transactionsRoot=digest(342), transactions=[], gasUsed="0x0")}
        self.receipts = {}
        self.receipt_overrides = {}
        for height in range(10, 16):
            self.blocks[height] = dict(number=hex(height), hash=digest(height),
                parentHash=digest(height-1), stateRoot=digest(height+100),
                receiptsRoot=receipt_root, transactionsRoot=digest(height+300), gasUsed=hex(63000),
                transactions=[digest(height*10+i) for i in range(3)])
            self.receipts[height] = [dict(blockHash=digest(height), blockNumber=hex(height),
                transactionHash=digest(height*10+i), transactionIndex=hex(i), status="0x1",
                cumulativeGasUsed=hex((i+1)*21000), gasUsed=hex(21000), logs=[]) for i in range(3)]
        self.overrides = {}
        self.hashes, self.by_hash, self.headers, self.header_overrides = {}, {}, {}, {}
        self.rehash()

    def rehash(self, first=0):
        for height, block in sorted(self.blocks.items()):
            if height < first:
                continue
            block["parentHash"] = self.hashes.get(height-1, digest(max(0, height-1)))
            fields = [audit.data_bytes(block["parentHash"]), bytes(32), bytes(20),
                      audit.data_bytes(block["stateRoot"]), audit.data_bytes(block["transactionsRoot"]),
                      audit.data_bytes(block["receiptsRoot"]), bytes(256), 0, height, 100000000,
                      audit.quantity(block["gasUsed"]), height, b"", bytes(32), bytes(8),
                      0, b"", 0, 0, b"", b"", b"", bytes(32)]
            raw = audit.rlp(fields)
            block["hash"] = audit.keccak_chunks([raw])
            self.hashes[height] = block["hash"]
            self.by_hash[block["hash"]] = height
            self.headers[height] = "0x"+raw.hex()
            for receipt in self.receipts.get(height, []):
                receipt["blockHash"] = block["hash"]

    def rpc(self, url, method, params):
        index = self.urls.index(url)
        if method == "n42_stateReadStatus":
            result = dict(schema=1, instanceId=digest(500+index), processId=100+index,
                          mode="only", backend="gov5_qmdb_binary", coverage="exact_block_provider",
                          walEnabled=True, chainId=94, genesisHash=self.hashes[0],
                          baseBlockHash=self.hashes[0], baseRoot=digest(142),
                          validatorPublicKey=self.trusted["validators"][index],
                          requestedBlockHash=params[0],
                          durableRoot=self.blocks[self.by_hash[params[0]]]["stateRoot"] if params[0] else None,
                          counters={field: 0 for field in audit.READ_COUNTERS})
            result["counters"].update(accountReads=1000+self.counter_epoch*100,
                                       pinnedProviders=10+self.counter_epoch)
            result.update(copy.deepcopy(self.read_overrides.get(url, {})))
            return result
        if method == "n42_validatorSet":
            return dict(active=[dict(index=i, publicKey=self.trusted["validators"][i]) for i in range(4)])
        if method == "eth_chainId":
            return "0x5e"
        if method == "n42_consensusStatus":
            height = self.heads[self.urls.index(url)]
            return dict(hasCommittedQc=True, validatorCount=4,
                        latestCommittedView=height*2, latestCommittedBlockHash=self.hashes[height],
                        commitQc=copy.deepcopy(SIGNED_QCS[self.hashes[height]]))
        if method in ("eth_getBlockByHash", "eth_getBlockByNumber"):
            height = self.by_hash.get(params[0]) if method.endswith("Hash") else int(params[0], 16)
            return copy.deepcopy(self.overrides.get((url, height), self.blocks.get(height)))
        if method == "eth_getBlockReceipts":
            height = self.by_hash.get(params[0])
            return copy.deepcopy(self.receipt_overrides.get((url, height), self.receipts.get(height)))
        if method == "n42_nativeHeader":
            height = self.by_hash.get(params[0])
            return self.header_overrides.get((url, height), self.headers.get(height))
        raise AssertionError(f"must not use latest block height: {method}")

    def capture(self, start, finish):
        self.counter_epoch = start // 1_000_000_000
        times = iter((start, finish))
        return audit.capture(self.urls, self.rpc, lambda: next(times), boot_id="same-host-boot", trusted=self.trusted)


class AuditTests(unittest.TestCase):
    def test_receipt_fetch_retries_only_the_captured_hash(self):
        block_hash = digest(5)
        responses = iter((None, [{"transactionHash": digest(50)}]))
        calls = []

        def request(url, method, params):
            calls.append((method, params))
            return next(responses)

        receipts = audit.fetch_block_receipts("node0", block_hash, 1,
                                              call=request, sleep=lambda _: None)
        self.assertEqual(len(receipts), 1)
        self.assertEqual(calls, [("eth_getBlockReceipts", [block_hash])] * 2)

    def test_rpc_error_preserves_method_code_and_message_for_large_receipts(self):
        response = dict(jsonrpc="2.0", id=1,
                        error=dict(code=-32005, message="response exceeds 160 MiB limit"))
        with patch.object(audit.urllib.request, "urlopen",
                          return_value=io.BytesIO(json.dumps(response).encode())):
            with self.assertRaisesRegex(audit.AuditError,
                                        r"eth_getBlockReceipts.*-32005.*160 MiB"):
                audit.rpc("http://example.invalid", "eth_getBlockReceipts", [digest(5)])

    def test_seven_validator_trust_requires_f_two_and_distinct_keys(self):
        config = dict(TRUSTED, faultTolerance=2,
                      validators=TRUSTED["validators"] + [f"{i:096x}" for i in (5, 6, 7)])
        self.assertEqual(len(audit.trusted_config(config)["validators"]), 7)
        with self.assertRaises(audit.AuditError):
            audit.trusted_config(dict(config, faultTolerance=1))
        with self.assertRaises(audit.AuditError):
            audit.trusted_config(dict(config, validators=config["validators"][:-1] + [config["validators"][0]]))

    def setUp(self):
        self.fleet = Fleet()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.trust_path = Path(directory.name)/"trusted.json"
        self.trust_path.write_text(json.dumps(self.fleet.trusted))
        self.start = self.fleet.capture(0, 1_000_000_000)
        self.fleet.heads = [13, 14, 15, 13]
        self.end = self.fleet.capture(61_000_000_000, 63_000_000_000)

    def check(self):
        return audit.audit(self.start, self.end, self.fleet.rpc, trusted=self.fleet.trusted)

    def test_all_eight_boundary_certificates_are_reverified_and_archived(self):
        result = self.check()
        self.assertEqual(len(result["boundary_commit_proofs"]), 8)
        self.assertEqual(len(result["boundary_ancestry"]), 8)
        self.assertEqual(result["trusted_config"], self.fleet.trusted)
        self.assertTrue(all(proof["verification"]["chainBoundSignature"] for proof in result["boundary_commit_proofs"]))
        self.assertEqual(sum(len(path["headers"]) for path in result["boundary_ancestry"]), 3)

    def test_missing_or_invalid_raw_commit_qc_cannot_pass_capture_on_any_node(self):
        for target in self.fleet.urls:
            for qc in (None, dict(SIGNED_QCS[self.fleet.hashes[13]], signature="00"*96)):
                def changed(url, method, params):
                    value = self.fleet.rpc(url, method, params)
                    if url == target and method == "n42_consensusStatus":
                        value["commitQc"] = qc
                    return value
                with self.subTest(url=target), self.assertRaisesRegex(audit.AuditError, "certificate"):
                    audit.capture(self.fleet.urls, changed, boot_id="test", trusted=self.fleet.trusted)

    def test_capture_never_derives_trust_from_reported_roster(self):
        with self.assertRaisesRegex(audit.AuditError, "trusted configuration"):
            audit.capture(self.fleet.urls, self.fleet.rpc)
        for field, value in (("profile", "gov5legacy"), ("faultTolerance", 0),
                             ("validators", self.fleet.trusted["validators"][:3]),
                             ("validatorChangesHash", digest(1))):
            config = dict(self.fleet.trusted, **{field: value})
            with self.subTest(field=field), self.assertRaises(audit.AuditError):
                audit.capture(self.fleet.urls, self.fleet.rpc, trusted=config)
        for field, value in (("chainId", 95), ("genesisHash", digest(99)),
                             ("validators", list(reversed(self.fleet.trusted["validators"])))):
            config = dict(self.fleet.trusted, **{field: value})
            with self.subTest(field=field), self.assertRaisesRegex(audit.AuditError, "trusted configuration"):
                audit.capture(self.fleet.urls, self.fleet.rpc, trusted=config)

    def test_tampered_saved_qc_status_or_verifier_evidence_fails(self):
        for field, value in (("commit_proof", {}), ("status", dict(self.end["nodes"][0]["status"],
                              latestCommittedView=999)), ("status", dict(self.end["nodes"][0]["status"], commitQc=None))):
            end = copy.deepcopy(self.end)
            end["nodes"][0][field] = value
            with self.subTest(field=field), self.assertRaises(audit.AuditError):
                audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)
        for field, value in (("trusted_config_sha256", "00"*32), ("commit_verifier_sha256", "00"*32),
                             ("trusted_config", {}), ("schema", 4)):
            end = dict(self.end, **{field: value})
            with self.subTest(field=field), self.assertRaises(audit.AuditError):
                audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)

    def test_commit_helper_failure_does_not_qualify(self):
        for executable in ("/bin/false", "/bin/echo"):
            with patch.object(audit, "commit_binary", return_value=Path(executable)):
                with self.assertRaises(audit.AuditError):
                    self.fleet.capture(70, 80)

    def test_ancestry_above_common_end_is_verified_against_actual_certificate(self):
        # Node2's signed endpoint is height15 while the common end is height13.
        # Substitute an internally valid raw height14 header off that chain.
        fields = audit.native_header_fields(audit.data_bytes(self.fleet.headers[14]))
        fields[0] = audit.data_bytes(digest(999))
        raw = audit.rlp(fields)
        block = dict(self.fleet.blocks[14], parentHash=digest(999), hash=audit.keccak_chunks([raw]))
        self.fleet.overrides[("node2", 14)] = block
        self.fleet.by_hash[block["hash"]] = 14
        self.fleet.header_overrides[("node2", 14)] = "0x"+raw.hex()
        with self.assertRaisesRegex(audit.AuditError, "ancestry.*certified boundary"):
            self.check()

    def test_ancestry_below_common_start_is_verified_against_actual_certificate(self):
        self.fleet.heads = [11, 10, 12, 10]
        self.start = self.fleet.capture(0, 1_000_000_000)
        fields = audit.native_header_fields(audit.data_bytes(self.fleet.headers[11]))
        fields[0] = audit.data_bytes(digest(999))
        raw = audit.rlp(fields)
        block = dict(self.fleet.blocks[11], parentHash=digest(999), hash=audit.keccak_chunks([raw]))
        self.fleet.overrides[("node1", 11)] = block
        self.fleet.by_hash[block["hash"]] = 11
        self.fleet.header_overrides[("node1", 11)] = "0x"+raw.hex()
        with self.assertRaisesRegex(audit.AuditError, "ancestry.*certified boundary"):
            self.check()

    def test_bad_bls_signature_cannot_pass_cli_even_at_low_threshold(self):
        self.end["nodes"][3]["status"]["commitQc"]["signature"] = "00"*96
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory)/name for name in ("start.json", "end.json", "out.json")]
            paths[0].write_text(json.dumps(self.start))
            paths[1].write_text(json.dumps(self.end))
            args = ["h2-tps-audit.py", "audit", "--start", str(paths[0]), "--end", str(paths[1]),
                    "--out", str(paths[2]), "--min-tps", "0.000001", "--trusted-config", str(self.trust_path)]
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b, **kw: original(a, b, self.fleet.rpc, **kw)):
                with patch("sys.argv", args), self.assertRaises(SystemExit) as raised:
                    audit.main()
            self.assertEqual(raised.exception.code, 1)
            report = json.loads(paths[2].read_text())
            self.assertEqual(report["status"], "failed")
            self.assertIn("certificate", report["error"])

    def test_only_common_h2_commits_and_all_boundary_time_count(self):
        result = self.check()
        self.assertEqual(result["committed_transactions"], 9)
        self.assertEqual(result["committed_blocks"], 3)
        self.assertEqual(result["measurement_seconds"], 63)
        self.assertEqual(result["strict_committed_tps"], 9/63)
        self.assertEqual(result["successful_committed_tps"], 9/63)
        self.assertEqual(result["failed_committed_transactions"], 0)
        self.assertTrue(all(block["receipt_nodes_verified"] == 4 for block in result["blocks"]))

    def test_fastest_start_excludes_preexisting_commits(self):
        self.fleet.heads = [11, 10, 12, 10]
        self.start = self.fleet.capture(0, 1_000_000_000)
        self.assertEqual(self.check()["committed_transactions"], 3)

    def test_native_receipt_known_vectors_and_long_rlp(self):
        self.assertEqual(audit.rlp([1, 21000, []]).hex(), "c501825208c0")
        block = dict(hash=digest(1), number="0x1", transactions=[digest(9)], gasUsed="0x5208",
                     receiptsRoot="0x9ec602b25fc63e86a5feb8943d52cf66b24ed8e8021f3f74f077271ffae88c75")
        receipt = dict(blockHash=digest(1), blockNumber="0x1", transactionHash=digest(9),
                       transactionIndex="0x0", status="0x1", cumulativeGasUsed="0x5208",
                       gasUsed="0x5208", logs=[])
        self.assertEqual(audit.receipt_summary([receipt], block)["successful_transactions"], 1)
        block.update(transactions=[], gasUsed="0x0",
                     receiptsRoot="0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        self.assertEqual(audit.receipt_summary([], block)["successful_transactions"], 0)
        self.assertEqual(audit.rlp(bytes(56)), b"\xb8\x38" + bytes(56))
        self.assertEqual(audit.rlp([bytes(56)]), b"\xf8\x3a\xb8\x38" + bytes(56))

    def test_null_logs_only_allowed_with_empty_bloom_and_matching_root(self):
        block = dict(hash=digest(1), number="0x1", transactions=[digest(9)], gasUsed="0x5208",
                     receiptsRoot="0x9ec602b25fc63e86a5feb8943d52cf66b24ed8e8021f3f74f077271ffae88c75")
        receipt = dict(blockHash=digest(1), blockNumber="0x1", transactionHash=digest(9),
                       transactionIndex="0x0", status="0x1", cumulativeGasUsed="0x5208",
                       gasUsed="0x5208", logs=None, logsBloom="0x" + "00" * 256)
        self.assertEqual(audit.receipt_summary([receipt], block)["successful_transactions"], 1)
        receipt["logsBloom"] = "0x" + "01" + "00" * 255
        with self.assertRaisesRegex(audit.AuditError, "nonempty bloom"):
            audit.receipt_summary([receipt], block)

    def test_committed_block_read_retries_exact_hash_until_fcu_publishes_it(self):
        block = dict(hash=digest(1), number="0x1", transactions=[], gasUsed="0x0",
                     parentHash=digest(0), stateRoot=digest(2),
                     receiptsRoot=digest(3), transactionsRoot=digest(4))
        seen = []
        def call(url, method, params):
            seen.append((method, params))
            return None if len(seen) == 1 else block
        self.assertEqual(audit.read_committed_block("node0", digest(1), call), block)
        self.assertEqual(seen, [("eth_getBlockByHash", [digest(1), False])] * 2)

    def test_receipt_preflight_checks_each_node_and_fails_before_workload(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)/"capture.json"
            args = ["audit", "capture", "--rpc", ",".join(self.fleet.urls),
                    "--out", str(out), "--verify-receipts"]
            args += ["--trusted-config", str(self.trust_path)]
            original = audit.capture
            with (patch.object(audit, "rpc", side_effect=self.fleet.rpc),
                  patch.object(audit, "capture", side_effect=lambda urls, **kw: original(urls, self.fleet.rpc, boot_id="test", **kw)),
                  patch("sys.argv", args)):
                audit.main()
                self.assertEqual(len(json.loads(out.read_text())["receipt_preflight"]), 4)
                self.fleet.receipt_overrides[("node3", 13)] = None
                with self.assertRaises(SystemExit) as raised:
                    audit.main()
                self.assertEqual(raised.exception.code, 1)
                self.assertEqual(json.loads(out.read_text())["status"], "failed")

    def test_keccak_helper_failure_never_returns_a_digest(self):
        for executable in ("/bin/false", "/bin/echo"):
            with patch.object(audit, "keccak_binary", return_value=Path(executable)):
                with self.assertRaises(audit.AuditError):
                    audit.keccak_chunks([])

    def test_keccak_stream_crosses_helper_buffer_boundaries(self):
        payload = bytes(range(256)) * 1024
        expected = audit.keccak_chunks([payload])
        self.assertEqual(audit.keccak_chunks(payload[i:i+997] for i in range(0, len(payload), 997)), expected)

    def test_log_and_failure_fixture_matches_production_rust_root(self):
        fixture = json.loads(Path(__file__).with_name("fixtures").joinpath("gov5-receipt-audit.json").read_text())
        result = audit.receipt_summary(fixture["receipts"], fixture["block"])
        self.assertEqual(result["successful_transactions"], 2)
        self.assertEqual(result["failed_transactions"], 1)
        fixture["receipts"][0]["logs"][0]["data"] = "0x"+bytes(80).hex()
        with self.assertRaisesRegex(audit.AuditError, "recomputed Gov5"):
            audit.receipt_summary(fixture["receipts"], fixture["block"])

    def test_native_header_matches_chain94_fixture_including_nil_and_mobile_fields(self):
        fixture = json.loads(Path(__file__).with_name("fixtures").joinpath("gov5-native-header-audit.json").read_text())
        result = audit.verify_native_header(fixture["raw"], fixture["block"])
        self.assertEqual(result["verified_header_hash"], fixture["block"]["hash"])
        fields = audit.native_header_fields(audit.data_bytes(fixture["raw"]))
        self.assertEqual(len(fields), 23)
        self.assertEqual(fields[17:19], [b"", b""])
        self.assertEqual(fields[20:22], [b"", b""])
        self.assertEqual(fields[22], bytes(32))
        with self.assertRaisesRegex(audit.AuditError, "header hash differs"):
            audit.verify_native_header("0x"+audit.rlp(fields[:-1]).hex(), fixture["block"])

    def test_native_header_rejects_malformed_rlp_and_field_encodings(self):
        raw = audit.data_bytes(self.fleet.headers[12])
        fields = audit.native_header_fields(raw)
        invalid = [b"", b"\xc0", raw+b"\x00", raw[:-1], b"\xf8\x01\x80",
                   audit.rlp(fields[:14]), audit.rlp(fields+[b""])]
        for index, value in [(0, bytes(31)), (2, bytes(19)), (6, bytes(255)),
                             (8, b"\x00"), (9, b"\x01"+bytes(8)),
                             (12, []), (16, b"\x01")]:
            changed = fields.copy()
            changed[index] = value
            invalid.append(audit.rlp(changed))
        for value in invalid:
            with self.subTest(value=value[:12]), self.assertRaises(audit.AuditError):
                audit.native_header_fields(value)

    def test_missing_or_substituted_native_header_fails_on_each_validator(self):
        for url in self.fleet.urls:
            for raw in (None, self.fleet.headers[13]):
                self.fleet.header_overrides = {(url, 12): raw}
                with self.subTest(url=url, raw=raw), self.assertRaises(audit.AuditError):
                    self.check()

    def test_forged_rpc_fields_do_not_change_the_committed_header(self):
        for field, value in [("stateRoot", digest(999)), ("receiptsRoot", digest(999)),
                             ("transactionsRoot", digest(999)), ("gasUsed", "0x0")]:
            old = self.fleet.blocks[12][field]
            self.fleet.blocks[12][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(audit.AuditError, "native header differs"):
                self.check()
            self.fleet.blocks[12][field] = old
        self.start["nodes"][0]["native_header_rlp"] = self.fleet.headers[11]
        with self.assertRaisesRegex(audit.AuditError, "header hash differs"):
            self.check()

    def test_old_receipt_only_boundaries_cannot_pass_header_qualification(self):
        self.start["schema"] = 3
        with self.assertRaisesRegex(audit.AuditError, "schema"):
            self.check()

    def test_receipt_status_is_bound_to_root_on_every_validator(self):
        for url in self.fleet.urls:
            receipts = copy.deepcopy(self.fleet.receipts[12])
            receipts[0]["status"] = "0x0"
            self.fleet.receipt_overrides = {(url, 12): receipts}
            with self.subTest(url=url), self.assertRaisesRegex(audit.AuditError, "recomputed Gov5"):
                self.check()

    def test_valid_failed_receipts_cannot_pass_an_inclusion_only_threshold(self):
        root = audit.keccak_chunks([audit.rlp([status, gas, []]) for status, gas in
                                    [(0, 21000), (1, 42000), (1, 63000)]])
        for height in range(11, 16):
            self.fleet.receipts[height][0]["status"] = "0x0"
            self.fleet.blocks[height]["receiptsRoot"] = root
        self.fleet.rehash(11)
        self.end = self.fleet.capture(61_000_000_000, 63_000_000_000)
        report = self.check()
        self.assertEqual(report["successful_committed_transactions"], 6)
        self.assertEqual(report["failed_committed_transactions"], 3)
        self.assertGreater(report["strict_committed_tps"], 0.1)
        self.assertLess(report["successful_committed_tps"], 0.1)
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory)/name for name in ("start.json", "end.json", "out.json")]
            paths[0].write_text(json.dumps(self.start))
            paths[1].write_text(json.dumps(self.end))
            args = ["audit", "audit", "--start", str(paths[0]), "--end", str(paths[1]),
                    "--out", str(paths[2]), "--min-tps", "0.1"]
            args += ["--trusted-config", str(self.trust_path)]
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b, **kw: original(a, b, self.fleet.rpc, **kw)):
                with patch("sys.argv", args), self.assertRaises(SystemExit) as raised:
                    audit.main()
            self.assertEqual(raised.exception.code, 1)
            self.assertEqual(json.loads(paths[2].read_text())["status"], "below_target")

    def test_missing_extra_reordered_and_wrong_identity_receipts_fail(self):
        original = self.fleet.receipts[12]
        invalid = [None, [], original[:-1], original+[original[0]], list(reversed(original))]
        for field, value in [("blockHash", digest(99)), ("blockNumber", "0xb"),
                             ("transactionHash", digest(99)), ("transactionIndex", "0x1"),
                             ("status", None), ("status", "0x2"),
                             ("gasUsed", "0x1"), ("cumulativeGasUsed", "0x1")]:
            receipts = copy.deepcopy(original)
            receipts[0][field] = value
            invalid.append(receipts)
        for receipts in invalid:
            self.fleet.receipt_overrides = {("node2", 12): receipts}
            with self.subTest(receipts=receipts), self.assertRaises(audit.AuditError):
                self.check()

    def test_receipt_logs_bind_content_and_metadata(self):
        original = copy.deepcopy(self.fleet.receipts[12])
        log = dict(address="0x"+"11"*20, topics=[digest(33)], data="0x010203", removed=False,
                   blockHash=self.fleet.hashes[12], blockNumber="0xc", transactionHash=digest(120),
                   transactionIndex="0x0", logIndex="0x0")
        original[0]["logs"] = [log]
        for field, value in [("removed", True), ("logIndex", "0x1"), ("blockHash", digest(99)),
                             ("topics", [digest(1)]*5), ("address", "0x12"), ("data", "0x1")]:
            receipts = copy.deepcopy(original)
            receipts[0]["logs"][0][field] = value
            self.fleet.receipt_overrides = {("node1", 12): receipts}
            with self.subTest(field=field), self.assertRaises(audit.AuditError):
                self.check()
        self.fleet.receipt_overrides = {("node1", 12): original}
        with self.assertRaisesRegex(audit.AuditError, "recomputed Gov5"):
            self.check()

    def test_wrong_block_gas_duplicate_transactions_and_old_schema_fail(self):
        block = copy.deepcopy(self.fleet.blocks[12])
        block["gasUsed"] = "0x0"
        self.fleet.overrides[("node0", 12)] = block
        with self.assertRaises(audit.AuditError):
            self.check()
        block["transactions"][1] = block["transactions"][0]
        with self.assertRaisesRegex(audit.AuditError, "duplicate transaction"):
            self.check()
        self.start["schema"] = 2
        with self.assertRaisesRegex(audit.AuditError, "schema"):
            self.check()

    def test_stalled_fourth_validator_cannot_pass(self):
        self.fleet.heads = [13, 14, 15, 10]
        self.end = self.fleet.capture(61_000_000_000, 63_000_000_000)
        with self.assertRaisesRegex(audit.AuditError, "no common"):
            self.check()

    def test_missing_commit_qc_is_rejected(self):
        def no_qc(url, method, params):
            if method == "n42_consensusStatus":
                return dict(hasCommittedQc=False)
            return self.fleet.rpc(url, method, params)
        with self.assertRaises(audit.AuditError):
            audit.capture(self.fleet.urls, no_qc, boot_id="test", trusted=self.fleet.trusted)

    def test_qc_must_bind_execution_hash(self):
        self.fleet.blocks[13]["hash"] = digest(999)
        with self.assertRaisesRegex(audit.AuditError, "differs from H2"):
            self.fleet.capture(70, 80)

    def test_captured_commit_reorg_fails(self):
        self.fleet.blocks[10]["hash"] = digest(999)
        with self.assertRaisesRegex(audit.AuditError, "captured commit changed"):
            self.check()

    def test_broken_parent_chain_fails(self):
        self.fleet.blocks[12]["parentHash"] = digest(999)
        with self.assertRaisesRegex(audit.AuditError, "broken ancestry"):
            self.check()

    def test_follower_root_disagreement_fails(self):
        divergent = copy.deepcopy(self.fleet.blocks[13])
        divergent["stateRoot"] = digest(999)
        self.fleet.overrides[("node1", 13)] = divergent
        with self.assertRaisesRegex(audit.AuditError, "common committed chain differs"):
            self.check()

    def test_missing_block_or_transactions_fails(self):
        for replacement in (None, {"number": "0xc"}):
            self.fleet.overrides[("node0", 12)] = replacement
            with self.assertRaises(audit.AuditError):
                self.check()

    def test_duplicate_endpoints_and_clock_changes_fail(self):
        with self.assertRaises(audit.AuditError):
            audit.capture(["node0"]*4, self.fleet.rpc, boot_id="test", trusted=self.fleet.trusted)
        self.end["boot_id"] = "rebooted"
        with self.assertRaises(audit.AuditError):
            self.check()
        self.end["boot_id"] = self.start["boot_id"]
        self.end["started_ns"] = 0
        with self.assertRaises(audit.AuditError):
            self.check()

    def test_cli_threshold_failure_is_nonzero_and_report_is_saved(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory)/name for name in ("start.json", "end.json", "out.json")]
            paths[0].write_text(json.dumps(self.start))
            paths[1].write_text(json.dumps(self.end))
            args = ["h2-tps-audit.py", "audit", "--start", str(paths[0]),
                    "--end", str(paths[1]), "--out", str(paths[2])]
            # Bind our offline transport while exercising the real CLI logic.
            args += ["--trusted-config", str(self.trust_path)]
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b, **kw: original(a, b, self.fleet.rpc, **kw)):
                with patch("sys.argv", args), self.assertRaises(SystemExit) as raised:
                    audit.main()
                self.assertEqual(raised.exception.code, 1)
                self.assertEqual(json.loads(paths[2].read_text())["status"], "below_target")
                with patch("sys.argv", args + ["--min-tps", "0.1"]):
                    audit.main()
                self.assertEqual(json.loads(paths[2].read_text())["status"], "passed")

    def test_modes_and_memory_only_store_cannot_qualify(self):
        for override in [dict(mode="off"), dict(mode="verify"), dict(mode="on"),
                         dict(walEnabled=False), dict(backend="mpt")]:
            with self.subTest(override=override):
                self.fleet.read_overrides["node1"] = override
                with self.assertRaisesRegex(audit.AuditError, "requires.*QMDB only"):
                    self.fleet.capture(70_000_000_000, 71_000_000_000)

    def test_distinct_urls_must_be_distinct_validator_processes(self):
        for override, error in [(dict(instanceId=digest(500)), "same process"),
                                (dict(validatorPublicKey=self.fleet.trusted["validators"][0]), "duplicate validator")]:
            self.fleet.read_overrides["node1"] = override
            with self.assertRaisesRegex(audit.AuditError, error):
                self.fleet.capture(70_000_000_000, 71_000_000_000)

    def test_qmdb_commit_root_and_chain_binding(self):
        for override, error in [(dict(durableRoot=digest(999)), "unexpected QMDB requested"),
                                (dict(chainId=95), "differs from execution"),
                                (dict(genesisHash=digest(99)), "differs from execution")]:
            self.fleet.read_overrides["node1"] = override
            with self.assertRaisesRegex(audit.AuditError, error):
                self.fleet.capture(70_000_000_000, 71_000_000_000)
        # A root supplied for the requested commit must match that block.
        self.fleet.read_overrides = {}
        self.end["nodes"][1]["read_after"]["durableRoot"] = digest(999)
        with self.assertRaisesRegex(audit.AuditError, "durable root differs"):
            self.check()

    def test_zero_reads_errors_resets_and_restarts_fail(self):
        for field in ("mismatches", "readErrors", "providerErrors", "unavailableProviders"):
            end = copy.deepcopy(self.end)
            end["nodes"][1]["read_after"]["counters"][field] += 1
            with self.subTest(field=field), self.assertRaisesRegex(audit.AuditError, field):
                audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)
        end = copy.deepcopy(self.end)
        for edge in ("read_before", "read_after"):
            end["nodes"][1][edge]["counters"] = copy.deepcopy(self.start["nodes"][1][edge]["counters"])
        with self.assertRaisesRegex(audit.AuditError, "no observed QMDB"):
            audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)
        end["nodes"][1]["read_after"]["counters"]["accountReads"] = 0
        with self.assertRaisesRegex(audit.AuditError, "counter regressed"):
            audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)
        end = copy.deepcopy(self.end)
        for edge in ("read_before", "read_after"):
            end["nodes"][1][edge]["instanceId"] = digest(900)
        with self.assertRaisesRegex(audit.AuditError, "identity changed"):
            audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)

    def test_counter_envelope_covers_capture_time_and_reports_actual_deltas(self):
        self.start["nodes"][0]["read_after"]["counters"]["accountReads"] += 10
        self.end["nodes"][0]["read_before"]["counters"]["accountReads"] -= 20
        report = self.check()
        self.assertEqual(report["read_evidence"][0]["counters_delta"]["accountReads"], 6100)
        self.assertEqual(report["scope"], "h2v4_commit_qcs_qmdb_only_native_headers_and_receipts")

    def test_missing_counter_and_inactive_validator_are_rejected(self):
        del self.end["nodes"][0]["read_after"]["counters"]["readErrors"]
        with self.assertRaisesRegex(audit.AuditError, "invalid QMDB counter"):
            self.check()
        self.fleet.read_overrides["node0"] = dict(validatorPublicKey=f"{999:096x}")
        with self.assertRaisesRegex(audit.AuditError, "not an active validator"):
            self.fleet.capture(70_000_000_000, 71_000_000_000)

    def test_roster_disagreement_and_transition_are_rejected(self):
        for changed_nodes, error in [([0], "rosters.*differ"),
                                     (range(4), "roster changed")]:
            end = copy.deepcopy(self.end)
            for index in changed_nodes:
                active = end["nodes"][index]["validator_set"]["active"]
                # Keep all valid identities but change consensus index order.
                active[0]["index"], active[1]["index"] = 1, 0
            with self.subTest(nodes=changed_nodes), self.assertRaisesRegex(audit.AuditError, error):
                audit.audit(self.start, end, self.fleet.rpc, trusted=self.fleet.trusted)

    def test_capture_rejects_restart_between_its_own_samples(self):
        def restarted(url, method, params):
            result = self.fleet.rpc(url, method, params)
            if method == "n42_stateReadStatus" and params[0] is not None:
                result["instanceId"] = digest(900)
            return result
        with self.assertRaisesRegex(audit.AuditError, "identity changed"):
            audit.capture(self.fleet.urls, restarted, boot_id="test", trusted=self.fleet.trusted)

    def test_cli_architecture_failure_saves_failed_report_even_with_low_tps_threshold(self):
        self.end["nodes"][0]["read_after"]["counters"]["readErrors"] += 1
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory)/name for name in ("start.json", "end.json", "out.json")]
            paths[0].write_text(json.dumps(self.start))
            paths[1].write_text(json.dumps(self.end))
            args = ["h2-tps-audit.py", "audit", "--start", str(paths[0]),
                    "--end", str(paths[1]), "--out", str(paths[2]), "--min-tps", "0.00001"]
            args += ["--trusted-config", str(self.trust_path)]
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b, **kw: original(a, b, self.fleet.rpc, **kw)):
                with patch("sys.argv", args), self.assertRaises(SystemExit) as raised:
                    audit.main()
            self.assertEqual(raised.exception.code, 1)
            report = json.loads(paths[2].read_text())
            self.assertEqual(report["status"], "failed")
            self.assertIn("readErrors", report["error"])


if __name__ == "__main__":
    unittest.main()
