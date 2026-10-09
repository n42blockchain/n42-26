"""Behavior tests for preserving a dirty Reth checkout without mutating it."""

import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("prepare-reth-upgrade.py")
SPEC = importlib.util.spec_from_file_location("prepare_reth_upgrade", SCRIPT)
prepare_reth_upgrade = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare_reth_upgrade)


def git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args], check=True, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    return result.stdout.strip()


class PrepareRethUpgradeTests(unittest.TestCase):
    def make_repo(self, root: Path) -> str:
        root.mkdir()
        git(root, "init", "-q")
        git(root, "config", "user.name", "Test")
        git(root, "config", "user.email", "test@example.invalid")
        tracked = root / "tracked.txt"
        tracked.write_text("before\n")
        git(root, "add", "tracked.txt")
        git(root, "commit", "-qm", "baseline")
        return git(root, "rev-parse", "HEAD")

    def test_refuses_an_absent_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "missing-reth"
            with self.assertRaises(FileNotFoundError):
                prepare_reth_upgrade.prepare(root, Path(directory) / "snapshot")

    def test_refuses_a_non_git_source(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "not-a-repository"
            root.mkdir()
            with self.assertRaisesRegex(ValueError, "Git repository"):
                prepare_reth_upgrade.prepare(root, Path(directory) / "snapshot")

    def test_records_head_and_preserves_a_tracked_edit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "reth"
            expected_head = self.make_repo(root)
            (root / "tracked.txt").write_text("after\n")
            destination = Path(directory) / "snapshot"

            manifest = prepare_reth_upgrade.prepare(root, destination)

            self.assertEqual(manifest["head"], expected_head)
            self.assertEqual(manifest["entry_count"], 1)
            self.assertEqual(manifest["entries"][0]["path"], "tracked.txt")
            self.assertTrue(manifest["entries"][0]["captured"])
            patch_text = (destination / "current-dirty.patch").read_text()
            self.assertIn("-before", patch_text)
            self.assertIn("+after", patch_text)
            self.assertEqual((root / "tracked.txt").read_text(), "after\n")

    def test_copies_untracked_source_but_skips_target_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "reth"
            expected_head = self.make_repo(root)
            source_file = root / "crates/storage/storage-api/src/n42_state.rs"
            source_file.parent.mkdir(parents=True)
            source_file.write_text("pub struct N42State;\n")
            (root / "target").symlink_to(Path(directory) / "external-build")
            destination = Path(directory) / "snapshot"
            before_status = git(root, "status", "--porcelain=v1", "--untracked-files=all")

            manifest = prepare_reth_upgrade.prepare(root, destination)

            self.assertEqual(manifest["head"], expected_head)
            by_path = {entry["path"]: entry for entry in manifest["entries"]}
            self.assertEqual(len(by_path), 2)
            self.assertTrue(by_path["crates/storage/storage-api/src/n42_state.rs"]["captured"])
            self.assertFalse(by_path["target"]["captured"])
            self.assertEqual(by_path["target"]["reason"], "build_output_or_symlink")
            copied = destination / "untracked-source/crates/storage/storage-api/src/n42_state.rs"
            self.assertEqual(copied.read_text(), "pub struct N42State;\n")
            self.assertFalse((destination / "untracked-source/target").is_symlink())
            self.assertEqual(json.loads((destination / "current-source-manifest.json").read_text()), manifest)
            self.assertEqual(git(root, "status", "--porcelain=v1", "--untracked-files=all"), before_status)

    def test_refuses_existing_destination(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "reth"
            self.make_repo(root)
            destination = Path(directory) / "snapshot"
            destination.mkdir()
            marker = destination / "keep.txt"
            marker.write_text("do not overwrite\n")

            with self.assertRaisesRegex(FileExistsError, "already exists"):
                prepare_reth_upgrade.prepare(root, destination)

            self.assertEqual(marker.read_text(), "do not overwrite\n")


if __name__ == "__main__":
    unittest.main()
