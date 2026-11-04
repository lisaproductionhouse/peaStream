# FB Live Audio — bản "pipe luồng thô"

Nghe Facebook Live **chỉ bằng âm thanh** (Android + Windows). Ứng dụng **không tự tải và không phân tích file `.mpd`**:
một công cụ ngoài (yt-dlp) lo từ link tới luồng âm thanh và ghi ra stdout, ứng dụng chỉ đọc dòng byte đó.

## Khi gặp lỗi: gửi gì để chẩn đoán

Ứng dụng Windows không có console, nên mọi thứ cần thiết nằm ở ba nơi:

1. **Nhật ký:** `%TEMP%\fbaudio.log` (lần chạy trước: `fbaudio.old.log`; đường dẫn cũng hiện ở mục "Nâng cao"). Ghi cách đang lấy luồng
   (ffmpeg hay chạy lại từng lượt), thời gian mỗi lượt, mọi lần lùi cách, và cứ 10 s một dòng tóm tắt đệm/số lần ngắt.
2. **Dòng nhỏ dưới nút phát** khi đang phát: `ffmpeg (bám live) · đệm đích 0,4 s` hoặc `chạy lại từng lượt (~5,8 s/lượt) · đệm đích 2,0 s`.
3. `python scripts/probe_live.py "LINK" --cookies cookies.txt`: yt-dlp thấy gì, trang có cung cấp URL manifest không, manifest có
   thật sự tiến thêm đoạn mới không (tức là cách 1 có dùng được hay chỉ còn cách 2).

## Điều quan trọng nhất: yt-dlp không tự bám live trên Facebook

Quan sát trên Facebook thật: yt-dlp tải khoảng 11 đoạn rồi **thoát**, dù đã cập nhật bản mới nhất. Nguyên nhân đọc được từ mã nguồn
(yt-dlp 2026.08.19, `extractor/facebook.py`): trang Facebook trả manifest dạng **XML nhúng sẵn**, extractor phân tích bản XML đó ngay lúc
tải trang (một ảnh chụp tĩnh) chứ không tải lại manifest, và **không đặt `is_live`**. Bộ tải gốc chỉ tải đúng danh sách đoạn trong ảnh
chụp rồi kết thúc. Đây là hành vi của extractor, cập nhật yt-dlp không đổi được.

Vì vậy ở chế độ live, app thử lần lượt hai cách (`core/src/player.rs`):

1. **Ép ffmpeg** (`--downloader dash:ffmpeg`). ffmpeg được đưa URL manifest *nếu trang có cung cấp* và tự bám theo *nếu đó là manifest
   sống*. Độ trễ thấp nhất (~2 s). Không có URL, hoặc ffmpeg lỗi/treo (30 s không ra byte) → tự lùi về cách 2. Máy không có ffmpeg
   thì yt-dlp tự rơi về bộ tải gốc (đã kiểm: chỉ cảnh báo "Continuing without ffmpeg"), tức là cũng thành cách 2.
2. **Chạy lại yt-dlp từng lượt và nối tiếp.** Mỗi lượt yt-dlp tải cả cửa sổ rồi thoát; lượt sau luôn bắt đầu bằng phần *chồng lấn* với
   lượt trước. `Stitch` (`core/src/demux.rs`) nhận ra phần đầu đã thấy và chỉ giữ từ access unit mới đầu tiên. Vẫn là pipe thuần.
   Không cần ffmpeg, nhưng độ trễ cao hơn (≈ một đến hai chu kỳ lượt) và tốn dữ liệu hơn (xem dưới).

```
 yt-dlp … -o -  ──stdout──►  đọc byte thô ─► demux ─► nối tiếp giữa lượt ─► lọc lặp theo dải ─► giải mã AAC ─► cổng vào live
 (chạy lại từng lượt         (luồng đọc        ADTS|    (Stitch: bỏ phần       (Dedupe: bỏ dải      Symphonia    (giữ phần đuôi
  nếu nó thoát sớm)           riêng)           fMP4      chồng lấn)            phát lại)            / FDK-AAC     mới nhất)
                                                                               vòng đệm → loa: bám live, đệm chống giật, điều tốc
```

Nguồn byte thô (`core/src/source.rs`): yt-dlp tại chỗ · máy chủ yt-dlp qua HTTP (`scripts/ytdlp_helper.py`, cho Android) · lệnh tuỳ ý
(`--cmd`) · stdin (`yt-dlp … -o - | fbaudio - --replay`).

> **Facebook thật chưa được thử với app này bởi người viết.** Số đo dưới đây dùng luồng DASH live *mô phỏng* (ffmpeg phát, đoạn 2 s) và
> plugin extractor bắt chước đường mã của `facebook.py` (`scripts/fblike-plugin`). Thông tin từ Facebook thật chỉ gồm: yt-dlp tải ~11 đoạn
> rồi thoát; một ảnh chụp màn hình với 7 lần ngắt ở mức Cân bằng; và tiêu đề vỡ dấu (đã sửa, xem dưới).

## Cách dùng

**Cần:** yt-dlp. **ffmpeg không bắt buộc** (có thì app thử cách 1 trước). Windows: `winget install yt-dlp.yt-dlp` và `winget install Gyan.FFmpeg`.

```
fbaudio "https://www.facebook.com/…/videos/123"             # LIVE (mặc định): bám live
fbaudio LINK --replay                                       # video đã kết thúc: phát đủ từ đầu
fbaudio LINK --mode balanced|stable                         # đệm lớn hơn nếu mạng chập chờn
fbaudio LINK --ytdlp-args "--cookies C:\cookies.txt"        # video cần đăng nhập
fbaudio LINK --no-ffmpeg                                    # bỏ cách 1, đi thẳng cách 2 (= FBAUDIO_NO_FFMPEG=1)
fbaudio LINK --server http://192.168.1.5:8787 --token MATKHAU
yt-dlp -f ba -o - LINK | fbaudio - --replay
```

**Cookie trên Windows:** Chrome/Edge mã hoá cookie nên `--cookies-from-browser` có thể lỗi (DPAPI). Dùng file thuần `--cookies cookies.txt`.

**Live hay phát lại do bạn chọn** (ô "Video đã kết thúc" trong app, `--replay` ở CLI): yt-dlp không báo `is_live` cho Facebook nên app không tự biết được.

**Android** không chạy được yt-dlp. Chạy máy chủ yt-dlp (Termux trên điện thoại, hoặc máy tính cùng mạng) rồi nhập địa chỉ vào "Nâng cao":
```
pkg install python ffmpeg && pip install -U yt-dlp           # Termux (ffmpeg không bắt buộc)
python scripts/ytdlp_helper.py [--host 0.0.0.0 --token MATKHAU]
```

## Lỗi thật đã tìm ra bằng cách đo (và đã sửa)

Phần lớn các lỗi này **không thấy được trên localhost** vì ở đó mọi thứ về tức thì; chúng chỉ lộ ra khi cho nguồn giả lập chậm và dao động
như mạng thật (`ALIAS_JITTER_MS`, `FBLIKE_DELAY/JITTER/SEED`).

- **Tiêu đề vỡ dấu** (`Khn voan la … c�c`): `yt-dlp.exe` (PyInstaller) bỏ qua `PYTHONUTF8`/`PYTHONIOENCODING`, ghi stderr bằng bảng mã ANSI và
  *bỏ hẳn* ký tự không biểu diễn được (ă, ệ, ậ…); á, à… thành byte đơn bị giải mã thành `�`. Sửa: in tiêu đề dạng JSON thuần ASCII
  (`%(title)j`). Đã kiểm với yt-dlp thật rằng đầu ra là ASCII `"Kh\u0103n voan l\u00e0 …"`, kể cả emoji; **chưa kiểm trên Windows**.
- **Cổng "đã tới mép live" bị lừa bởi độ trễ mạng.** Bản cũ coi khoảng im lặng ≥150 ms giữa hai lần dữ liệu về là dấu hiệu hết lịch sử. Trên
  mạng thật mỗi đoạn lịch sử tải mất vài trăm ms nên giữa các đoạn lịch sử nào cũng có khoảng im lặng: app phát **đoạn cổ nhất của lịch
  sử trước**, rồi bộ đệm cắt vụn (đo: 11–16 lần cắt, tổng 78–94 s bị bỏ trong 80 s). Sửa: nhận diện bằng **tốc độ âm thanh về trong cửa sổ 5 s**
  (lịch sử ≥ 2× thời gian thực, live ≈ 1×). Ở chế độ chạy lại, lượt đầu luôn chờ EOF. Cái giá: tiếng đầu tiên chậm hơn ~4–5 s sau khi hết lịch sử;
  độ trễ của âm thanh so với live không đổi vì vẫn chỉ phát phần đuôi mới nhất.
- **Phát lặp từ bản thứ ba.** Bộ lọc cũ chỉ loại "bản sao thứ hai". Khi nguồn giật ffmpeg phát một đoạn live nhiều hơn hai lần, từ bản thứ ba
  lọt qua (chuỗi mốc `38 39 40 41 | 38 39 42 43 | 40 41 44 45`, bước âm). Sửa: nhận diện **dải phát lại theo thứ tự** (AU trùng AU cũ *và* AU
  liền kề cũng trùng AU liền kề tương ứng), nên im lặng CBR và AU trùng ngẫu nhiên đứng một mình không bị loại.
- **Bộ cắt "đuổi theo live" tự phá bộ đệm.** Khi một đoạn trễ rồi các đoạn sau về dồn, hàng đợi phình lên vài giây; cửa sổ đáy 3 s thấy "dư" và cắt
  ngay về mức đệm, nên lần trễ kế tiếp lại cạn. Cửa sổ nay là 12 s (chỉ cắt khi dư *kéo dài*). Đây là sửa theo cơ chế (kiểm được trong nhật ký);
  **tác dụng lên số lần ngắt chưa chứng minh được** vì nhiễu giữa các lần chạy (xem dưới).
- **Đuôi lượt đầu quá ngắn ở cách 2:** đuôi 2,5 s trong khi một chu kỳ lượt ~6 s gây khe 3,7 s ngay giây thứ 7. Nay đuôi dài bằng 1,2 chu kỳ lượt đã đo (tối đa 12 s),
  và chỉ nhả khi EOF vì tiến trình thoát trễ vài trăm ms sau byte cuối.
- **Cách 2 cần đệm sàn:** dữ liệu về theo cục nên đệm sàn theo mức người dùng chọn (Siêu thấp 1,0 s, Cân bằng 2,0 s, Ổn định 4,0 s).
- **Máy chủ yt-dlp** ném `TypeError` ở nhánh lỗi (client chỉ nhận "HTTP 502" trống): đã sửa.

**Đã thử rồi gỡ: đệm thích ứng** (nới đệm sau mỗi lần cạn). Ba biến thể, không biến thể nào cho ít lần ngắt hơn đệm cố định (4 so với 4–6), mà còn làm độ
trễ tăng và khe dài hơn (mỗi lần cạn chèn khoảng lặng bằng cả đệm đích: khe 4–5 s). Không giữ lại cơ chế mà số đo không biện minh được.

## Đo thực tế

Nguồn: ffmpeg phát DASH live có **cả video lẫn audio** (video đứng trước), đoạn 2 s, qua link không kết thúc bằng `.mpd`. yt-dlp 2026.10.26, ffmpeg 6.1.1,
bản debug, ghi WAV. Tín hiệu có mốc mã hoá hai âm (nhận dạng không mơ hồ trong 64 s) nên đo được độ trễ tuyệt đối. Tái tạo bằng
`scripts/e2e-pipe.sh` (biến môi trường ở đầu file).

> **Độ tin cậy:** số lần ngắt giữa các lần chạy *cùng cấu hình* dao động ±2–3 (máy 2 nhân vừa mã hoá video vừa chạy app). Vì vậy chỉ so sánh
> số lần ngắt khi chênh lệch lớn; các kiểm tra theo cơ chế (chuỗi mốc có bước nhảy/bước âm không, bao nhiêu âm thanh bị cắt) đáng tin hơn.

**Tốc độ đọc của ffmpeg quyết định mọi thứ** (cách 1, extractor Generic, lịch sử = cửa sổ DVR):

| ffmpeg đọc | lịch sử | kết quả |
|---|---|---|
| `-re` (mặc định của yt-dlp) | 15 s / 40 s | liền mạch nhưng **trễ 21,3 s / 46,4 s** (≈ cả cửa sổ + ~6 s; theo mã `dashdec` tối đa ~60 s, **chưa đo** trên lịch sử dài hơn 40 s) |
| `-readrate 100` | ~10 s | phát lặp cả lịch sử (764 KB ≈ 64 s âm thanh trong 8 s) rồi **luồng đứng** |
| **`-readrate 8` + cổng** | 40 s | liền mạch, tổng ≈ 2,1–2,3 s |

**Cấu hình mặc định của app (mã cuối cùng):**

| kịch bản | tiếng đầu | chất lượng | tổng (đoạn 2 s) |
|---|---|---|---|
| cách 1, nguồn êm, lịch sử 40 s, Cân bằng | 11,2 s | 0 khe, 0 lần ngắt, chuỗi mốc liền (một bước +3 lúc nối, có chủ đích) | **≈ 2,3 s** |
| cách 1, **nguồn giật** (mỗi đoạn chậm 0–1,5 s), Cân bằng | 18,2 s | chuỗi mốc liền, **không bước âm**, 3 lần cắt tổng 2,6 s, 2 khe (0,9 s và 0,5 s) / 70 s | ≈ 4,8 s |
| cách 2, nguồn êm (lượt nhanh), cửa sổ 11 đoạn, Cân bằng | 2,7 s | 58 mốc liền, 0 khe | ≈ 5,4 s |
| cách 2, **lượt chậm 3 s + dao động 0–4 s**, Cân bằng, 120 s | 8,7 s | 112 mốc liền, **0 khe, 0 lần ngắt** | ≈ 10,5 s |
| qua máy chủ yt-dlp (đường Android), cách 1, lịch sử 30 s | 6,8 s | 34 mốc liền | ≈ 2,1 s (đo ở bản trước khi đổi cổng) |
| phát lại (`--replay`, bộ tải gốc, không ffmpeg) | 0,5 s | 22 mốc liền, kết thúc gọn | — |

- **Độ trễ của cách 2 xấp xỉ một đến hai chu kỳ lượt** (mỗi lượt gồm khởi động yt-dlp + tải trang + trích xuất + tải cả cửa sổ) cộng đệm sàn. Trên localhost
  lượt chỉ ~2 s; trên Facebook thật chắc chắn lớn hơn, **chưa đo được**. Đổi lại cách 2 liền mạch (0 khe ở cả hai kịch bản đo).
- **Cách 2 tốn dữ liệu:** mỗi lượt tải lại *cả cửa sổ*. Đo được 440 yêu cầu âm thanh (~10,4 MB) trong 60 s cho âm thanh 96 kbps (~0,7 MB thật), vì trên
  localhost lượt nào cũng xong ngay và app chỉ chờ tối thiểu 1,5 s giữa hai lượt. Trên Facebook thật các lượt thưa hơn nên bớt, nhưng vẫn gấp nhiều
  lần âm thanh thật. Cân nhắc nếu dùng mạng di động có giới hạn.
- Bộ tải gốc chạy với `--concurrent-fragments 4` (đã kiểm: ghi ra stdout cho *đúng từng byte* như tải tuần tự); lợi ích trên mạng thật chưa đo.
- So sánh: bản DASH tự viết (`fb-live-audio.zip`, bản trước nữa) đo được tổng ≈ 2,2 s, không cần ffmpeg, nhưng tự phân tích manifest.

## Điều học được khi đọc mã yt-dlp và đo

- Extractor Facebook phân tích **manifest XML nhúng** (`extract_dash_manifest`); URL manifest (`dash_manifest_urls`) chỉ dùng làm `mpd_url`. `FFmpegFD` đưa
  cho ffmpeg đúng `fmt['url']` = `mpd_url or base_url`: có URL manifest thì ffmpeg đọc được manifest thật, không có thì lỗi → app lùi về cách 2.
- Bộ tải gốc từ chối live DASH; `--live-from-start` (`http_dash_segments_generator`) **chỉ có ở extractor YouTube**.
- Muxer mặc định (`ipod`) không ghi được ra stdout → ép `-f adts -flush_packets 1`.
- yt-dlp bộ tải gốc để tệp tạm `-Frag<N>` ở thư mục làm việc nếu bị ngắt → app chạy nó trong thư mục tạm riêng, xoá khi xong.
- Streamlink đã đánh giá, không dùng: plugin facebook chỉ đọc khoá cũ `dash_manifest_url`; `dash://` chỉ liệt kê luồng ghép video+audio.

## Chưa kiểm chứng (đừng bỏ qua)

- **Facebook thật**: trang có cung cấp URL manifest không, manifest đó có sống không (chạy `probe_live.py`), độ dài đoạn, độ trễ thật của cách 2.
  Không biết máy bạn đang chạy cách 1 hay cách 2: xem dòng "đang chạy cách nào" và nhật ký.
- **Cổng vào live** (`GATE_*`) chỉnh bằng thực nghiệm với đoạn 2 s; đoạn dài ≥ 4 s chỉ có test đơn vị, chưa chạy thật.
- **`-map 0:N` của yt-dlp** (cách 1) giả định thứ tự AdaptationSet trong MPD trùng thứ tự luồng của ffmpeg (video trước). Nếu Facebook để audio
  trước, yt-dlp map nhầm video và lỗi muxer; app lùi về cách 2.
- ffmpeg mở **mọi** Representation lúc thăm dò. Đo được 2 yêu cầu video với 1 Representation video; Facebook có nhiều chất lượng nên có thể tải thêm một ít (chưa đo).
- Cách 2 kết thúc khi 4 lượt liên tiếp (≥ 20 s) không có access unit mới hoặc yt-dlp báo lỗi. Một phiên live **im lặng tuyệt đối** hơn 20 s có thể bị coi nhầm là đã kết thúc.
- **Windows/Android chưa biên dịch.** Mã `cfg(windows)` (`taskkill`, `CREATE_NO_WINDOW`) chưa được compile; dịch vụ nền Kotlin và JNI chưa dựng. Crate Tauri đã `cargo check` được trên Linux.
- Số đo từ bản debug. Tự cập nhật `yt-dlp -U` chỉ được thử với yt-dlp giả.
- HE-AAC: mặc định phát phần lõi (thiếu dải cao); `--features sbr` dùng FDK-AAC (giấy phép Fraunhofer riêng) cho đủ dải cao.

## Cấu trúc

```
core/        fbaudio-core: source (nguồn byte) · demux (ADTS|fMP4, lọc lặp theo dải, nối tiếp) · decode · output · player (vòng chạy lại, cổng vào live)
cli/         fbaudio (dòng lệnh, để thử nhanh)
app/         Tauri 2: src-tauri (lõi nối UI, nhật ký) · ui/index.html · android-overlay/ (dịch vụ nền Kotlin)
scripts/     probe_live.py (chẩn đoán link thật) · ytdlp_helper.py (máy chủ yt-dlp) · e2e-pipe.sh + analyze_wav.py (đo)
             alias_server.py (nguồn giả lập, có ALIAS_JITTER_MS) · fblike-plugin/ (bắt chước extractor Facebook) · make-adts-fixtures.sh · he-fixtures/
```

Test: `cargo test -p fbaudio-core` (42 test; đạt cả khi thêm `--features sbr`). Gồm yt-dlp giả bằng shell script (phát đủ, hiện dòng lỗi, Dừng diệt cả cây tiến
trình, tự cập nhật, thư mục tạm được dọn, cửa sổ hữu hạn chạy lại và nối tiếp không phát lặp, ffmpeg hỏng/treo thì lùi), cổng vào live (đoạn 2 s và 4 s, lịch sử đổ
về với độ trễ mạng, lượt đầu của bộ tải gốc), bộ lọc lặp (bội 2, 4, dải phát lại, im lặng, AU đứng một mình), tiêu đề JSON.

Biến môi trường: `FBAUDIO_YTDLP` (đường dẫn yt-dlp), `FBAUDIO_YTDLP_ARGS` (tham số thêm), `FBAUDIO_NO_UPDATE=1`, `FBAUDIO_NO_FFMPEG=1`.
