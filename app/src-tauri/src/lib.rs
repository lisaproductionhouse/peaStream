//! Lớp mỏng nối giao diện (ui/index.html) với lõi `fbaudio-core`.
//! Toàn bộ xử lý âm thanh chạy trong luồng Rust riêng, KHÔNG phụ thuộc WebView:
//! tắt màn hình / WebView bị tạm dừng thì âm thanh vẫn phát bình thường.

use fbaudio_core::{LatencyMode, Player, PlayerConfig, Remote, Source, Status, YtDlp};
use serde::Deserialize;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

// ---- Nhật ký ra tệp. Ứng dụng Windows không có console nên không thể "chạy với -v"; khi báo lỗi hãy gửi tệp này.
struct FileLog {
    file: Mutex<std::fs::File>,
    t0: Instant,
}

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("fbaudio") && m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            if let Ok(mut f) = self.file.lock() {
                let _ = writeln!(f, "{:>8.2}s [{}] {}", self.t0.elapsed().as_secs_f64(), r.level(), r.args());
            }
        }
    }
    fn flush(&self) {}
}

fn log_file() -> std::path::PathBuf {
    std::env::temp_dir().join("fbaudio.log")
}

/// Giữ lại nhật ký lần chạy trước (`fbaudio.old.log`). Trên Android thư mục tạm có thể không ghi được: bỏ qua lặng lẽ.
fn init_log() {
    let path = log_file();
    let _ = std::fs::rename(&path, path.with_extension("old.log"));
    if let Ok(f) = std::fs::File::create(&path) {
        let l: &'static FileLog = Box::leak(Box::new(FileLog { file: Mutex::new(f), t0: Instant::now() }));
        if log::set_logger(l).is_ok() {
            log::set_max_level(log::LevelFilter::Debug);
        }
    }
}

#[tauri::command]
fn log_path() -> String {
    log_file().display().to_string()
}

/// Chỉ ghi tên miền của link (không ghi cả đường dẫn/tham số) vào nhật ký.
fn host_of(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or("")
}

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
    log::info!(
        "bắt đầu phát: {} · mức {mode} · {} · nguồn {}",
        host_of(&url),
        if live { "live" } else { "phát lại" },
        match &source {
            Source::Remote(_) => "máy chủ yt-dlp",
            _ => "yt-dlp tại chỗ",
        }
    );
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
    let st = PLAYER.lock().unwrap().as_ref().map_or_else(Status::idle, |p| p.status());
    // giao diện hỏi ~1 lần/giây: chỉ ghi nhật ký tóm tắt mỗi 10 giây
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if st.state != fbaudio_core::State::Idle && last.map_or(true, |t| t.elapsed().as_secs() >= 10) {
        *last = Some(Instant::now());
        log::info!(
            "{:?}: đệm {} ms (đích {} ms), ngắt {}, {} KB, {} · {}",
            st.state, st.buffer_ms, st.reserve_ms, st.underruns, st.received_kb, st.mode, st.message
        );
    }
    st
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
    init_log();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![start, stop, status, log_path])
        .run(tauri::generate_context!())
        .expect("không chạy được ứng dụng");
}
