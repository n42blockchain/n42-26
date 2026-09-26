import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("run-native-seven-220k-baseline.sh")


class BaselineControllerTest(unittest.TestCase):
    def test_refuses_to_create_artifacts_without_the_shared_claim(self):
        with tempfile.TemporaryDirectory() as directory:
            campaign = Path(directory) / "campaign"
            env = dict(os.environ, N42_QUIET_CLAIM="0",
                       N42_SEVEN_CAMPAIGN_DIR=str(campaign))
            result = subprocess.run(["bash", str(SCRIPT)], env=env,
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 2)
            self.assertIn("shared box claim", result.stderr)
            self.assertFalse(campaign.exists())


if __name__ == "__main__":
    unittest.main()
