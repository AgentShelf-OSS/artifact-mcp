"""Authenticated Qwen TTS adapter for native artifact narration.

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

import urllib.request
import wave
import struct

CUSTOM_UPSTREAM = os.environ.get('QWEN_CUSTOM_URL', '').rstrip('/')
VOICES = {'qwen_reference': 'Reference voice · Qwen'} if os.environ.get('QWEN_REFERENCE_ENABLED', '1') != '0' else {}
if CUSTOM_UPSTREAM:
    VOICES.update({'qwen_ryan': 'Ryan · Qwen', 'qwen_aiden': 'Aiden · Qwen'})
MAX_CHARS = 1500
MAX_BODY = 8192
MAX_CACHE_BYTES = 512 * 1024 * 1024
CACHE = Path(os.environ.get("CACHE", "/cache"))
TOKEN = Path(os.environ.get("TOKEN_FILE", "/run/secrets/tts_token")).read_text().strip()
if len(TOKEN) < 32:
    raise RuntimeError("Worker token missing or too short")
MODEL_ID = "Qwen/Qwen3-TTS-12Hz-1.7B-Base"
UPSTREAM = os.environ.get("QWEN_URL", "http://qwen3-tts:8000").rstrip("/")
REFERENCE = "file:///voices/voxcpm_user_reference.wav"
REFERENCE_VERSION = os.environ["REFERENCE_SHA256"]
MAX_AUDIO = 4 * 1024 * 1024
MAX_INSTRUCTIONS = 500
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None
OPENER = urllib.request.build_opener(NoRedirect)

def duration_of(payload):
    with wave.open(io.BytesIO(payload)) as audio:
        return audio.getnframes() / audio.getframerate()

GATE = threading.BoundedSemaphore(1)
CACHE.mkdir(exist_ok=True)


def normalize_instructions(value, voice=None):
    """Return a bounded style instruction, or reject unsafe control text."""
    if value is None:
        raise ValueError("instructions must be a string")
    if not isinstance(value, str):
        raise ValueError("instructions must be a string")
    value = value.strip()
    if len(value) > MAX_INSTRUCTIONS:
        raise ValueError("instructions too long")
    if value and voice not in {"qwen_ryan", "qwen_aiden"}:
        raise ValueError("instructions require a CustomVoice preset")
    if any((ord(char) < 0x20 and char not in "\t\n\r") or ord(char) == 0x7F for char in value):
        raise ValueError("instructions contain control characters")
    return value


def upstream_request(text, voice, streaming=False, instructions=""):
    custom = voice != "qwen_reference"
    body = {"model": MODEL_ID.replace("-Base", "-CustomVoice") if custom else MODEL_ID,
            "input": text, "task_type": "CustomVoice" if custom else "Base",
            "response_format": "pcm" if streaming else "wav", "language": "English"}
    if custom:
        body["voice"] = voice.removeprefix("qwen_")
        if instructions:
            # vLLM-Omni exposes this as OpenAI's `instructions` field and maps it
            # to Qwen3-TTS's native `instruct` argument.
            body["instructions"] = instructions
    else:
        body.update(ref_audio=REFERENCE, x_vector_only_mode=True)
    if streaming:
        body["stream"] = True
    return urllib.request.Request((CUSTOM_UPSTREAM if custom else UPSTREAM) + "/v1/audio/speech",
        data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})


def cache_path(text, voice, extension, instructions=""):
    # Keep the established default key stable so existing model-default audio
    # remains reusable after style instructions are introduced.
    key_parts = [MODEL_ID, REFERENCE_VERSION, voice, text]
    if instructions:
        key_parts.append(instructions)
    key = hashlib.sha256(json.dumps(key_parts).encode()).hexdigest()
    return CACHE / (key + extension)


def save_cache(path, payload):
    files = sorted((p for p in CACHE.iterdir() if p.suffix in {".wav", ".pcm"}), key=lambda p: p.stat().st_mtime)
    size = sum(p.stat().st_size for p in files)
    while files and size + len(payload) > MAX_CACHE_BYTES:
        old = files.pop(0)
        size -= old.stat().st_size
        old.unlink()
    temp = path.with_suffix(".tmp")
    temp.write_bytes(payload)
    temp.replace(path)


def synthesize(text, voice, instructions=""):
    path = cache_path(text, voice, ".wav", instructions)
    started = time.monotonic()
    cached = path.is_file()
    if cached:
        payload = path.read_bytes()
    else:
        with OPENER.open(upstream_request(text, voice, instructions=instructions), timeout=55) as response:
            if not response.headers.get("Content-Type", "").startswith("audio/wav"):
                raise ValueError("Invalid upstream audio type")
            payload = response.read(MAX_AUDIO + 1)
        if len(payload) > MAX_AUDIO:
            raise ValueError("Audio exceeds limit")
        duration_of(payload)
        save_cache(path, payload)
    return payload, {"cached": cached, "generation_seconds": round(time.monotonic() - started, 3),
                     "audio_seconds": round(duration_of(payload), 3)}


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

    def do_GET(self):
        if self.path == "/health":
            try:
                with OPENER.open(UPSTREAM + "/health", timeout=3) as response:
                    if response.status != 200:
                        raise ValueError("Upstream unhealthy")
                return self.respond(200, {"status": "ok", "engine": "qwen3-tts", "voices": list(VOICES)})
            except Exception:
                return self.respond(503, {"status": "unavailable"})
        self.respond(404, {"error": "not_found"})

    def stream_audio(self, text, voice, instructions=""):
        path = cache_path(text, voice, ".pcm", instructions)
        started = time.monotonic()
        cached = path.is_file()
        response = None
        sent_headers = False
        total = 0
        collected = bytearray()
        pending = b""
        try:
            response = open(path, "rb") if cached else OPENER.open(upstream_request(text, voice, True, instructions), timeout=55)
            while True:
                block = response.read(9600) if cached else response.read1(9600)
                if not block:
                    break
                if time.monotonic() - started > 55:
                    raise TimeoutError("Streaming deadline")
                total += len(block)
                if total > MAX_AUDIO:
                    raise ValueError("Audio exceeds limit")
                pending += block
                even = len(pending) - len(pending) % 2
                if not even:
                    continue
                pcm, pending = pending[:even], pending[even:]
                if not sent_headers:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/vnd.artifact.pcm")
                    self.send_header("Cache-Control", "private, no-store, no-transform")
                    self.send_header("X-Accel-Buffering", "no")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    sent_headers = True
                    print(json.dumps({"event": "stream_start", "voice": voice, "chars": len(text),
                                      "first_audio_seconds": round(time.monotonic() - started, 3), "cached": cached}), flush=True)
                self.write_chunk(struct.pack(">I", len(pcm)) + pcm)
                if not cached:
                    collected.extend(pcm)
            if not sent_headers or pending:
                raise ValueError("Empty or misaligned audio")
            self.write_chunk(struct.pack(">I", 0))
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            if not cached:
                save_cache(path, collected)
            print(json.dumps({"event": "stream_done", "voice": voice, "generation_seconds": round(time.monotonic() - started, 3),
                              "audio_seconds": round(total / 48000, 3)}), flush=True)
        except Exception as error:
            print(json.dumps({"event": "stream_failed", "error_type": type(error).__name__}), flush=True)
            if not sent_headers:
                self.respond(503, {"error": "speech_unavailable"})
            # Missing protocol end marker makes interruption detectable downstream.
            self.close_connection = True
        finally:
            if response:
                response.close()

    def write_chunk(self, data):
        self.wfile.write(("%x\r\n" % len(data)).encode() + data + b"\r\n")
        self.wfile.flush()

    def do_POST(self):
        if not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + TOKEN):
            return self.respond(401, {"error": "unauthorized"})
        if self.path not in {"/speech", "/speech/stream"}:
            return self.respond(404, {"error": "not_found"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("Transfer-Encoding") or not 1 <= length <= MAX_BODY:
                return self.respond(413, {"error": "too_large"})
            data = json.loads(self.rfile.read(length))
            if not isinstance(data, dict) or not set(data).issubset({"text", "voice", "instructions"}) or not {"text", "voice"}.issubset(data):
                raise ValueError()
            text, voice = data["text"], data["voice"]
            if not isinstance(text, str) or not text.strip() or not 1 <= len(text) <= MAX_CHARS or not isinstance(voice, str) or voice not in VOICES:
                raise ValueError()
            instructions = normalize_instructions(data["instructions"], voice) if "instructions" in data else ""
        except (ValueError, TypeError, KeyError, TimeoutError):
            return self.respond(400, {"error": "invalid_request"})
        if not GATE.acquire(blocking=False):
            return self.respond(429, {"error": "busy"}, extra={"Retry-After": "2"})
        try:
            if self.path == "/speech/stream":
                return self.stream_audio(text.strip(), voice, instructions)
            payload, metrics = synthesize(text.strip(), voice, instructions)
            print(json.dumps({"event": "speech", "voice": voice, "chars": len(text), **metrics}), flush=True)
            self.respond(200, payload, "audio/wav", {"X-TTS-Seconds": metrics["generation_seconds"], "X-Audio-Seconds": metrics["audio_seconds"], "X-TTS-Cached": str(metrics["cached"]).lower()})
        except Exception as error:
            print(json.dumps({"event": "speech_failed", "error_type": type(error).__name__}), flush=True)
            self.respond(500, {"error": "synthesis_failed"})
        finally:
            GATE.release()


if __name__ == "__main__":
    print(json.dumps({"event": "ready", "model_id": MODEL_ID}), flush=True)
    ThreadingHTTPServer(("0.0.0.0", 8788), Handler).serve_forever()
