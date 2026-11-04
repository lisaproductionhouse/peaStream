#!/usr/bin/env python3
"""Máy chủ tĩnh cho kiểm thử: phục vụ một thư mục DASH và thêm bí danh /stream → manifest.mpd, để link KHÔNG
kết thúc bằng .mpd (buộc app đi qua yt-dlp thay vì dùng thẳng). Ghi mọi yêu cầu vào <log>.
   alias_server.py <thư mục> <cổng> <file log>
Biến môi trường (giả lập mạng giật): ALIAS_JITTER_MS=1500 làm mỗi đoạn AUDIO bị chậm thêm 0..1500 ms, XÁC ĐỊNH theo
ALIAS_SEED và số thứ tự đoạn (mọi lần chạy cùng hạt giống thấy cùng độ trễ → so sánh A/B công bằng)."""
import os
import random
import sys
import time
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

root, port, logfile = sys.argv[1], int(sys.argv[2]), sys.argv[3]

JITTER_MS = float(os.environ.get("ALIAS_JITTER_MS") or 0)
SEED = os.environ.get("ALIAS_SEED", "1")


class H(SimpleHTTPRequestHandler):
    def do_GET(self):
        if JITTER_MS and "/chunk-1-" in self.path:  # Representation 1 = audio (video đứng trước)
            time.sleep(JITTER_MS / 1000 * random.Random(f"{SEED}:{self.path.split('?')[0]}").random())
        super().do_GET()

    def translate_path(self, path):
        return super().translate_path("/manifest.mpd" if path.split("?")[0] == "/stream" else path)
    def guess_type(self, path):
        return "application/dash+xml" if path.endswith(".mpd") else super().guess_type(path)
    def log_message(self, fmt, *a):
        with open(logfile, "a") as f:
            f.write(f"{self.command} {self.path.split('?')[0]} {a[1] if len(a) > 1 else ''}\n")

ThreadingHTTPServer(("127.0.0.1", port), partial(H, directory=root)).serve_forever()
