"""Resident, CPU-only Kokoro worker for the artifact reader trial.

Only authenticated bounded text requests enter inference. No text or token logging.
"""
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import resource
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import onnxruntime as ort
import soundfile as sf
from kokoro_onnx import Kokoro

VOICES = {"bm_george": "George · British", "bf_emma": "Emma · British", "af_heart": "Heart · American"}
MAX_CHARS = 1500
MAX_BODY = 8192
MAX_CACHE_BYTES = 512 * 1024 * 1024
CACHE = Path("/cache")
TOKEN = Path(os.environ.get("TOKEN_FILE", "/run/secrets/tts_token")).read_text().strip()
if len(TOKEN) < 32:
    raise RuntimeError("Worker token missing or too short")
MODEL_PATH = Path("/models") / os.environ.get("MODEL_FILE", "kokoro-v1.0.onnx")
MODEL_HASH = hashlib.sha256(MODEL_PATH.read_bytes()).hexdigest()
opts = ort.SessionOptions()
ort.disable_telemetry_events()
opts.intra_op_num_threads = 2
opts.inter_op_num_threads = 1
opts.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
started = time.monotonic()
model = Kokoro.from_session(ort.InferenceSession(str(MODEL_PATH), sess_options=opts, providers=["CPUExecutionProvider"]), "/models/voices-v1.0.bin")
MODEL_LOAD_SECONDS = time.monotonic() - started
GATE = threading.BoundedSemaphore(1)
CACHE.mkdir(exist_ok=True)


def synthesize(text, voice):
    key = hashlib.sha256(json.dumps([MODEL_HASH, "kokoro-onnx-0.6.1", voice, text], ensure_ascii=False).encode()).hexdigest()
    path = CACHE / (key + ".wav")
    started = time.monotonic()
    cached = path.is_file()
    if cached:
        payload = path.read_bytes()
        duration = sf.info(io.BytesIO(payload)).duration
    else:
        samples, rate = model.create(text, voice=voice, speed=1.0, lang="en-gb" if voice.startswith("b") else "en-us")
        output = io.BytesIO()
        sf.write(output, samples, rate, format="WAV", subtype="PCM_16")
        payload = output.getvalue()
        duration = len(samples) / rate
        files = sorted(CACHE.glob("*.wav"), key=lambda p: p.stat().st_mtime)
        size = sum(p.stat().st_size for p in files)
        while files and size + len(payload) > MAX_CACHE_BYTES:
            old = files.pop(0)
            size -= old.stat().st_size
            old.unlink()
        temp = path.with_suffix(".tmp")
        temp.write_bytes(payload)
        temp.replace(path)
    elapsed = time.monotonic() - started
    return payload, {"cached": cached, "generation_seconds": round(elapsed, 3), "audio_seconds": round(duration, 3), "rtf": round(elapsed / max(duration, .001), 4), "peak_rss_mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(15)

    def respond(self, status, body, mime="application/json", extra=None):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for key, value in (extra or {}).items():
            self.send_header(key, str(value))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if self.path == "/health":
            return self.respond(200, {"status": "ok", "engine": "kokoro-onnx", "model_load_seconds": round(MODEL_LOAD_SECONDS, 3), "providers": model.sess.get_providers()})
        self.respond(404, {"error": "not_found"})

    def do_POST(self):
        if not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + TOKEN):
            return self.respond(401, {"error": "unauthorized"})
        if self.path != "/speech":
            return self.respond(404, {"error": "not_found"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("Transfer-Encoding") or not 1 <= length <= MAX_BODY:
                return self.respond(413, {"error": "too_large"})
            data = json.loads(self.rfile.read(length))
            if not isinstance(data, dict) or set(data) != {"text", "voice"}:
                raise ValueError()
            text, voice = data["text"], data["voice"]
            if not isinstance(text, str) or not 1 <= len(text.strip()) <= MAX_CHARS or not isinstance(voice, str) or voice not in VOICES:
                raise ValueError()
        except (ValueError, TypeError, KeyError, TimeoutError):
            return self.respond(400, {"error": "invalid_request"})
        if not GATE.acquire(blocking=False):
            return self.respond(429, {"error": "busy"}, extra={"Retry-After": "2"})
        try:
            payload, metrics = synthesize(text.strip(), voice)
            print(json.dumps({"event": "speech", "voice": voice, "chars": len(text), **metrics}), flush=True)
            self.respond(200, payload, "audio/wav", {"X-TTS-Seconds": metrics["generation_seconds"], "X-Audio-Seconds": metrics["audio_seconds"], "X-TTS-Cached": str(metrics["cached"]).lower()})
        except Exception as error:
            print(json.dumps({"event": "speech_failed", "error_type": type(error).__name__}), flush=True)
            self.respond(500, {"error": "synthesis_failed"})
        finally:
            GATE.release()


if __name__ == "__main__":
    print(json.dumps({"event": "ready", "model_load_seconds": MODEL_LOAD_SECONDS, "model_sha256": MODEL_HASH}), flush=True)
    ThreadingHTTPServer(("0.0.0.0", 8788), Handler).serve_forever()
