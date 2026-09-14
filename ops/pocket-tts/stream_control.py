"""Cooperative bounds for the pinned Pocket 3.1.0 background generator."""
import threading
import time
from collections import deque


class QueueFull(Exception):
    """The bounded inference wait queue has no room for another request."""


class QueueTimeout(Exception):
    """A request waited too long for the inference slot."""


class InferenceGate:
    """A single active operation with a small FIFO wait queue.

    Cache hits are admitted before this gate. Misses wait here so a transient
    burst does not turn into an immediate busy response, while the queue stays
    bounded and every waiter has a finite deadline.
    """

    def __init__(self, max_waiters=2):
        self.max_waiters = max_waiters
        self._condition = threading.Condition()
        self._active = False
        self._waiters = deque()

    def acquire(self, timeout, cancel_event=None, cancel_check=None):
        started = time.monotonic()
        ticket = object()
        with self._condition:
            if not self._active and not self._waiters:
                self._active = True
                return 0.0
            if len(self._waiters) >= self.max_waiters:
                raise QueueFull()
            self._waiters.append(ticket)
            deadline = started + timeout
            try:
                while True:
                    if cancel_event is not None and cancel_event.is_set():
                        raise InterruptedError("speech cancelled")
                    if cancel_check is not None and cancel_check():
                        raise InterruptedError("speech client disconnected")
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise QueueTimeout()
                    if self._waiters[0] is ticket and not self._active:
                        self._waiters.popleft()
                        self._active = True
                        return time.monotonic() - started
                    self._condition.wait(min(remaining, 0.1))
            finally:
                if ticket in self._waiters:
                    self._waiters.remove(ticket)
                    self._condition.notify_all()

    def release(self):
        with self._condition:
            if not self._active:
                raise RuntimeError("inference gate released while idle")
            self._active = False
            self._condition.notify_all()


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


def finish_stream_budget(budget, iterator):
    """Finish only the budget owned by this request.

    A cache-only request has no budget and must never cancel another request's
    iterator merely because it shares the worker process.
    """
    if budget is not None:
        budget.finish(iterator)
