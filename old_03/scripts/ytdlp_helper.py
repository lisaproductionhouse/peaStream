#!/usr/bin/env python3
"""Máy chủ yt-dlp nhỏ cho FB Live Audio.

Android không có Python nên app không tự chạy yt-dlp được. Chạy script này ở nơi có yt-dlp rồi nhập địa
chỉ vào mục "Nâng cao" của app:

  * ngay trên điện thoại, bằng Termux (không cần máy chủ nào khác):
        pkg install python
        pip install -U yt-dlp
        python ytdlp_helper.py                      # địa chỉ trong app:  http://127.0.0.1:8787
  * hoặc trên máy tính cùng mạng Wi-Fi (nhớ đặt mã truy cập):
        python ytdlp_helper.py --host 0.0.0.0 --token MATKHAU    # địa chỉ:  http://<IP máy tính>:8787

Giao thức:  GET /info?url=<link>  → 200 + JSON của `yt-dlp -J`;  lỗi → mã 4xx/5xx + một dòng giải thích
            GET /health           → "ok"

An toàn: mặc định chỉ lắng nghe 127.0.0.1, chỉ nhận link Facebook, có thể đòi mã truy cập, URL luôn được
truyền sau `--` (không thể bị hiểu thành tuỳ chọn của yt-dlp) và không dùng shell.
"""
import argparse
import hmac
import json
import re
import shutil
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

ALLOWED_HOSTS = ("facebook.com", "fb.watch", "fb.com", "fb.me")
BREAK = re.compile(r"cannot parse data|unable to extract|please report this issue|no video formats", re.I)
updated_once = threading.Event()
slots = threading.BoundedSemaphore(2)  # tối đa 2 yt-dlp chạy cùng lúc


def ytdlp_cmd(path):
    if path:
        return [path]
    exe = shutil.which("yt-dlp")
    return [exe] if exe else [sys.executable, "-m", "yt_dlp"]


def error_line(stderr):
    text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", stderr or "")
    lines = [l for l in text.splitlines() if l.strip()]
    errs = [l for l in lines if l.lstrip().startswith("ERROR:")]
    line = (errs or lines or ["yt-dlp thoát với lỗi không rõ"])[-1]
    return line.replace("ERROR:", "", 1).strip()[:240]


def run_info(cfg, url):
    """Trả (mã HTTP, nội dung bytes)."""
    cmd = ytdlp_cmd(cfg.yt_dlp)
    args = cmd + ["-J", "--no-playlist", "--no-warnings", "--ignore-config", "--socket-timeout", "15"]
    args += cfg.extra + ["--", url]
    for attempt in (0, 1):
        try:
            p = subprocess.run(args, capture_output=True, timeout=cfg.timeout)
        except subprocess.TimeoutExpired:
            return 504, f"yt-dlp không phản hồi sau {cfg.timeout} giây".encode()
        except FileNotFoundError:
            return 500, "máy chủ chưa cài yt-dlp (pip install -U yt-dlp)".encode()
        if p.returncode == 0:
            return 200, p.stdout
        msg = error_line(p.stderr.decode("utf-8", "replace"))
        if "No module named" in msg:
            return 500, "máy chủ chưa cài yt-dlp (pip install -U yt-dlp)".encode()
        # Facebook vừa đổi cấu trúc → thử cập nhật yt-dlp một lần rồi hỏi lại
        if attempt == 0 and cfg.auto_update and BREAK.search(msg) and not updated_once.is_set():
            updated_once.set()
            log("yt-dlp báo trích xuất hỏng → cập nhật")
            upd = ([sys.executable, "-m", "pip", "install", "-U", "yt-dlp"] if len(cmd) > 1 and cmd[1] == "-m"
                   else cmd + ["-U"])
            try:
                subprocess.run(upd, capture_output=True, timeout=300)
            except Exception as e:  # noqa: BLE001
                log(f"cập nhật thất bại: {e}")
            continue
        return 502, msg.encode()
    return 502, b"yt-dlp that bai"


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def make_handler(cfg):
    class H(BaseHTTPRequestHandler):
        def log_message(self, fmt, *a):  # không ghi URL đầy đủ/token vào log
            pass

        def reply(self, code, body=b"", ctype="text/plain; charset=utf-8"):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            if not cfg.token:
                return True
            got = self.headers.get("Authorization", "")
            return hmac.compare_digest(got.encode(), f"Bearer {cfg.token}".encode())

        def do_GET(self):
            u = urlparse(self.path)
            if u.path == "/health":
                return self.reply(200, b"ok")
            if u.path != "/info":
                return self.reply(404, b"not found")
            if not self.authorized():
                return self.reply(401, "sai mã truy cập".encode())
            target = (parse_qs(u.query).get("url") or [""])[0].strip()
            t = urlparse(target)
            if t.scheme not in ("http", "https") or not t.hostname:
                return self.reply(400, "url không hợp lệ".encode())
            host = t.hostname.lower()
            if not cfg.allow_any and not any(host == h or host.endswith("." + h) for h in ALLOWED_HOSTS):
                return self.reply(403, f"chỉ nhận link Facebook (không nhận {host})".encode())
            if not slots.acquire(timeout=1):
                return self.reply(429, "máy chủ đang bận, thử lại sau".encode())
            try:
                log(f"yt-dlp: {host}{t.path[:60]}")
                code, body = run_info(cfg, target)
            finally:
                slots.release()
            return self.reply(code, body, "application/json; charset=utf-8" if code == 200 else "text/plain; charset=utf-8")

    return H


def main():
    ap = argparse.ArgumentParser(description="Máy chủ yt-dlp nhỏ cho FB Live Audio")
    ap.add_argument("--host", default="127.0.0.1", help="địa chỉ lắng nghe (0.0.0.0 để cả mạng LAN truy cập được)")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--token", default="", help="mã truy cập (app gửi trong header Authorization: Bearer ...)")
    ap.add_argument("--allow-any", action="store_true", help="nhận link của mọi trang (mặc định chỉ Facebook)")
    ap.add_argument("--yt-dlp", default="", help="đường dẫn yt-dlp (mặc định tự tìm)")
    ap.add_argument("--ytdlp-args", default="", help='tham số thêm, ví dụ "--cookies-from-browser firefox"')
    ap.add_argument("--timeout", type=int, default=60)
    ap.add_argument("--no-update", dest="auto_update", action="store_false", help="không tự cập nhật yt-dlp khi trích xuất hỏng")
    cfg = ap.parse_args()
    import shlex
    cfg.extra = shlex.split(cfg.ytdlp_args)
    if cfg.host not in ("127.0.0.1", "localhost", "::1") and not cfg.token:
        log("CẢNH BÁO: đang mở cho cả mạng mà không có --token. Ai trong mạng cũng dùng được máy chủ này.")
    srv = ThreadingHTTPServer((cfg.host, cfg.port), make_handler(cfg))
    log(f"Máy chủ yt-dlp: http://{cfg.host}:{cfg.port}  (Ctrl+C để dừng)")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
