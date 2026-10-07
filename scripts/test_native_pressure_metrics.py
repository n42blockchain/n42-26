"""Small parser tests; these do not start a fleet or a pressure workload."""

import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


collector = module("collect_native_pressure_metrics")
summary = module("summarize_native_pressure_metrics")


class MetricsTests(unittest.TestCase):
    def test_labels_with_spaces_and_nonfinite_values(self):
        body = b'n42_engine_wait_ms_sum{phase="payload resolve"} 20\nn42_engine_wait_ms_count{phase="payload resolve"} 4\nn42_qmdb_bad NaN\nother_metric 5\n'
        with patch.object(collector, "urlopen", return_value=io.BytesIO(body)):
            row = collector.scrape(23600)
        self.assertNotIn("error", row)
        self.assertEqual(row["metrics"], {
            'n42_engine_wait_ms_sum{phase="payload resolve"}': 20,
            'n42_engine_wait_ms_count{phase="payload resolve"}': 4,
        })

    def test_scrape_failure_is_explicit(self):
        with patch.object(collector, "urlopen", side_effect=TimeoutError("timeout")):
            row = collector.scrape(23600)
        self.assertIn("timeout", row["error"])
        self.assertEqual(row["metrics"], {})

    def summarize(self, values):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metrics.jsonl"
            path.write_text("".join(json.dumps({"nodes": [{"port": 23600, "time_ns": i * 1_000_000_000, "metrics": metrics}]}) + "\n" for i, metrics in enumerate(values)))
            return summary.summarize(path)["nodes"]["23600"]

    def test_histogram_mean_and_missing_values(self):
        result = self.summarize([
            {'n42_engine_wait_ms_sum{phase="resolve"}': 10, 'n42_engine_wait_ms_count{phase="resolve"}': 2},
            {},
            {'n42_engine_wait_ms_sum{phase="resolve"}': 30, 'n42_engine_wait_ms_count{phase="resolve"}': 6},
        ])
        self.assertEqual(result["histogram_means"]['n42_engine_wait_ms{phase="resolve"}'], {"observations": 4, "mean": 5})
        self.assertEqual(result["metrics"]['n42_engine_wait_ms_sum{phase="resolve"}']["samples"], 2)

    def test_counter_reset_is_not_reported_as_a_gain(self):
        result = self.summarize([{"n42_ingest_accepted_total": 20}, {"n42_ingest_accepted_total": 1}, {"n42_ingest_accepted_total": 30}])
        self.assertIsNone(result["metrics"]["n42_ingest_accepted_total"]["delta"])

    def test_misaligned_histograms_have_no_mean(self):
        result = self.summarize([{"n42_engine_wait_ms_sum": 10, "n42_engine_wait_ms_count": 2}, {"n42_engine_wait_ms_sum": 20}, {"n42_engine_wait_ms_sum": 30, "n42_engine_wait_ms_count": 6}])
        self.assertEqual(result["histogram_means"], {})

    def test_measurement_window_excludes_empty_block_audit_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metrics.jsonl"
            path.write_text("".join(json.dumps({"nodes": [{"port": 23600, "time_ns": i,
                "metrics": {"n42_engine_wait_ms_sum": total, "n42_engine_wait_ms_count": count}}]}) + "\n"
                for i, total, count in [(1, 0, 0), (2, 10, 1), (3, 30, 2), (4, 30, 100)]))
            result = summary.summarize(path, 2, 3)
            self.assertEqual(result["snapshots"], 2)
            self.assertEqual(result["nodes"]["23600"]["histogram_means"]["n42_engine_wait_ms"]["mean"], 20)

    def test_reversed_window_is_rejected(self):
        with self.assertRaises(ValueError):
            summary.summarize(Path("unused"), 3, 2)


if __name__ == "__main__":
    unittest.main()
