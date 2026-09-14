"""Small lock-protected bounded audio cache used by the Pocket worker."""
import threading


class CacheStore:
    def __init__(self, root, max_bytes, suffixes=(".wav", ".pcm", ".timed")):
        self.root = root
        self.max_bytes = max_bytes
        self.suffixes = set(suffixes)
        self._lock = threading.RLock()

    def read(self, path, max_entry_bytes):
        with self._lock:
            if not path.is_file():
                return None
            if path.stat().st_size > max_entry_bytes:
                path.unlink()
                return None
            return path.read_bytes()

    def write(self, path, payload):
        with self._lock:
            if len(payload) > self.max_bytes:
                raise ValueError("cache entry exceeds total limit")
            files = sorted(
                (item for item in self.root.iterdir() if item.suffix in self.suffixes),
                key=lambda item: item.stat().st_mtime,
            )
            size = sum(item.stat().st_size for item in files)
            existing = path.stat().st_size if path.is_file() else 0
            while files and size - existing + len(payload) > self.max_bytes:
                old = files.pop(0)
                if old == path:
                    continue
                size -= old.stat().st_size
                old.unlink()
            temp = path.with_name(path.name + ".%s.tmp" % threading.get_ident())
            temp.write_bytes(payload)
            temp.replace(path)

    def invalidate(self, path):
        with self._lock:
            try:
                path.unlink()
            except FileNotFoundError:
                pass
