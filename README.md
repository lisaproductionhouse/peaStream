# FB Live Audio — bản "pipe luồng thô"

Nghe Facebook Live **chỉ bằng âm thanh** (Android + Windows). Ứng dụng **không tự tải và không phân tích file `.mpd`**:
một công cụ ngoài (yt-dlp) lo từ link tới luồng âm thanh và ghi ra stdout, ứng dụng chỉ đọc dòng byte đó.

## Điều quan trọng nhất: yt-dlp không tự bám live trên Facebook

Quan sát trên Facebook thật: yt-dlp tải khoảng 11 đoạn rồi **thoát**, dù đã cập nhật bản mới nhất. Nguyên nhân đọc được từ
mã nguồn (yt-dlp 2026.08.19, `extractor/facebook.py`): trang Facebook trả manifest dạng **XML nhúng sẵn**, extractor
phân tích bản XML đó ngay lúc tải trang (một ảnh chụp tĩnh) chứ không tải lại manifest, và **không đặt `is_live`**. Bộ tải
gốc chỉ tải đúng danh sách đoạn trong ảnh chụp rồi kết thúc. Đây là hành vi của extractor, cập nhật yt-dlp không đổi được.

Vì vậy ở chế độ live, app thử lần lượt hai cách (`core/src/player.rs`):

1. **Ép ffmpeg** (`--downloader dash:ffmpeg`). ffmpeg được đưa URL manifest *nếu trang có cung cấp* và tự bám theo *nếu đó là
   manifest sống*. Độ trễ thấp nhất (~2 s). Không có URL, hoặc ffmpeg lỗi/treo (30 s không ra byte) → tự lùi về cách 2. Máy không
   có ffmpeg thì yt-dlp tự rơi về bộ tải gốc (đã kiểm: chỉ cảnh báo "Continuing without ffmpeg"), tức là cũng thành cách 2.
2. **Chạy lại yt-dlp từng lượt và nối tiếp.** Mỗi lượt yt-dlp tải cả cửa sổ rồi thoát; lượt sau luôn bắt đầu bằng phần
   *chồng lấn* với lượt trước. `Stitch` (`core/src/demux.rs`) nhận ra phần đầu đã thấy và chỉ giữ từ access unit mới đầu tiên.
   Vẫn là pipe thuần (chỉ đọc byte). Không cần ffmpeg, nhưng độ trễ cao hơn và tốn dữ liệu hơn (xem dưới).

**Việc đầu tiên nên làm với link thật:** `python scripts/probe_live.py "LINK" --cookies cookies.txt`. Nó cho biết yt-dlp thấy gì,
trang có cung cấp URL manifest không, và manifest có thật sự tiến thêm đoạn mới không, tức là cách 1 có dùng được hay chỉ còn cách 2.

```
 yt-dlp … -o -  ──stdout──►  đọc byte thô ─► demux ─► nối tiếp giữa lượt ─► lọc AU lặp ─► giải mã AAC ─► cổng bỏ lịch sử
 (chạy lại từng lượt         (luồng đọc        ADTS|    (Stitch: bỏ phần       (ffmpeg phát    Symphonia    (giữ phần đuôi)
  nếu nó thoát sớm)           riêng)           fMP4      chồng lấn)            đoạn 2 lần)     / FDK-AAC            │
                                                                               vòng đệm → loa: bám live, đệm chống giật, điều tốc
```

Nguồn byte thô (`core/src/source.rs`): yt-dlp tại chỗ · máy chủ yt-dlp qua HTTP (`scripts/ytdlp_helper.py`, cho Android) ·
lệnh tuỳ ý (`--cmd`) · stdin (`yt-dlp … -o - | fbaudio - --replay`).

> **Facebook thật chưa được thử với app này.** Mọi số đo bên dưới dùng luồng DASH live *mô phỏng* (ffmpeg phát, đoạn 2 s)
> và một plugin extractor bắt chước đường mã của `facebook.py`. Thông tin duy nhất từ Facebook thật là quan sát "yt-dlp
> tải ~11 đoạn rồi thoát", và cách 2 được thiết kế đúng cho hành vi đó.

## Cách dùng

**Cần:** yt-dlp. **ffmpeg không bắt buộc** (có thì app thử cách 1 trước). Windows: `winget install yt-dlp.yt-dlp` và
`winget install Gyan.FFmpeg`.

```
fbaudio "https://www.facebook.com/…/videos/123"             # LIVE (mặc định): bám live
fbaudio LINK --replay                                       # video đã kết thúc: phát đủ từ đầu
fbaudio LINK --mode balanced|stable                         # đệm lớn hơn nếu mạng chập chờn
fbaudio LINK --ytdlp-args "--cookies C:\cookies.txt"        # video cần đăng nhập
fbaudio LINK --no-ffmpeg                                    # bỏ cách 1, đi thẳng cách 2 (= FBAUDIO_NO_FFMPEG=1)
fbaudio LINK --server http://192.168.1.5:8787 --token MATKHAU
yt-dlp -f ba -o - LINK | fbaudio - --replay
```

**Cookie trên Windows:** Chrome/Edge mã hoá cookie nên `--cookies-from-browser` có thể lỗi (DPAPI). Dùng file thuần
`--cookies cookies.txt` (xuất bằng tiện ích "Get cookies.txt").

**Live hay phát lại do bạn chọn** (ô "Video đã kết thúc" trong app, `--replay` ở CLI): yt-dlp không báo `is_live` cho Facebook
nên ứng dụng không tự biết được.

**Android** không chạy được yt-dlp. Chạy máy chủ yt-dlp (Termux ngay trên điện thoại, hoặc máy tính cùng mạng) rồi nhập địa chỉ
vào "Nâng cao":
```
pkg install python ffmpeg && pip install -U yt-dlp           # Termux (ffmpeg không bắt buộc)
python scripts/ytdlp_helper.py [--host 0.0.0.0 --token MATKHAU]
```

## Đo thực tế

Nguồn: ffmpeg phát DASH live có **cả video lẫn audio** (video đứng trước), đoạn 2 s, qua link không kết thúc bằng `.mpd`.
yt-dlp 2026.08.19, ffmpeg 6.1.1, bản debug, ghi WAV. Tín hiệu có mốc mã hoá hai âm (nhận dạng không mơ hồ trong 64 s) nên đo
được độ trễ tuyệt đối. Tái tạo: `scripts/e2e-pipe.sh` (biến môi trường ở đầu file, gồm `WINDOW=11`, `FBAUDIO_NO_FFMPEG=1`).

**Cách 1 (ffmpeg) — tốc độ đọc của ffmpeg quyết định mọi thứ** (extractor Generic, lịch sử = cửa sổ DVR):

| ffmpeg đọc | lịch sử | kết quả |
|---|---|---|
| `-re` (mặc định của yt-dlp) | 15 s | liền mạch nhưng **trễ 21,3 s** |
| `-re` | 40 s | liền mạch nhưng **trễ 46,4 s** (≈ cả cửa sổ lịch sử + ~6 s; theo mã `dashdec` tối đa ~60 s, **chưa đo** trên lịch sử dài hơn 40 s) |
| `-readrate 100` | ~10 s | phát lặp cả lịch sử (764 KB ≈ 64 s âm thanh trong 8 s) rồi **luồng đứng** |
| **`-readrate 8` + cổng bỏ lịch sử** | 40 s | im lặng ~7 s lúc vào, rồi **liền mạch**, +102 ms |

**Cấu hình mặc định của app:**

| kịch bản | tiếng đầu | liền mạch | độ trễ cộng thêm | tổng (đoạn 2 s) |
|---|---|---|---|---|
| cách 1, manifest sống có URL, Ultra, lịch sử 40 s | 6,6 s | 1 khe ~170 ms (xem ghi chú) | trung vị 113 ms | **≈ 2,1 s** |
| cách 1, mức Balanced | 6,8 s | **0 khe, 0 lần ngắt** | trung vị 366 ms | ≈ 2,4 s |
| cách 1 qua máy chủ yt-dlp (đường Android), lịch sử 30 s | 6,8 s | 34 mốc liên tục | trung vị 143 ms | ≈ 2,1 s |
| **cách 2: cửa sổ 11 đoạn, yt-dlp thoát mỗi lượt (như Facebook thật)** | **1,7 s** | **49 mốc liên tục trong 50 s** | trung vị 2,3 s (2,1–2,5) | **≈ 4,3 s, chỉ là cận dưới** |
| phát lại (`--replay`, bộ tải gốc, không ffmpeg) | 0,5 s | 22 mốc liên tục, kết thúc gọn | — | — |

- Khe ~170 ms ở Ultra lặp lại đúng một chỗ trong nhiều lần chạy và biến mất ở Balanced (đệm 350 ms): đó là một đoạn từ nguồn
  cục bộ về trễ hơn đệm 120 ms trên máy 2 nhân đang mã hoá video, không phải lỗi logic của app.
- Lúc vào live ở cách 1, app bỏ ~1 s (một mốc) một lần để kéo độ trễ từ ~1,5 s về ~0,1 s: đó là "đuổi theo live" làm đúng việc.
- **Cách 2: 4,3 s chỉ là cận dưới.** Trên localhost mỗi lượt yt-dlp chỉ ~2 s. Độ trễ của cách 2 xấp xỉ *thời gian một lượt
  yt-dlp* + độ dài đoạn. Trên Facebook thật mỗi lượt gồm tải trang + trích xuất + tải cả cửa sổ nên chắc chắn lớn hơn, **chưa đo được**.
- **Cách 2 tốn dữ liệu:** mỗi lượt tải lại *cả cửa sổ* dù chỉ vài đoạn là mới. Đo được 363 yêu cầu âm thanh (~8,7 MB) trong 50 s
  cho âm thanh 96 kbps (~0,6 MB thật), vì trên localhost lượt nào cũng xong ngay và app chỉ chờ khoảng tối thiểu 1,5 s. Trên
  Facebook thật các lượt thưa hơn nên bớt, nhưng vẫn gấp nhiều lần âm thanh thật. Cân nhắc nếu dùng mạng di động có giới hạn.
- So sánh: bản DASH tự viết (`fb-live-audio.zip`, bản trước) đo được tổng ≈ 2,2 s, không cần ffmpeg, nhưng tự phân tích manifest.

## Điều học được khi đọc mã yt-dlp và đo — quyết định thiết kế

- Extractor Facebook phân tích **manifest XML nhúng** (`extract_dash_manifest`), URL manifest (`dash_manifest_urls`) chỉ dùng làm
  `mpd_url`. `FFmpegFD` đưa cho ffmpeg đúng `fmt['url']` = `mpd_url or base_url`: có URL manifest thì ffmpeg đọc được manifest thật,
  không có thì lỗi → app lùi về cách 2.
- Bộ tải gốc từ chối live DASH; `--live-from-start` (`http_dash_segments_generator`) **chỉ có ở extractor YouTube**.
- Muxer mặc định (`ipod`) không ghi được ra stdout → ép `-f adts -flush_packets 1`.
- **ffmpeg phát mỗi đoạn live 2 lần** (chuỗi mốc `46,47,48,49 | 48,49,50,51 | …`) → `Dedupe` loại bản lặp theo nội dung.
- Cách "gom đợt theo khoảng cách giữa các lần đẩy" từng thử ở `Output` bị chia vụn sau khi lọc lặp và cắt nhầm âm thanh. Thay bằng
  **đáy mức đệm trong 3 s**. Có thêm `burst_mode` cho cách 2: dữ liệu về theo lượt thì ưu tiên liền mạch (không điều tốc, chỉ
  cắt khi đệm > 8 s).
- Bug thật đã sửa: phần "bỏ qua" đang chờ callback bị tính trùng khi nhiều lần đẩy liên tiếp.
- Cổng bỏ lịch sử giữ lại **2,5 s đuôi** của đợt dồn và phát nó khi đợt dồn dừng: cửa sổ hữu hạn của cách 2 thì phần đuôi chính
  là nội dung mới nhất hiện có, bỏ trắng sẽ không phát được gì ở lượt đầu.
- yt-dlp bộ tải gốc để tệp tạm `-Frag<N>` ở thư mục làm việc nếu bị ngắt → app chạy nó trong thư mục tạm riêng, xoá khi xong.
- Streamlink đã đánh giá, không dùng: plugin facebook chỉ đọc khoá cũ `dash_manifest_url`; `dash://` chỉ liệt kê luồng ghép
  video+audio, không có audio-only.

## Chưa kiểm chứng (đừng bỏ qua)

- **Facebook thật**: trang có cung cấp URL manifest không, manifest đó có sống không (chạy `probe_live.py`), và độ trễ thật của
  cách 2.
- **`-map 0:N` của yt-dlp** (cách 1) giả định thứ tự AdaptationSet trong MPD trùng thứ tự luồng của ffmpeg (video trước). Nếu
  Facebook để audio trước, yt-dlp map nhầm video và lỗi muxer; app lùi về cách 2 (không mất khả năng nghe, chỉ mất độ trễ thấp).
- ffmpeg mở **mọi** Representation lúc thăm dò. Đo được 2 yêu cầu video với 1 Representation video; Facebook có nhiều chất lượng
  nên có thể tải thêm một ít video lúc mở (chưa đo).
- Cách 2 kết thúc khi 4 lượt liên tiếp (≥ 20 s) không có access unit mới hoặc yt-dlp báo lỗi. Một phiên live phát **im lặng
  tuyệt đối** hơn 20 s (khung AAC giống hệt nhau) có thể bị coi nhầm là đã kết thúc.
- Cổng bỏ lịch sử (`GATE_*`) chỉnh bằng thực nghiệm với đoạn 2 s; đoạn dài ≥ 4,5 s chưa thử.
- **Windows/Android chưa biên dịch.** Mã `cfg(windows)` (`taskkill`, `CREATE_NO_WINDOW`) chưa được compile; dịch vụ nền Kotlin và JNI
  chưa dựng. Chỉ chạy được trên Linux trong môi trường phát triển. Crate Tauri đã `cargo check` được trên Linux.
- Số đo từ bản debug. Tự cập nhật `yt-dlp -U` chỉ được thử với yt-dlp giả.
- HE-AAC: mặc định phát phần lõi (thiếu dải cao); `--features sbr` dùng FDK-AAC (giấy phép Fraunhofer riêng) cho đủ dải cao.

## Cấu trúc

```
core/        fbaudio-core: source (nguồn byte) · demux (ADTS|fMP4, lọc lặp, nối tiếp) · decode · output · player (vòng chạy lại, cổng)
cli/         fbaudio (dòng lệnh, để thử nhanh)
app/         Tauri 2: src-tauri (lõi nối UI) · ui/index.html · android-overlay/ (dịch vụ nền Kotlin)
scripts/     probe_live.py (chẩn đoán link thật) · ytdlp_helper.py (máy chủ yt-dlp) · e2e-pipe.sh + analyze_wav.py (đo)
             alias_server.py · make-adts-fixtures.sh · he-fixtures/
```

Test: `cargo test -p fbaudio-core` (40 test; đạt cả khi thêm `--features sbr`). Gồm yt-dlp giả bằng shell script: phát đủ, bản lặp bị
loại, hiện dòng lỗi của yt-dlp, gợi ý khi thiếu ffmpeg/đổi định dạng, nút Dừng diệt cả cây tiến trình, tự cập nhật rồi thử lại, thư
mục tạm được dọn, cửa sổ hữu hạn chạy lại và nối tiếp không phát lặp, ffmpeg hỏng hoặc treo thì lùi về bộ tải gốc, cổng bỏ lịch sử.

Biến môi trường: `FBAUDIO_YTDLP` (đường dẫn yt-dlp), `FBAUDIO_YTDLP_ARGS` (tham số thêm), `FBAUDIO_NO_UPDATE=1`, `FBAUDIO_NO_FFMPEG=1`.
