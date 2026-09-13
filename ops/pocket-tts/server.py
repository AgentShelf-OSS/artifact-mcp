"""Resident CPU Pocket TTS worker for native artifact narration.

Only authenticated bounded text requests enter inference. No text or token logging.
"""
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import resource
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import logging
import numpy as np
from stream_control import GenerationBudget
import torch
import soundfile as sf
from pocket_tts import TTSModel

PRESETS = ['alba', 'marius', 'javert', 'jean', 'cosette', 'eponine', 'fantine', 'azelma',
           'anna', 'bill_boerst', 'caro_davy', 'charles', 'eve', 'george', 'jane',
           'mary', 'michael', 'paul', 'peter_yearsley', 'stuart_bell', 'vera']
VOICES = {'pocket_' + name: name.replace('_', ' ').title() + ' · Pocket' for name in PRESETS}
MAX_CHARS = 1500
MAX_BODY = 8192
MAX_CACHE_BYTES = 512 * 1024 * 1024
MAX_AUDIO_BYTES = 4 * 1024 * 1024
MAX_STREAM_SECONDS = 55
CACHE = Path("/cache")
TOKEN = Path(os.environ.get("TOKEN_FILE", "/run/secrets/tts_token")).read_text().strip()
if len(TOKEN) < 32:
    raise RuntimeError("Worker token missing or too short")
# Pocket uses a decoder thread in addition to model inference. Keep the same
# two-core container quota as Kokoro and one PyTorch thread per operation.
logging.getLogger('pocket_tts').setLevel(logging.WARNING)
torch.set_num_threads(1)
torch.set_num_interop_threads(1)
started = time.monotonic()
os.environ["KPOCKET_TTS_ERROR_WITHOUT_EOS"] = "1"
model = TTSModel.load_model("english_2026-04")
voices = {"pocket_" + name: model.get_state_for_audio_prompt(name) for name in PRESETS}
MODEL_ID = "pocket-tts-3.1.0-english_2026-04"
MODEL_LOAD_SECONDS = time.monotonic() - started
GATE = threading.BoundedSemaphore(1)
CACHE.mkdir(exist_ok=True)
ACTIVE_BUDGET = None
_original_autoregressive = model._autoregressive_generation


def _bounded_autoregressive(model_state, max_gen_len, frames_after_eos, latents_queue):
    # This private signature is verified against the pinned Pocket 3.1.0 build.
    # Pocket's producer catches this error and stops/joins the decoder via its
    # normal result queue, so interruption cannot leave inference running.
    budget = ACTIVE_BUDGET
    budget.check()
    return _original_autoregressive(model_state, max_gen_len, frames_after_eos, budget.queue(latents_queue))


model._autoregressive_generation = _bounded_autoregressive


def save_cache(path, payload):
    files = sorted((item for item in CACHE.iterdir() if item.suffix in {".wav", ".pcm"}), key=lambda item: item.stat().st_mtime)
    size = sum(item.stat().st_size for item in files)
    while files and size + len(payload) > MAX_CACHE_BYTES:
        old = files.pop(0); size -= old.stat().st_size; old.unlink()
    temp = path.with_suffix(".tmp"); temp.write_bytes(payload); temp.replace(path)


def synthesize(text, voice):
    key = hashlib.sha256(json.dumps([MODEL_ID, "pocket-tts-3.1.0", voice, text], ensure_ascii=False).encode()).hexdigest()
    path = CACHE / (key + ".wav")
    started = time.monotonic()
    cached = path.is_file()
    if cached:
        if path.stat().st_size > MAX_AUDIO_BYTES:
            raise ValueError("cached audio exceeds limit")
        payload = path.read_bytes()
        duration = sf.info(io.BytesIO(payload)).duration
    else:
        samples = model.generate_audio(voices[voice], text, copy_state=True).detach().cpu().numpy()
        rate = model.sample_rate
        output = io.BytesIO()
        sf.write(output, samples, rate, format="WAV", subtype="PCM_16")
        payload = output.getvalue()
        duration = len(samples) / rate
        if len(payload) > MAX_AUDIO_BYTES:
            raise ValueError("audio exceeds limit")
        save_cache(path, payload)
    elapsed = time.monotonic() - started
    return payload, {"cached": cached, "generation_seconds": round(elapsed, 3), "audio_seconds": round(duration, 3), "rtf": round(elapsed / max(duration, .001), 4), "peak_rss_mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)}


def stream_synthesize(handler, text, voice):
    """Stream framed PCM and cache only a complete successful generation."""
    path = CACHE / (hashlib.sha256(json.dumps([MODEL_ID, voice, text], ensure_ascii=False).encode()).hexdigest() + ".pcm")
    began = time.monotonic()
    cached = path.is_file()
    total = 0
    collected = bytearray()
    sent = False
    iterator = None
    try:
        if cached:
            if path.stat().st_size > MAX_AUDIO_BYTES:
                raise ValueError("cached audio exceeds limit")
            cached_payload = path.read_bytes()
            iterator = (cached_payload[offset:offset + 9600] for offset in range(0, len(cached_payload), 9600))
        else:
            iterator = model.generate_audio_stream(voices[voice], text, copy_state=True)
        for frame in iterator:
            if time.monotonic() - began > MAX_STREAM_SECONDS:
                raise TimeoutError("streaming deadline exceeded")
            if cached:
                pcm = bytes(frame)
            else:
                samples = frame.detach().cpu().numpy().reshape(-1)
                pcm = (np.clip(samples, -1.0, 1.0) * 32767.0).astype("<i2").tobytes()
            if not pcm or len(pcm) % 2:
                raise ValueError("invalid audio frame")
            total += len(pcm)
            if total > MAX_AUDIO_BYTES:
                raise ValueError("audio exceeds limit")
            if not sent:
                handler.send_response(200)
                handler.send_header("Content-Type", "application/vnd.artifact.pcm")
                handler.send_header("Cache-Control", "private, no-store, no-transform")
                handler.send_header("X-Accel-Buffering", "no")
                handler.send_header("Transfer-Encoding", "chunked")
                handler.end_headers(); sent = True
            for offset in range(0, len(pcm), 9600):
                piece = pcm[offset:offset + 9600]
                handler.write_chunk(struct.pack(">I", len(piece)) + piece)
            if not cached:
                collected.extend(pcm)
        if not sent or not collected and not cached:
            raise ValueError("empty stream")
        handler.write_chunk(struct.pack(">I", 0)); handler.wfile.write(b"0\r\n\r\n"); handler.wfile.flush()
        if not cached:
            save_cache(path, bytes(collected))
    except Exception as error:
        print(json.dumps({"event": "stream_failed", "error_type": type(error).__name__}), flush=True)
        if not sent:
            handler.respond(503, {"error": "speech_unavailable"})
        handler.close_connection = True
    finally:
        ACTIVE_BUDGET.finish(iterator)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
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

    def write_chunk(self, data):
        self.wfile.write(("%x\r\n" % len(data)).encode() + data + b"\r\n")
        self.wfile.flush()

    def do_GET(self):
        if self.path == "/health":
            return self.respond(200, {"status": "ok", "engine": "pocket-tts", "voices": list(VOICES), "model_load_seconds": round(MODEL_LOAD_SECONDS, 3), "providers": ["CPU"]})
        self.respond(404, {"error": "not_found"})

    def do_POST(self):
        global ACTIVE_BUDGET
        if not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + TOKEN):
            return self.respond(401, {"error": "unauthorized"})
        if self.path not in {"/speech", "/speech/stream"}:
            return self.respond(404, {"error": "not_found"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("Transfer-Encoding") or not 1 <= length <= MAX_BODY:
                return self.respond(413, {"error": "too_large"})
            data = json.loads(self.rfile.read(length))
            if not isinstance(data, dict) or set(data) != {"text", "voice"}:
                raise ValueError()
            text, voice = data["text"], data["voice"]
            if not isinstance(text, str) or not text.strip() or len(text) > MAX_CHARS or not isinstance(voice, str) or voice not in VOICES:
                raise ValueError()
        except (ValueError, TypeError, KeyError, TimeoutError):
            return self.respond(400, {"error": "invalid_request"})
        if not GATE.acquire(blocking=False):
            return self.respond(429, {"error": "busy"}, extra={"Retry-After": "2"})
        ACTIVE_BUDGET = GenerationBudget(MAX_STREAM_SECONDS)
        try:
            if self.path == "/speech/stream":
                return stream_synthesize(self, text.strip(), voice)
            payload, metrics = synthesize(text.strip(), voice)
            print(json.dumps({"event": "speech", "voice": voice, "chars": len(text), **metrics}), flush=True)
            self.respond(200, payload, "audio/wav", {"X-TTS-Seconds": metrics["generation_seconds"], "X-Audio-Seconds": metrics["audio_seconds"], "X-TTS-Cached": str(metrics["cached"]).lower()})
        except Exception as error:
            print(json.dumps({"event": "speech_failed", "error_type": type(error).__name__}), flush=True)
            self.respond(500, {"error": "synthesis_failed"})
        finally:
            GATE.release()


if __name__ == "__main__":
    print(json.dumps({"event": "ready", "model_load_seconds": MODEL_LOAD_SECONDS, "model_id": MODEL_ID}), flush=True)
    ThreadingHTTPServer(("0.0.0.0", 8788), Handler).serve_forever()
