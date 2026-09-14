import unittest

import threading
import time

from stream_control import (GenerationBudget, InferenceGate, QueueFull, QueueTimeout,
                            finish_stream_budget)


class _Queue:
    def __init__(self):
        self.values = []

    def put(self, value, *args, **kwargs):
        self.values.append(value)


class _Iterator:
    def __init__(self, values):
        self.values = iter(values)
        self.drained = []
        self.closed = False

    def __iter__(self):
        return self

    def __next__(self):
        value = next(self.values)
        self.drained.append(value)
        return value

    def close(self):
        self.closed = True


class GenerationBudgetTests(unittest.TestCase):
    def test_queue_checks_cancellation_before_enqueue(self):
        queue = _Queue()
        budget = GenerationBudget(10)
        checked = budget.queue(queue)

        checked.put("first")
        self.assertEqual(queue.values, ["first"])
        budget.cancelled.set()
        with self.assertRaises(InterruptedError):
            checked.put("second")
        self.assertEqual(queue.values, ["first"])

    def test_check_enforces_deadline(self):
        budget = GenerationBudget(-1)
        with self.assertRaises(TimeoutError):
            budget.check()

    def test_finish_cancels_drains_and_closes_iterator(self):
        budget = GenerationBudget(10)
        iterator = _Iterator(["remaining-1", "remaining-2"])

        budget.finish(iterator)

        self.assertTrue(budget.cancelled.is_set())
        self.assertEqual(iterator.drained, ["remaining-1", "remaining-2"])
        self.assertTrue(iterator.closed)

    def test_finish_is_safe_without_iterator(self):
        budget = GenerationBudget(10)
        budget.finish(None)
        self.assertTrue(budget.cancelled.is_set())


class InferenceGateTests(unittest.TestCase):
    def test_cached_request_does_not_finish_active_generation_budget(self):
        active = GenerationBudget(10)
        iterator = _Iterator(["still-generating"])

        # The cache path owns no budget. Its cleanup must not cancel the active
        # request represented by this separate iterator.
        finish_stream_budget(None, iterator)

        self.assertFalse(active.cancelled.is_set())
        self.assertFalse(iterator.closed)

    def test_waiters_are_fifo_and_measure_wait(self):
        gate = InferenceGate(max_waiters=2)
        self.assertEqual(gate.acquire(1), 0.0)
        order = []

        def waiter(name):
            waited = gate.acquire(1)
            order.append((name, waited))
            gate.release()

        first = threading.Thread(target=waiter, args=("first",))
        second = threading.Thread(target=waiter, args=("second",))
        first.start()
        time.sleep(0.02)
        second.start()
        time.sleep(0.02)
        gate.release()
        first.join()
        second.join()
        self.assertEqual([name for name, _ in order], ["first", "second"])
        self.assertGreater(order[0][1], 0)

    def test_queue_is_bounded(self):
        gate = InferenceGate(max_waiters=1)
        gate.acquire(1)
        started = threading.Event()
        release = threading.Event()

        def waiter():
            started.set()
            try:
                gate.acquire(1)
            except QueueTimeout:
                pass
            finally:
                release.set()

        thread = threading.Thread(target=waiter)
        thread.start()
        started.wait(1)
        with self.assertRaises(QueueFull):
            gate.acquire(1)
        gate.release()
        release.wait(1)
        thread.join()

    def test_waiter_timeout_does_not_block_next_request(self):
        gate = InferenceGate(max_waiters=2)
        gate.acquire(1)
        with self.assertRaises(QueueTimeout):
            gate.acquire(0.01)
        gate.release()
        self.assertEqual(gate.acquire(1), 0.0)
        gate.release()


if __name__ == "__main__":
    unittest.main()
