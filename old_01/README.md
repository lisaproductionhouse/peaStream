# FB Live Audio

Nghe Facebook Live **chỉ bằng âm thanh**, độ trễ thấp nhất có thể. Viết bằng Rust, giao diện Tauri 2.
Một codebase cho **Android** (ưu tiên) và **Windows**.

- Chỉ tải luồng audio của manifest DASH. Luồng video không bao giờ được yêu cầu, nên băng thông chỉ ~64–128 kbps.
- Khi tắt màn hình vẫn phát (Android: dịch vụ foreground + wake lock). Việc này **không thêm độ trễ**, chỉ tốn thêm pin.
- Dán link, nhấn phát. Trên Android còn có thể **Chia sẻ → FB Live Audio** từ app Facebook để phát luôn.

## Trạng thái kiểm chứng

**Đã kiểm chứng** (Linux, luồng DASH giả lập bằng ffmpeg): toàn bộ đường đi từ manifest đến PCM (DASH → chỉ audio → fMP4 → AAC → resample → bộ đệm), kể cả bám live, mạng giật, nguồn khựng rồi chạy bù; 18 test tự động; `cargo check` lớp Tauri cùng `tauri.conf.json` và capabilities; giao diện render đúng ở nhiều kích thước và cả hai chế độ sáng/tối.

**Chưa kiểm chứng**, bạn nên thử trước khi tin:

- Lấy link từ **Facebook thật**. Môi trường dựng không truy cập được facebook.com.
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

## Cấu trúc

```
core/      thư viện Rust, không phụ thuộc giao diện
  extract  link Facebook → manifest DASH (đọc JSON nhúng trong trang, như yt-dlp)
  dash     phân tích MPD, chỉ chọn audio, lập lịch đoạn live (timeline / $Number$ / $Time$)
  fmp4     tách AAC frame dạng luồng từ fMP4/CMAF
  decode   AAC-LC (Symphonia, thuần Rust)
  output   vòng đệm + resample điều tốc + bám live; cpal → WASAPI (Windows) / Oboe-AAudio (Android)
  player   điều phối, chạy trong một luồng nền riêng (không phụ thuộc WebView)
cli/       bản dòng lệnh để thử nhanh
app/
  ui/index.html            giao diện (cảm ứng, tối/sáng theo hệ thống)
  src-tauri/               lớp Tauri mỏng: start / stop / status
  android-overlay/         Kotlin: dịch vụ nền, cầu nối JS, nhận link chia sẻ
scripts/   kiểm thử đầu-cuối bằng ffmpeg (không cần Facebook)
```

## Thử nhanh bằng dòng lệnh (Windows / Linux / macOS)

```
cargo run -p fbaudio-cli --release -- "https://www.facebook.com/<trang>/videos/<id>" --mode ultra
```

Tuỳ chọn: `--mode ultra|balanced|stable`, `--wav out.wav` (ghi file thay vì ra loa), `--secs N`, `-v` (log chi tiết).
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
cargo test -p fbaudio-core                          # 18 test: DASH, fMP4 với dữ liệu thật chia mẩu bất kỳ, giải mã, resample…
bash scripts/e2e-local-dash.sh ultra 2                   # ffmpeg phát DASH live giả lập → fbaudio → WAV → đo độ trễ
JITTER_MS=250 bash scripts/e2e-local-dash.sh ultra 2     # giả lập mạng giật
STALL=1       bash scripts/e2e-local-dash.sh ultra 2     # nguồn khựng 5 s rồi chạy bù
```

Cần `ffmpeg`, `python3`, `numpy`.

## Hạn chế và xử lý sự cố

**Phần dễ gãy nhất là lấy link từ Facebook.** Facebook không có API công khai cho người xem; ứng dụng đọc JSON mà trang video nhúng sẵn (như yt-dlp) và Facebook đổi cấu trúc này thường xuyên. Môi trường mình dựng không truy cập được facebook.com, nên bước này **chưa được thử với Facebook thật**, chỉ có test trên dữ liệu mô phỏng. Nếu báo *"Không tìm thấy luồng DASH"*:

1. Mở video live trên trình duyệt máy tính → F12 → tab *Network* → lọc `mpd`.
2. Sao chép URL `.mpd` và **dán thẳng vào ô nhập** của ứng dụng. Phần còn lại (DASH → audio) không phụ thuộc Facebook.

Các giới hạn khác:

- Chỉ phát được video **công khai**, không cần đăng nhập. Video riêng tư, nhóm kín: không hỗ trợ.
- Chỉ **AAC-LC** (Facebook dùng `mp4a.40.2`). Gặp HE-AAC sẽ báo lỗi rõ ràng thay vì phát sai tốc độ.
- Chỉ DASH, chưa có HLS.
- Phần Android/Tauri **chưa được build trong môi trường dựng** (không có Android SDK/NDK). Đã kiểm tra bằng `cargo check` trên Linux cho lớp Tauri và cấu hình; phần Kotlin viết theo API của Tauri 2 nên có thể cần chỉnh nhỏ theo phiên bản bạn dùng, ví dụ `onWebViewCreate` yêu cầu bản `tauri` mới.
- Vuốt đóng ứng dụng khỏi danh sách gần đây có thể dừng phát (chưa kiểm chứng trên máy thật). Cứ tắt màn hình hoặc chuyển sang app khác là được.

Một vài lỗi có thể gặp khi build Android:

- `onWebViewCreate overrides nothing` → cập nhật `tauri` lên bản 2.x mới nhất.
- `Unresolved reference: enableEdgeToEdge` → xoá dòng `enableEdgeToEdge()` và dòng `import` tương ứng trong `MainActivity.kt`.
- Thiếu `libc++_shared.so` lúc chạy → bật feature `oboe-shared-stdcxx` của `cpal`.
- Báo không tìm thấy thiết bị âm thanh → cập nhật `tauri`/`tao` để `ndk-context` được khởi tạo đúng.

## Lưu ý pháp lý

Điều khoản Facebook hạn chế thu thập dữ liệu tự động. Dùng cho mục đích cá nhân với nội dung công khai và tôn trọng bản quyền của người phát.
