//! Lớp mỏng nối giao diện (ui/index.html) với lõi `fbaudio-core`.
//! Toàn bộ xử lý âm thanh chạy trong luồng Rust riêng, KHÔNG phụ thuộc WebView:
//! tắt màn hình / WebView bị tạm dừng thì âm thanh vẫn phát bình thường.

use fbaudio_core::{LatencyMode, Player, PlayerConfig, Status};
use std::sync::Mutex;

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

#[tauri::command]
fn start(url: String, mode: String) {
    let mut guard = PLAYER.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    *guard = Some(Player::start(PlayerConfig {
        url,
        latency: LatencyMode::parse(&mode),
        wav: None,
    }));
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
