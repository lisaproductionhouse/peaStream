#!/usr/bin/env python3
"""Phân tích WAV do `fbaudio --wav` ghi khi nghe luồng thử nghiệm của e2e-local-dash.sh.

Tín hiệu nguồn = âm nền 440 Hz liên tục + một "mốc" dài 150 ms ở đầu mỗi giây k.
Cao độ mốc = 800·1,15^(k mod 8) Hz. Hai mốc liền kề cách nhau 15% > mức đổi cao độ tối đa
của bộ điều tốc (6%) nên vẫn đọc được k khi đang phát nhanh/chậm.

Báo cáo: (1) đoạn nào bị mất/lặp, (2) khoảng lặng giữa chừng,
(3) "độ trễ do ứng dụng thêm vào" = lúc nghe − (mtime thật của file đoạn + vị trí mốc trong đoạn).
Số (3) không phụ thuộc cách ffmpeg pace nguồn nên đáng tin; độ trễ tuyệt đối chỉ là ước lượng thô.

Dùng:  analyze_wav.py <thư mục DASH> <epoch lúc ffmpeg bắt đầu>      (cần numpy)
"""
import os, re, sys, wave
try:
    import numpy as np
except ImportError:
    sys.exit("Cần numpy:  pip install numpy")

ddir = sys.argv[1]
ff_start = float(sys.argv[2]) if len(sys.argv) > 2 else None
wav_path = os.path.join(ddir, "out.wav")

w = wave.open(wav_path, "rb")
rate, n = w.getframerate(), w.getnframes()
x = np.frombuffer(w.readframes(n), dtype="<i2").astype(np.float32)[0::2] / 32768
dur = n / rate
# WAV được ghi theo thời gian thực nên: lúc bắt đầu = mtime − độ dài (chính xác hơn mốc lấy từ shell)
t_wav0 = os.stat(wav_path).st_mtime - dur

# 1) khoảng lặng: âm nền 440 Hz phải liên tục
h20 = int(rate * 0.02)
rms = np.array([np.sqrt(np.mean(x[i:i + h20] ** 2)) for i in range(0, len(x) - h20, h20)])
voiced = np.where(rms > 0.03)[0]
if len(voiced) == 0:
    sys.exit("KHÔNG có âm thanh trong WAV")
gaps = [(a * 0.02, (b - a - 1) * 0.02) for a, b in zip(voiced, voiced[1:]) if b - a > 3]
print(f"WAV dài {dur:.1f}s; âm thanh bắt đầu lúc {voiced[0]*0.02:.2f}s; "
      f"khoảng lặng giữa chừng: {len(gaps)}" + (" → " + ", ".join(f"{t:.1f}s(+{d*1000:.0f}ms)" for t, d in gaps) if gaps else ""))

# 2) tìm mốc: cửa sổ 20 ms, bước 5 ms; mốc = năng lượng băng 700–2600 Hz vượt ngưỡng
hop, win = int(rate * 0.005), int(rate * 0.02)
fr = np.fft.rfftfreq(win, 1 / rate)
band = (fr > 700) & (fr < 2600)
f440 = int(np.argmin(np.abs(fr - 440)))
hann = np.hanning(win)
events, cur = [], None
for i in range(0, len(x) - win, hop):
    mag = np.abs(np.fft.rfft(x[i:i + win] * hann))
    j = int(np.argmax(np.where(band, mag, 0)))
    t = i / rate
    if mag[j] > 0.15 * mag[f440] and mag[j] > 2:
        if cur and t - cur["last"] < 0.03:
            cur["last"] = t; cur["f"].append(fr[j])
        else:
            cur = {"t": t + 0.01, "last": t, "f": [fr[j]]}; events.append(cur)
marks = []
for e in events:
    if e["last"] - e["t"] < 0.08:
        continue  # quá ngắn → nhiễu
    s = int(round(np.log(np.median(e["f"]) / 800) / np.log(1.15)))
    if 0 <= s < 8:
        marks.append((e["t"], s))
print(f"phát hiện {len(marks)} mốc thời gian")
if not marks:
    sys.exit(1)

# 3) số giây tuyệt đối k (k ≡ s mod 8) sao cho độ trễ thô nằm trong [-0.5, 7.5) giây
ks = []
for t, s in marks:
    base = (t_wav0 + t) - (ff_start or t_wav0)
    k = int(np.floor(base)) - ((int(np.floor(base)) - s) % 8)
    while base - k >= 7.5: k += 8
    while base - k < -0.5: k -= 8
    ks.append(k)
steps = [b - a for a, b in zip(ks, ks[1:])]
print("bước nhảy giữa các mốc liên tiếp (1 = liên tục):", steps)

# 4) độ trễ do ứng dụng thêm vào, đo so với mtime file đoạn
mpd = open(os.path.join(ddir, "manifest.mpd"), encoding="utf-8").read()
mpd = mpd.split("</AdaptationSet>")[0]  # chỉ AdaptationSet đầu tiên (audio); MPD có video thì phần sau là video
ts = int(re.search(r'timescale="(\d+)"', mpd).group(1))
num = int(re.search(r'startNumber="(\d+)"', mpd).group(1))
segs, c = [], 0
for m in re.finditer(r'<S\s+([^/]*?)/>', mpd):
    a = dict(re.findall(r'(\w+)="(\d+)"', m.group(1)))
    c = int(a.get("t", c)); d = int(a["d"])
    for _ in range(int(a.get("r", 0)) + 1):
        segs.append((c, d)); c += d
added = []
for (t, _), k in zip(marks, ks):
    for i, (t0, d) in enumerate(segs):
        if t0 <= k * ts < t0 + d:
            f = os.path.join(ddir, f"chunk-0-{num + i:05d}.m4s")
            if os.path.exists(f):
                added.append((t_wav0 + t) - (os.stat(f).st_mtime + (k - t0 / ts)))
            break
if added:
    first, tail = added[0], added[len(added) // 3:]
    print(f"\n>>> ỨNG DỤNG THÊM VÀO ngay khi vào live: {first*1000:.0f} ms")
    print(f">>> ỨNG DỤNG THÊM VÀO khi ổn định:      trung vị {np.median(tail)*1000:.0f} ms "
          f"(min {min(tail)*1000:.0f}, max {max(tail)*1000:.0f})  = đệm mục tiêu + thời gian tải")
    print(">>> 6 mốc cuối (ms):", " ".join(f"{v*1000:.0f}" for v in added[-6:]),
          " ← phải về mức ổn định nếu đã hồi phục sau sự cố")
    seg_s = np.median([d / ts for _, d in segs[:-1]])
    print(f">>> + độ dài đoạn nguồn {seg_s:.2f}s  ⇒ độ trễ toàn phần xấp xỉ {seg_s + np.median(tail):.2f}s "
          f"(đoạn phải đủ dài mới xuất bản được)")
