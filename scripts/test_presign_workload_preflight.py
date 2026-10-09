import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("preflight", Path(__file__).with_name("presign-workload-preflight.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class WorkloadPreflightTests(unittest.TestCase):
    def test_capacity_chain_and_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "txs.bin"
            # Minimal framing fixture; this helper deliberately does not check signatures.
            path.write_bytes(module.HEADER.pack(b"N42T", 2, 941004, 1, 3)
                             + struct.pack("<Q", 3) + (b"\x01\x00\x02" + bytes(20)) * 3)
            result = module.inspect(path, 941004, 1, "2.1")
            self.assertEqual(result["minimumTransactions"], 3)
            self.assertEqual(result["transactions"], 3)
            self.assertEqual(len(result["sha256"]), 64)
            for chain, seconds, rate in [(94, 1, 1), (941004, 60, 1000000),
                                          (941004, 0, 1), (941004, 1, "NaN")]:
                with self.assertRaises(ValueError):
                    module.inspect(path, chain, seconds, rate)

    def test_bad_header_and_impossible_counts(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.bin"
            for raw in [b"N42T", module.HEADER.pack(b"N42T", 1, 1, 1, 1),
                        module.HEADER.pack(b"N42T", 2, 1, 0, 1),
                        module.HEADER.pack(b"N42T", 2, 1, 1, 60_000_000)]:
                path.write_bytes(raw)
                with self.assertRaises(ValueError):
                    module.inspect(path, 1, 1, 1)


if __name__ == "__main__":
    unittest.main()
