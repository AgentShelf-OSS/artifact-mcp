import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

// Exercise the shipped DSP, not a mock of its time-stretching algorithm.
const processorSource = readFileSync(new URL('../assets/vendor/soundtouch-processor.js', import.meta.url), 'utf8')
  + '\n' + readFileSync(new URL('../assets/reader-pitch-worklet.js', import.meta.url), 'utf8');

for (const rate of [0.8, 1.25, 1.5, 2]) {
  test(`streaming ${rate}x preserves pitch, amplitude and the delayed tail`, () => {
    const registered = {}, errors = [], sampleRate = 24000;
    const scope = { sampleRate, console, AudioWorkletProcessor: class {
      constructor() { this.port = { postMessage: message => errors.push(message) }; }
    }, registerProcessor: (name, processor) => { registered[name] = processor; } };
    vm.createContext(scope); vm.runInContext(processorSource, scope);
    const processor = new registered['artifact-pitch']({ processorOptions: {} });
    const frequency = 440, start = 0.2, end = start + 0.8 / rate;
    const output = new Float32Array(Math.ceil((end + 0.5) * sampleRate / 128) * 128);
    for (let offset = 0; offset < output.length; offset += 128) {
      const input = new Float32Array(128), block = new Float32Array(128);
      for (let i = 0; i < 128; i++) {
        const time = (offset + i) / sampleRate;
        if (time >= start && time < end) input[i] = 0.35 * Math.sin(2 * Math.PI * frequency * rate * (time - start));
      }
      processor.process([[input]], [[block]], { pitch: [1], pitchSemitones: [0], playbackRate: [rate] });
      output.set(block, offset);
    }
    const begin = Math.round((start + 0.25) * sampleRate), finish = Math.round((end + 0.05) * sampleRate);
    let crossings = 0, power = 0, peak = 0;
    for (let i = begin + 1; i < finish; i++) {
      if (output[i - 1] < 0 && output[i] >= 0) crossings++;
      power += output[i] ** 2; peak = Math.max(peak, Math.abs(output[i]));
    }
    const measured = crossings * sampleRate / (finish - begin);
    assert.ok(Math.abs(measured - frequency) / frequency < 0.02, `pitch changed to ${measured} Hz`);
    assert.ok(Math.sqrt(power / (finish - begin)) > 0.2, 'audio dropped out or cancelled at overlaps');
    assert.ok(peak < 0.4, 'overlaps amplified/clipped audio');
    const tail = output.findLastIndex(value => Math.abs(value) > 0.01) / sampleRate;
    assert.ok(Math.abs(tail - (end + 0.2)) < 0.06, 'audio tail or timeline drifted');
    assert.deepEqual(errors, []);
  });
}
