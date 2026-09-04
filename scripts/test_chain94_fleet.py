"""Unit tests for read-only fleet acceptance and safe process ownership checks."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("fleet", Path(__file__).with_name("chain94-fleet.py"))
fleet = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fleet)


class FleetTests(unittest.TestCase):
    def test_pid_reuse_is_not_owned(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            (runtime / "pids").mkdir()
            fleet.write_json(runtime / "pids/node0.json", dict(pid=123, starttime="old"))
            with patch.object(fleet, "process_identity", return_value="new"):
                self.assertIsNone(fleet.owned_process(runtime, 0))

    def test_wrong_datadir_is_never_signalled(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            (runtime / "pids").mkdir()
            fleet.write_json(runtime / "pids/node0.json", dict(pid=123, starttime="same"))
            with patch.object(fleet, "process_identity", return_value="same"), \
                    patch.object(Path, "read_bytes", return_value=b"n42-node\x00--datadir\x00/another/fleet\x00"):
                with self.assertRaisesRegex(ValueError, "refusing to signal"):
                    fleet.owned_process(runtime, 0)

    def test_committed_hash_stays_pinned_during_fcu(self):
        status = dict(latestCommittedBlockHash="0x1234")
        block = dict(number="0x10", hash="0x1234")
        with patch.object(fleet, "rpc", side_effect=[None, block]) as rpc, \
                patch.object(fleet.time, "sleep"):
            self.assertEqual(fleet.committed_block((22400, status)), block)
        self.assertEqual(rpc.call_args_list[0], rpc.call_args_list[1])

    def test_missing_execution_does_not_pass_after_grace(self):
        with patch.object(fleet, "rpc", return_value=None), \
                patch.object(fleet.time, "monotonic", side_effect=[0, 3]):
            with self.assertRaisesRegex(ValueError, "no executed block"):
                fleet.committed_block((22400, dict(latestCommittedBlockHash="0x1234")))

    def test_sampling_compares_same_height_and_rejects_root_divergence(self):
        def rpc(port, method, params):
            if method == "eth_blockNumber":
                return hex(100 + port % 7)
            if method == "n42_consensusStatus":
                return dict(hasCommittedQc=True, validatorCount=7,
                            latestCommittedBlockHash="committed", latestCommittedView=80)
            if method == "eth_getBlockByHash":
                return dict(number="0x64")
            self.assertEqual(params, ["0x64", False])
            return dict(hash="block", stateRoot="bad" if port == 22403 else "root",
                        transactionsRoot="tx", receiptsRoot="receipts")
        with patch.object(fleet, "read_json", return_value=dict(ports=dict(http=22400))), \
                patch.object(fleet, "rpc", side_effect=rpc):
            sample = fleet.sample(Path("/not-opened"))
        self.assertFalse(sample["identical"])
        self.assertEqual(sample["comparison_height"], 100)


if __name__ == "__main__":
    unittest.main()
