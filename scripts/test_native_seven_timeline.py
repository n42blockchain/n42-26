import json
import tempfile
import unittest
from pathlib import Path

from native_seven_timeline import build_timeline


class TimelineTest(unittest.TestCase):
    def test_only_audited_transaction_hashes_count_in_commit_windows(self):
        with tempfile.TemporaryDirectory() as directory:
            campaign = Path(directory)
            q = campaign / "result-probe/qualification"
            q.mkdir(parents=True)
            (q / "h2-audit.json").write_text(json.dumps({"blocks": [
                {"hash": "0xaaaa", "successful_transactions": 10},
                {"hash": "0xbbbb", "successful_transactions": 20},
                {"hash": "0xcccc", "successful_transactions": 0},
            ]}))
            (q / "ingest-start.ns").write_text("1000000000000000000\n")
            logs = campaign / "runtime-probe/logs"
            logs.mkdir(parents=True)
            (logs / "node0.log").write_text(
                "2001-09-09T01:46:45Z INFO N42_CADENCE: inter-block commit interval block_hash=0xaaaa\n"
                "2001-09-09T01:47:02Z INFO N42_CADENCE: inter-block commit interval block_hash=0xbbbb\n"
                "2001-09-09T01:46:46Z INFO N42_CADENCE: inter-block commit interval block_hash=0xdddd\n"
                "2001-09-09T01:46:47Z INFO N42_CADENCE: inter-block commit interval block_hash=0xcccc\n"
                "2001-09-09T01:46:47Z INFO N42_TIMEOUT_VIEW: leader_build_start view=7\n"
                "2001-09-09T01:46:47Z INFO N42_PAYLOAD_PACK: tx packing complete tx_count=10 packing_ms=45\n"
                "2001-09-09T01:46:47Z INFO N42_FINISH_BREAKDOWN: builder.finish() total_finish_ms=30\n"
                "2001-09-09T01:46:48Z INFO N42_CADENCE: build_start->broadcast hash=0xaaaa build_start_to_broadcast_ms=110\n"
                "2001-09-09T01:46:49Z INFO N42_TIMEOUT_VIEW: leader_build_start view=8\n"
                "2001-09-09T01:46:49Z INFO N42_PAYLOAD_PACK: tx packing complete tx_count=0 packing_ms=99\n"
                "2001-09-09T01:46:50Z INFO N42_CADENCE: build_start->broadcast hash=0xdddd build_start_to_broadcast_ms=120\n"
                "2001-09-09T01:47:03Z INFO N42_CADENCE: build_start->broadcast hash=0xbbbb build_start_to_broadcast_ms=130\n"
            )
            (logs / "node1.log").write_text(
                "2001-09-09T01:46:49Z INFO N42_FOLLOWER_IMPORT: block_data->accepted hash=0xaaaa follower_import_ms=70\n"
            )
            result = build_timeline(campaign, "probe")
            self.assertEqual(result["successful_commit_windows_15s"][:3], [10, 20, 0])
            self.assertEqual(result["unmatched_audited_transaction_blocks"], 0)
            self.assertEqual([block["hash"] for block in result["blocks"]], ["0xaaaa", "0xbbbb"])
            self.assertEqual(result["blocks"][0]["leader_broadcast_ms"], 110)
            self.assertEqual(result["blocks"][0]["packing_ms"], 45)
            self.assertEqual(result["blocks"][0]["builder_finish_ms"], 30)
            self.assertEqual(result["blocks"][0]["follower_import_ms"], {"node1": 70})
            self.assertEqual(result["blocks"][1]["leader_broadcast_ms"], 130)
            self.assertIsNone(result["blocks"][1]["packing_ms"])
            self.assertEqual(result["stages"]["packing_ms"]["missing_count"], 1)

    def test_missing_node0_commit_is_reported_and_other_node_cannot_fill_it(self):
        with tempfile.TemporaryDirectory() as directory:
            campaign = Path(directory)
            q = campaign / "result-probe/qualification"
            q.mkdir(parents=True)
            (q / "h2-audit.json").write_text(json.dumps({"blocks": [
                {"hash": "0xaaaa", "successful_transactions": 5},
            ]}))
            (q / "ingest-start.ns").write_text("1000000000000000000\n")
            logs = campaign / "runtime-probe/logs"
            logs.mkdir(parents=True)
            (logs / "node0.log").write_text("")
            (logs / "node1.log").write_text(
                "2001-09-09T01:46:45Z INFO N42_CADENCE: inter-block commit interval block_hash=0xaaaa\n"
            )
            result = build_timeline(campaign, "probe")
            self.assertEqual(result["successful_commit_windows_15s"][0], 0)
            self.assertEqual(result["unmatched_audited_transaction_blocks"], 1)


if __name__ == "__main__":
    unittest.main()
