# FB Live Audio — bản "pipe luồng thô"

Nghe Facebook Live **chỉ bằng âm thanh** (Android + Windows), độ trễ thấp. Bản này đổi hẳn kiến trúc so với bản
DASH tự viết: ứng dụng **không tự tải và không phân tích file `.mpd`**; một công cụ ngoài (yt-dlp) lo từ link tới
luồng âm thanh và ghi ra stdout, ứng dụng chỉ liên tục đọc dòng byte đó.

```
 yt-dlp … -o -  ──stdout──►  đọc byte thô ─► demux (ADTS | fMP4) ─► lọc AU lặp ─► giải mã AAC ─► cổng bỏ lịch sử
 (link → manifest → tải         (luồng đọc         tự nhận dạng       (ffmpeg phát        Symphonia │              │
  → xuất AAC; live qua ffmpeg)   riêng)                                đoạn live 2 lần)    / FDK-AAC  ▼              ▼
                                                                                       vòng đệm → loa: bám live, đệm chống giật, điều tốc
```

Nguồn byte thô (`core/src/source.rs`): yt-dlp tại chỗ · máy chủ yt-dlp qua HTTP (`scripts/ytdlp_helper.py`, cho
Android) · lệnh tuỳ ý (`--cmd`) · stdin (`yt-dlp … -o - | fbaudio -`).

> **Đọc phần "Đo thực tế" và "Chưa kiểm chứng" trước khi tin vào bản này.** Mọi số đo dùng luồng DASH live *mô
> phỏng* (ffmpeg phát, đoạn 2 s). Facebook thật không truy cập được từ môi trường phát triển nên **chưa thử lần nào**.

## Cách dùng

**Cần:** yt-dlp, và **ffmpeg để nghe live** (Windows: `winget install yt-dlp.yt-dlp` + `winget install Gyan.FFmpeg`).
Phát lại video đã kết thúc không cần ffmpeg.

```
fbaudio "https://www.facebook.com/…/videos/123"             # LIVE (mặc định): bám live
fbaudio LINK --replay                                       # video đã kết thúc: phát đủ từ đầu
fbaudio LINK --mode balanced|stable                         # đệm lớn hơn nếu mạng chập chờn
fbaudio LINK --ytdlp-args "--cookies-from-browser firefox"  # video cần đăng nhập
fbaudio LINK --server http://192.168.1.5:8787 --token MATKHAU
yt-dlp -f ba -o - LINK | fbaudio - --replay
```

**Live hay phát lại do bạn chọn** (ô "Video đã kết thúc" trong app, `--replay` ở CLI): yt-dlp không báo `is_live` cho
Facebook nên ứng dụng không tự biết được.

**Android** không chạy được yt-dlp. Chạy máy chủ yt-dlp (Termux ngay trên điện thoại, hoặc máy tính cùng mạng) rồi nhập
địa chỉ vào "Nâng cao":
```
pkg install python ffmpeg && pip install -U yt-dlp           # Termux
python scripts/ytdlp_helper.py [--host 0.0.0.0 --token MATKHAU]
```

## Đo thực tế

Nguồn: ffmpeg phát DASH live có **cả video lẫn audio** (video đứng trước), đoạn 2 s, phục vụ qua link không kết thúc
bằng `.mpd`. yt-dlp 2026.08.19, ffmpeg 6.1.1, bản debug, mức Ultra, ghi WAV. Tín hiệu chứa mốc mã hoá hai âm (nhận dạng
không mơ hồ trong 64 s) nên đo được độ trễ tuyệt đối. Tái tạo: `scripts/e2e-pipe.sh` (xem biến môi trường ở đầu file).

**1. yt-dlp → ffmpeg, extractor Generic (tự đánh dấu live), thay đổi tốc độ đọc của ffmpeg:**

| ffmpeg đọc | lịch sử | kết quả |
|---|---|---|
| `-re` (mặc định của yt-dlp) | 15 s | liền mạch nhưng **trễ 21,3 s** |
| `-re` | 40 s | liền mạch nhưng **trễ 46,4 s** (trễ ≈ cả cửa sổ lịch sử + ~6 s khởi động; theo mã `dashdec` tối đa ~60 s, **chưa đo** trên lịch sử dài hơn 40 s) |
| `-readrate 100` | ~10 s | phát lặp cả lịch sử (764 KB ≈ 64 s âm thanh trong 8 s) rồi **luồng đứng** |
| **`-readrate 8` + cổng bỏ lịch sử** | 40 s | im lặng 7,9 s lúc vào, rồi **liền mạch**, +102 ms, **tổng ≈ 2,1 s** |

**2. Cấu hình mặc định của app (ép ffmpeg, `-readrate 8`):**

| kịch bản | tiếng đầu | liền mạch | độ trễ cộng thêm | tổng (đoạn 2 s) |
|---|---|---|---|---|
| đường mã Facebook mô phỏng, lịch sử 40 s, chạy 50 s | 7,7 s | 43 mốc liên tục, 1 khe 180 ms (1 lần ngắt) | trung vị 230 ms (tối đa 332) | **≈ 2,2 s** |
| qua máy chủ yt-dlp (đường Android), lịch sử 30 s, 40 s | 6,8 s | 34 mốc liên tục, 0 khe | trung vị 143 ms | **≈ 2,1 s** |
| **phát lại** (`--replay`, bộ tải gốc, không ffmpeg) | 0,5 s | 22 mốc liên tục, kết thúc gọn | — | — |

So sánh: bản DASH tự viết (`fb-live-audio.zip`, bản trước) đo được +160…205 ms, tổng ≈ 2,2 s với đoạn 2 s, không cần
ffmpeg, và nhảy thẳng tới đoạn mới nhất theo manifest nên không phải "nuốt lịch sử" lúc vào. **Tức là pipe đạt cùng độ
trễ khi mọi thứ chạy đúng, nhưng trả giá bằng thời gian vào live (~lịch sử/7 + 2 s) và phụ thuộc ffmpeg.**

## Điều học được khi đọc mã yt-dlp (2026.08.19) và đo — quyết định thiết kế

- **Extractor Facebook không đặt `is_live`** (chỉ extractor Generic đặt khi MPD `dynamic`). Với yt-dlp, live Facebook
  là "video thường": bộ tải gốc tải đúng các đoạn đang có trong manifest rồi **thoát**, không bám live. Đã mô phỏng
  bằng plugin extractor giống `facebook.py`: app phát từ đầu cửa sổ lịch sử. Vì vậy ở chế độ live app **ép ffmpeg cho
  DASH** (`--downloader dash:ffmpeg`; ffmpeg tự biết MPD là `dynamic`). Hệ quả: live **bắt buộc có ffmpeg**.
- Bộ tải gốc từ chối live DASH; `--live-from-start` (`http_dash_segments_generator`) **chỉ có ở extractor YouTube**.
- Muxer mặc định (`ipod`, đuôi .m4a) không ghi được ra stdout → ép `-f adts -flush_packets 1`.
- Không thêm `-readrate N` lớn "cho vào live nhanh" (đo ở trên). `-re` thì trễ bằng cả lịch sử. `8` là điểm đã đo ổn.
- **ffmpeg phát mỗi đoạn live 2 lần** (chuỗi mốc `46,47,48,49 | 48,49,50,51 | …`) → `Dedupe` loại bản lặp theo nội dung.
- Cách "gom đợt theo khoảng cách giữa các lần đẩy" từng thử ở `Output` bị chia vụn sau khi lọc lặp (đợt giả 23 ms) và
  cắt nhầm âm thanh. Thay bằng **đáy mức đệm trong 3 s** (đo phía tiêu thụ, không phụ thuộc cách công cụ chia dữ liệu).
- Bug thật tìm thấy: phần "bỏ qua" đang chờ callback bị tính trùng khi nhiều lần đẩy liên tiếp → đã sửa (trừ phần chờ).
- yt-dlp bộ tải gốc để tệp tạm `-Frag<N>` ở thư mục làm việc nếu bị ngắt → app chạy nó trong thư mục tạm riêng, xoá khi xong.
- **Streamlink đã đánh giá, không dùng:** plugin facebook chỉ đọc khoá cũ `dash_manifest_url`; `dash://` với MPD có cả
  video lẫn audio chỉ liệt kê luồng ghép (Matroska qua ffmpeg), không có audio-only.

## Chưa kiểm chứng (đừng bỏ qua)

- **Facebook thật.** Manifest thật có thể khác mô phỏng: kiểu `SegmentTimeline`/`$Number$`, độ dài cửa sổ DVR, số
  Representation video, thứ tự AdaptationSet. Chạy `fbaudio LINK -v` trên link thật là việc đầu tiên nên làm.
- **`-map 0:N` của yt-dlp** giả định thứ tự AdaptationSet trong MPD trùng thứ tự luồng của ffmpeg (video trước). Nếu MPD
  Facebook để audio trước, yt-dlp map nhầm video và lỗi muxer; app sẽ hiện lỗi ffmpeg, **chưa có cách vá** (tham số
  `-map` do yt-dlp sinh).
- ffmpeg mở **mọi** Representation lúc thăm dò. Đo được 2 yêu cầu video với 1 Representation video; Facebook có nhiều
  chất lượng nên có thể tải thêm một ít video lúc mở (chưa đo).
- Cổng bỏ lịch sử (`GATE_*` trong `core/src/player.rs`) chỉnh bằng thực nghiệm với đoạn 2 s; đoạn dài ≥ 4,5 s chưa thử.
- **Windows/Android chưa biên dịch.** Mã `cfg(windows)` (`taskkill`, `CREATE_NO_WINDOW`) chưa được compile; dịch vụ nền
  Kotlin và JNI chưa dựng. Chỉ chạy được trên Linux trong môi trường phát triển.
- Số đo từ bản debug với mức Ultra (đệm 120 ms) trên máy đang bận mã hoá video: có 1 lần ngắt trong 50 s.
- Tự cập nhật `yt-dlp -U` chỉ được thử với yt-dlp giả.
- HE-AAC: mặc định phát phần lõi (thiếu dải cao); `--features sbr` dùng FDK-AAC (giấy phép Fraunhofer riêng) cho đủ dải cao.

## Cấu trúc

```
core/        fbaudio-core: source (nguồn byte) · demux (ADTS|fMP4, lọc lặp) · decode · output · player
cli/         fbaudio (dòng lệnh, để thử nhanh)
app/         Tauri 2: src-tauri (lõi nối UI) · ui/index.html · android-overlay/ (dịch vụ nền Kotlin)
scripts/     ytdlp_helper.py (máy chủ yt-dlp) · e2e-pipe.sh + analyze_wav.py (đo) · alias_server.py
             make-adts-fixtures.sh · he-fixtures/ (tạo fixture HE-AAC)
```

Test: `cargo test -p fbaudio-core` (32 test; đạt cả khi thêm `--features sbr`). Gồm yt-dlp giả bằng shell script: phát
đủ, bản lặp bị loại, hiện dòng lỗi của yt-dlp, gợi ý khi thiếu ffmpeg/đổi định dạng, nút Dừng diệt cả cây tiến trình,
tự cập nhật rồi thử lại, thư mục tạm được dọn, cổng bỏ lịch sử.

Biến môi trường: `FBAUDIO_YTDLP` (đường dẫn yt-dlp), `FBAUDIO_YTDLP_ARGS` (tham số thêm), `FBAUDIO_NO_UPDATE=1`.
