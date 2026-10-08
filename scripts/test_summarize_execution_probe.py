import json
import unittest
from summarize_execution_probe import summarize


def rows(durations=(100, 100, 80, 110), prefix='PROBE'):
    return [prefix + ' ' + json.dumps({'tag': tag, 'iteration': i, 'transactions': 100,
            'duration_ns': duration * 1_000_000, 'gas_used': 2_100_000})
            for tag, duration in zip(('warmup', 'a1', 'b', 'a2'), durations) for i in range(10)]


class ProbeTests(unittest.TestCase):
    def test_complete_bookends(self):
        result = summarize(rows(), 'PROBE')
        self.assertTrue(result['candidate_faster_than_both_bookends'])
        self.assertAlmostEqual(result['bookend_duration_drift_pct'], 10)
        self.assertEqual(result['legs']['b']['median_ms'], 80)
        self.assertNotIn('tps', result)

    def test_warmup_does_not_change_comparison(self):
        self.assertEqual(summarize(rows((1, 100, 80, 110)), 'PROBE')['candidate_duration_change_pct'],
                         summarize(rows((999, 100, 80, 110)), 'PROBE')['candidate_duration_change_pct'])

    def test_candidate_within_drift(self):
        self.assertFalse(summarize(rows((100, 100, 105, 110)), 'PROBE')['candidate_faster_than_both_bookends'])

    def test_missing_sample(self):
        with self.assertRaises(ValueError):
            summarize(rows()[:-1], 'PROBE')

    def test_duplicate_sample(self):
        data = rows()
        with self.assertRaises(ValueError):
            summarize(data + [data[0]], 'PROBE')

    def test_mixed_counts(self):
        data = rows()
        data[0] = data[0].replace('"transactions": 100', '"transactions": 101')
        with self.assertRaises(ValueError):
            summarize(data, 'PROBE')

    def test_negative_duration(self):
        data = rows()
        data[0] = data[0].replace('100000000', '-1')
        with self.assertRaises(ValueError):
            summarize(data, 'PROBE')

    def test_transfer_gas_checked(self):
        data = rows(prefix='TRANSFER_PROBE')
        summarize(data, 'TRANSFER_PROBE')
        with self.assertRaises(ValueError):
            summarize([line.replace('2100000', '1') for line in data], 'TRANSFER_PROBE')


if __name__ == '__main__':
    unittest.main()
