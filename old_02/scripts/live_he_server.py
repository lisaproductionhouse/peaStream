#!/usr/bin/env python3
"""Server DASH "live" giả lập cho HE-AAC: mỗi giây xuất bản thêm một đoạn (đoạn chưa xuất bản trả 404).
Dùng các fixture HE-AAC trong core/tests/data (xoay vòng 2 đoạn) nên chỉ để thử đường ống live, không phải nội dung.
   live_he_server.py <port> [he1|he2]"""
import os, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port, kind = int(sys.argv[1]), (sys.argv[2] if len(sys.argv) > 2 else "he1")
data = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "core", "tests", "data")
read = lambda n: open(os.path.join(data, n), "rb").read()
INIT, SEGS = read(f"{kind}-init.m4s"), [read(f"{kind}-seg-1.m4s"), read(f"{kind}-seg-2.m4s")]
T0, D = time.time(), 48000          # đoạn dài 1 s, timescale 48000

def published():                    # số đoạn đã xuất bản (đoạn n xuất bản lúc T0 + n giây)
    return max(0, int(time.time() - T0))

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, code, body=b"", ctype="application/octet-stream"):
        self.send_response(code); self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        p = self.path.split("?")[0]
        if p.endswith("manifest.mpd"):
            n = published(); first = max(1, n - 5)
            s = "".join(f'<S t="{(i-1)*D}" d="{D}"/>' for i in range(first, n + 1))
            mpd = f"""<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" minimumUpdatePeriod="PT1S">
<Period id="0" start="PT0S"><AdaptationSet contentType="audio"><Representation id="a" mimeType="audio/mp4" codecs="mp4a.40.5" bandwidth="48000">
<SegmentTemplate timescale="{48000}" startNumber="{first}" initialization="init.m4s" media="seg-$Number$.m4s"><SegmentTimeline>{s}</SegmentTimeline></SegmentTemplate>
</Representation></AdaptationSet></Period></MPD>"""
            return self.reply(200, mpd.encode(), "application/dash+xml")
        if p.endswith("init.m4s"): return self.reply(200, INIT)
        if p.startswith("/seg-") and p.endswith(".m4s"):
            n = int(p[5:-4])
            return self.reply(200, SEGS[(n - 1) % 2]) if 1 <= n <= published() else self.reply(404)
        self.reply(404)

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
