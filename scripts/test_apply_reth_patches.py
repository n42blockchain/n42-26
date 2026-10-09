import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("apply-reth-patches.sh").resolve()
PATCH_NAMES = (
    "reth-n42-perf",
    "reth-payload-transactions-root",
    "reth-qmdb-state-reader",
    "reth-import-batch",
    "reth-gov5-cached-state-root",
    "reth-engine-state-provider-factory",
)


class ApplyRethPatchesTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.reth = self.root / "reth"
        self.reth.mkdir()
        self.patches = self.root / "patches"
        self.patches.mkdir()
        subprocess.run(["git", "init", "-q", str(self.reth)], check=True)
        subprocess.run(["git", "-C", str(self.reth), "config", "user.email", "test@example.invalid"], check=True)
        subprocess.run(["git", "-C", str(self.reth), "config", "user.name", "Test"], check=True)
        self.paths = {}
        for index, name in enumerate(PATCH_NAMES):
            path = self.reth / f"src/{index}.rs"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"// upstream {index}\n")
            self.paths[name] = path
        subprocess.run(["git", "-C", str(self.reth), "add", "."], check=True)
        subprocess.run(["git", "-C", str(self.reth), "commit", "-qm", "base"], check=True)
        self.base = subprocess.check_output(["git", "-C", str(self.reth), "rev-parse", "HEAD"], text=True).strip()
        for index, name in enumerate(PATCH_NAMES):
            path = self.paths[name]
            original = path.read_text()
            path.write_text(original + f"// n42 patch {index}\n")
            diff = subprocess.check_output(["git", "-C", str(self.reth), "diff", "--", path.relative_to(self.reth).as_posix()], text=True)
            (self.patches / f"{name}.patch").write_text(diff)
            path.write_text(original)

    def tearDown(self):
        self.temp.cleanup()

    def invoke(self, *, base=None):
        env = os.environ.copy()
        env["N42_RETH_PATCH_DIR"] = str(self.patches)
        env["N42_RETH_BASE_REVISION"] = base or self.base
        env["N42_RETH_ADAPTED_REVISION"] = "adapted-revision-not-in-fixture"
        return subprocess.run(["bash", str(SCRIPT), str(self.reth)], env=env, text=True, capture_output=True)

    def contents(self):
        return {name: path.read_text() for name, path in self.paths.items()}

    def test_clean_base_applies_all_patches_and_second_run_is_idempotent(self):
        first = self.invoke()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertTrue(all("// n42 patch" in body for body in self.contents().values()))
        second = self.invoke()
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(second.stdout.count("already applied"), len(PATCH_NAMES))

    def test_dependent_patch_series_is_preflighted_and_applied_in_order(self):
        path = self.paths[PATCH_NAMES[0]]
        original = path.read_text()
        path.write_text(original + "// perf prerequisite\n")
        first = subprocess.check_output(["git", "-C", str(self.reth), "diff", "--", path.relative_to(self.reth).as_posix()], text=True)
        (self.patches / f"{PATCH_NAMES[0]}.patch").write_text(first)
        subprocess.run(["git", "-C", str(self.reth), "add", str(path.relative_to(self.reth))], check=True)
        path.write_text(path.read_text() + "// transaction root extension\n")
        second = subprocess.check_output(["git", "-C", str(self.reth), "diff", "--", path.relative_to(self.reth).as_posix()], text=True)
        (self.patches / f"{PATCH_NAMES[1]}.patch").write_text(second)
        subprocess.run(["git", "-C", str(self.reth), "reset", "-q", "HEAD", "--", str(path.relative_to(self.reth))], check=True)
        path.write_text(original)

        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("// perf prerequisite", path.read_text())
        self.assertIn("// transaction root extension", path.read_text())

    def test_partial_series_stages_an_untracked_prerequisite_in_temporary_index(self):
        relative = Path("src/cache.rs")
        path = self.reth / relative
        path.write_text("// cached output\n")
        subprocess.run(["git", "-C", str(self.reth), "add", str(relative)], check=True)
        first = subprocess.check_output(["git", "-C", str(self.reth), "diff", "--cached", "--", str(relative)], text=True)
        (self.patches / f"{PATCH_NAMES[0]}.patch").write_text(first)
        subprocess.run(["git", "-C", str(self.reth), "reset", "-q", "HEAD", "--", str(relative)], check=True)
        path.unlink()

        path.write_text("// cached output\n")
        subprocess.run(["git", "-C", str(self.reth), "add", str(relative)], check=True)
        path.write_text("// cached output\n// transaction root extension\n")
        second = subprocess.check_output(["git", "-C", str(self.reth), "diff", "--", str(relative)], text=True)
        (self.patches / f"{PATCH_NAMES[1]}.patch").write_text(second)
        subprocess.run(["git", "-C", str(self.reth), "reset", "-q", "HEAD", "--", str(relative)], check=True)
        path.unlink()

        subprocess.run(["git", "-C", str(self.reth), "apply", str(self.patches / f"{PATCH_NAMES[0]}.patch")], check=True)
        self.assertFalse(subprocess.run(["git", "-C", str(self.reth), "ls-files", "--error-unmatch", str(relative)], capture_output=True).returncode == 0)
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(path.read_text(), "// cached output\n// transaction root extension\n")

    def test_unknown_descendant_is_refused_without_changes(self):
        subprocess.run(["git", "-C", str(self.reth), "commit", "--allow-empty", "-qm", "unknown change"], check=True)
        before = self.contents()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly pinned base", result.stderr)
        self.assertEqual(self.contents(), before)

    def test_wrong_base_is_refused_without_changes(self):
        before = self.contents()
        result = self.invoke(base="deadbeef")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pinned base", result.stderr)
        self.assertEqual(self.contents(), before)

    def test_modified_context_is_refused_without_partial_changes(self):
        self.paths[PATCH_NAMES[1]].write_text("// incompatible source\n")
        before = self.contents()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.contents(), before)

    def test_missing_required_patch_is_refused_without_changes(self):
        (self.patches / f"{PATCH_NAMES[-1]}.patch").unlink()
        before = self.contents()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.contents(), before)


if __name__ == "__main__":
    unittest.main()
