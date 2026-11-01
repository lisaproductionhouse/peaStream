#!/usr/bin/env python3
"""Máy chủ yt-dlp nhỏ cho FB Live Audio: chạy `yt-dlp -o -` và truyền luồng byte thô qua HTTP.

Android không có Python nên app không tự chạy yt-dlp được. Chạy script này ở nơi có yt-dlp (và ffmpeg, nếu muốn
nghe live) rồi nhập địa chỉ vào mục "Nâng cao" của app:

  * ngay trên điện thoại, bằng Termux (không cần máy chủ nào khác):
        pkg install python ffmpeg
        pip install -U yt-dlp
        python ytdlp_helper.py                      # địa chỉ trong app:  http://127.0.0.1:8787
  * hoặc trên máy tính cùng mạng Wi-Fi (nhớ đặt mã truy cập):
        python ytdlp_helper.py --host 0.0.0.0 --token MATKHAU    # địa chỉ:  http://<IP máy tính>:8787

Giao thức:
  GET /stream?url=<link>&live=1|0   (live=1: bám live, cần ffmpeg;  live=0: video đã kết thúc, không cần ffmpeg)
      → 200, thân là luồng AAC thô (ADTS hoặc fMP4) kết thúc khi đóng kết nối (HTTP/1.0), kèm header
        X-Fbaudio-Title (tiêu đề, mã hoá %XX). Máy chủ chỉ trả đầu response khi yt-dlp đã có byte đầu tiên,
        nên lỗi (link sai, cần đăng nhập, thiếu ffmpeg...) luôn là mã 4xx/5xx + một dòng giải thích.
  GET /health   → "ok"

An toàn: mặc định chỉ lắng nghe 127.0.0.1, chỉ nhận link Facebook, có thể đòi mã truy cập, URL luôn được truyền
sau `--` (không thể bị hiểu thành tuỳ chọn của yt-dlp), không dùng shell, và khi máy khách ngắt kết nối thì diệt
cả nhóm tiến trình (yt-dlp cùng ffmpeg của nó).

Tham số yt-dlp phải khớp với core/src/source.rs (hàm ytdlp_args); lý do từng tham số ghi ở đó và trong README.
"""
import argparse
import collections
import hmac
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, quote, urlparse

ALLOWED_HOSTS = ("facebook.com", "fb.watch", "fb.com", "fb.me")
BREAK = re.compile(r"cannot parse data|unable to extract|please report this issue|no video formats", re.I)
LIVE_CATCHUP_RATE = 8  # khớp LIVE_CATCHUP_RATE trong core/src/source.rs
updated_once = threading.Event()
slots = threading.BoundedSemaphore(2)  # tối đa 2 yt-dlp chạy cùng lúc


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def ytdlp_cmd(path):
    if path:
        return [path]
    exe = shutil.which("yt-dlp")
    return [exe] if exe else [sys.executable, "-m", "yt_dlp"]


def ytdlp_args(cfg, url, live):
    a = ["-f", "ba", "-o", "-", "--no-playlist", "--no-warnings", "--ignore-config", "--no-mtime",
         "--socket-timeout", "15", "--print", "before_dl:FBAUDIO_TITLE=%(title)s"]
    if live:
        # extractor Facebook không đặt is_live nên yt-dlp sẽ không tự bám live: ép ffmpeg cho DASH
        a += ["--downloader", "dash:ffmpeg",
              "--downloader-args", f"ffmpeg_i:-readrate {LIVE_CATCHUP_RATE}",
              "--downloader-args", "ffmpeg_o:-f adts -flush_packets 1"]
    return a + cfg.extra + ["--", url]


def clean(line):
    return re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", line).rstrip()


def error_line(tail):
    lines = [l for l in tail if l.strip()]
    errs = [l for l in lines if l.lstrip().startswith("ERROR:")]
    line = (errs or lines or ["yt-dlp thoát với lỗi không rõ"])[-1]
    line = line.replace("ERROR:", "", 1).strip().split("; please report")[0]
    return line.rstrip(".,; ")[:240]


def kill_tree(proc):
    try:
        if os.name == "nt":
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"], capture_output=True)
        else:
            os.killpg(proc.pid, signal.SIGKILL)  # nhóm riêng (start_new_session) nên không bắn nhầm ai khác
    except (ProcessLookupError, PermissionError, OSError):
        pass
    try:
        proc.kill()
    except OSError:
        pass


class Job:
    """Một lần chạy yt-dlp: stderr được đọc ở luồng riêng để lấy tiêu đề và dòng lỗi."""

    def __init__(self, cfg, url, live):
        cmd = ytdlp_cmd(cfg.yt_dlp) + ytdlp_args(cfg, url, live)
        kw = {"start_new_session": True} if os.name != "nt" else {"creationflags": 0x00000200}
        # thư mục làm việc riêng: yt-dlp (bộ tải gốc) để tệp tạm `-Frag<N>` ở đó nếu bị ngắt giữa chừng
        self.tmp = tempfile.mkdtemp(prefix="fbaudio-")
        try:
            self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                         cwd=self.tmp, env={**os.environ, "PYTHONIOENCODING": "utf-8", "PYTHONUTF8": "1"}, **kw)
        except OSError:
            shutil.rmtree(self.tmp, ignore_errors=True)
            raise
        self.tail = collections.deque(maxlen=16)
        self.title = None
        threading.Thread(target=self._read_stderr, daemon=True).start()

    def _read_stderr(self):
        for raw in self.proc.stderr:
            line = clean(raw.decode("utf-8", "replace"))
            if line.startswith("FBAUDIO_TITLE="):
                t = line.split("=", 1)[1].strip()
                self.title = None if t in ("", "NA") else t
            elif line.strip():
                self.tail.append(line)

    def cleanup(self):
        kill_tree(self.proc)
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            pass
        shutil.rmtree(self.tmp, ignore_errors=True)

    def first_chunk(self, timeout):
        """Chờ byte đầu tiên. Trả (bytes | None, lý do thất bại | None)."""
        box = {}

        def reader():
            box["d"] = os.read(self.proc.stdout.fileno(), 65536)

        th = threading.Thread(target=reader, daemon=True)
        th.start()
        th.join(timeout)
        if th.is_alive():
            kill_tree(self.proc)
            return None, f"yt-dlp không xuất ra dữ liệu sau {timeout} giây (link có phát được không?)"
        data = box.get("d", b"")
        if data:
            return data, None
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            kill_tree(self.proc)
        time.sleep(0.1)  # cho luồng stderr kịp nhận dòng cuối
        return None, error_line(self.tail)


def open_stream(cfg, url, live):
    """Chạy yt-dlp tới khi có byte đầu tiên. Trả (Job, chunk đầu, None) hoặc (None, None, (mã HTTP, thông báo))."""
    for attempt in (0, 1):
        try:
            job = Job(cfg, url, live)
        except FileNotFoundError:
            return None, None, (500, "máy chủ chưa cài yt-dlp (pip install -U yt-dlp)")
        chunk, err = job.first_chunk(cfg.timeout)
        if chunk:
            return job, chunk, None
        job.cleanup()
        if "No module named" in err:
            return None, None, (500, "máy chủ chưa cài yt-dlp (pip install -U yt-dlp)")
        # Facebook vừa đổi cấu trúc → thử cập nhật yt-dlp một lần rồi chạy lại
        if attempt == 0 and cfg.auto_update and BREAK.search(err) and not updated_once.is_set():
            updated_once.set()
            log("yt-dlp báo trích xuất hỏng → cập nhật")
            cmd = ytdlp_cmd(cfg.yt_dlp)
            upd = [sys.executable, "-m", "pip", "install", "-U", "yt-dlp"] if len(cmd) > 1 and cmd[1] == "-m" else cmd + ["-U"]
            try:
                subprocess.run(upd, capture_output=True, timeout=300)
            except Exception as e:  # noqa: BLE001
                log(f"cập nhật thất bại: {e}")
            continue
        if "ffmpeg" in err.lower():
            err += " — luồng live cần ffmpeg trên máy chủ (Termux: pkg install ffmpeg)"
        return None, None, (502, err)
    return None, None, (502, "yt-dlp thất bại")


def make_handler(cfg):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"  # thân kết thúc khi đóng kết nối: không cần Content-Length

        def log_message(self, fmt, *a):  # không ghi URL đầy đủ/token vào log
            pass

        def reply(self, code, body=b""):
            self.send_response(code)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
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
            if u.path != "/stream":
                return self.reply(404, b"not found")
            if not self.authorized():
                return self.reply(401, "sai mã truy cập".encode())
            q = parse_qs(u.query)
            target = (q.get("url") or [""])[0].strip()
            live = (q.get("live") or ["1"])[0] != "0"
            t = urlparse(target)
            if t.scheme not in ("http", "https") or not t.hostname:
                return self.reply(400, "url không hợp lệ".encode())
            host = t.hostname.lower()
            if not cfg.allow_any and not any(host == h or host.endswith("." + h) for h in ALLOWED_HOSTS):
                return self.reply(403, f"chỉ nhận link Facebook (không nhận {host})".encode())
            if not slots.acquire(timeout=1):
                return self.reply(429, "máy chủ đang bận, thử lại sau".encode())
            job = None
            try:
                log(f"yt-dlp ({'live' if live else 'phát lại'}): {host}{t.path[:60]}")
                job, chunk, err = open_stream(cfg, target, live)
                if err:
                    return self.reply(*err)
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                if job.title:
                    self.send_header("X-Fbaudio-Title", quote(job.title, safe=""))
                self.end_headers()
                self.wfile.write(chunk)
                self.wfile.flush()
                fd = job.proc.stdout.fileno()
                while True:
                    data = os.read(fd, 65536)
                    if not data:
                        break
                    self.wfile.write(data)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass  # máy khách (app) đã ngắt: bấm Dừng
            finally:
                if job:
                    job.cleanup()  # dù thoát bình thường hay máy khách ngắt: không để ffmpeg mồ côi, xoá thư mục tạm
                slots.release()

    return H


def main():
    ap = argparse.ArgumentParser(description="Máy chủ yt-dlp nhỏ cho FB Live Audio")
    ap.add_argument("--host", default="127.0.0.1", help="địa chỉ lắng nghe (0.0.0.0 để cả mạng LAN truy cập được)")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--token", default="", help="mã truy cập (app gửi trong header Authorization: Bearer ...)")
    ap.add_argument("--allow-any", action="store_true", help="nhận link của mọi trang (mặc định chỉ Facebook)")
    ap.add_argument("--yt-dlp", default="", help="đường dẫn yt-dlp (mặc định tự tìm)")
    ap.add_argument("--ytdlp-args", default="", help='tham số thêm, ví dụ "--cookies-from-browser firefox"')
    ap.add_argument("--timeout", type=int, default=90, help="chờ byte đầu tiên tối đa bao nhiêu giây")
    ap.add_argument("--no-update", dest="auto_update", action="store_false", help="không tự cập nhật yt-dlp khi trích xuất hỏng")
    cfg = ap.parse_args()
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
