//! Lớp mỏng nối giao diện (ui/index.html) với lõi `fbaudio-core`.
//! Toàn bộ xử lý âm thanh chạy trong luồng Rust riêng, KHÔNG phụ thuộc WebView:
//! tắt màn hình / WebView bị tạm dừng thì âm thanh vẫn phát bình thường.

use fbaudio_core::{LatencyMode, Player, PlayerConfig, Remote, Source, Status, YtDlp};
use serde::Deserialize;
use std::sync::Mutex;

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

/// Tuỳ chọn nguồn luồng do giao diện gửi lên (mục "Nâng cao" và công tắc "Video đã kết thúc").
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SourceArgs {
    /// Máy chủ yt-dlp (scripts/ytdlp_helper.py), ví dụ http://127.0.0.1:8787 — dùng cho Android.
    endpoint: Option<String>,
    token: Option<String>,
    /// Đường dẫn yt-dlp nếu không nằm trong PATH (Windows/Linux/macOS).
    ytdlp_path: Option<String>,
    /// Tham số thêm cho yt-dlp, ví dụ `--cookies-from-browser firefox` (video cần đăng nhập).
    ytdlp_args: Option<String>,
    /// Video đã kết thúc: phát đủ từ đầu thay vì bám live.
    replay: Option<bool>,
}

#[tauri::command]
fn start(url: String, mode: String, source: Option<SourceArgs>) {
    let mut guard = PLAYER.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    // Mặc định: chạy yt-dlp tại chỗ (tự tìm trên máy). Có địa chỉ máy chủ thì đọc luồng từ máy chủ đó.
    let mut yt = YtDlp::default();
    let (mut remote, mut live) = (None, true);
    if let Some(s) = source {
        remote = Remote::new(s.endpoint.as_deref().unwrap_or(""), s.token.as_deref());
        yt.path = s.ytdlp_path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).map(Into::into);
        if let Some(a) = s.ytdlp_args {
            yt.args.extend(fbaudio_core::split_args(&a));
        }
        live = !s.replay.unwrap_or(false);
    }
    let source = match remote {
        Some(r) => Source::Remote(r),
        None => Source::YtDlp(yt),
    };
    *guard = Some(Player::start(PlayerConfig { url, latency: LatencyMode::parse(&mode), wav: None, source, live }));
}

#[tauri::command]
fn stop() {
    if let Some(p) = PLAYER.lock().unwrap().take() {
        p.stop();
    }
}

#[tauri::command]
fn status() -> Status {
    PLAYER.lock().unwrap().as_ref().map_or_else(Status::idle, |p| p.status())
}

/// Android: nút "Dừng" trên thông báo (PlaybackService.kt → `nativeStop`) gọi thẳng vào đây.
/// Tên hàm JNI phải khớp package `com.fbaudio.live` và lớp `PlaybackService`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_fbaudio_live_PlaybackService_nativeStop(
    _env: *mut std::ffi::c_void,
    _this: *mut std::ffi::c_void,
) {
    stop();
}

/// Android: dịch vụ nền hỏi định kỳ "còn đang phát không?" để tự tắt (nhả wake lock) khi luồng
/// đã kết thúc/lỗi lúc màn hình tắt — lúc đó WebView bị tạm dừng nên JS không dọn được.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_fbaudio_live_PlaybackService_nativeIsActive(
    _env: *mut std::ffi::c_void,
    _this: *mut std::ffi::c_void,
) -> u8 {
    PLAYER.lock().unwrap().as_ref().map_or(false, |p| p.is_busy()) as u8
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![start, stop, status])
        .run(tauri::generate_context!())
        .expect("không chạy được ứng dụng");
}
