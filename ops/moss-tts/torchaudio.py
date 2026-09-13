"""Import shim: arbitrary reference-audio cloning is intentionally not exposed."""
class _Unsupported:
    def load(self, *_args, **_kwargs):
        raise RuntimeError("reference audio is not enabled in this worker")
    def resample(self, *_args, **_kwargs):
        raise RuntimeError("reference audio is not enabled in this worker")
functional = _Unsupported()
def load(*_args, **_kwargs):
    raise RuntimeError("reference audio is not enabled in this worker")
