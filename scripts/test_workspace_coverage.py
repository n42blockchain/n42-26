"""Behavior tests for the coverage gate; run with unittest discovery."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "workspace_coverage", Path(__file__).with_name("workspace-coverage.py")
)
coverage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coverage)


def report(*rows):
    return {"data": [{"files": [
        {"filename": name, "summary": {"lines": {"count": count, "covered": covered}}}
        for name, count, covered in rows
    ]}]}


class CoverageGateTests(unittest.TestCase):
    def test_weights_lines_instead_of_averaging_file_percentages(self):
        summary = coverage.summarize(report(("large.rs", 90, 60), ("small.rs", 10, 10)))
        self.assertEqual(summary["lines"], 100)
        self.assertEqual(summary["covered"], 70)
        self.assertEqual(coverage.gate_status(summary, 70, 0), 0)

    def test_does_not_round_a_below_threshold_result_up_to_success(self):
        summary = coverage.summarize(report(("lib.rs", 100_000, 69_999)))
        self.assertEqual(coverage.gate_status(summary, 70, 0), 1)

    def test_failed_tests_cannot_pass_even_with_full_coverage(self):
        summary = coverage.summarize(report(("lib.rs", 10, 10)))
        self.assertNotEqual(coverage.gate_status(summary, 70, 101), 0)

    def test_rejects_missing_empty_or_invalid_coverage(self):
        for data in [{}, {"data": []}, report(), report(("lib.rs", 0, 0)),
                     report(("lib.rs", 1, 2)), report(("lib.rs", -1, 0)),
                     report(("lib.rs", 2, -1))]:
            with self.subTest(data=data), self.assertRaises(ValueError):
                coverage.summarize(data)

    def test_duplicate_file_cannot_inflate_coverage(self):
        with self.assertRaises(ValueError):
            coverage.summarize(report(("lib.rs", 10, 10), ("lib.rs", 10, 10)))

    def test_collects_only_this_builds_workspace_executables(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "messages.jsonl"
            artifact = {"reason": "compiler-artifact", "package_id": "ours",
                        "executable": "/target/ours"}
            log.write_text("\n".join([
                "running 10 tests", json.dumps(artifact), json.dumps(artifact),
                json.dumps({**artifact, "package_id": "dependency", "executable": "/target/dep"}),
                json.dumps({**artifact, "executable": None}),
                json.dumps({"reason": "build-finished", "success": True}),
            ]))
            self.assertEqual(coverage.collect_objects(log, {"ours"}), ["/target/ours"])

    def test_rejects_an_incomplete_build_even_if_it_produced_objects(self):
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "messages.jsonl"
            log.write_text(json.dumps({"reason": "build-finished", "success": False}))
            with self.assertRaises(ValueError):
                coverage.collect_objects(log, {"ours"})


if __name__ == "__main__":
    unittest.main()
