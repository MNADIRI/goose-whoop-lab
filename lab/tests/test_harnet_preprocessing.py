import importlib.util
from pathlib import Path
import unittest
import numpy as np

spec = importlib.util.spec_from_file_location('harnet_eval', Path(__file__).parents[1] / 'sport/harnet_eval.py')
har = importlib.util.module_from_spec(spec)
spec.loader.exec_module(har)

class ResamplingTests(unittest.TestCase):
    def test_rejects_aliasing_but_preserves_activity_band(self):
        t = np.arange(4000) / 100
        low = np.sin(2 * np.pi * 2 * t)
        high = np.sin(2 * np.pi * 24 * t)
        a = np.vstack([low, high, np.ones_like(t)])
        result = har.windows(t, a, a)
        self.assertTrue(result)
        aw, _ = result[0]
        self.assertEqual(aw.shape, (3, 300))
        self.assertGreater(np.std(aw[0]), .6)
        self.assertLess(np.std(aw[1]), .03)

    def test_does_not_join_short_segments_across_gap(self):
        t = np.r_[np.arange(800)/100, 20 + np.arange(800)/100]
        a = np.ones((3, len(t)))
        self.assertEqual(har.windows(t, a, a), [])

    def test_nonmonotonic_segment_is_rejected(self):
        t = np.arange(2000)/100
        t[100] = t[99]
        a = np.ones((3, len(t)))
        self.assertEqual(har.windows(t, a, a), [])

if __name__ == '__main__':
    unittest.main()
