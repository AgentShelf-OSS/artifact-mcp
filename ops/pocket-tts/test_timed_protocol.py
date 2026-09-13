import json
import struct
import unittest

from timed_protocol import record, records


class TimedProtocolTests(unittest.TestCase):
    def test_round_trips_audio_and_word_records(self):
        payload = record(1, b"\x01\x00\x02\x00") + record(
            2, {"word": "Hello", "index": 0, "start": 0.0}
        )
        parsed = list(records(payload))
        self.assertEqual(parsed[0][4], 1)
        self.assertEqual(parsed[0][5:], b"\x01\x00\x02\x00")
        self.assertEqual(json.loads(parsed[1][5:]), {"word": "Hello", "index": 0, "start": 0.0})

    def test_rejects_truncated_and_unknown_records(self):
        with self.assertRaises(ValueError):
            list(records(struct.pack(">I", 4) + b"\x01"))
        with self.assertRaises(ValueError):
            list(records(record(1, b"\x00\x00").replace(b"\x01", b"\x03", 1)))

    def test_rejects_odd_audio(self):
        with self.assertRaises(ValueError):
            record(1, b"\x00")


if __name__ == "__main__":
    unittest.main()
