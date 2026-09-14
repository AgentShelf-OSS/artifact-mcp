import tempfile
import threading
import unittest
from pathlib import Path

from cache_store import CacheStore


class CacheStoreTests(unittest.TestCase):
    def test_concurrent_read_sees_complete_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = CacheStore(root, 1024)
            path = root / "audio.pcm"
            payload = b"\x01\x02" * 200
            errors = []

            def writer():
                try:
                    for _ in range(30):
                        store.write(path, payload)
                except Exception as error:
                    errors.append(error)

            def reader():
                try:
                    for _ in range(30):
                        value = store.read(path, 1024)
                        if value is not None:
                            self.assertEqual(value, payload)
                except Exception as error:
                    errors.append(error)

            threads = [threading.Thread(target=writer), threading.Thread(target=reader)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            self.assertEqual(errors, [])
            self.assertEqual(store.read(path, 1024), payload)

    def test_eviction_respects_total_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = CacheStore(root, 5)
            first = root / "first.pcm"
            second = root / "second.pcm"
            store.write(first, b"1234")
            store.write(second, b"5678")
            self.assertFalse(first.exists())
            self.assertEqual(second.read_bytes(), b"5678")

    def test_oversize_entry_is_removed_before_regeneration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = CacheStore(root, 1024)
            path = root / "stale.wav"
            path.write_bytes(b"x" * 20)
            self.assertIsNone(store.read(path, 8))
            self.assertFalse(path.exists())

    def test_oversize_write_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            store = CacheStore(root, 4)
            with self.assertRaises(ValueError):
                store.write(root / "too-large.pcm", b"12345")


if __name__ == "__main__":
    unittest.main()
