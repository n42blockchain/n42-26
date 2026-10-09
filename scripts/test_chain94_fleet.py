"""Unit tests for read-only fleet acceptance and safe process ownership checks."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch, Mock
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("fleet", Path(__file__).with_name("chain94-fleet.py"))
fleet = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fleet)


class FleetTests(unittest.TestCase):
    def test_foreground_child_exit_fails_and_reaps_other_children(self):
        # Real, tiny Python subprocesses exercise lifecycle only: no node,
        # sockets, signing, compilation, or fleet workload is started.
        children = []

        def launch(_args, handles, _cancelled):
            for code in ("import time; time.sleep(60)", "raise SystemExit(7)"):
                child = subprocess.Popen([sys.executable, "-c", code],
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL, start_new_session=True)
                children.append(child)
                handles.append(child)

        try:
            with patch.object(fleet, "_start", side_effect=launch), patch("builtins.print"):
                with self.assertRaisesRegex(RuntimeError, "node1 exited with status 7"):
                    fleet.start(SimpleNamespace(foreground=True))
            self.assertTrue(all(child.poll() is not None for child in children))
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()

    def test_partial_start_failure_cleans_up_even_without_foreground(self):
        child = Mock()
        child.poll.side_effect = [None, 0]

        def launch(_args, handles, _cancelled):
            handles.append(child)
            raise OSError("second node spawn failed")

        with patch.object(fleet, "_start", side_effect=launch):
            with self.assertRaisesRegex(OSError, "second node spawn failed"):
                fleet.start(SimpleNamespace(foreground=False))
        child.send_signal.assert_called_once_with(fleet.signal.SIGINT)
        child.wait.assert_called_once()

    def test_foreground_cancellation_waits_and_restores_handlers(self):
        child = Mock()
        child.poll.side_effect = [None, None, 0]
        original = {sig: fleet.signal.getsignal(sig)
                    for sig in (fleet.signal.SIGINT, fleet.signal.SIGTERM)}

        def launch(_args, handles, _cancelled):
            handles.append(child)
            fleet.signal.getsignal(fleet.signal.SIGTERM)(fleet.signal.SIGTERM, None)

        with patch.object(fleet, "_start", side_effect=launch), \
                patch.object(fleet.time, "sleep"), patch("builtins.print"):
            with self.assertRaises(SystemExit) as result:
                fleet.start(SimpleNamespace(foreground=True))
        self.assertEqual(result.exception.code, 128 + fleet.signal.SIGTERM)
        child.send_signal.assert_called_once_with(fleet.signal.SIGINT)
        child.wait.assert_called_once()
        for sig, handler in original.items():
            self.assertEqual(fleet.signal.getsignal(sig), handler)

    def test_slow_shutdown_retains_ownership_until_exit(self):
        child = Mock()
        child.poll.side_effect = [None, None, None, 0]
        with patch.object(fleet.time, "monotonic", side_effect=[0, 61]), \
                patch.object(fleet.time, "sleep"), patch("builtins.print") as output:
            fleet.shutdown_children([child])
        output.assert_called_once()
        child.wait.assert_called_once()
        child.kill.assert_not_called()

    def test_explicit_execution_options_reach_every_spawned_node(self):
        # Reproduce the upstream harness failure: inherited experiment flags
        # are cleared, so requested options must be installed after that clear.
        for fast, parallel, importing, reads in [(True, True, True, "only"), (False, False, False, "verify"), (False, True, False, "only"), (False, False, True, "only")]:
            with self.subTest(fast=fast, parallel=parallel, reads=reads), tempfile.TemporaryDirectory() as directory:
                runtime = Path(directory)
                (runtime / "logs").mkdir()
                (runtime / "pids").mkdir()
                binary = runtime / "node-binary"
                binary.write_bytes(b"not executed")
                fleet.write_json(runtime / "consensus.json", dict(slot_time_ms=3000))
                fleet.write_json(runtime / "manifest.json", dict(
                    artifacts_sha256={}, ports=fleet.BASES, genesis_hash="genesis",
                    peers=[f"peer{i}" for i in range(7)],
                    snapshot=dict(block_number=42, block_hash="base", state_root="root"),
                ))
                args = SimpleNamespace(runtime=runtime, binary=binary, rpc_max_response_mb=512,
                                       fast_transfers=fast, parallel_build=parallel, parallel_import=importing, qmdb_reads=reads)
                with patch.object(fleet, "owned_process", return_value=None), \
                        patch.object(fleet.socket, "socket"), \
                        patch.object(fleet, "process_identity", return_value="owned"), \
                        patch.object(fleet.subprocess, "Popen", return_value=Mock(pid=123)) as spawn, \
                        patch.dict(fleet.os.environ, {"N42_PARALLEL_IMPORT": "stale", "N42_PARALLEL_BUILD": "stale", "N42_FAST_TRANSFER": "stale", "N42_QMDB_READS": "off", "N42_SKIP_TX_VERIFY": "1", "N42_GOV5_REUSE_BUILDER_EXECUTION": "0"}), \
                        patch("builtins.print"):
                    fleet.start(args)
                self.assertEqual(spawn.call_count, 7)
                for call in spawn.call_args_list:
                    command = call.args[0]
                    self.assertEqual(command[command.index("--rpc.max-response-size") + 1], "512")
                    self.assertEqual(command[command.index("--engine.persistence-threshold") + 1], "0")
                    self.assertEqual(command[command.index("--engine.num-state-masking-blocks") + 1], "0")
                    self.assertEqual(command[command.index("--engine.memory-block-buffer-target") + 1], "0")
                    env = call.kwargs["env"]
                    self.assertEqual(env["N42_FAST_TRANSFER"], "1" if fast else "0")
                    self.assertEqual(env["N42_PARALLEL_BUILD"], "1" if parallel else "0")
                    self.assertEqual(env["N42_PARALLEL_IMPORT"], "1" if importing else "0")
                    self.assertEqual(env["N42_QMDB_READS"], reads)
                    self.assertEqual(env["N42_GOV5_REUSE_BUILDER_EXECUTION"], "0")
                    self.assertNotIn("N42_SKIP_TX_VERIFY", env)
                    self.assertEqual(env["N42_GOV5_LEGACY_SIGNING"], "1")
                    self.assertEqual(env["N42_LOW_MEMORY"], "1")
                    self.assertEqual(env["N42_DISABLE_TX_FORWARD"], "0")
                recorded = fleet.read_json(runtime / "run.json")
                self.assertEqual(recorded["immediate_persistence"], dict(
                    persistence_threshold=0, state_masking_blocks=0, memory_block_buffer_target=0
                ))
                self.assertEqual(recorded["fast_transfers"], fast)
                self.assertEqual(recorded["parallel_build"], parallel)
                self.assertEqual(recorded["parallel_import"], importing)
                self.assertEqual(recorded["qmdb_reads"], reads)
                self.assertEqual(recorded["gov5_reuse_builder_execution"], "0")
                self.assertEqual(recorded["rpc_max_response_mb"], 512)

    def test_fresh_four_node_launch_preserves_cancun_and_full_validation(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            for sub in ["logs", "pids", "artifacts"]:
                (runtime / sub).mkdir()
            binary = runtime / "binary"
            binary.write_bytes(b"not executed")
            validators = [dict(address=f"a{i}", bls_public_key=f"key{i}") for i in range(4)]
            fleet.write_json(runtime / "consensus.json", dict(slot_time_ms=200, validator_set_size=4,
                fault_tolerance=1, initial_validators=validators))
            fleet.write_json(runtime / "artifacts/genesis.json", dict(config=dict(hotstuff=dict(
                validators=[dict(address=v["address"], blsKey=v["bls_public_key"]) for v in validators]))))
            fleet.write_json(runtime / "manifest.json", dict(node_count=4, profile="fresh-native-h2-qmdb-cancun",
                peers=[f"peer{i}" for i in range(4)], artifacts_sha256={}, ports=fleet.BASES,
                genesis_hash="genesis", prague_time=None, gas_limit=5000000000,
                max_txs_per_block=220000, build_budget_ms=200,
                snapshot=dict(block_number=0, block_hash="genesis", state_root="root")))
            args = SimpleNamespace(runtime=runtime, binary=binary, fast_transfers=True,
                parallel_build=True, parallel_import=True, qmdb_reads="only", disable_tx_forward=True)
            with patch.object(fleet, "owned_process", return_value=None), \
                    patch.object(fleet.socket, "socket"), \
                    patch.object(fleet, "process_identity", return_value="owned"), \
                    patch.object(fleet.subprocess, "Popen", return_value=Mock(pid=123)) as spawn, \
                    patch.dict(fleet.os.environ, {"N42_GOV5_PRAGUE_TIME":"stale", "N42_SKIP_TX_VERIFY":"1"}), \
                    patch("builtins.print"):
                fleet.start(args)
            self.assertEqual(spawn.call_count, 4)
            for call in spawn.call_args_list:
                env = call.kwargs["env"]
                self.assertNotIn("N42_GOV5_PRAGUE_TIME", env)
                self.assertNotIn("N42_SKIP_TX_VERIFY", env)
                self.assertEqual(env["N42_GOV5_H2_PARTICIPANT"], "1")
                self.assertEqual(env["N42_GOV5_LEGACY_SIGNING"], "0")
                self.assertEqual(env["N42_QMDB_READS"], "only")
                self.assertEqual(env["N42_PARALLEL_IMPORT"], "1")
                self.assertEqual(env["N42_MAX_TXS_PER_BLOCK"], "220000")
                self.assertEqual(env["N42_LOW_MEMORY"], "0")
                self.assertEqual(env["N42_DISABLE_TX_FORWARD"], "1")
                self.assertIn("--builder.gaslimit", call.args[0])
                self.assertEqual(len(env["N42_TRUSTED_PEERS"].split(",")), 3)

            recorded = fleet.read_json(runtime / "run.json")
            self.assertFalse(recorded["low_memory"])
            self.assertTrue(recorded["disable_tx_forward"])

    def test_fresh_seven_node_launch_uses_funded_throughput_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = Path(directory)
            for sub in ["logs", "pids", "artifacts"]:
                (runtime / sub).mkdir()
            binary = runtime / "binary"
            binary.write_bytes(b"not executed")
            validators = [dict(address=f"a{i}", bls_public_key=f"key{i}") for i in range(7)]
            fleet.write_json(runtime / "consensus.json", dict(slot_time_ms=200,
                validator_set_size=7, fault_tolerance=2, initial_validators=validators))
            fleet.write_json(runtime / "artifacts/genesis.json", dict(config=dict(hotstuff=dict(
                validators=[dict(address=v["address"], blsKey=v["bls_public_key"]) for v in validators]))))
            fleet.write_json(runtime / "manifest.json", dict(node_count=7, profile="fresh-native-h2-qmdb-cancun",
                peers=[f"peer{i}" for i in range(7)], artifacts_sha256={}, ports=fleet.BASES,
                genesis_hash="genesis", prague_time=None, gas_limit=5_000_000_000,
                max_txs_per_block=220_000, build_budget_ms=200,
                snapshot=dict(block_number=0, block_hash="genesis", state_root="root")))
            args = SimpleNamespace(runtime=runtime, binary=binary, fast_transfers=False,
                parallel_build=False, parallel_import=False, qmdb_reads="only", disable_tx_forward=True)
            with patch.object(fleet, "owned_process", return_value=None), \
                    patch.object(fleet.socket, "socket"), \
                    patch.object(fleet, "process_identity", return_value="owned"), \
                    patch.object(fleet.subprocess, "Popen", return_value=Mock(pid=123)) as spawn, \
                    patch("builtins.print"):
                fleet.start(args)
            self.assertEqual(spawn.call_count, 7)
            for call in spawn.call_args_list:
                self.assertEqual(call.kwargs["env"]["N42_GOV5_LEGACY_SIGNING"], "0")
                self.assertEqual(call.kwargs["env"]["N42_LOW_MEMORY"], "0")
                self.assertEqual(call.kwargs["env"]["N42_DISABLE_TX_FORWARD"], "1")
                self.assertIn("--builder.gaslimit", call.args[0])
            self.assertFalse(fleet.read_json(runtime / "run.json")["low_memory"])

    def test_old_snapshot_cannot_be_truncated_into_four_nodes(self):
        with self.assertRaisesRegex(ValueError, "newly generated native genesis"):
            fleet.fleet_size(dict(node_count=4, peers=["p"]*4, snapshot=dict(block_number=42)))

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
        with patch.object(fleet, "read_json", return_value=dict(ports=dict(http=22400), peers=[f"peer{i}" for i in range(7)])), \
                patch.object(fleet, "rpc", side_effect=rpc):
            sample = fleet.sample(Path("/not-opened"))
        self.assertFalse(sample["identical"])
        self.assertEqual(sample["comparison_height"], 100)


if __name__ == "__main__":
    unittest.main()
