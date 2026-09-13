"""Small authenticated HTTP adapter for the native RAVEN streaming API.

This is a trial service.  It deliberately exposes only the two voices whose
reference caches have been validated and keeps the native handle resident.
"""
import ctypes as C
import hmac
import io
import json
import os
from pathlib import Path
import struct
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import soundfile as sf

SAMPLE_RATE = 24000
MAX_CHARS = 1500
MAX_BODY = 8192
MAX_AUDIO_BYTES = 4 * 1024 * 1024
MAX_SECONDS = 55
FRAME_BYTES = 9600
VOICES = {"raven_alba": "alba.wav", "raven_marius": "marius.wav"}
TOKEN_FILE = os.environ.get("TOKEN_FILE", "/run/secrets/tts_token")
TOKEN = Path(TOKEN_FILE).read_text().strip()
if len(TOKEN) < 32:
    raise RuntimeError("Worker token missing or too short")

GATE = threading.BoundedSemaphore(1)


class NativeEngine:
    def __init__(self):
        lib = C.CDLL(os.environ.get("RAVEN_LIBRARY", "/raven/libpocket_tts.so"))
        P = C.POINTER(C.c_float)
        lib.ptt_create.argtypes = [C.c_char_p] * 4 + [C.c_float, C.c_int, C.c_int]
        lib.ptt_create.restype = C.c_void_p
        lib.ptt_destroy.argtypes = [C.c_void_p]
        lib.ptt_destroy.restype = None
        lib.ptt_set_soften_commas.argtypes = [C.c_void_p, C.c_int]
        lib.ptt_set_soften_commas.restype = None
        lib.ptt_stream_start.argtypes = [C.c_void_p, C.c_char_p, C.c_char_p]
        lib.ptt_stream_start.restype = C.c_void_p
        lib.ptt_stream_read.argtypes = [C.c_void_p, C.POINTER(P), C.POINTER(C.c_int)]
        lib.ptt_stream_read.restype = C.c_int
        for name in ("ptt_stream_stop", "ptt_stream_end"):
            getattr(lib, name).argtypes = [C.c_void_p]
            getattr(lib, name).restype = None
        lib.ptt_free_audio.argtypes = [P]
        lib.ptt_free_audio.restype = None
        self.lib, self.P = lib, P
        self.handle = lib.ptt_create(
            os.environ.get("RAVEN_MODELS", "/raven/models").encode(),
            os.environ.get("RAVEN_VOICES", "/voices").encode(),
            os.environ.get("RAVEN_TOKENIZER", "/raven/models/tokenizer.model").encode(),
            b"int8", float(os.environ.get("RAVEN_TEMPERATURE", "0.3")), 1, 2,
        )
        if not self.handle:
            raise RuntimeError("RAVEN initialization failed")
        # Match the Pocket route's authored punctuation handling.
        lib.ptt_set_soften_commas(self.handle, 0)

    def start(self, text, voice):
        context = self.lib.ptt_stream_start(self.handle, text.encode(), VOICES[voice].encode())
        if not context:
            raise RuntimeError("RAVEN could not start stream")
        return NativeStream(self, context)

    def close(self):
        self.lib.ptt_destroy(self.handle)


class NativeStream:
    def __init__(self, engine, context):
        self.engine, self.context = engine, context
        self.stopped = False
        self._lock = threading.Lock()

    def stop(self):
        with self._lock:
            if not self.stopped:
                self.stopped = True
                self.engine.lib.ptt_stream_stop(self.context)

    def end(self):
        self.stop()
        self.engine.lib.ptt_stream_end(self.context)

    def read(self):
        ptr = self.engine.P()
        count = C.c_int()
        result = self.engine.lib.ptt_stream_read(self.context, C.byref(ptr), C.byref(count))
        if result != 1:
            return None
        try:
            return np.ctypeslib.as_array(ptr, shape=(count.value,)).copy()
        finally:
            self.engine.lib.ptt_free_audio(ptr)


class FakeStream:
    def __init__(self, chunks):
        self.chunks = iter(chunks)
        self.stopped = False

    def read(self):
        if self.stopped:
            return None
        return next(self.chunks, None)

    def stop(self):
        self.stopped = True

    def end(self):
        self.stop()


class FakeEngine:
    def start(self, text, voice):
        return FakeStream([np.zeros(1200, dtype=np.float32), np.ones(1200, dtype=np.float32) * .1])

    def close(self):
        pass


ENGINE = FakeEngine() if os.environ.get("RAVEN_FAKE_ENGINE") == "1" else NativeEngine()


def pcm_bytes(samples):
    samples = np.asarray(samples, dtype=np.float32).reshape(-1)
    if not len(samples) or not np.isfinite(samples).all():
        raise ValueError("invalid audio")
    return (np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes()


def validate_request(handler):
    if not hmac.compare_digest(handler.headers.get("Authorization", ""), "Bearer " + TOKEN):
        raise HTTPError(401, "unauthorized")
    try:
        length = int(handler.headers.get("Content-Length", "0"))
    except ValueError:
        raise HTTPError(400, "invalid_request")
    if handler.headers.get("Transfer-Encoding") or not 1 <= length <= MAX_BODY:
        raise HTTPError(413, "too_large")
    try:
        body = json.loads(handler.rfile.read(length))
        if not isinstance(body, dict) or set(body) != {"text", "voice"}:
            raise ValueError
        text, voice = body["text"], body["voice"]
        if not isinstance(text, str) or not text.strip() or len(text) > MAX_CHARS or voice not in VOICES:
            raise ValueError
        return text.strip(), voice
    except (ValueError, TypeError, json.JSONDecodeError):
        raise HTTPError(400, "invalid_request")


class HTTPError(Exception):
    def __init__(self, status, message):
        self.status, self.message = status, message


def generate(text, voice, on_chunk):
    stream = ENGINE.start(text, voice)
    started = time.monotonic()
    timed_out = threading.Event()

    def watchdog():
        if not timed_out.wait(MAX_SECONDS):
            stream.stop()
            timed_out.set()

    watcher = threading.Thread(target=watchdog, daemon=True)
    watcher.start()
    total = 0
    try:
        while True:
            samples = stream.read()
            if samples is None:
                break
            pcm = pcm_bytes(samples)
            total += len(pcm)
            if total > MAX_AUDIO_BYTES or time.monotonic() - started > MAX_SECONDS:
                stream.stop()
                raise TimeoutError("generation limit exceeded")
            on_chunk(pcm)
        if timed_out.is_set():
            raise TimeoutError("generation deadline exceeded")
        if not total:
            raise ValueError("empty stream")
    finally:
        timed_out.set()
        # Wake the watchdog before releasing the native context.  Without
        # this join, its late stop() could race ptt_stream_end().
        watcher.join()
        stream.end()


def wav_audio(text, voice):
    parts = []
    generate(text, voice, parts.append)
    output = io.BytesIO()
    sf.write(output, np.frombuffer(b"".join(parts), dtype="<i2"), SAMPLE_RATE, format="WAV", subtype="PCM_16")
    payload = output.getvalue()
    if len(payload) > MAX_AUDIO_BYTES:
        raise ValueError("audio exceeds limit")
    return payload


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *_):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(15)

    def respond(self, status, body, mime="application/json", extra=None):
        payload = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", len(payload))
        self.send_header("Cache-Control", "no-store")
        for key, value in (extra or {}).items(): self.send_header(key, str(value))
        self.end_headers()
        try: self.wfile.write(payload); self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError): pass

    def do_GET(self):
        if self.path == "/health":
            return self.respond(200, {"status": "ok", "engine": "raven", "voices": list(VOICES), "providers": ["CPU"]})
        self.respond(404, {"error": "not_found"})

    def do_POST(self):
        try:
            text, voice = validate_request(self)
        except HTTPError as error:
            return self.respond(error.status, {"error": error.message})
        if self.path not in {"/speech", "/speech/stream"}:
            return self.respond(404, {"error": "not_found"})
        if not GATE.acquire(blocking=False):
            return self.respond(429, {"error": "busy"}, extra={"Retry-After": "2"})
        try:
            if self.path == "/speech":
                payload = wav_audio(text, voice)
                return self.respond(200, payload, "audio/wav")
            sent = False
            total = 0
            def write(pcm):
                nonlocal sent, total
                total += len(pcm)
                if not sent:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/vnd.artifact.pcm")
                    self.send_header("Cache-Control", "private, no-store, no-transform")
                    self.send_header("X-Accel-Buffering", "no")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers(); sent = True
                for offset in range(0, len(pcm), FRAME_BYTES):
                    data = pcm[offset:offset + FRAME_BYTES]
                    self.wfile.write(('%x\r\n' % (len(data) + 4)).encode() + struct.pack('>I', len(data)) + data + b'\r\n')
                    self.wfile.flush()
            generate(text, voice, write)
            self.wfile.write(b'4\r\n\x00\x00\x00\x00\r\n0\r\n\r\n'); self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, TimeoutError, ValueError, OSError, RuntimeError):
            self.close_connection = True
            if not locals().get("sent", False): self.respond(503, {"error": "speech_unavailable"})
        finally:
            GATE.release()


if __name__ == "__main__":
    server = ThreadingHTTPServer((os.environ.get("RAVEN_BIND_ADDRESS", "0.0.0.0"), 8788), Handler)
    try: server.serve_forever()
    finally: ENGINE.close()
