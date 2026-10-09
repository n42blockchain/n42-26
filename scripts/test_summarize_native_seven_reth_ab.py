import json
import tempfile
import unittest
from pathlib import Path

from scripts.summarize_native_seven_reth_ab import summarize


class RethAbSummaryTest(unittest.TestCase):
    tags = ("reth-a1", "reth-b", "reth-a2")

    def make_campaign(self, root, rates=(100, 110, 101), *, workload="b", config="c",
                      validators="7", failed="0", windows=None, restart=True):
        campaign = Path(root)
        for tag, rate in zip(self.tags, rates):
            result = campaign / f"result-{tag}"
            q = result / "qualification"
            q.mkdir(parents=True)
            leg_windows = windows or ([200, 300, 300, 200] if tag == "reth-b" else [250, 250, 250, 250])
            (q / "summary.tsv").write_text(
                "metric\tvalue\nthroughput_status\tpassed\n"
                f"validators\t{validators}\nfailed_committed_transactions\t{failed}\n"
                f"successful_committed_transactions\t{sum(leg_windows)}\n"
                f"successful_committed_tps\t{rate}\nmeasurement_seconds\t10\n"
                "trusted_config_sha256\t" + "c" * 64 + "\n"
            )
            (result / "timeline-score.json").write_text(json.dumps({
                "unmatched_audited_transaction_blocks": 0,
                "successful_commit_windows_15s": leg_windows,
                "stages": {"execution": {"p50_ms": 1}},
            }))
            (result / "leg.tsv").write_text(
                f"tag\t{tag}\noptions\t--parallel-build\nmax_txs_per_block\t100000\n"
                "gov5_reuse_builder_execution\t1\nrpc_max_response_mb\t512\n"
            )
            (result / "run-config-sha256.txt").write_text("".join(
                f"{digit * 64}  {name}\n" for digit, name in zip("12345", (
                    "manifest.json", "consensus.json", "trusted-config.json",
                    "test-accounts.json", "recipients.json"))
            ))
            source = "a" * 64 if tag != "reth-b" else "d" * 64
            (result / "binary-workload-sha256.txt").write_text(
                f"{source}  /immutable/{tag}/n42-node\n"
                f"{workload * 64}  /immutable/presigned-48m-20260927.bin\n"
            )
            (result / "source-manifest.json").write_text(json.dumps({
                "reth_revision": "old" if tag != "reth-b" else "new",
                "reth_source_sha256": "e" * 64 if tag != "reth-b" else "f" * 64,
                "lockfile_sha256": "1" * 64 if tag != "reth-b" else "2" * 64,
                "source_tree_sha256": "3" * 64 if tag != "reth-b" else "4" * 64,
            }))
            (result / "verify-after.log").write_text(
                "PASS: 100 common blocks, all 7 roots/hashes agree\n"
                + ("PASS: post-restart state agrees on all 7 validators\n" if restart else "")
            )
        return campaign

    def test_valid_campaign_reports_sustained_windows_and_upgrade_uplift(self):
        with tempfile.TemporaryDirectory() as directory:
            report = summarize(self.make_campaign(directory, rates=(100, 120, 102)))
        self.assertEqual(report["decision"], "candidate-improved")
        self.assertEqual(report["legs"]["reth-b"]["windows_15s"], [200, 300, 300, 200])
        self.assertGreater(report["b_sustained_uplift_pct"], 0)

    def test_large_a_bookend_drift_is_inconclusive(self):
        with tempfile.TemporaryDirectory() as directory:
            report = summarize(self.make_campaign(directory, rates=(80, 130, 120)))
        self.assertEqual(report["decision"], "inconclusive-bookend-drift")

    def test_changed_workload_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            campaign = self.make_campaign(directory)
            path = campaign / "result-reth-b" / "binary-workload-sha256.txt"
            path.write_text(path.read_text().replace("b" * 64, "c" * 64))
            with self.assertRaisesRegex(ValueError, "workload"):
                summarize(campaign)

    def test_changed_validator_configuration_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            campaign = self.make_campaign(directory)
            path = campaign / "result-reth-b" / "run-config-sha256.txt"
            path.write_text(path.read_text().replace("1" * 64, "f" * 64))
            with self.assertRaisesRegex(ValueError, "configuration hashes"):
                summarize(campaign)

    def test_missing_validator_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "seven validators"):
                summarize(self.make_campaign(directory, validators="6"))

    def test_failed_receipts_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "failed committed"):
                summarize(self.make_campaign(directory, failed="1"))

    def test_incomplete_windows_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "windows"):
                summarize(self.make_campaign(directory, windows=[250, 250]))

    def test_missing_post_restart_audit_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "restart"):
                summarize(self.make_campaign(directory, restart=False))


if __name__ == "__main__":
    unittest.main()
