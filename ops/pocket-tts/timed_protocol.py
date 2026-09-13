"""Version-one records used by the artifact timed PCM stream."""
import json
import struct


def record(kind, payload):
    if kind == 1:
        if not payload or len(payload) % 2 or len(payload) > 9600:
            raise ValueError("invalid audio frame")
        body = bytes([1]) + payload
    elif kind == 2:
        body = bytes([2]) + json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode()
    else:
        raise ValueError("invalid timed record type")
    return struct.pack(">I", len(body)) + body


def records(payload):
    """Validate and yield complete records from a timed cache payload."""
    offset = 0
    while offset < len(payload):
        if offset + 4 > len(payload):
            raise ValueError("truncated timed cache")
        size = struct.unpack(">I", payload[offset:offset + 4])[0]
        offset += 4
        if size < 2 or offset + size > len(payload):
            raise ValueError("invalid timed cache record")
        body = payload[offset:offset + size]
        if body[0] not in (1, 2):
            raise ValueError("invalid timed cache record type")
        if body[0] == 1 and (size - 1 > 9600 or (size - 1) % 2):
            raise ValueError("invalid timed audio record")
        yield payload[offset - 4:offset + size]
        offset += size
