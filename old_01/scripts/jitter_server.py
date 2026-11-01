#!/usr/bin/env python3
"""HTTP server tĩnh có độ trễ ngẫu nhiên 0..JITTER_MS cho mỗi response (giả lập mạng di động giật)."""
import os, random, sys, time
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

root, port, jitter = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])

class H(SimpleHTTPRequestHandler):
    def do_GET(self):
        time.sleep(random.uniform(0, jitter) / 1000)
        super().do_GET()
    def log_message(self, *a): pass

ThreadingHTTPServer(("127.0.0.1", port), partial(H, directory=root)).serve_forever()
