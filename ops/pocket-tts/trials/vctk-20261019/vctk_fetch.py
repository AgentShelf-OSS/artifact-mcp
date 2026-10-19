"""Fetch selected members of the official VCTK 0.92 zip with HTTP range requests.

Usage:
  vctk_fetch.py index <out-dir>              central directory -> index.json, plus the
                                             top-level text files (speaker-info, README, license)
  vctk_fetch.py fetch <out-dir> <list.txt>   extract the member names in list.txt to out-dir/

Standard library only (runs on the VM310 host). Every member is checked against the
CRC-32 in the zip central directory by zipfile itself. The full-archive MD5 is checked
separately (see README).
"""
import io, json, sys, threading, time, urllib.request, zipfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

URL = "https://datashare.ed.ac.uk/bitstream/handle/10283/3443/VCTK-Corpus-0.92.zip"
UA = {"user-agent": "homelab-vctk-fetch/1 (range reader)"}


class HTTPRange(io.RawIOBase):
    def __init__(self, url, block=1 << 20):
        self.url, self.block, self.pos = url, block, 0
        req = urllib.request.Request(url, headers={**UA, "range": "bytes=0-0"})
        with urllib.request.urlopen(req, timeout=60) as r:
            self.size = int(r.headers["content-range"].rsplit("/", 1)[1])
            self.etag = r.headers.get("etag", "").strip('"')
            self.final_url = r.geturl()
        self.cache = {}

    def seekable(self): return True
    def readable(self): return True
    def tell(self): return self.pos

    def seek(self, off, whence=0):
        self.pos = off if whence == 0 else self.pos + off if whence == 1 else self.size + off
        return self.pos

    def _get(self, start, end):
        for attempt in range(6):
            try:
                req = urllib.request.Request(self.final_url, headers={**UA, "range": f"bytes={start}-{end}"})
                with urllib.request.urlopen(req, timeout=120) as r:
                    data = r.read()
                if len(data) != end - start + 1:
                    raise IOError("short read")
                return data
            except Exception:
                if attempt == 5:
                    raise
                time.sleep(2 ** attempt)

    def readinto(self, b):
        n = min(len(b), self.size - self.pos)
        if n <= 0:
            return 0
        data = self._get(self.pos, self.pos + n - 1) if n > self.block else self._blocked(n)
        b[:len(data)] = data
        self.pos += len(data)
        return len(data)

    def _blocked(self, n):
        # Small reads (headers, central directory) go through a 1 MiB block cache.
        out = b""
        while len(out) < n:
            bi = (self.pos + len(out)) // self.block
            if bi not in self.cache:
                s = bi * self.block
                self.cache[bi] = self._get(s, min(self.size, s + self.block) - 1)
                if len(self.cache) > 64:
                    self.cache.pop(next(iter(self.cache)))
            off = self.pos + len(out) - bi * self.block
            out += self.cache[bi][off:off + n - len(out)]
        return out


def open_zip():
    f = HTTPRange(URL)
    z = zipfile.ZipFile(io.BufferedReader(f, buffer_size=1 << 16))
    # 1 MiB blocks suit the central directory; member reads need far less.
    f.block, f.cache = 1 << 16, {}
    return f, z


def main():
    cmd, out = sys.argv[1], Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    f, z = open_zip()
    if cmd == "index":
        infos = z.infolist()
        json.dump({"url": URL, "final_url": f.final_url, "size": f.size, "etag": f.etag,
                   "members": [[i.filename, i.file_size, i.compress_size, "%08x" % i.CRC] for i in infos]},
                  open(out / "index.json", "w"))
        for i in infos:
            if "/" not in i.filename:
                (out / i.filename).write_bytes(z.read(i))
                print("top-level", i.filename, i.file_size)
        print("members", len(infos), "size", f.size, "etag", f.etag)
        return
    names = [l.strip() for l in open(sys.argv[3]) if l.strip()]
    todo = [n for n in names if not (out / n).exists()]
    lock = threading.Lock(); done = [0]; local = threading.local()

    def one(name):
        # One ZipFile per thread (own file position); the central directory is read once per thread.
        if not hasattr(local, "z"):
            local.z = open_zip()[1]
        zz = local.z
        data = zz.read(name)  # raises BadZipFile on CRC mismatch
        dest = out / name; dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_suffix(dest.suffix + ".part"); tmp.write_bytes(data); tmp.replace(dest)
        with lock:
            done[0] += 1
            if done[0] % 100 == 0:
                print("fetched", done[0], "of", len(todo), flush=True)

    with ThreadPoolExecutor(6) as ex:
        list(ex.map(one, todo))
    print("done", len(todo), "fetched;", len(names) - len(todo), "already present")


if __name__ == "__main__":
    main()
