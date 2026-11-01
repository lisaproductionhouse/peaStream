#!/usr/bin/env python3
"""Thay AudioSpecificConfig trong esds của một init segment fMP4 (dựng lại kích thước mọi hộp cha)."""
import struct, sys

CONT = {b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"edts", b"dinf", b"udta"}

def parse(buf):
    out, p = [], 0
    while p + 8 <= len(buf):
        size, typ = struct.unpack(">I4s", buf[p:p + 8])
        out.append((typ, buf[p + 8:p + size])); p += size
    return out

def box(t, payload): return struct.pack(">I4s", 8 + len(payload), t) + payload

def desc(tag, body):
    n = len(body)
    return bytes([tag, 0x80 | (n >> 21) & 0x7f, 0x80 | (n >> 14) & 0x7f, 0x80 | (n >> 7) & 0x7f, n & 0x7f]) + body

def rd(d):
    tag, i, n = d[0], 1, 0
    while True:
        b = d[i]; i += 1; n = (n << 7) | (b & 0x7f)
        if not b & 0x80: break
    return tag, d[i:i + n], d[i + n:]

def patch_esds(payload, asc):
    head, rest = payload[:4], payload[4:]
    tag, es, _ = rd(rest); assert tag == 3
    es_head, inner = es[:3], es[3:]
    new_inner = b""
    while inner:
        t, body, inner = rd(inner)
        if t == 4:
            dc_head, dc = body[:13], body[13:]
            new_dc = b""
            while dc:
                t2, b2, dc = rd(dc)
                new_dc += desc(5, asc) if t2 == 5 else desc(t2, b2)
            body = dc_head + new_dc
        new_inner += desc(t, body)
    return head + desc(3, es_head + new_inner)

def patch(t, payload, asc):
    if t in CONT: return box(t, b"".join(patch(a, b, asc) for a, b in parse(payload)))
    if t == b"stsd": return box(t, payload[:8] + b"".join(patch(a, b, asc) for a, b in parse(payload[8:])))
    if t == b"mp4a": return box(t, payload[:28] + b"".join(patch(a, b, asc) for a, b in parse(payload[28:])))
    if t == b"esds": return box(t, patch_esds(payload, asc))
    return box(t, payload)

src, dst, asc_hex = sys.argv[1:4]
data = open(src, "rb").read()
open(dst, "wb").write(b"".join(patch(t, p, bytes.fromhex(asc_hex)) for t, p in parse(data)))
