#!/usr/bin/env python3
"""Máy chủ tĩnh cho kiểm thử: phục vụ một thư mục DASH và thêm bí danh /stream → manifest.mpd, để link KHÔNG
kết thúc bằng .mpd (buộc app đi qua yt-dlp thay vì dùng thẳng). Ghi mọi yêu cầu vào <log>.
   alias_server.py <thư mục> <cổng> <file log>"""
import sys
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

root, port, logfile = sys.argv[1], int(sys.argv[2]), sys.argv[3]

class H(SimpleHTTPRequestHandler):
    def translate_path(self, path):
        return super().translate_path("/manifest.mpd" if path.split("?")[0] == "/stream" else path)
    def guess_type(self, path):
        return "application/dash+xml" if path.endswith(".mpd") else super().guess_type(path)
    def log_message(self, fmt, *a):
        with open(logfile, "a") as f:
            f.write(f"{self.command} {self.path.split('?')[0]} {a[1] if len(a) > 1 else ''}\n")

ThreadingHTTPServer(("127.0.0.1", port), partial(H, directory=root)).serve_forever()
