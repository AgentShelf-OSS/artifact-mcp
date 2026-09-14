// Appended to the unmodified SoundTouchJS 2.1.1 processor bundle by the server.
// Keep one processor per paragraph/rate, so WSOLA history spans network frames.
// A fixed 120ms output reservoir absorbs the algorithm's varying block latency.
// The shell uses this same delay when saving position and draining the final tail.
class ArtifactPitchProcessor extends SoundTouchProcessor {
  constructor(options) {
    super(options);
    this.warmup = Math.round(sampleRate * 0.12);
    this._pipe.setStretchParameters({ sequenceMs: 60, seekWindowMs: 20, overlapMs: 8, quickSeek: false });
    this.failed = false;
  }
  extractSamples(left, right, count, available, parameters) {
    const silence = Math.min(this.warmup, count);
    this.warmup -= silence;
    left.fill(0); right.fill(0);
    const needed = count - silence;
    if (!needed) return { outputRms: 0, outputPeak: 0 };
    if (this._pipe.outputBuffer.frameCount < needed) {
      if (!this.failed) this.port.postMessage({ type: 'error', message: 'Pitch processor ran out of audio.' });
      this.failed = true;
      return { outputRms: 0, outputPeak: 0 };
    }
    return super.extractSamples(left.subarray(silence), right.subarray(silence), needed, needed, parameters);
  }
  onProcessComplete() {}
}
registerProcessor('artifact-pitch', ArtifactPitchProcessor);
