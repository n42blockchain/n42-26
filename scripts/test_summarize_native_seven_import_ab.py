import json
import tempfile
import unittest
from pathlib import Path

from summarize_native_seven_import_ab import summarize


class ImportAbSummaryTest(unittest.TestCase):
    def make_campaign(self, root, rates, unmatched=0):
        campaign = Path(root)
        for tag, rate in zip(("import-a1", "import-b", "import-a2"), rates):
            result = campaign / f"result-{tag}"
            q = result / "qualification"
            q.mkdir(parents=True)
            (q / "summary.tsv").write_text(
                "metric\tvalue\n"
                "throughput_status\tpassed\n"
                "validators\t7\n"
                "failed_committed_transactions\t0\n"
                "successful_committed_transactions\t1000\n"
                f"successful_committed_tps\t{rate}\n"
                "measurement_seconds\t10\n"
                "trusted_config_sha256\t" + "c" * 64 + "\n"
            )
            (result / "timeline-score.json").write_text(json.dumps({
                "unmatched_audited_transaction_blocks": unmatched,
                "successful_commit_windows_15s": [250, 250, 250, 250],
                "stages": {},
            }))
            options = "--parallel-build" if tag != "import-b" else "--parallel-build --parallel-import"
            (result / "leg.tsv").write_text(f"tag\t{tag}\noptions\t{options}\n")
            (result / "binary-workload-sha256.txt").write_text(
                "a" * 64 + "  target/release/n42-node\n"
                + "b" * 64 + "  /tmp/presigned-48m-20260927.bin\n"
            )
            (result / "verify-after.log").write_text("PASS: 100 common blocks, all 7 roots/hashes agree\n")
        return campaign

    def test_candidate_requires_a_bookends_and_complete_audited_windows(self):
        with tempfile.TemporaryDirectory() as directory:
            report = summarize(self.make_campaign(directory, [100, 112, 102]))
            self.assertEqual(report["decision"], "candidate-improved")
            self.assertEqual(report["a_bookend_drift_pct"], 1.9801980198019802)

    def test_large_a_drift_is_inconclusive(self):
        with tempfile.TemporaryDirectory() as directory:
            report = summarize(self.make_campaign(directory, [90, 130, 110]))
            self.assertEqual(report["decision"], "inconclusive-bookend-drift")

    def test_unmatched_audited_hash_fails_the_campaign(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "unmatched"):
                summarize(self.make_campaign(directory, [100, 110, 102], unmatched=1))


if __name__ == "__main__":
    unittest.main()
