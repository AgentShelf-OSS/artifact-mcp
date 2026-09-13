"""Authenticated CPU MOSS-TTS-Nano ONNX worker for artifact narration."""
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

import numpy as np
import soundfile as sf
from scipy.signal import resample_poly
from onnx_tts_runtime import OnnxTtsRuntime

MAX_CHARS = 1500
MAX_BODY = 8192
MAX_CACHE_BYTES = 512 * 1024 * 1024
MAX_AUDIO_BYTES = 4 * 1024 * 1024
MAX_SYNTHESIS_SECONDS = 55
CACHE = Path("/cache")
TOKEN = Path(os.environ.get("TOKEN_FILE", "/run/secrets/tts_token")).read_text().strip()
if len(TOKEN) < 32:
    raise RuntimeError("Worker token missing or too short")
MODEL_DIR = os.environ.get("MODEL_DIR")
THREADS = max(1, min(4, int(os.environ.get("CPU_THREADS", "2"))))
GATE = threading.BoundedSemaphore(1)
CACHE.mkdir(exist_ok=True)
started = time.monotonic()
runtime = OnnxTtsRuntime(model_dir=MODEL_DIR, thread_count=THREADS, execution_provider="cpu", output_dir="/tmp/moss-audio")
VOICE_ROWS = runtime.list_builtin_voices()
ENGLISH_VOICES = ("Trump", "Ava", "Bella", "Adam", "Nathan")
VOICE_MAP = {"moss_" + name.lower(): name for name in ENGLISH_VOICES}
VOICE_LABELS = {key: next(str(row.get("display_name", row["voice"])) for row in VOICE_ROWS if str(row["voice"]) == value)
                for key, value in VOICE_MAP.items()}
MODEL_LOAD_SECONDS = time.monotonic() - started
MODEL_ID = "worker-v2; MOSS-TTS-Nano-100M-ONNX@f52645cb467506d8e18e746ddd59482685b74e58; codec@ceff0d0749bfb3fa2d61149794ec6feef0d1e1ae"


def _normalize_audio(waveform, sample_rate):
    audio = np.asarray(waveform, dtype=np.float32)
    if audio.ndim == 1:
        audio = audio[:, None]
    if audio.ndim != 2 or audio.shape[1] not in (1, 2):
        raise ValueError("unexpected audio shape")
    if audio.shape[1] == 2:
        audio = audio.mean(axis=1, keepdims=True)
    # The artifact stream/WAV contract is mono 24 kHz. MOSS normally emits 48 kHz.
    if int(sample_rate) != 24000:
        audio = resample_poly(audio[:, 0], 24000, int(sample_rate))[:, None].astype(np.float32)
    return np.clip(audio, -1.0, 1.0)


def synthesize(text, voice):
    key = hashlib.sha256(json.dumps([MODEL_ID, voice, text], ensure_ascii=False).encode()).hexdigest()
    path = CACHE / (key + ".wav")
    began = time.monotonic()
    cached = path.is_file()
    if cached:
        if path.stat().st_size > MAX_AUDIO_BYTES:
            raise ValueError("cached audio exceeds response limit")
        payload = path.read_bytes()
        with sf.SoundFile(io.BytesIO(payload)) as wav:
            duration = len(wav) / wav.samplerate
    else:
        began_deadline = time.monotonic()
        original_generate = runtime.generate_audio_frames

        def generate_with_deadline(request_rows, on_frame=None):
            def checked_on_frame(generated_frames, step_index, frame):
                if time.monotonic() - began_deadline > MAX_SYNTHESIS_SECONDS:
                    raise TimeoutError("synthesis deadline exceeded")
                if on_frame is not None:
                    on_frame(generated_frames, step_index, frame)
            if time.monotonic() - began_deadline > MAX_SYNTHESIS_SECONDS:
                raise TimeoutError("synthesis deadline exceeded")
            frames = original_generate(request_rows, on_frame=checked_on_frame)
            if len(frames) >= int(runtime.manifest["generation_defaults"]["max_new_frames"]):
                raise ValueError("synthesis reached frame limit and may be truncated")
            return frames

        runtime.generate_audio_frames = generate_with_deadline
        try:
            result = runtime.synthesize(text=text, voice=voice, streaming=True,
                                        max_new_frames=375, voice_clone_max_text_tokens=75,
                                        enable_wetext=False)
        finally:
            runtime.generate_audio_frames = original_generate
        if time.monotonic() - began_deadline > MAX_SYNTHESIS_SECONDS:
            raise TimeoutError("synthesis deadline exceeded")
        audio = _normalize_audio(result["waveform"], int(result["sample_rate"]))
        if audio.shape[0] == 0:
            raise ValueError("empty synthesis")
        output = io.BytesIO()
        sf.write(output, audio, 24000, format="WAV", subtype="PCM_16")
        payload = output.getvalue()
        if len(payload) > MAX_AUDIO_BYTES:
            raise ValueError("audio exceeds bounded response size")
        duration = len(audio) / 24000
        files = sorted(CACHE.glob("*.wav"), key=lambda item: item.stat().st_mtime)
        size = sum(item.stat().st_size for item in files)
        while files and size + len(payload) > MAX_CACHE_BYTES:
            old = files.pop(0); size -= old.stat().st_size; old.unlink()
        temp = path.with_suffix(".tmp"); temp.write_bytes(payload); temp.replace(path)
    elapsed = time.monotonic() - began
    return payload, {"cached": cached, "generation_seconds": round(elapsed, 3),
                     "audio_seconds": round(duration, 3), "rtf": round(elapsed / max(duration, .001), 4),
                     "peak_rss_mb": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1)}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def setup(self):
        super().setup(); self.connection.settimeout(60)

    def respond(self, status, body, mime="application/json", extra=None):
        if not isinstance(body, bytes): body = json.dumps(body).encode()
        self.send_response(status); self.send_header("Content-Type", mime)
        self.send_header("Content-Length", str(len(body))); self.send_header("Cache-Control", "no-store")
        for key, value in (extra or {}).items(): self.send_header(key, str(value))
        self.end_headers()
        try: self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError): pass

    def do_GET(self):
        if self.path == "/health":
            return self.respond(200, {"status": "ok", "engine": "moss-tts-nano-onnx", "model": MODEL_ID,
                                      "voices": {key: label + " · MOSS" for key, label in VOICE_LABELS.items()},
                                      "model_load_seconds": round(MODEL_LOAD_SECONDS, 3), "providers": ["CPU"]})
        self.respond(404, {"error": "not_found"})

    def do_POST(self):
        if not hmac.compare_digest(self.headers.get("Authorization", ""), "Bearer " + TOKEN):
            return self.respond(401, {"error": "unauthorized"})
        if self.path != "/speech": return self.respond(404, {"error": "not_found"})
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if self.headers.get("Transfer-Encoding") or not 1 <= length <= MAX_BODY: return self.respond(413, {"error": "too_large"})
            data = json.loads(self.rfile.read(length))
            if not isinstance(data, dict) or set(data) != {"text", "voice"}: raise ValueError()
            text, voice = data["text"], data["voice"]
            if not isinstance(text, str) or not text.strip() or len(text) > MAX_CHARS or not isinstance(voice, str) or voice not in VOICE_MAP: raise ValueError()
        except (ValueError, TypeError, KeyError, json.JSONDecodeError): return self.respond(400, {"error": "invalid_request"})
        if not GATE.acquire(blocking=False): return self.respond(429, {"error": "busy"}, extra={"Retry-After": "2"})
        try:
            payload, metrics = synthesize(text.strip(), VOICE_MAP[voice])
            print(json.dumps({"event": "speech", "voice": voice, "chars": len(text), **metrics}), flush=True)
            self.respond(200, payload, "audio/wav", {"X-TTS-Seconds": metrics["generation_seconds"], "X-Audio-Seconds": metrics["audio_seconds"], "X-TTS-Cached": str(metrics["cached"]).lower()})
        except Exception as exc:
            print(json.dumps({"event": "speech_failed", "type": type(exc).__name__}), flush=True); self.respond(500, {"error": "synthesis_failed"})
        finally: GATE.release()


if __name__ == "__main__":
    print(json.dumps({"event": "ready", "model_load_seconds": MODEL_LOAD_SECONDS, "model_id": MODEL_ID, "voices": list(VOICE_MAP)}), flush=True)
    ThreadingHTTPServer(("0.0.0.0", 8788), Handler).serve_forever()
