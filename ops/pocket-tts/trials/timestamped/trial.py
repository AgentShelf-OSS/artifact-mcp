"""Isolated CPU comparison; writes audio and measured stream events, never serves traffic."""
import gc
import json
import logging
import os
from pathlib import Path
import threading
import time

import numpy as np
import soundfile as sf
import torch
from pocket_tts import TTSModel as Baseline
from pocket_tts_timestamped import TTSModel, AudioChunk, WordStart, WordEnd

logging.disable(logging.INFO)
torch.set_num_threads(1)
torch.set_num_interop_threads(1)
os.environ['KPOCKET_TTS_ERROR_WITHOUT_EOS'] = '1'
OUT = Path('/results')
PRESETS = ['alba', 'marius', 'javert', 'jean', 'cosette', 'eponine', 'fantine', 'azelma', 'anna', 'bill_boerst', 'caro_davy', 'charles', 'eve', 'george', 'jane', 'mary', 'michael', 'paul', 'peter_yearsley', 'stuart_bell', 'vera']
CASES = {
    'prose': 'The library was quiet. Beyond the window, rain traced silver lines across the glass. She opened the book and began to read, slowly at first, then with growing confidence.',
    'mixed': 'The report lists three priorities: reduce waiting time, improve navigation, and preserve reading progress. Revenue rose 12.5%, reaching $1,240. Dr. Evans reviewed the API results on September 12, 2026.',
    'long': ' '.join(['At the edge of the garden, a narrow path led towards the old stone bridge. The river moved quietly beneath it, carrying fallen leaves towards the distant hills.'] * 5),
}
report = {'checkpoint': 'english_2026-04', 'fork_commit': '65037e84c1885e7faa3e482b89fe3c304e2dada2', 'torch': torch.__version__, 'runs': []}
for mode, cls, quantized in [('baseline-int8', Baseline, True), ('timestamped-int8', TTSModel, True), ('timestamped-fp32', TTSModel, False)]:
    model = cls.load_model('english_2026-04', quantize=quantized)
    states = {name: model.get_state_for_audio_prompt(name) for name in PRESETS}
    report[mode + '-presets'] = len(states)
    list(model.generate_audio_stream(states['alba'], 'Ready to read.', copy_state=True))
    for voice, case in [('alba', 'prose'), ('marius', 'prose'), ('alba', 'mixed'), ('alba', 'long')]:
        torch.manual_seed(1234)
        start = time.monotonic()
        first = None
        chunks, events, words = [], [], []
        emitted = 0.0
        timestamped = mode.startswith('timestamped')
        stream = (model.generate_audio_with_timestamps_stream if timestamped else model.generate_audio_stream)(states[voice], CASES[case], copy_state=True)
        for item in stream:
            arrived = time.monotonic() - start
            if not timestamped or isinstance(item, AudioChunk):
                samples = item.audio if timestamped else item
                chunks.append(samples.cpu().numpy())
                emitted += samples.numel() / model.sample_rate
                if first is None:
                    first = arrived
            else:
                event = {'type': type(item).__name__, 'word': item.word, 'index': item.word_index, 'start': item.start_time, 'arrival_seconds': arrived, 'audio_emitted_seconds': emitted}
                if isinstance(item, WordEnd):
                    event['end'] = item.end_time
                    words.append(event)
                events.append(event)
        elapsed = time.monotonic() - start
        audio = np.concatenate(chunks)
        duration = len(audio) / model.sample_rate
        name = f'{mode}-{voice}-{case}'
        sf.write(OUT / (name + '.flac'), audio, model.sample_rate)
        checks = {
            'finite_audio': bool(np.isfinite(audio).all()),
            'word_count': len(words),
            'word_indices_contiguous': [w['index'] for w in words] == list(range(len(words))),
            'times_in_audio_bounds': all(0 <= w['start'] <= w['end'] <= duration + .001 for w in words),
            'starts_monotonic': all(a['start'] <= b['start'] for a, b in zip(words, words[1:])),
            'zero_duration_words': sum(w['start'] == w['end'] for w in words),
        }
        run = {'name': name, 'text': CASES[case], 'first_audio_seconds': first, 'generation_seconds': elapsed, 'audio_seconds': duration, 'realtime_multiple': duration / elapsed, 'checks': checks, 'events': events}
        report['runs'].append(run)
        print(json.dumps({k:v for k,v in run.items() if k not in ('events', 'text')}), flush=True)
        (OUT / 'results.json').write_text(json.dumps(report, indent=2))
    if mode.startswith('timestamped'):
        before = set(threading.enumerate())
        stream = model.generate_audio_with_timestamps_stream(states['alba'], CASES['long'], copy_state=True)
        for item in stream:
            if isinstance(item, AudioChunk):
                break
        stopped = time.monotonic()
        stream.close()
        report[mode + '-cancel'] = {'close_seconds': time.monotonic() - stopped, 'remaining_threads': [t.name for t in threading.enumerate() if t not in before]}
        list(model.generate_audio_with_timestamps_stream(states['alba'], 'Reading resumes after cancellation.', copy_state=True))
    del states, model
    gc.collect()
(OUT / 'results.json').write_text(json.dumps(report, indent=2))
