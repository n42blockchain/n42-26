"""Keep every CI Reth source checkout compatible with the patch lock."""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]


class RethCiBaselineTest(unittest.TestCase):
    def test_every_reth_checkout_uses_locked_upstream(self):
        lock = json.loads((ROOT / 'reth-source.lock').read_text())
        found = 0
        for path in (ROOT / '.github/workflows').glob('*.yml'):
            text = path.read_text()
            for block in re.split(r'(?m)^      - ', text):
                if not re.search(r'(?m)^          path: reth\s*$', block):
                    continue
                found += 1
                with self.subTest(workflow=path.name, checkout=found):
                    self.assertIn('repository: paradigmxyz/reth', block)
                    self.assertRegex(block, r'ref: ' + re.escape(lock['upstream_commit']) + r'(?:\s|$)')
            if 'path: reth' in text:
                with self.subTest(workflow=path.name):
                    self.assertIn('scripts/apply-reth-patches.sh reth', text)
        self.assertGreater(found, 0, 'no Reth source checkout was audited')


if __name__ == '__main__':
    unittest.main()
