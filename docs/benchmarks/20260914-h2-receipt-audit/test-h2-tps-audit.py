#!/usr/bin/env python3
"""Offline regressions for H2 throughput accounting, with no network required."""

import copy
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


def digest(number):
    return f"0x{number:064x}"


class Fleet:
    urls = [f"node{i}" for i in range(4)]

    def __init__(self):
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

    def rpc(self, url, method, params):
        index = self.urls.index(url)
        if method == "n42_stateReadStatus":
            result = dict(schema=1, instanceId=digest(500+index), processId=100+index,
                          mode="only", backend="gov5_qmdb_binary", coverage="exact_block_provider",
                          walEnabled=True, chainId=94, genesisHash=digest(42),
                          baseBlockHash=digest(42), baseRoot=digest(142),
                          validatorPublicKey=f"{100+index:096x}",
                          requestedBlockHash=params[0],
                          durableRoot=self.blocks[int(params[0], 16)]["stateRoot"] if params[0] else None,
                          counters={field: 0 for field in audit.READ_COUNTERS})
            result["counters"].update(accountReads=1000+self.counter_epoch*100,
                                       pinnedProviders=10+self.counter_epoch)
            result.update(copy.deepcopy(self.read_overrides.get(url, {})))
            return result
        if method == "n42_validatorSet":
            return dict(active=[dict(index=i, publicKey=f"{100+i:096x}") for i in range(4)])
        if method == "eth_chainId":
            return "0x5e"
        if method == "n42_consensusStatus":
            height = self.heads[self.urls.index(url)]
            return dict(hasCommittedQc=True, validatorCount=4,
                        latestCommittedView=height*2, latestCommittedBlockHash=digest(height))
        if method in ("eth_getBlockByHash", "eth_getBlockByNumber"):
            height = int(params[0], 16)
            return copy.deepcopy(self.overrides.get((url, height), self.blocks.get(height)))
        if method == "eth_getBlockReceipts":
            height = int(params[0], 16)
            return copy.deepcopy(self.receipt_overrides.get((url, height), self.receipts.get(height)))
        raise AssertionError(f"must not use latest block height: {method}")

    def capture(self, start, finish):
        self.counter_epoch = start // 1_000_000_000
        times = iter((start, finish))
        return audit.capture(self.urls, self.rpc, lambda: next(times), boot_id="same-host-boot")


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.fleet = Fleet()
        self.start = self.fleet.capture(0, 1_000_000_000)
        self.fleet.heads = [13, 14, 15, 13]
        self.end = self.fleet.capture(61_000_000_000, 63_000_000_000)

    def check(self):
        return audit.audit(self.start, self.end, self.fleet.rpc)

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

    def test_receipt_preflight_checks_each_node_and_fails_before_workload(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)/"capture.json"
            args = ["audit", "capture", "--rpc", ",".join(self.fleet.urls),
                    "--out", str(out), "--verify-receipts"]
            original = audit.capture
            with (patch.object(audit, "rpc", side_effect=self.fleet.rpc),
                  patch.object(audit, "capture", side_effect=lambda urls: original(urls, self.fleet.rpc, boot_id="test")),
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
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b: original(a, b, self.fleet.rpc)):
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
                   blockHash=digest(12), blockNumber="0xc", transactionHash=digest(120),
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
            audit.capture(self.fleet.urls, no_qc, boot_id="test")

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
            audit.capture(["node0"]*4, self.fleet.rpc, boot_id="test")
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
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b: original(a, b, self.fleet.rpc)):
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
                                (dict(validatorPublicKey=f"{100:096x}"), "duplicate validator")]:
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
                audit.audit(self.start, end, self.fleet.rpc)
        end = copy.deepcopy(self.end)
        for edge in ("read_before", "read_after"):
            end["nodes"][1][edge]["counters"] = copy.deepcopy(self.start["nodes"][1][edge]["counters"])
        with self.assertRaisesRegex(audit.AuditError, "no observed QMDB"):
            audit.audit(self.start, end, self.fleet.rpc)
        end["nodes"][1]["read_after"]["counters"]["accountReads"] = 0
        with self.assertRaisesRegex(audit.AuditError, "counter regressed"):
            audit.audit(self.start, end, self.fleet.rpc)
        end = copy.deepcopy(self.end)
        for edge in ("read_before", "read_after"):
            end["nodes"][1][edge]["instanceId"] = digest(900)
        with self.assertRaisesRegex(audit.AuditError, "identity changed"):
            audit.audit(self.start, end, self.fleet.rpc)

    def test_counter_envelope_covers_capture_time_and_reports_actual_deltas(self):
        self.start["nodes"][0]["read_after"]["counters"]["accountReads"] += 10
        self.end["nodes"][0]["read_before"]["counters"]["accountReads"] -= 20
        report = self.check()
        self.assertEqual(report["read_evidence"][0]["counters_delta"]["accountReads"], 6100)
        self.assertEqual(report["scope"], "h2_rpc_commits_qmdb_only_and_gov5_receipt_roots")

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
                audit.audit(self.start, end, self.fleet.rpc)

    def test_capture_rejects_restart_between_its_own_samples(self):
        def restarted(url, method, params):
            result = self.fleet.rpc(url, method, params)
            if method == "n42_stateReadStatus" and params[0] is not None:
                result["instanceId"] = digest(900)
            return result
        with self.assertRaisesRegex(audit.AuditError, "identity changed"):
            audit.capture(self.fleet.urls, restarted, boot_id="test")

    def test_cli_architecture_failure_saves_failed_report_even_with_low_tps_threshold(self):
        self.end["nodes"][0]["read_after"]["counters"]["readErrors"] += 1
        with tempfile.TemporaryDirectory() as directory:
            paths = [Path(directory)/name for name in ("start.json", "end.json", "out.json")]
            paths[0].write_text(json.dumps(self.start))
            paths[1].write_text(json.dumps(self.end))
            args = ["h2-tps-audit.py", "audit", "--start", str(paths[0]),
                    "--end", str(paths[1]), "--out", str(paths[2]), "--min-tps", "0.00001"]
            original = audit.audit
            with patch.object(audit, "audit", side_effect=lambda a, b: original(a, b, self.fleet.rpc)):
                with patch("sys.argv", args), self.assertRaises(SystemExit) as raised:
                    audit.main()
            self.assertEqual(raised.exception.code, 1)
            report = json.loads(paths[2].read_text())
            self.assertEqual(report["status"], "failed")
            self.assertIn("readErrors", report["error"])


if __name__ == "__main__":
    unittest.main()
