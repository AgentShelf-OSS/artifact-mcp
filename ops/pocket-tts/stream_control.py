"""Cooperative bounds for the pinned Pocket 3.1.0 background generator."""
import threading
import time


class GenerationBudget:
    def __init__(self, seconds):
        self.cancelled = threading.Event()
        self.deadline = time.monotonic() + seconds

    def check(self):
        if self.cancelled.is_set():
            raise InterruptedError("speech cancelled")
        if time.monotonic() >= self.deadline:
            raise TimeoutError("speech deadline exceeded")

    def queue(self, target):
        budget = self

        class CheckedQueue:
            def put(self, value, *args, **kwargs):
                budget.check()
                return target.put(value, *args, **kwargs)

        return CheckedQueue()

    def finish(self, iterator):
        # Pocket joins its decoder only when its iterator completes or reports
        # the producer error. Closing it at a yield skips that join. Cancel the
        # producer at its next latent, then drain through Pocket's cleanup while
        # the caller still holds the model gate.
        self.cancelled.set()
        if iterator is not None:
            try:
                for _ in iterator:
                    pass
            except Exception:
                pass
            finally:
                close = getattr(iterator, "close", None)
                if close is not None:
                    close()
