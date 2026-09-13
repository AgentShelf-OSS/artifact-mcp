import json
import os
import sys
import tempfile
import unittest
from pathlib import Path


_tmp = tempfile.TemporaryDirectory()
_token = Path(_tmp.name) / "token"
_token.write_text("t" * 32)
os.environ["TOKEN_FILE"] = str(_token)
os.environ["CACHE"] = str(Path(_tmp.name) / "cache")
os.environ["QWEN_CUSTOM_URL"] = "http://qwen.test"
os.environ["QWEN_REFERENCE_ENABLED"] = "0"
os.environ["REFERENCE_SHA256"] = "test-reference"
sys.path.insert(0, str(Path(__file__).parent))
import server  # noqa: E402


class AdapterRequestTests(unittest.TestCase):
    def test_custom_voice_maps_instructions_to_openai_field(self):
        request = server.upstream_request(
            "Read this calmly.", "qwen_ryan", instructions="Calm audiobook delivery."
        )
        body = json.loads(request.data)
        self.assertEqual(body["instructions"], "Calm audiobook delivery.")
        self.assertEqual(body["voice"], "ryan")
        self.assertEqual(body["task_type"], "CustomVoice")

    def test_empty_instructions_preserve_model_default(self):
        request = server.upstream_request("Read this.", "qwen_ryan")
        self.assertNotIn("instructions", json.loads(request.data))
        self.assertEqual(
            server.cache_path("Read this.", "qwen_ryan", ".wav"),
            server.cache_path("Read this.", "qwen_ryan", ".wav", ""),
        )

    def test_instructions_are_part_of_cache_key(self):
        self.assertNotEqual(
            server.cache_path("Read this.", "qwen_ryan", ".wav", "Calm"),
            server.cache_path("Read this.", "qwen_ryan", ".wav", "Expressive"),
        )

    def test_instruction_validation(self):
        self.assertEqual(server.normalize_instructions("  Calm delivery.  ", "qwen_ryan"), "Calm delivery.")
        with self.assertRaises(ValueError):
            server.normalize_instructions(None)
        with self.assertRaises(ValueError):
            server.normalize_instructions("x" * 501)
        with self.assertRaises(ValueError):
            server.normalize_instructions("calm\x00 delivery")
        with self.assertRaises(ValueError):
            server.normalize_instructions(42)
        self.assertEqual(server.normalize_instructions("calm\n delivery", "qwen_ryan"), "calm\n delivery")

    def test_instructions_are_restricted_to_custom_voice_presets(self):
        with self.assertRaises(ValueError):
            server.normalize_instructions("calm", "qwen_reference")
        with self.assertRaises(ValueError):
            server.normalize_instructions("calm", "af_heart")


if __name__ == "__main__":
    unittest.main()
