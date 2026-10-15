"""HTTP-level cache regression using stubbed Pocket runtime modules."""
import io
import json
import os
import sys
import tempfile
import types
import unittest
from email.message import Message
from pathlib import Path


class _FakeModel:
    sample_rate = 24_000

    def __init__(self):
        self._autoregressive_generation = lambda *args, **kwargs: iter(())

    @classmethod
    def load_model(cls, *_args, **_kwargs):
        return cls()

    def get_state_for_audio_prompt(self, name):
        return name


def _load_server():
    pocket = types.ModuleType("pocket_tts_timestamped")
    pocket.AudioChunk = type("AudioChunk", (), {})
    pocket.TTSModel = _FakeModel
    pocket.WordEnd = type("WordEnd", (), {})
    pocket.WordStart = type("WordStart", (), {})
    sys.modules["pocket_tts_timestamped"] = pocket
    torch = types.ModuleType("torch")
    torch.set_num_threads = lambda *_args: None
    torch.set_num_interop_threads = lambda *_args: None
    sys.modules["torch"] = torch
    soundfile = types.ModuleType("soundfile")
    soundfile.info = lambda *_args: types.SimpleNamespace(duration=0.25)
    sys.modules["soundfile"] = soundfile
    token = tempfile.NamedTemporaryFile(delete=False)
    token.write(b"t" * 32)
    token.close()
    os.environ["TOKEN_FILE"] = token.name
    cache_dir = tempfile.mkdtemp(prefix="pocket-server-cache-")
    os.environ["CACHE_DIR"] = cache_dir
    sys.path.insert(0, str(Path(__file__).parent))
    import server
    return server, token.name


class _Handler:
    def __init__(self, server, path, body):
        self.server = server
        self.path = path
        self.headers = Message()
        self.headers["Authorization"] = "Bearer " + server.TOKEN
        self.headers["Content-Length"] = str(len(body))
        self.rfile = io.BytesIO(body)
        self.wfile = io.BytesIO()
        self.status = None
        self.response_headers = {}
        self.close_connection = False
        self.connection = None

    def send_response(self, status):
        self.status = status

    def send_header(self, key, value):
        self.response_headers[key] = value

    def end_headers(self):
        pass

    def write_chunk(self, data):
        self.wfile.write(data)

    def respond(self, status, body, mime="application/json", extra=None):
        self.status = status
        self.response_headers.update(extra or {})
        self.wfile.write(body if isinstance(body, bytes) else json.dumps(body).encode())


class ServerCacheTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server, cls.token_path = _load_server()

    @classmethod
    def tearDownClass(cls):
        os.unlink(cls.token_path)

    def test_cached_stream_does_not_cancel_active_budget(self):
        server = self.server
        with tempfile.TemporaryDirectory() as directory:
            server.CACHE = Path(directory)
            server.CACHE.mkdir(exist_ok=True)
            server.CACHE_STORE = server.CacheStore(server.CACHE, server.MAX_CACHE_BYTES)
            text, voice = "Cached passage.", "pocket_alba"
            path = server.CACHE / (server.cache_key(server.MODEL_ID, voice, text) + ".pcm")
            path.write_bytes(b"\x01\x00" * 20)
            active = server.GenerationBudget(10)
            server.ACTIVE_BUDGET = active
            body = json.dumps({"text": text, "voice": voice}).encode()
            handler = _Handler(server, "/speech/stream", body)
            server.Handler.do_POST(handler)
            self.assertEqual(handler.status, 200)
            self.assertFalse(active.cancelled.is_set())
            server.ACTIVE_BUDGET = None

    def test_voice_directory_loads_valid_file_and_hashes_its_contents(self):
        server = self.server
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "quiet_voice.safetensors"
            path.write_bytes(b"test-state")
            states, names = {}, {}
            calls = []

            class Loader:
                def get_state_for_audio_prompt(self, prompt):
                    calls.append(prompt)
                    return "loaded-state"

            server.VOICE_CONTENT_HASHES.clear()
            server.load_voice_directory(directory, Loader(), states, names)
            self.assertEqual(states, {"pocket_quiet_voice": "loaded-state"})
            self.assertEqual(names, {"pocket_quiet_voice": "Quiet Voice · Pocket"})
            self.assertEqual(calls, [path])
            self.assertEqual(server.VOICE_CONTENT_HASHES["pocket_quiet_voice"],
                             server.hashlib.sha256(b"test-state").hexdigest())

    def test_voice_directory_skips_builtin_collision_and_bad_name(self):
        server = self.server
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "alba.safetensors").write_bytes(b"collision")
            (root / "Bad-Name.safetensors").write_bytes(b"bad-name")
            states, names, calls = {}, {}, []

            class Loader:
                def get_state_for_audio_prompt(self, prompt):
                    calls.append(prompt)
                    return "loaded"

            server.load_voice_directory(directory, Loader(), states, names)
            self.assertEqual(states, {})
            self.assertEqual(names, {})
            self.assertEqual(calls, [])

    def test_voice_directory_skips_symlink_and_oversize_file(self):
        server = self.server
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.state"
            source.write_bytes(b"source")
            (root / "linked.safetensors").symlink_to(source)
            oversized = root / "oversized.safetensors"
            with oversized.open("wb") as output:
                output.truncate(server.MAX_VOICE_FILE_BYTES + 1)
            states, names, calls = {}, {}, []

            class Loader:
                def get_state_for_audio_prompt(self, prompt):
                    calls.append(prompt)
                    return "loaded"

            server.load_voice_directory(directory, Loader(), states, names)
            self.assertEqual(states, {})
            self.assertEqual(names, {})
            self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
