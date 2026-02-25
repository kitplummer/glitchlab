from collections import deque
import time

class FailureTracker:
    def __init__(self, max_failures: int, window_seconds: int):
        self.max_failures = max_failures
        self.window_seconds = window_seconds
        self.failures = deque()

    def record_failure(self):
        self.failures.append(time.time())
        self._prune_old_failures()

    def is_failure_threshold_exceeded(self) -> bool:
        self._prune_old_failures()
        return len(self.failures) > self.max_failures

    def _prune_old_failures(self):
        current_time = time.time()
        while self.failures and current_time - self.failures[0] > self.window_seconds:
            self.failures.popleft()
