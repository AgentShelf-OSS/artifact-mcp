import unittest

from stream_control import GenerationBudget


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


if __name__ == "__main__":
    unittest.main()
