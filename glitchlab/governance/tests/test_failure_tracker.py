import unittest
from glitchlab.governance.failure_tracker import FailureTracker
import time

class TestFailureTracker(unittest.TestCase):

    def test_record_failure(self):
        tracker = FailureTracker(max_failures=2, window_seconds=10)
        self.assertEqual(len(tracker.failures), 0)
        tracker.record_failure()
        self.assertEqual(len(tracker.failures), 1)
        self.assertAlmostEqual(tracker.failures[0], time.time(), delta=0.1)

    def test_is_failure_threshold_exceeded_no_failures(self):
        tracker = FailureTracker(max_failures=2, window_seconds=10)
        self.assertFalse(tracker.is_failure_threshold_exceeded())

    def test_is_failure_threshold_exceeded_below_threshold(self):
        tracker = FailureTracker(max_failures=2, window_seconds=10)
        tracker.record_failure()
        self.assertFalse(tracker.is_failure_threshold_exceeded())

    def test_is_failure_threshold_exceeded_at_threshold(self):
        tracker = FailureTracker(max_failures=1, window_seconds=10)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_is_failure_threshold_exceeded_above_threshold(self):
        tracker = FailureTracker(max_failures=1, window_seconds=10)
        tracker.record_failure()
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_is_failure_threshold_exceeded_old_failures_pruned(self):
        tracker = FailureTracker(max_failures=1, window_seconds=1)
        tracker.record_failure()
        time.sleep(1.1) # Sleep longer than window_seconds
        self.assertFalse(tracker.is_failure_threshold_exceeded())
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_max_failures_zero(self):
        tracker = FailureTracker(max_failures=0, window_seconds=10)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_window_seconds_zero(self):
        tracker = FailureTracker(max_failures=1, window_seconds=0)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())
        time.sleep(0.1)
        self.assertFalse(tracker.is_failure_threshold_exceeded()) # Should prune immediately
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_multiple_failures_within_window(self):
        tracker = FailureTracker(max_failures=3, window_seconds=5)
        tracker.record_failure()
        time.sleep(0.5)
        tracker.record_failure()
        time.sleep(0.5)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())
        time.sleep(1.0)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())

    def test_failures_across_window_boundary(self):
        tracker = FailureTracker(max_failures=2, window_seconds=2)
        tracker.record_failure()
        time.sleep(1.5)
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())
        time.sleep(1.0) # First failure is now outside window
        self.assertFalse(tracker.is_failure_threshold_exceeded())
        tracker.record_failure()
        self.assertTrue(tracker.is_failure_threshold_exceeded())
