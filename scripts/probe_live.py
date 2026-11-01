#!/usr/bin/env python3
"""Chẩn đoán một link Facebook Live: yt-dlp thấy gì, và manifest có thực sự "sống" không.

Chạy cái này trên LINK THẬT trước khi tin vào bất kỳ chiến lược nào:

    python scripts/probe_live.py "https://www.facebook.com/.../videos/123" [--cookies cookies.txt]

Nó trả lời ba câu hỏi quyết định cách app lấy luồng live:
  1. yt-dlp có coi đây là live không (`is_live`)? (extractor Facebook không đặt trường này)
  2. Trang có cung cấp URL manifest không, hay chỉ có bản XML nhúng (ảnh chụp tĩnh tại lúc tải trang)?
  3. Nếu có URL: tải nó hai lần cách nhau vài giây, manifest có TIẾN THÊM đoạn mới không?
     - có  → ép ffmpeg bám theo được (độ trễ thấp nhất, ~2 s);
     - không → chỉ còn cách chạy lại yt-dlp từng lượt và nối tiếp (app tự làm; độ trễ cao hơn).

Chỉ đọc, không tải âm thanh. Cần `pip install yt-dlp`.
"""
import argparse
import sys
import time
import xml.etree.ElementTree as ET

try:
    from yt_dlp import YoutubeDL
    from yt_dlp.networking import Request
except ImportError:
    sys.exit("Cần yt-dlp: pip install -U yt-dlp")


def local(tag):
    return tag.rsplit("}", 1)[-1]


def mpd_state(data):
    """Tóm tắt một file MPD: loại, chu kỳ cập nhật, và điểm cuối timeline của từng AdaptationSet."""
    root = ET.fromstring(data)
    st = {k: root.get(k) for k in ("type", "minimumUpdatePeriod", "timeShiftBufferDepth", "availabilityStartTime",
                                   "mediaPresentationDuration")}
    st["sets"] = []
    for aset in root.iter():
        if local(aset.tag) != "AdaptationSet":
            continue
        kind = aset.get("contentType") or (aset.get("mimeType") or "?").split("/")[0]
        tmpl = next((e for e in aset.iter() if local(e.tag) == "SegmentTemplate"), None)
        info = {"kind": kind, "segments": None, "first_s": None, "end_s": None}
        if tmpl is not None:
            ts = int(tmpl.get("timescale") or 1)
            t = None
            total = n = 0
            for s in tmpl.iter():
                if local(s.tag) != "S":
                    continue
                if t is None:
                    t = int(s.get("t") or 0)
                d, r = int(s.get("d")), int(s.get("r") or 0)
                total += d * (r + 1)
                n += r + 1
            if n:
                info.update(segments=n, first_s=round(t / ts, 2), end_s=round((t + total) / ts, 2))
        st["sets"].append(info)
    return st


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("url")
    ap.add_argument("--cookies", help="file cookies.txt (định dạng Netscape)")
    ap.add_argument("--wait", type=float, default=6.0, help="chờ bao nhiêu giây giữa hai lần tải manifest (mặc định 6)")
    ap.add_argument("--plugin-dirs", help="thư mục plugin extractor (chỉ để kiểm thử)")
    a = ap.parse_args()

    params = {"quiet": True, "no_warnings": True, "skip_download": True, "noplaylist": True}
    if a.cookies:
        params["cookiefile"] = a.cookies
    if a.plugin_dirs:
        params["plugin_dirs"] = [a.plugin_dirs]
    with YoutubeDL(params) as ydl:
        try:
            info = ydl.extract_info(a.url, download=False)
        except Exception as e:  # noqa: BLE001
            sys.exit(f"yt-dlp không lấy được thông tin: {e}")

        print(f"Tiêu đề: {info.get('title')}")
        print(f"is_live={info.get('is_live')!r}  live_status={info.get('live_status')!r}  duration={info.get('duration')!r}")
        print(f"extractor: {info.get('extractor_key')}")
        audio = [f for f in info.get("formats") or [] if f.get("acodec") not in (None, "none")]
        print(f"\nĐịnh dạng có âm thanh: {len(audio)}")
        manifest = None
        headers = {}
        for f in audio:
            frags = f.get("fragments") or []
            dur = sum(x.get("duration") or 0 for x in frags)
            murl = f.get("manifest_url")
            print(f"  {f.get('format_id')}: protocol={f.get('protocol')} vcodec={f.get('vcodec')} "
                  f"abr={f.get('abr')} manifest_url={'CÓ' if murl else 'KHÔNG'} "
                  f"đoạn={len(frags)} (~{dur:.0f}s)")
            if murl and not manifest and f.get("protocol") == "http_dash_segments":
                manifest, headers = murl, f.get("http_headers") or {}
        dash = [f for f in audio if f.get("protocol") == "http_dash_segments"]

        print("\n=== KẾT LUẬN ===")
        if not manifest:
            print("Trang KHÔNG cung cấp URL manifest cho định dạng DASH (chỉ có XML nhúng = ảnh chụp tĩnh lúc tải trang).")
            print("→ Ép ffmpeg không dùng được. App sẽ chạy lại yt-dlp từng lượt và nối tiếp (độ trễ ≈ thời gian mỗi lượt + đoạn).")
            if dash:
                n = max(len(f.get("fragments") or []) for f in dash)
                print(f"  Mỗi lượt yt-dlp chỉ tải {n} đoạn rồi thoát: đó là toàn bộ cửa sổ mà trang cho thấy.")
            return

        def fetch():
            req = Request(manifest, headers=headers)
            return ydl.urlopen(req).read()

        try:
            s1 = mpd_state(fetch())
            print(f"Manifest: type={s1['type']} minimumUpdatePeriod={s1['minimumUpdatePeriod']} "
                  f"timeShiftBufferDepth={s1['timeShiftBufferDepth']}")
            for s in s1["sets"]:
                print(f"  AdaptationSet {s['kind']}: {s['segments']} đoạn, timeline {s['first_s']}s → {s['end_s']}s")
            print(f"Chờ {a.wait:g} s rồi tải lại manifest...")
            time.sleep(a.wait)
            s2 = mpd_state(fetch())
        except Exception as e:  # noqa: BLE001
            print(f"Không tải/đọc được manifest ({e}).")
            print("→ Ép ffmpeg nhiều khả năng cũng không đọc được; app sẽ lùi về chạy lặp từng lượt.")
            return

        def end_of(st):
            ends = [s["end_s"] for s in st["sets"] if s["kind"] == "audio" and s["end_s"] is not None]
            return max(ends) if ends else None

        e1, e2 = end_of(s1), end_of(s2)
        print(f"Điểm cuối timeline audio: {e1}s → {e2}s")
        if s2["type"] == "dynamic" and e1 is not None and e2 is not None and e2 > e1:
            print(f"→ MANIFEST SỐNG (tiến thêm {e2 - e1:.1f}s). Ép ffmpeg bám live được: app sẽ thử đường này trước,")
            print("  độ trễ kỳ vọng ~2 s. Nếu app vẫn lỗi ở đường ffmpeg, hãy gửi dòng lỗi (thường do thứ tự AdaptationSet).")
        elif s2["type"] == "static":
            print("→ Manifest là TĨNH (type=static): ffmpeg cũng chỉ đọc hết danh sách rồi dừng. App sẽ chạy lặp từng lượt.")
        else:
            print("→ Manifest không tiến thêm trong thời gian chờ (live đang tạm dừng, hoặc URL chỉ là ảnh chụp).")
            print("  Thử lại với --wait 15. Nếu vẫn đứng yên, coi như tĩnh: app sẽ chạy lặp từng lượt.")


if __name__ == "__main__":
    main()
