import http.client
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
import sys
import types
import wave
from unittest.mock import patch


class RavenServerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        token = Path(cls.tmp.name) / "token"
        token.write_text("t" * 40)
        os.environ.update(TOKEN_FILE=str(token), RAVEN_FAKE_ENGINE="1", CACHE_DIR=cls.tmp.name)
        # The production image supplies soundfile; keep the host-side protocol
        # tests dependency-light with a tiny WAV writer substitute.
        soundfile = types.ModuleType("soundfile")
        soundfile.write = lambda output, samples, rate, format="WAV", subtype="PCM_16": cls._write_wav(output, samples, rate)
        sys.modules["soundfile"] = soundfile
        path = Path(__file__).with_name("server.py")
        spec = importlib.util.spec_from_file_location("raven_server_test_module", path)
        cls.mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.mod)
        cls.httpd = cls.mod.ThreadingHTTPServer(("127.0.0.1", 0), cls.mod.Handler)
        cls.thread = threading.Thread(target=cls.httpd.serve_forever, daemon=True)
        cls.thread.start()

    @staticmethod
    def _write_wav(output, samples, rate):
        with wave.open(output, "wb") as wav:
            wav.setnchannels(1)
            wav.setsampwidth(2)
            wav.setframerate(rate)
            wav.writeframes(samples.tobytes())

    @classmethod
    def tearDownClass(cls):
        cls.httpd.shutdown()
        cls.httpd.server_close()
        cls.tmp.cleanup()

    def request(self, method, path, body=None, token="t" * 40):
        conn = http.client.HTTPConnection(*self.httpd.server_address, timeout=5)
        headers = {"Authorization": "Bearer " + token}
        if body is not None:
            payload = json.dumps(body).encode()
            headers.update({"Content-Type": "application/json", "Content-Length": str(len(payload))})
        else:
            payload = None
        conn.request(method, path, payload, headers)
        response = conn.getresponse()
        data = response.read()
        conn.close()
        return response, data

    def test_health_is_public_and_lists_validated_voices(self):
        response, data = self.request("GET", "/health")
        self.assertEqual(response.status, 200)
        self.assertEqual(json.loads(data)["voices"], ["raven_alba", "raven_marius"])

    def test_auth_and_request_validation(self):
        response, _ = self.request("POST", "/speech", {"text": "hello", "voice": "raven_alba"}, token="wrong")
        self.assertEqual(response.status, 401)
        response, _ = self.request("POST", "/speech", {"text": "hello", "voice": "pocket_alba"})
        self.assertEqual(response.status, 400)

    def test_wav_route_returns_valid_pcm_audio(self):
        response, data = self.request("POST", "/speech", {"text": "hello", "voice": "raven_alba"})
        self.assertEqual(response.status, 200)
        self.assertEqual(data[:4], b"RIFF")
        self.assertEqual(response.getheader("Content-Type"), "audio/wav")

    def test_stream_route_frames_pcm_and_eof(self):
        response, data = self.request("POST", "/speech/stream", {"text": "hello", "voice": "raven_alba"})
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "application/vnd.artifact.pcm")
        # http.client removes HTTP chunk coding; the body is protocol frames.
        sizes = []
        offset = 0
        while offset + 4 <= len(data):
            size = int.from_bytes(data[offset:offset + 4], "big")
            offset += 4
            sizes.append(size)
            if size == 0:
                break
            self.assertEqual(len(data[offset:offset + size]), size)
            offset += size
        self.assertGreater(len(sizes), 1)
        self.assertEqual(sizes[-1], 0)

    def test_deadline_stops_blocked_native_read_and_joins(self):
        class Blocked:
            def __init__(self): self.stopped = threading.Event(); self.ended = False
            def read(self): self.stopped.wait(2); return None
            def stop(self): self.stopped.set()
            def end(self): self.ended = True
        stream = Blocked()
        with patch.object(self.mod.ENGINE, 'start', return_value=stream), patch.object(self.mod, 'MAX_SECONDS', .02):
            with self.assertRaises(TimeoutError): self.mod.generate('hello', 'raven_alba', lambda _: None)
        self.assertTrue(stream.stopped.is_set())
        self.assertTrue(stream.ended)

    def test_disconnect_and_audio_limit_clean_up_stream(self):
        for failure in ['disconnect', 'limit']:
            stream = self.mod.FakeStream([self.mod.np.ones(20)])
            def write(_): raise BrokenPipeError()
            with patch.object(self.mod.ENGINE, 'start', return_value=stream), patch.object(self.mod, 'MAX_AUDIO_BYTES', 1 if failure == 'limit' else 4096):
                with self.assertRaises((BrokenPipeError, TimeoutError)):
                    self.mod.generate('hello', 'raven_alba', write)
            self.assertTrue(stream.stopped)

    def test_concurrent_generation_is_rejected(self):
        acquired = self.mod.GATE.acquire(blocking=False)
        self.assertTrue(acquired)
        try:
            response, _ = self.request("POST", "/speech", {"text": "hello", "voice": "raven_alba"})
            self.assertEqual(response.status, 429)
        finally:
            self.mod.GATE.release()


if __name__ == "__main__":
    unittest.main()
