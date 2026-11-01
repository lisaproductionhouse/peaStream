# FB Live Audio

Nghe Facebook Live **chỉ bằng âm thanh**, độ trễ thấp nhất có thể. Viết bằng Rust, giao diện Tauri 2.
Một codebase cho **Android** (ưu tiên) và **Windows**.

- Lấy link Facebook bằng **yt-dlp** (cộng đồng cập nhật theo Facebook trong vài ngày), có bộ cào tích hợp làm dự phòng.
- Chỉ tải luồng audio của manifest DASH. Luồng video không bao giờ được yêu cầu, nên băng thông chỉ ~48–128 kbps.
- Hỗ trợ **AAC-LC và HE-AAC** (luồng Facebook đã thử thực tế dùng HE-AAC, xem mục *HE-AAC* bên dưới).
- Khi tắt màn hình vẫn phát (Android: dịch vụ foreground + wake lock). Việc này **không thêm độ trễ**, chỉ tốn thêm pin.
- Dán link, nhấn phát. Trên Android còn có thể **Chia sẻ → FB Live Audio** từ app Facebook để phát luôn.

## Trạng thái kiểm chứng

**Đã kiểm chứng** (Linux, luồng DASH giả lập bằng ffmpeg): toàn bộ đường đi từ manifest đến PCM (DASH → chỉ audio → fMP4 → AAC-LC / HE-AAC → resample → bộ đệm), kể cả bám live, mạng giật, nguồn khựng rồi chạy bù, phiên live kết thúc; HE-AAC v1/v2 thật (mã hóa bằng FDK-AAC, ASC báo hiệu tường minh như Facebook); 39 test tự động, đều đạt ở cả hai cấu hình (mặc định và `--features sbr`); `cargo check` lớp Tauri cùng `tauri.conf.json` và capabilities; **lấy link bằng yt-dlp thật (2026.08.19)** trên một luồng DASH live có cả video lẫn audio, chạy tại chỗ lẫn qua máy chủ yt-dlp: app chỉ tải audio (0 request video), nghe liền mạch, ổn định +170 ms; giao diện render đúng ở nhiều kích thước và cả hai chế độ sáng/tối.

**Chưa kiểm chứng**, bạn nên thử trước khi tin:

- Lấy link từ **Facebook thật**, kể cả bằng yt-dlp. Môi trường dựng không truy cập được facebook.com; yt-dlp mới được thử trên luồng DASH cục bộ (extractor Generic), không phải extractor Facebook.
- Các nhánh chỉ dành cho **Windows** trong `ytdlp.rs` (ẩn cửa sổ console, `taskkill`): chưa biên dịch được ở đây.
- Máy chủ yt-dlp chạy trong **Termux** trên Android.
- **Phát ra loa thật** (WASAPI trên Windows, Oboe/AAudio trên Android). Môi trường dựng không có thiết bị âm thanh nên chỉ thử bằng ghi WAV; riêng lớp `cpal` chưa chạy trên máy thật.
- **Build và chạy trên Android** (không có Android SDK/NDK): phần Kotlin, manifest, dịch vụ nền và chia sẻ link.

## Độ trễ: kỳ vọng thực tế

Độ trễ toàn phần = **phần của Facebook** + **phần của ứng dụng**.

- Phần của Facebook (encoder → ingest → đóng gói → CDN) ứng dụng không can thiệp được. Audio được xuất bản theo từng đoạn, nên độ trễ tối thiểu xấp xỉ **độ dài một đoạn** (thường 1–2 s, tuỳ phiên). Không có ứng dụng bên thứ ba nào kéo xuống "gần 0" được.
- Phần của ứng dụng được ép xuống **~0,16–0,21 s** ở mức *Siêu thấp*: vào thẳng đoạn mới nhất, giữ kết nối, giải mã ngay khi byte về, phát qua vòng đệm nhỏ, tự bám live edge.
- Nếu máy chủ phát CMAF chunked low-latency, lõi giải mã từng chunk ngay khi về, không chờ hết đoạn (đã có test với dữ liệu đến ở ranh giới bất kỳ; **chưa thử với Facebook thật**).

Số đo trên luồng DASH giả lập cục bộ (ffmpeg, `scripts/e2e-local-dash.sh`). Cột "App thêm" đo bằng mtime thật của file đoạn trên đĩa:

| Kịch bản | App thêm | Toàn phần | Ngắt / lặng |
|---|---|---|---|
| đoạn 2 s · Siêu thấp | 159–205 ms (nhiều lần chạy) | ≈ 2,2 s | 0 / 0 |
| đoạn 1 s · Siêu thấp | 170 ms | ≈ 1,2 s | 0 / 0 |
| đoạn 2 s · Cân bằng | 406 ms | ≈ 2,4 s | 0 / 0 |
| mạng giật 0–250 ms · Siêu thấp | 230 ms | ≈ 2,25 s | 0 / 0 |
| mạng giật 0–250 ms · Ổn định | 1024 ms | ≈ 3,0 s | 0 / 0 |
| mạng giật 0–700 ms · Siêu thấp | 306 ms | ≈ 2,3 s | 1 lần (260 ms), tự hồi phục |
| nguồn khựng 5 s rồi chạy bù | về mức ổn định sau khi cắt phần dồn | | |

Các dòng khác là một lần chạy. Mạng thật (4G, Wi-Fi yếu) sẽ khác. Nếu nghe bị ngắt quãng, chuyển *Cân bằng* hoặc *Ổn định*.

## Lấy link từ Facebook: yt-dlp

Facebook đổi cấu trúc trang liên tục nên bộ cào tự viết luôn chạy theo sau. **yt-dlp** được cộng đồng cập nhật theo trong vài ngày, vì vậy ứng dụng dùng nó làm nguồn chính. Chỉ khâu *lấy link* nhờ yt-dlp: nó trả về URL manifest và header cần dùng; còn việc tải riêng luồng audio với độ trễ thấp vẫn do engine của dự án làm (kiểm tra bằng số request: video = 0).

Thứ tự thử (chế độ mặc định `auto`); link `.mpd` dán thẳng thì bỏ qua cả ba bước:

1. **Máy chủ yt-dlp**, nếu bạn đã nhập địa chỉ (dành cho Android, xem dưới).
2. **yt-dlp tại chỗ**, tự tìm theo thứ tự: ô/tham số “Đường dẫn yt-dlp”, biến `FBAUDIO_YTDLP`, `yt-dlp(.exe)` cạnh file chạy của app, `PATH`, rồi `python -m yt_dlp`.
3. **Bộ cào tích hợp** (không cần cài gì, dùng làm dự phòng).

| Nền tảng | Cách có yt-dlp |
|---|---|
| Windows | `winget install yt-dlp.yt-dlp`, hoặc tải `yt-dlp.exe` rồi đặt cạnh ứng dụng. App tự tìm, không cần cấu hình. |
| Linux / macOS | `pip install -U yt-dlp` hoặc bản độc lập. |
| Android | Không có Python nên app **không tự chạy được** yt-dlp: dùng *máy chủ yt-dlp* (bên dưới). |

Hành vi cần biết:

- **Tự cập nhật khi Facebook vừa đổi.** Nếu yt-dlp báo trích xuất hỏng (“Cannot parse data”, “Unable to extract”…), app chạy `yt-dlp -U` rồi thử lại, tối đa một lần mỗi lần mở app. Tắt bằng `FBAUDIO_NO_UPDATE=1` hoặc `--no-update`. Bản cài bằng pip hoặc trình quản lý gói của hệ điều hành (apt, brew…) sẽ từ chối `-U`: hãy cập nhật bằng chính công cụ đó (`pip install -U yt-dlp`, `winget upgrade yt-dlp.yt-dlp`…). Bản sửa lỗi mới nhất thường ra ở kênh nightly: `yt-dlp --update-to nightly`.
- **Video cần đăng nhập:** truyền cookies qua ô “Tham số thêm cho yt-dlp” (hoặc biến `FBAUDIO_YTDLP_ARGS`, hoặc `--ytdlp-args` ở CLI), ví dụ `--cookies-from-browser firefox`. Lưu ý: dùng tài khoản cá nhân với công cụ tự động có thể bị Facebook hạn chế tài khoản.
- **Header media:** dùng `http_headers` do yt-dlp báo (với Facebook là `User-Agent: facebookexternalhit/1.1`) cho mọi request audio, vì dùng User-Agent trình duyệt thì CDN Facebook giới hạn tốc độ tải, mà phát trực tiếp mà đoạn về chậm thì sẽ giật.
- **Nhớ 3 phút** kết quả lấy link, nên dừng rồi phát lại hoặc đổi mức độ trễ không phải chờ yt-dlp thêm vài giây. Nếu link đã nhớ không còn dùng được thì tự lấy lại.
- Bấm *Dừng* khi đang chờ yt-dlp sẽ diệt luôn cả cây tiến trình của nó.

### Android: máy chủ yt-dlp

Chạy `scripts/ytdlp_helper.py` ở nơi có yt-dlp rồi nhập địa chỉ vào mục *Nâng cao* của app:

```
# ngay trên điện thoại, bằng Termux (không cần máy khác)
pkg install python
pip install -U yt-dlp
python ytdlp_helper.py                     # địa chỉ trong app: http://127.0.0.1:8787

# hoặc trên máy tính cùng Wi-Fi
python ytdlp_helper.py --host 0.0.0.0 --token MATKHAU    # địa chỉ: http://<IP máy tính>:8787, nhập cả mã truy cập
```

Máy chủ này mặc định chỉ lắng nghe `127.0.0.1`, chỉ nhận link Facebook (`--allow-any` để bỏ), đòi mã truy cập nếu đặt `--token`, luôn truyền URL sau `--` (không thể bị hiểu thành tuỳ chọn) và không dùng shell. Đừng mở nó ra Internet. Khi trích xuất hỏng nó cũng tự cập nhật yt-dlp một lần (`--no-update` để tắt). Nếu dùng Termux, nhớ đặt pin của Termux ở chế độ *Không hạn chế* (và chạy `termux-wake-lock`) kẻo Android đóng nó khi tắt màn hình.

Hướng khác (chưa làm): nhúng yt-dlp thẳng vào APK bằng thư viện `youtubedl-android`, để không cần máy chủ nào. Việc này cần thêm phụ thuộc Gradle và lớp cầu nối Kotlin, mà mình không có Android SDK để kiểm chứng.

## HE-AAC (SBR)

Luồng Facebook đã thử thực tế dùng **HE-AAC** (ASC mở đầu bằng loại đối tượng 5), không phải AAC-LC như giả định ban đầu của dự án. Không loại trừ luồng khác dùng AAC-LC; cả hai đều được hỗ trợ.

| | Mặc định | `--features sbr` |
|---|---|---|
| Bộ giải mã | Symphonia (thuần Rust): giải mã **phần lõi**, bỏ qua dữ liệu SBR | FDK-AAC: giải mã **đủ SBR + PS** |
| Tần số đầu ra | tần số lõi (ví dụ 24 kHz), bộ resample nâng lên | tần số sau SBR (ví dụ 48 kHz) |
| Âm thanh | đúng cao độ và tốc độ, nhưng **thiếu dải cao** (giao diện hiện một dòng ghi chú) | gần như bản gốc |
| Yêu cầu build | không | trình biên dịch C++ (MSVC / NDK / gcc) |

Đo trên tín hiệu nhiễu hồng + 440 Hz mã hóa HE-AAC v1 48 kbps (năng lượng theo dải, dB):

| Dải | Gốc | Mặc định (lõi) | `sbr` |
|---|---|---|---|
| 0,3–3 kHz | −39,8 | −40,2 | −40,2 |
| 3–8 kHz | −46,3 | −47,0 | −46,6 |
| 8–12 kHz | −50,2 | **−63,6** | −50,4 |
| 12–20 kHz | −49,1 | **−70,1** | −50,7 |

Bản mặc định tái tạo trung thực phần dưới ~8 kHz (đủ nghe rõ tiếng nói); dải cao hơn do SBR tạo ra bị thiếu 13–21 dB nên âm thanh kém "sáng". Muốn đủ chất lượng thì bật `sbr`:

```
cargo run -p fbaudio-cli --release --features sbr -- "<link>"
cargo tauri build --features sbr                 # Windows, chạy trong thư mục app/
cargo tauri android build --features sbr ...     # Android
```

Lưu ý khi bật `sbr`:

- Đã kiểm chứng trên **Linux** (gcc): 22 test đạt và đầu ra khớp bản giải mã FDK trực tiếp trong vòng 0,1 dB. **Chưa thử biên dịch bằng MSVC và NDK.** Thư viện FDK được kiểm tra là không tham chiếu runtime C++ (không `operator new`, `__cxa_*`), nên dự kiến không vướng `libc++_shared.so` trên Android.
- FDK-AAC có **giấy phép riêng của Fraunhofer** (không phải MIT/Apache) và không kèm quyền sáng chế. Đọc tệp `NOTICE` trong crate `fdk-aac-sys` trước khi phát hành ứng dụng.
- Nếu build lỗi, bỏ `--features sbr`: ứng dụng vẫn phát được như bảng trên.

## Cấu trúc

```
core/      thư viện Rust, không phụ thuộc giao diện
  extract  link Facebook → manifest DASH: điều phối nhiều nguồn, nhớ kết quả, bộ cào tích hợp dự phòng
  ytdlp    chạy yt-dlp tại chỗ (huỷ được, tự cập nhật) / gọi máy chủ yt-dlp, đọc JSON của `yt-dlp -J`
  dash     phân tích MPD, chỉ chọn audio, lập lịch đoạn live (timeline / $Number$ / $Time$)
  fmp4     tách AAC frame dạng luồng từ fMP4/CMAF
  decode   AAC-LC và HE-AAC (Symphonia thuần Rust; tuỳ chọn FDK-AAC cho SBR/PS đầy đủ)
  output   vòng đệm + resample điều tốc + bám live; cpal → WASAPI (Windows) / Oboe-AAudio (Android)
  player   điều phối, chạy trong một luồng nền riêng (không phụ thuộc WebView)
cli/       bản dòng lệnh để thử nhanh
app/
  ui/index.html            giao diện (cảm ứng, tối/sáng theo hệ thống)
  src-tauri/               lớp Tauri mỏng: start / stop / status
  android-overlay/         Kotlin: dịch vụ nền, cầu nối JS, nhận link chia sẻ
scripts/   ytdlp_helper.py (máy chủ yt-dlp cho Android); kiểm thử đầu-cuối bằng ffmpeg (không cần Facebook);
           he-fixtures/ tạo lại dữ liệu test HE-AAC
```

## Thử nhanh bằng dòng lệnh (Windows / Linux / macOS)

```
cargo run -p fbaudio-cli --release -- "https://www.facebook.com/<trang>/videos/<id>" --mode ultra
```

Tuỳ chọn: `--mode ultra|balanced|stable`, `--wav out.wav` (ghi file thay vì ra loa), `--secs N`, `-v` (log chi tiết).
Cách lấy link: `--resolver auto|ytdlp|builtin`, `--ytdlp PATH`, `--ytdlp-args "--cookies-from-browser firefox"`, `--ytdlp-server URL`, `--ytdlp-token T`, `--no-update`; `--update-ytdlp` chạy `yt-dlp -U` rồi thoát.
Cũng nhận link `.mpd` trực tiếp. Trên Linux cần `libasound2-dev` để build.

## Build cho Windows

Cần: Rust (toolchain MSVC), Visual Studio Build Tools (workload C++), WebView2 (có sẵn trên Windows 10/11).

```
cargo install tauri-cli --version "^2" --locked
cd app
cargo tauri dev                      # chạy thử
cargo tauri build --bundles nsis     # tạo bộ cài .exe
```

Bộ cài nằm ở `app/src-tauri/target/release/bundle/nsis/`. Thu nhỏ cửa sổ vẫn phát.

Cài yt-dlp để lấy link Facebook ổn định: `winget install yt-dlp.yt-dlp` (app tự tìm thấy). Không cài thì app dùng bộ cào tích hợp, dễ gãy hơn.

## Build cho Android

Cần: Android Studio (SDK + NDK), JDK 17, các biến `ANDROID_HOME`, `NDK_HOME`, `JAVA_HOME`, và:

```
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
cargo install tauri-cli --version "^2" --locked
cd app
cargo tauri android init
```

Sau `init`, làm đúng 2 việc (một lần duy nhất):

1. Chép Kotlin đè lên file do Tauri sinh ra:
   - Windows (PowerShell): `Copy-Item android-overlay\*.kt src-tauri\gen\android\app\src\main\java\com\fbaudio\live\ -Force`
   - Linux/macOS: `cp android-overlay/*.kt src-tauri/gen/android/app/src/main/java/com/fbaudio/live/`
2. Sửa `src-tauri/gen/android/app/src/main/AndroidManifest.xml` theo `android-overlay/manifest-snippet.xml` (quyền, `intent-filter` chia sẻ, thẻ `<service>`).

Rồi build:

```
cargo tauri android build --debug --apk --target aarch64
```

APK ký debug cài thẳng được (`adb install <file>.apk`, hoặc chép sang điện thoại). Đã bật `opt-level = 3` cho profile dev nên bản này không bị chậm.
Muốn phát hành thì build `release` và ký bằng keystore của bạn.

### Dùng trên Android

- **Lấy link:** Android không chạy được yt-dlp, nên hãy chạy *máy chủ yt-dlp* trong Termux (xem mục *Lấy link từ Facebook*) rồi nhập `http://127.0.0.1:8787` ở *Nâng cao*. Không có máy chủ thì app dùng bộ cào tích hợp.
- **Cách nhanh nhất:** trong app Facebook mở video live → *Chia sẻ* → *Sao chép liên kết* → mở FB Live Audio → *Dán* → phát. Nếu FB Live Audio có trong bảng chia sẻ thì chọn thẳng, app sẽ tự phát.
- **Tắt màn hình vẫn phát.** Nếu bị dừng, vào *Cài đặt → Pin → FB Live Audio → Không hạn chế* (Xiaomi, Oppo, Samsung… thường giết ứng dụng nền rất mạnh tay).
- Muốn dừng: nút *Dừng* trong ứng dụng hoặc trên thông báo. Nếu luồng kết thúc, dịch vụ tự tắt sau vài giây để không giữ pin.

## Ba mức độ trễ

| Mức | Đệm chống giật | Nhảy tới live khi dư | Hợp với |
|---|---|---|---|
| Siêu thấp | 120 ms | > 250 ms | Wi-Fi tốt |
| Cân bằng | 350 ms | > 500 ms | đa số trường hợp |
| Ổn định | 900 ms | > 1200 ms | 4G chập chờn |

Cơ chế bám live: khi đệm dư thì phát nhanh hơn tối đa 6% (resample, gần như không nghe ra); dư nhiều thì bỏ phần cũ nhảy thẳng tới live; cạn đệm thì dựng lại đệm.

## Kiểm thử

```
cargo test -p fbaudio-core                          # 39 test: DASH, fMP4 (AAC-LC và HE-AAC thật) chia mẩu bất kỳ, giải mã, resample, VOD, yt-dlp (JSON thật + yt-dlp giả)…
cargo test -p fbaudio-core --features sbr           # cùng bộ test với bộ giải mã FDK-AAC
bash scripts/e2e-local-dash.sh ultra 2                   # ffmpeg phát DASH live giả lập → fbaudio → WAV → đo độ trễ
JITTER_MS=250 bash scripts/e2e-local-dash.sh ultra 2     # giả lập mạng giật
STALL=1       bash scripts/e2e-local-dash.sh ultra 2     # nguồn khựng 5 s rồi chạy bù
bash scripts/e2e-ytdlp.sh                           # lấy link bằng yt-dlp THẬT (cần yt-dlp), đếm request video = 0
VIA=server bash scripts/e2e-ytdlp.sh                # như trên nhưng qua máy chủ yt-dlp
```

Cần `ffmpeg`, `python3`, `numpy` (và `yt-dlp` cho `e2e-ytdlp.sh`).

## Hạn chế và xử lý sự cố

**Phần dễ gãy nhất là lấy link từ Facebook**, và đó là lý do ứng dụng dùng yt-dlp. Bộ cào tích hợp vẫn còn làm dự phòng, đã chỉnh theo cấu trúc mà extractor Facebook của yt-dlp 2026.08 đang đọc (khoá `manifest_url` trong `dash_manifest_urls`, ghép với `manifest_xml`), nhưng cũng chưa được thử với Facebook thật. Nếu báo *“Không lấy được luồng âm thanh”*:

1. Cập nhật yt-dlp (`yt-dlp -U`, hoặc `yt-dlp --update-to nightly`) rồi thử lại.
2. Video cần đăng nhập thì thêm cookies (xem mục yt-dlp ở trên).
3. Cách chắc chắn nhất: mở video live trên trình duyệt máy tính → F12 → tab *Network* → lọc `mpd`, sao chép URL `.mpd` và **dán thẳng vào ô nhập** của ứng dụng. Phần còn lại (DASH → audio) không phụ thuộc Facebook.

Các giới hạn khác:

- Video **công khai** chạy được ngay. Video cần đăng nhập chỉ phát được khi truyền cookies cho yt-dlp (xem trên); video riêng tư, nhóm kín: không bảo đảm.
- Chỉ DASH. Nếu yt-dlp chỉ trả về HLS hoặc định dạng ghép sẵn hình + tiếng, app báo rõ chứ không phát được.
- Codec: AAC-LC và HE-AAC (v1, v2). Profile khác (ví dụ ER AAC) sẽ báo lỗi rõ ràng.
- Nội dung đã kết thúc (bản phát lại, link `.mpd` tĩnh) phát đủ từ đầu, không đuổi live. Khi phiên live đang nghe kết thúc, app tự dừng sau vài giây và báo *Phiên live đã kết thúc*.
- Phần Android/Tauri **chưa được build trong môi trường dựng** (không có Android SDK/NDK). Đã kiểm tra bằng `cargo check` trên Linux cho lớp Tauri và cấu hình; phần Kotlin viết theo API của Tauri 2 nên có thể cần chỉnh nhỏ theo phiên bản bạn dùng, ví dụ `onWebViewCreate` yêu cầu bản `tauri` mới.
- Vuốt đóng ứng dụng khỏi danh sách gần đây có thể dừng phát (chưa kiểm chứng trên máy thật). Cứ tắt màn hình hoặc chuyển sang app khác là được.

Một vài lỗi có thể gặp khi build Android:

- `onWebViewCreate overrides nothing` → cập nhật `tauri` lên bản 2.x mới nhất.
- `Unresolved reference: enableEdgeToEdge` → xoá dòng `enableEdgeToEdge()` và dòng `import` tương ứng trong `MainActivity.kt`.
- Thiếu `libc++_shared.so` lúc chạy → bật feature `oboe-shared-stdcxx` của `cpal`.
- Báo không tìm thấy thiết bị âm thanh → cập nhật `tauri`/`tao` để `ndk-context` được khởi tạo đúng.

## Lưu ý pháp lý

Điều khoản Facebook hạn chế thu thập dữ liệu tự động. Dùng cho mục đích cá nhân với nội dung công khai và tôn trọng bản quyền của người phát.
