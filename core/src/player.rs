//! Điều phối: mở nguồn byte thô (yt-dlp…) → bóc container → loại AU lặp → giải mã AAC → loa.
//!
//! Ứng dụng KHÔNG tự tải hay phân tích manifest: yt-dlp lo từ link Facebook tới luồng âm thanh và ghi ra
//! stdout; phía ta chỉ liên tục đọc dòng byte đó. Phần ta vẫn tự làm là phần quyết định độ trễ nghe được:
//! cắt bỏ phần dồn cũ để bám live, đệm chống giật, điều tốc.

use crate::decode::AacDec;
use crate::demux::{Dedupe, Demux, Ev, Stitch};
use crate::fmp4::TrackInfo;
use crate::output::{Meter, Output, SinkKind, Tuning};
use crate::source::{self, looks_like_extractor_break, Killer, NotInstalled, Opened, Source};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Ba mức đánh đổi giữa độ trễ và độ mượt (xem `Tuning`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LatencyMode {
    /// Đệm ~120 ms — thấp nhất, mạng yếu có thể ngắt quãng.
    Ultra,
    /// Đệm ~350 ms.
    Balanced,
    /// Đệm ~900 ms — hợp mạng di động chập chờn.
    Stable,
}

impl LatencyMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "stable" | "on-dinh" => Self::Stable,
            "balanced" | "can-bang" => Self::Balanced,
            _ => Self::Ultra,
        }
    }

    fn tuning(self) -> Tuning {
        match self {
            Self::Ultra => Tuning { margin_ms: 120, slack_ms: 250 },
            Self::Balanced => Tuning { margin_ms: 350, slack_ms: 500 },
            Self::Stable => Tuning { margin_ms: 900, slack_ms: 1200 },
        }
    }
}

pub struct PlayerConfig {
    /// Link Facebook Live (hoặc bất kỳ link nào yt-dlp hiểu). Không dùng với `Source::Stdin`.
    pub url: String,
    pub latency: LatencyMode,
    /// Nếu có: ghi ra WAV thay vì phát ra loa (dùng để kiểm thử).
    pub wav: Option<PathBuf>,
    /// Công cụ ngoài sinh ra luồng byte thô (yt-dlp tại chỗ, máy chủ yt-dlp, lệnh tuỳ ý, stdin).
    pub source: Source,
    /// `true` (mặc định của app): đang phát trực tiếp → bám live, bỏ phần lịch sử/dồn. `false`: video đã kết thúc →
    /// phát đủ từ đầu, không bỏ gì. Không tự suy ra được: yt-dlp không báo `is_live` cho Facebook.
    pub live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Resolving,
    Connecting,
    Playing,
    Reconnecting,
    Error,
    Ended,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub state: State,
    pub message: String,
    pub title: Option<String>,
    /// Lượng âm thanh đang đệm phía trước điểm phát.
    pub buffer_ms: u32,
    /// Số lần cạn đệm (nghe thành ngắt quãng).
    pub underruns: u32,
    pub bitrate_kbps: u32,
    pub codec: String,
    /// Số KB đã đọc từ luồng thô.
    pub received_kb: u64,
    /// Ghi chú về chất lượng âm thanh (ví dụ HE-AAC chỉ phát phần lõi).
    pub note: Option<String>,
    /// Đang lấy luồng bằng cách nào: "ffmpeg (bám live)", "chạy lại từng lượt (~5,8 s/lượt)", "phát lại".
    pub mode: String,
    /// Đệm đích hiện tại (ms): mức cài sẵn cộng phần tự nới sau các lần cạn.
    pub reserve_ms: u32,
}

impl Status {
    pub fn idle() -> Self {
        Self {
            state: State::Idle,
            message: String::new(),
            title: None,
            buffer_ms: 0,
            underruns: 0,
            bitrate_kbps: 0,
            codec: String::new(),
            received_kb: 0,
            note: None,
            mode: String::new(),
            reserve_ms: 0,
        }
    }
}

struct Shared {
    stop: Arc<AtomicBool>,
    st: Mutex<Status>,
    meter: Mutex<Option<Arc<Meter>>>,
    killer: Mutex<Killer>,
}

impl Shared {
    fn set(&self, state: State, msg: impl Into<String>) {
        let mut s = self.st.lock().unwrap();
        s.state = state;
        s.message = msg.into();
    }
    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.st.lock().unwrap());
    }
    fn stopped(&self) -> bool {
        self.stop.load(Relaxed)
    }
}

pub struct Player {
    sh: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Player {
    /// Khởi động luồng nền; trả về ngay. Theo dõi tiến trình bằng `status()`.
    pub fn start(cfg: PlayerConfig) -> Player {
        // Trạng thái ban đầu là "đang chờ" (không phải Idle): giao diện có thể hỏi status ngay sau
        // `start`, thấy Idle sẽ tưởng lõi đã tự dừng và tắt dịch vụ nền vừa bật.
        let mut first = Status::idle();
        first.state = State::Resolving;
        first.message = waiting_message(&cfg.source).into();
        let sh = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(false)),
            st: Mutex::new(first),
            meter: Mutex::new(None),
            killer: Mutex::new(Killer::none()),
        });
        let sh2 = sh.clone();
        let handle = std::thread::Builder::new()
            .name("fbaudio-player".into())
            .spawn(move || {
                let r = run(&sh2, &cfg);
                if sh2.stopped() {
                    sh2.set(State::Idle, "Đã dừng");
                } else {
                    match r {
                        Ok(()) => sh2.set(State::Ended, "Phiên phát đã kết thúc"),
                        Err(e) => sh2.set(State::Error, format!("{e:#}")),
                    }
                }
            })
            .expect("không tạo được luồng player");
        Player { sh, handle: Mutex::new(Some(handle)) }
    }

    /// Dừng ngay: âm thanh tắt tức thì và tiến trình yt-dlp (cùng ffmpeg của nó) bị diệt, nên `read` đang chặn
    /// trong luồng nền trả về ngay thay vì treo tới khi công cụ tự thoát.
    pub fn stop(&self) {
        self.sh.stop.store(true, Relaxed);
        self.sh.killer.lock().unwrap().kill();
        self.sh.set(State::Idle, "Đã dừng");
    }

    pub fn status(&self) -> Status {
        let mut s = self.sh.st.lock().unwrap().clone();
        if let Some(m) = self.sh.meter.lock().unwrap().as_ref() {
            let rate = m.rate.load(Relaxed).max(1) as u64;
            s.buffer_ms = (m.fill_frames.load(Relaxed) as u64 * 1000 / rate) as u32;
            s.underruns = m.underruns.load(Relaxed);
            s.reserve_ms = m.reserve_ms.load(Relaxed);
        }
        s
    }

    /// Còn đang chờ/phát? (dùng cho dịch vụ nền Android tự dọn khi không còn phát)
    pub fn is_busy(&self) -> bool {
        matches!(
            self.sh.st.lock().unwrap().state,
            State::Resolving | State::Connecting | State::Playing | State::Reconnecting
        )
    }

    /// Chờ luồng nền kết thúc (tối đa `timeout`). Trả `true` nếu đã xong.
    pub fn wait_finished(&self, timeout: Duration) -> bool {
        let t0 = Instant::now();
        loop {
            let done = self.handle.lock().unwrap().as_ref().map_or(true, |h| h.is_finished());
            if done {
                return true;
            }
            if t0.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------- vòng đọc

const HE_NOTE: &str =
    "Luồng HE-AAC: đang phát phần lõi, âm thanh có thể kém sáng hơn bản gốc (thiếu dải cao).";

/// Không có byte nào sau chừng này → coi như công cụ treo.
const FIRST_BYTE_LIMIT_MS: u64 = 90_000;
/// Riêng cách 1 (ép ffmpeg) chỉ chờ ngắn hơn: nếu ffmpeg treo (URL manifest không dùng được) thì lùi về cách 2 sớm.
#[cfg(not(test))]
const FFMPEG_FIRST_BYTE_LIMIT_MS: u64 = 30_000;
#[cfg(test)]
const FFMPEG_FIRST_BYTE_LIMIT_MS: u64 = 1_000;
/// Đang phát mà im lặng chừng này → mất tín hiệu / phiên live đã kết thúc.
const STALL_LIMIT_MS: u64 = 45_000;

fn waiting_message(src: &Source) -> &'static str {
    match src {
        Source::YtDlp(_) => "Đang chờ yt-dlp",
        Source::Remote(_) => "Đang chờ máy chủ yt-dlp",
        Source::Command { .. } | Source::Stdin => "Đang đọc luồng",
    }
}

/// Người gác: diệt công cụ nếu nó im lặng quá lâu (read đang chặn sẽ trả về).
struct Monitor {
    t0: Instant,
    last_ms: Arc<AtomicU64>,
    first: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    fired: Arc<AtomicBool>,
}

impl Monitor {
    fn start(killer: Killer, first_byte_limit_ms: u64) -> Self {
        let m = Monitor {
            t0: Instant::now(),
            last_ms: Arc::new(AtomicU64::new(0)),
            first: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            fired: Arc::new(AtomicBool::new(false)),
        };
        let (t0, last, first, done, fired) =
            (m.t0, m.last_ms.clone(), m.first.clone(), m.done.clone(), m.fired.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            if done.load(Relaxed) {
                break;
            }
            let silent = (t0.elapsed().as_millis() as u64).saturating_sub(last.load(Relaxed));
            let limit = if first.load(Relaxed) { STALL_LIMIT_MS } else { first_byte_limit_ms };
            if silent > limit {
                fired.store(true, Relaxed);
                killer.kill();
                break;
            }
        });
        m
    }
    fn touch(&self) {
        self.last_ms.store(self.t0.elapsed().as_millis() as u64, Relaxed);
        self.first.store(true, Relaxed);
    }
    fn fired(&self) -> Option<&'static str> {
        self.fired.load(Relaxed).then(|| {
            if self.first.load(Relaxed) {
                "Mất tín hiệu hơn 45 giây — có thể phiên live đã kết thúc."
            } else {
                "Không nhận được dữ liệu đúng hạn — công cụ có thể bị treo hoặc video không phát được."
            }
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.done.store(true, Relaxed);
    }
}

enum Outcome {
    /// Người dùng bấm Dừng.
    Stopped,
    /// Công cụ đã kết thúc (hết luồng hoặc bị lỗi). `new_aus`: số access unit MỚI (chưa từng thấy) trong lượt này.
    Ran { new_aus: u64, err: Option<String> },
}

/// Cách lấy luồng live.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Strategy {
    /// Ép yt-dlp giao DASH cho ffmpeg: nếu manifest là manifest "sống" thì bám live liên tục, độ trễ thấp nhất.
    Ffmpeg,
    /// Bộ tải gốc của yt-dlp. Với Facebook nó tải đúng các đoạn trong bản manifest chụp lúc tải trang (đã gặp: 11
    /// đoạn) rồi THOÁT, không bám live. App chạy lại từng lượt và nối tiếp, bỏ phần chồng lấn.
    Native,
}

/// Chỉ cho tự cập nhật yt-dlp một lần mỗi lần chạy ứng dụng.
static UPDATED: AtomicBool = AtomicBool::new(false);

/// Live mà công cụ cứ thoát sau mỗi lượt: gặp chừng này lượt liên tiếp KHÔNG có gì mới (hoặc lỗi), kéo dài ít nhất
/// `IDLE_RUNS_MIN`, thì coi là phiên live đã kết thúc.
#[cfg(not(test))]
const IDLE_RUNS_LIMIT: u32 = 4;
#[cfg(test)]
const IDLE_RUNS_LIMIT: u32 = 2;
#[cfg(not(test))]
const IDLE_RUNS_MIN: Duration = Duration::from_secs(20);
#[cfg(test)]
const IDLE_RUNS_MIN: Duration = Duration::ZERO;
/// Khoảng cách tối thiểu giữa hai lượt chạy (không dồn dập vào Facebook).
#[cfg(not(test))]
const MIN_RUN_INTERVAL: Duration = Duration::from_millis(1500);
#[cfg(test)]
const MIN_RUN_INTERVAL: Duration = Duration::from_millis(30);

/// Trạng thái xuyên suốt các lượt chạy của công cụ.
struct Session {
    stitch: Stitch,
    dedupe: Dedupe,
    dec: Option<AacDec>,
    /// Chỉ có với luồng live.
    gate: Option<Gate>,
    playing_ever: bool,
    /// Trung bình trượt thời gian một lượt chạy công cụ (ms), 0 nếu chưa đo.
    cycle_ms: f64,
    core_rate: u32,
    total: u64,
    au_bytes: u64,
    au_frames: u64,
}

impl Session {
    fn new(live: bool) -> Self {
        Session {
            stitch: Stitch::new(),
            dedupe: Dedupe::new(),
            dec: None,
            gate: live.then(Gate::new),
            playing_ever: false,
            cycle_ms: 0.0,
            core_rate: 48_000,
            total: 0,
            au_bytes: 0,
            au_frames: 0,
        }
    }
}

fn mark_playing(sh: &Shared, sess: &mut Session) {
    if !sess.playing_ever {
        sess.playing_ever = true;
        sh.set(State::Playing, "Đang phát");
    }
}

fn run(sh: &Shared, cfg: &PlayerConfig) -> Result<()> {
    let kind = match &cfg.wav {
        Some(p) => SinkKind::Wav(p.clone()),
        None => SinkKind::Device,
    };
    // Mở thiết bị âm thanh ngay, song song với lúc yt-dlp khởi động: không thêm độ trễ vào tiếng đầu tiên.
    let mut out = Output::new(cfg.latency.tuning(), kind, sh.stop.clone())?;
    *sh.meter.lock().unwrap() = Some(out.meter.clone());
    out.set_live(cfg.live);
    log::info!("chế độ {}", if cfg.live { "live: bám live, bỏ phần lịch sử/dồn" } else { "phát lại: phát đủ từ đầu" });

    let mut sess = Session::new(cfg.live);
    // Live: thử ffmpeg trước (độ trễ thấp nếu manifest sống), không được thì bộ tải gốc chạy lặp từng lượt.
    let mut strategy = match &cfg.source {
        Source::YtDlp(y) if cfg.live && y.ffmpeg_live => Strategy::Ffmpeg,
        Source::Remote(_) if cfg.live => Strategy::Ffmpeg,
        _ => Strategy::Native,
    };
    let (mut updated, mut idle_runs, mut idle_since) = (false, 0u32, None::<Instant>);
    loop {
        let started = Instant::now();
        sh.update(|s| {
            s.mode = match (cfg.live, strategy) {
                (false, _) => "phát lại".into(),
                (true, Strategy::Ffmpeg) => "ffmpeg (bám live)".into(),
                (true, Strategy::Native) => "chạy lại từng lượt".into(),
            }
        });
        if !sess.playing_ever {
            sh.set(State::Resolving, waiting_message(&cfg.source));
        }
        let opened = match source::open(&cfg.source, &cfg.url, strategy == Strategy::Ffmpeg) {
            Ok(o) => o,
            // máy chủ yt-dlp từ chối/thất bại khi bị ép ffmpeg (thiếu ffmpeg, không có URL manifest…) → bộ tải gốc
            Err(e) if strategy == Strategy::Ffmpeg && matches!(cfg.source, Source::Remote(_)) => {
                log::info!("máy chủ yt-dlp lỗi ở chế độ ffmpeg ({e:#}) → bộ tải gốc, chạy lặp từng lượt");
                strategy = Strategy::Native;
                continue;
            }
            Err(e) => {
                return Err(if e.downcast_ref::<NotInstalled>().is_some() && cfg!(target_os = "android") {
                    anyhow!("{e}. Android không chạy được yt-dlp: nhập địa chỉ máy chủ yt-dlp ở mục Nâng cao.")
                } else {
                    e
                })
            }
        };
        *sh.killer.lock().unwrap() = opened.killer.clone();
        if sh.stopped() {
            opened.killer.kill(); // bấm Dừng ngay lúc đang mở
            return Ok(());
        }
        if cfg.live && strategy == Strategy::Native {
            out.set_burst_mode(true); // bộ tải gốc chắc chắn thoát sau mỗi lượt: dữ liệu về theo cục ngay từ lượt đầu
        }
        let limit = if strategy == Strategy::Ffmpeg { FFMPEG_FIRST_BYTE_LIMIT_MS } else { FIRST_BYTE_LIMIT_MS };
        // bộ tải gốc chắc chắn sẽ thoát: chờ EOF để nhả đuôi, đừng tin vào khoảng im lặng
        let join = if strategy == Strategy::Ffmpeg { Join::Continuous } else { Join::Bounded };
        let outcome = match pump(sh, opened, &mut out, &mut sess, limit, join) {
            Ok(o) => o,
            // cách 1 hỏng trước khi có tiếng đầu tiên (treo, định dạng lạ, lỗi đọc…) → lùi về cách 2
            Err(e) if strategy == Strategy::Ffmpeg && !sess.playing_ever && !sh.stopped() => {
                log::info!("cách ffmpeg hỏng ({e:#}) → bộ tải gốc, chạy lặp từng lượt");
                strategy = Strategy::Native;
                continue;
            }
            Err(e) => return Err(e),
        };
        let (new_aus, err) = match outcome {
            Outcome::Stopped => return Ok(()),
            Outcome::Ran { new_aus, err } => (new_aus, err),
        };

        if !sess.playing_ever {
            // chưa phát được âm thanh nào: công cụ lỗi hoặc ra rỗng
            let msg = err.unwrap_or_else(|| "Công cụ kết thúc mà không xuất ra âm thanh nào.".into());
            if strategy == Strategy::Ffmpeg {
                log::info!("ffmpeg không dùng được ({msg}) → bộ tải gốc, chạy lặp từng lượt");
                strategy = Strategy::Native;
                continue;
            }
            if let Source::YtDlp(y) = &cfg.source {
                // Facebook vừa đổi cấu trúc → yt-dlp (bản mới) thường đã sửa: cập nhật rồi thử lại một lần
                if y.auto_update && !updated && looks_like_extractor_break(&msg) && !UPDATED.swap(true, Relaxed) {
                    log::info!("yt-dlp báo trích xuất hỏng ({msg}) → thử cập nhật");
                    sh.set(State::Resolving, "Đang cập nhật yt-dlp");
                    // gọi TRƯỚC khi log: macro `log` không đánh giá tham số khi mức log đang tắt
                    let result = source::update_ytdlp(y);
                    log::info!("kết quả cập nhật: {:?}", result.map(|t| t.lines().last().unwrap_or("").to_string()));
                    updated = true;
                    continue;
                }
            }
            bail!("{}", explain(&msg));
        }
        if !cfg.live {
            // phát lại: công cụ xong là hết video; chết giữa chừng thì báo lỗi của nó
            if let Some(e) = err {
                bail!("{}", explain(&e));
            }
            out.drain(&sh.stop);
            return Ok(());
        }

        // Live mà công cụ đã thoát: hoặc yt-dlp chỉ tải cửa sổ hữu hạn (chạy lại và nối tiếp), hoặc phiên live đã hết.
        out.set_burst_mode(true);
        let run_ms = started.elapsed().as_secs_f64() * 1000.0;
        sess.cycle_ms = if sess.cycle_ms == 0.0 { run_ms } else { 0.7 * sess.cycle_ms + 0.3 * run_ms };
        let cycle = sess.cycle_ms;
        sh.update(|s| s.mode = format!("chạy lại từng lượt (~{:.1} s/lượt)", cycle / 1000.0));
        log::info!("lượt xong sau {:.1} s: {new_aus} AU mới; chu kỳ trung bình {:.1} s", run_ms / 1000.0, cycle / 1000.0);
        if new_aus == 0 || err.is_some() {
            idle_runs += 1;
            idle_since.get_or_insert_with(Instant::now);
        } else {
            idle_runs = 0;
            idle_since = None;
        }
        if idle_runs >= IDLE_RUNS_LIMIT && idle_since.is_some_and(|t| t.elapsed() >= IDLE_RUNS_MIN) {
            out.drain(&sh.stop); // phát nốt phần đã đệm rồi mới kết thúc/báo lỗi
            if let Some(e) = err {
                bail!("{}", explain(&e));
            }
            return Ok(());
        }
        log::debug!("lỗi của lượt: {err:?}");
        while started.elapsed() < MIN_RUN_INTERVAL {
            if sh.stopped() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn explain(msg: &str) -> String {
    let mut s = msg.to_string();
    if msg.to_lowercase().contains("ffmpeg") {
        s.push_str("\nLuồng live cần ffmpeg (Windows: winget install Gyan.FFmpeg · Termux: pkg install ffmpeg).");
    } else if looks_like_extractor_break(msg) {
        s.push_str("\nThử cập nhật yt-dlp: yt-dlp -U (hoặc yt-dlp --update-to nightly).");
    }
    s
}

fn codec_label(t: &TrackInfo, d: &AacDec) -> String {
    let name = match t.object_type {
        5 => "HE-AAC".to_string(),
        29 => "HE-AAC v2".to_string(),
        2 if d.core_only => "HE-AAC".to_string(),
        2 => "AAC-LC".to_string(),
        o => format!("AAC (loại {o})"),
    };
    format!("{name} {} kHz", (d.rate as f32 / 100.0).round() / 10.0)
}

/// Cập nhật số liệu hiển thị: dung lượng đã đọc, bitrate thực (AU/giây âm thanh), tiêu đề do công cụ báo.
fn report(sh: &Shared, info: &source::Info, s: &Session) {
    let secs = s.au_frames as f64 * 1024.0 / s.core_rate.max(1) as f64;
    sh.update(|st| {
        st.received_kb = s.total / 1024;
        if secs > 1.0 {
            st.bitrate_kbps = (s.au_bytes as f64 * 8.0 / secs / 1000.0).round() as u32;
        }
        if st.title.is_none() {
            st.title = info.title();
        }
    });
}

/// Tin nhắn từ luồng đọc (đọc chặn riêng để vòng chính còn "tick" được khi công cụ im lặng).
enum Msg {
    Data(Vec<u8>),
    /// Hết luồng; kèm kết quả `finish()` của công cụ (thoát bình thường hay báo lỗi).
    Eof(Result<()>),
    Err(std::io::Error),
}

fn spawn_reader(mut stream: Box<dyn source::Stream>) -> std::sync::mpsc::Receiver<Msg> {
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    std::thread::Builder::new()
        .name("fbaudio-reader".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Msg::Eof(stream.finish()));
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                            break; // vòng chính đã thoát (Dừng): thả stream → công cụ bị diệt
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = tx.send(Msg::Err(e));
                        break;
                    }
                }
            }
        })
        .expect("không tạo được luồng đọc");
    rx
}

/// Đưa PCM vừa giải mã qua cổng (nếu là live) rồi vào bộ đệm phát.
fn deliver(sh: &Shared, out: &mut Output, sess: &mut Session, pcm: &mut Vec<f32>) {
    if pcm.is_empty() {
        return;
    }
    let (rate, ch) = sess.dec.as_ref().map_or((48_000, 2), |d| (d.rate, d.channels));
    let ms = pcm.len() as f64 / ch.max(1) as f64 / rate.max(1) as f64 * 1000.0;
    let to_push: Option<(Vec<f32>, u32, usize)> = match sess.gate.as_mut() {
        None => Some((std::mem::take(pcm), rate, ch)),
        Some(g) => match g.feed(Instant::now(), ms, pcm, rate, ch) {
            Verdict::Push => Some((std::mem::take(pcm), rate, ch)),
            Verdict::Keep => None,
            Verdict::Release => g.take(),
        },
    };
    match to_push {
        Some((p, r, c)) => {
            out.push(&p, r, c);
            mark_playing(sh, sess);
        }
        None if !sess.playing_ever => sh.set(State::Connecting, "Đang bỏ qua phần lịch sử để vào live"),
        None => {}
    }
    pcm.clear();
}

/// Một lượt chạy của công cụ: đọc tới hết luồng, đưa âm thanh mới vào `out`.
fn pump(sh: &Shared, opened: Opened, out: &mut Output, sess: &mut Session, first_byte_limit_ms: u64, join: Join) -> Result<Outcome> {
    let Opened { stream, info, killer } = opened;
    let mon = Monitor::start(killer, first_byte_limit_ms);
    let rx = spawn_reader(stream);
    sess.stitch.begin_run();
    if let Some(g) = sess.gate.as_mut() {
        g.begin_run(join);
    }

    let run_started = Instant::now();
    let mut demux = Demux::new();
    let mut pcm: Vec<f32> = Vec::new();
    let mut last_report = Instant::now();
    let mut first_data = true;
    let mut eof: Option<Result<()>> = None;

    loop {
        if sh.stopped() {
            return Ok(Outcome::Stopped);
        }
        let data = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Msg::Data(d)) => d,
            Ok(Msg::Eof(r)) => {
                eof = Some(r);
                break;
            }
            Ok(Msg::Err(e)) => {
                if sh.stopped() {
                    return Ok(Outcome::Stopped);
                }
                if let Some(m) = mon.fired() {
                    bail!("{m}");
                }
                return Err(anyhow!(e).context("đọc luồng thất bại"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // công cụ đang im lặng: nếu một đợt giao vừa kết thúc thì nhả phần đang giữ / phần đuôi
                let released = sess.gate.as_mut().and_then(|g| if g.tick(Instant::now()) { g.take() } else { None });
                if let Some((p, rate, ch)) = released {
                    out.push(&p, rate, ch);
                    mark_playing(sh, sess);
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        mon.touch();
        sess.total += data.len() as u64;

        if first_data {
            first_data = false;
            sh.update(|s| s.title = s.title.take().or_else(|| info.title()));
            if sess.gate.is_some() && !sess.playing_ever {
                sh.set(State::Connecting, "Đã nhận dữ liệu, đang vào live");
            }
        }

        demux.feed(&data, &mut |ev| {
            match ev {
                Ev::Config(t) => {
                    // HE-AAC trong ADTS báo "ngầm" (tiêu đề ghi LC ở tần số lõi): tần số lõi thấp là dấu hiệu.
                    let he_hint = t.is_he() || (t.ext_rate.is_none() && matches!(t.sample_rate, 24_000 | 22_050 | 16_000));
                    if sess.dec.as_ref().map_or(true, |d| d.asc != t.asc) {
                        let d = AacDec::new(t, he_hint)?;
                        sess.core_rate = t.sample_rate;
                        sh.update(|s| {
                            s.codec = codec_label(t, &d);
                            if d.core_only {
                                s.note = Some(HE_NOTE.into());
                            }
                        });
                        sess.dec = Some(d);
                    }
                }
                Ev::Frame(f) => {
                    // bỏ phần chồng lấn với lượt trước, rồi bỏ bản lặp ngay trong luồng (ffmpeg phát lặp đoạn live);
                    // bộ lọc lặp nhìn trước một AU nên trả về AU của lượt trước
                    if !sess.stitch.accept(f) {
                        return Ok(());
                    }
                    if let Some(ready) = sess.dedupe.push(f) {
                        sess.au_bytes += ready.len() as u64;
                        sess.au_frames += 1;
                        if let Some(d) = sess.dec.as_mut() {
                            d.decode(&ready, &mut pcm)?;
                        }
                    }
                }
            }
            Ok(())
        })?;

        deliver(sh, out, sess, &mut pcm);
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            report(sh, &info, sess);
        }
    }

    if sh.stopped() {
        return Ok(Outcome::Stopped);
    }
    if let Some(m) = mon.fired() {
        bail!("{m}");
    }
    // hết luồng: nhả phần đang giữ (luồng ngắn) hoặc phần đuôi của đợt dồn (cửa sổ hữu hạn): đó là nội dung mới nhất có
    // AU cuối cùng còn nằm trong bộ lọc lặp (nó nhìn trước một AU): giải mã nốt
    if let Some(ready) = sess.dedupe.flush() {
        sess.au_bytes += ready.len() as u64;
        sess.au_frames += 1;
        if let Some(d) = sess.dec.as_mut() {
            d.decode(&ready, &mut pcm)?;
        }
        deliver(sh, out, sess, &mut pcm);
    }
    // Phần đuôi dài bằng MỘT CHU KỲ lượt (tối thiểu 2,5 s): lượt kế tiếp chỉ về sau chừng đó, nên đuôi ngắn hơn thì đệm cạn
    // trước khi dữ liệu mới tới (đo: đuôi 2,5 s với lượt ~6 s gây khe 3,7 s ngay giây thứ 7).
    let cycle_tail_ms = (run_started.elapsed().as_secs_f64() * 1200.0).clamp(GATE_TAIL_MS, GATE_TAIL_MAX_MS);
    let released = sess.gate.as_mut().and_then(|g| if g.finish(cycle_tail_ms) { g.take() } else { None });
    if let Some((p, rate, ch)) = released {
        out.push(&p, rate, ch);
        mark_playing(sh, sess);
    }
    report(sh, &info, sess); // lần báo cuối (luồng ngắn kết thúc trước chu kỳ 1 s)
    if sess.dedupe.dropped > 0 {
        log::debug!("đã loại {} access unit bị phát lặp tới giờ", sess.dedupe.dropped);
    }
    let err = eof.and_then(|r| r.err()).map(|e| format!("{e:#}"));
    Ok(Outcome::Ran { new_aus: sess.stitch.new_aus, err })
}

// ---------------------------------------------------------------- cổng bỏ lịch sử lúc vào live

/// Cửa sổ đo tốc độ âm thanh về. Đủ dài để dao động của mạng (mỗi đoạn tải mất 0,1–1,5 s) không làm một đợt đổ lịch sử trông
/// giống live: đo trên nguồn giật, cửa sổ 3 s với ngưỡng 1,5× nhầm lịch sử là live ngay giữa các đoạn.
const GATE_RATE_WINDOW: Duration = Duration::from_secs(5);
/// Quan sát tối thiểu trước khi dám kết luận "đang live".
const GATE_MIN_OBS: Duration = Duration::from_secs(4);
/// Âm thanh về dưới ngưỡng này (lần thời gian thực, đo trong cửa sổ) = đã tới mép live; lịch sử đổ ra thì ≥ 2×.
const GATE_LIVE_RATE: f64 = 1.6;

/// Phần đuôi nhớ lại: nội dung MỚI NHẤT có được, phát ngay khi biết đã tới mép live (hoặc khi công cụ thoát).
/// Mặc định 2,5 s; khi công cụ thoát (lượt chạy lại) thì dài bằng một chu kỳ lượt, tối đa `GATE_TAIL_MAX_MS` (cũng là lượng
/// đuôi được nhớ).
const GATE_TAIL_MS: f64 = 2_500.0;
const GATE_TAIL_MAX_MS: f64 = 12_000.0;

#[derive(Debug, PartialEq)]
enum Verdict {
    /// Đã vào live: phát phần này luôn.
    Push,
    /// Chưa biết/đang dồn lịch sử: cổng giữ lại (chỉ nhớ phần đuôi).
    Keep,
    /// Vừa xác định đã tới mép live: phát `Gate::take()` (đã gồm cả phần này).
    Release,
}

/// Cách công cụ giao dữ liệu, quyết định cách nhận ra "đã vào live".
#[derive(Clone, Copy, Debug, PartialEq)]
enum Join {
    /// Luồng không dừng (ffmpeg bám live): nhận ra mép live bằng tốc độ âm thanh về.
    Continuous,
    /// Công cụ chắc chắn sẽ thoát (bộ tải gốc): lượt ĐẦU luôn giữ tới EOF rồi phát phần đuôi; từ lượt sau mọi thứ đều là mới.
    Bounded,
}

/// Vào live giữa chừng một luồng có lịch sử (cửa sổ DVR), công cụ có thể đổ cả lịch sử ra nhanh gấp nhiều lần thời gian thực.
/// Phát hết thì nghe âm thanh CŨ rồi cắt vụn. Cổng giữ PCM lại (chỉ nhớ phần đuôi mới nhất) cho tới khi biết đã tới mép live.
///
/// Cố ý KHÔNG dùng "khoảng im lặng giữa hai lần dữ liệu về" làm dấu hiệu: trên mạng thật mỗi đoạn lịch sử tải mất vài trăm ms
/// nên giữa các đoạn lịch sử nào cũng có khoảng im lặng (đã đo: cổng tưởng là live, phát đoạn cũ nhất trước rồi bộ đệm cắt vụn).
struct Gate {
    joined: bool,
    join: Join,
    t_first: Option<Instant>,
    /// (lúc về, ms âm thanh) trong `GATE_RATE_WINDOW` gần nhất.
    events: VecDeque<(Instant, f64)>,
    tail: Vec<f32>,
    tail_fmt: (u32, usize),
    /// Chỉ nhả chừng này (ms) cuối của đuôi.
    tail_keep_ms: f64,
}

impl Gate {
    fn new() -> Self {
        Gate {
            joined: false,
            join: Join::Continuous,
            t_first: None,
            events: VecDeque::new(),
            tail: Vec::new(),
            tail_fmt: (48_000, 2),
            tail_keep_ms: GATE_TAIL_MS,
        }
    }

    /// Một lượt chạy mới của công cụ.
    fn begin_run(&mut self, join: Join) {
        self.join = join;
        self.t_first = None;
        self.events.clear();
        self.tail.clear();
        self.tail_keep_ms = GATE_TAIL_MS;
    }

    /// Có `ms` âm thanh (`pcm`, đã giải mã và lọc lặp) vừa về lúc `now`.
    fn feed(&mut self, now: Instant, ms: f64, pcm: &[f32], rate: u32, ch: usize) -> Verdict {
        if self.joined {
            return Verdict::Push;
        }
        self.keep_tail(pcm, rate, ch);
        if self.join == Join::Bounded {
            return Verdict::Keep; // chờ EOF
        }
        self.t_first.get_or_insert(now);
        self.events.push_back((now, ms));
        if self.live_now(now) {
            self.joined = true;
            Verdict::Release
        } else {
            Verdict::Keep
        }
    }

    /// Tốc độ âm thanh về trong cửa sổ gần nhất đã xuống mức live chưa.
    fn live_now(&mut self, now: Instant) -> bool {
        while self.events.front().is_some_and(|(t, _)| now.duration_since(*t) > GATE_RATE_WINDOW) {
            self.events.pop_front();
        }
        let Some(t0) = self.t_first else { return false };
        let elapsed = now.duration_since(t0);
        if elapsed < GATE_MIN_OBS {
            return false;
        }
        let win = GATE_RATE_WINDOW.min(elapsed).as_secs_f64();
        let media: f64 = self.events.iter().map(|(_, m)| m).sum();
        media / 1000.0 / win < GATE_LIVE_RATE
    }

    fn keep_tail(&mut self, pcm: &[f32], rate: u32, ch: usize) {
        self.tail_fmt = (rate, ch);
        self.tail.extend_from_slice(pcm);
        let ch = ch.max(1);
        let max = (rate as f64 * ch as f64 * GATE_TAIL_MAX_MS / 1000.0) as usize / ch * ch;
        if self.tail.len() > max {
            // cắt theo bội của số kênh để không đảo kênh trái/phải
            let cut = (self.tail.len() - max).div_ceil(ch) * ch;
            self.tail.drain(..cut.min(self.tail.len()));
        }
    }

    /// Gọi định kỳ khi không có dữ liệu mới: tốc độ về tụt xuống khi luồng chững lại ở mép live. `true` = vừa vào live.
    fn tick(&mut self, now: Instant) -> bool {
        if self.joined || self.join == Join::Bounded || self.t_first.is_none() {
            return false;
        }
        if self.live_now(now) {
            self.joined = true;
            return true;
        }
        false
    }

    /// Hết luồng (công cụ thoát): nếu chưa vào live thì phát phần đuôi (dài `tail_ms`). `true` = có gì để phát.
    fn finish(&mut self, tail_ms: f64) -> bool {
        if self.joined {
            return false;
        }
        self.joined = true;
        self.tail_keep_ms = tail_ms;
        !self.tail.is_empty()
    }

    /// Lấy phần đang giữ (chỉ `tail_keep_ms` cuối, cắt theo bội của số kênh).
    fn take(&mut self) -> Option<(Vec<f32>, u32, usize)> {
        let (rate, ch) = self.tail_fmt;
        let ch = ch.max(1);
        let keep = (rate as f64 * ch as f64 * self.tail_keep_ms / 1000.0) as usize / ch * ch;
        if self.tail.len() > keep {
            let cut = self.tail.len() - keep;
            self.tail.drain(..cut);
        }
        (!self.tail.is_empty()).then(|| (std::mem::take(&mut self.tail), rate, ch))
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// Đẩy một đoạn `secs` giây mang giá trị `v` (rate 1000, mono: 1 mẫu = 1 ms) vào cổng.
    fn seg(g: &mut Gate, t0: Instant, t_ms: u64, secs: f64, v: f32) -> Verdict {
        g.feed(at(t0, t_ms), secs * 1000.0, &vec![v; (secs * 1000.0) as usize], 1000, 1)
    }

    #[test]
    fn realtime_2s_segments_join_after_the_minimum_observation_then_everything_is_pushed() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        assert_eq!(seg(&mut g, t0, 0, 2.0, 0.0), Verdict::Keep);
        assert!(!g.tick(at(t0, 3_000)), "chưa đủ thời gian quan sát");
        assert_eq!(seg(&mut g, t0, 2_000, 2.0, 1.0), Verdict::Keep);
        // tới 4 s: 2 đoạn trong 4 s = 1× → live; phát đuôi mới nhất
        assert_eq!(seg(&mut g, t0, 4_000, 2.0, 2.0), Verdict::Release);
        let (tail, rate, ch) = g.take().unwrap();
        assert_eq!((tail.len(), rate, ch), (2_500, 1000, 1));
        assert_eq!(*tail.last().unwrap(), 2.0);
        for k in 3..8 {
            assert_eq!(seg(&mut g, t0, k * 2_000, 2.0, k as f32), Verdict::Push);
        }
    }

    /// Đoạn dài 4 s: ngay sau đoạn thứ hai, cửa sổ 5 s còn chứa cả hai đoạn (8 s/4 s = 2×) nên chưa kết luận; khi đoạn cũ trượt
    /// khỏi cửa sổ (qua `tick`, ~5,1 s) thì tốc độ về còn 1× → vào live, và đuôi kết thúc bằng đoạn thứ hai.
    #[test]
    fn four_second_segments_join_once_the_older_one_leaves_the_window() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        assert_eq!(seg(&mut g, t0, 0, 4.0, 0.0), Verdict::Keep);
        assert!(!g.tick(at(t0, 3_900)));
        assert_eq!(seg(&mut g, t0, 4_000, 4.0, 1.0), Verdict::Keep);
        assert!(!g.tick(at(t0, 4_900)), "đoạn đầu (4,9 s tuổi) còn trong cửa sổ 5 s");
        assert!(g.tick(at(t0, 5_100)), "đoạn đầu đã trượt khỏi cửa sổ: tốc độ về = 1×");
        let (tail, _, _) = g.take().unwrap();
        assert_eq!(*tail.last().unwrap(), 1.0);
        assert_eq!(seg(&mut g, t0, 8_000, 4.0, 2.0), Verdict::Push);
    }

    /// Đúng lỗi đã gặp: lịch sử đổ về với độ trễ mạng giữa các đoạn (mỗi đoạn 2 s âm thanh về cách nhau 0,4 s). Khoảng im lặng
    /// giữa các đoạn KHÔNG được coi là live; chỉ khi luồng chững lại ở mép live (mỗi 2 s một đoạn) mới vào live, và phần
    /// đuôi phải là âm thanh MỚI NHẤT (không phải đoạn cổ nhất của lịch sử).
    #[test]
    fn flood_with_network_latency_is_not_mistaken_for_live_and_the_tail_is_the_newest_audio() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        let mut t = 0u64;
        for k in 0..20u64 {
            assert_eq!(seg(&mut g, t0, t, 2.0, k as f32), Verdict::Keep, "đoạn lịch sử {k} ở {t} ms");
            assert!(!g.tick(at(t0, t + 300)), "khoảng im lặng 300 ms giữa các đoạn lịch sử không phải live");
            t += 400;
        }
        // lịch sử hết; từ đây mỗi 2 s một đoạn live (giá trị 100, 101, …)
        let mut joined_at = None;
        for k in 0..6u64 {
            t += 2_000;
            match seg(&mut g, t0, t, 2.0, 100.0 + k as f32) {
                Verdict::Release => {
                    joined_at = Some(t);
                    break;
                }
                Verdict::Keep => {}
                v => panic!("{v:?}"),
            }
        }
        let t_join = joined_at.expect("phải vào live sau khi luồng chững lại");
        let (tail, _, _) = g.take().unwrap();
        assert!(*tail.last().unwrap() >= 100.0, "đuôi phải kết thúc bằng âm thanh live, không phải lịch sử");
        assert!(tail.iter().all(|v| *v >= 18.0), "đuôi không được chứa đoạn cổ của lịch sử: {:?}", &tail[..3]);
        assert!(t_join <= 7_600 + 12_000, "vào live quá muộn: {t_join} ms");
    }

    #[test]
    fn bounded_tool_holds_the_first_run_until_eof_and_the_tail_covers_one_cycle() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        g.begin_run(Join::Bounded);
        for k in 0..40u64 {
            assert_eq!(seg(&mut g, t0, k * 50, 2.0, k as f32), Verdict::Keep);
        }
        // dù im lặng bao lâu cũng không tự nhả (tiến trình thoát trễ sau byte cuối)
        assert!(!g.tick(at(t0, 60_000)));
        // EOF: đuôi dài bằng một chu kỳ lượt (7 s)
        assert!(g.finish(7_000.0));
        let (tail, _, _) = g.take().unwrap();
        assert_eq!(tail.len(), 7_000);
        assert_eq!(*tail.last().unwrap(), 39.0);
        // lượt sau: mọi thứ đều là nội dung mới
        g.begin_run(Join::Bounded);
        assert_eq!(seg(&mut g, t0, 100_000, 3.0, 50.0), Verdict::Push);
    }

    #[test]
    fn short_stream_ending_before_it_joined_still_plays_what_it_has() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        assert_eq!(seg(&mut g, t0, 0, 1.5, 7.0), Verdict::Keep);
        assert!(g.finish(GATE_TAIL_MS), "luồng ngắn kết thúc trước khi vào live: vẫn phải phát");
        assert_eq!(g.take().map(|(p, r, c)| (p.len(), r, c)), Some((1_500, 1000, 1)));
    }

    #[test]
    fn tail_is_cut_on_a_channel_boundary() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        g.begin_run(Join::Bounded);
        // 2 kênh: mẫu chẵn là L, mẫu lẻ là R; đuôi tối đa 12 s = 24000 mẫu
        let pcm: Vec<f32> = (0..30_000).map(|k| k as f32).collect();
        assert_eq!(g.feed(at(t0, 0), 15_000.0, &pcm, 1000, 2), Verdict::Keep);
        g.feed(at(t0, 10), 2.0, &[7.0; 4], 1000, 2); // 2 khung nữa
        assert!(g.finish(2_500.0));
        let (tail, _, _) = g.take().unwrap();
        assert_eq!(tail.len(), 5_000);
        assert_eq!(tail[0] % 2.0, 0.0, "đuôi phải bắt đầu ở mẫu kênh trái (chẵn), không lệch cặp L/R");
        assert_eq!(&tail[4_996..], &[7.0; 4]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::source::YtDlp;
    use std::os::unix::fs::PermissionsExt;

    static LOCK: Mutex<()> = Mutex::new(());
    const LC_ADTS: &[u8] = include_bytes!("../tests/data/lc.adts"); // 2 s AAC-LC 44,1 kHz

    struct Fake {
        dir: PathBuf,
        tool: PathBuf,
        wav: PathBuf,
    }

    fn fake(name: &str, body: &str) -> Fake {
        let dir = std::env::temp_dir().join(format!("fbaudio-pipe-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("audio.adts"), LC_ADTS).unwrap();
        let tool = dir.join("yt-dlp");
        std::fs::write(&tool, format!("#!/bin/sh\nD=\"$(dirname \"$0\")\"\n{body}\n")).unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let wav = dir.join("out.wav");
        Fake { dir, tool, wav }
    }

    fn start(f: &Fake, auto_update: bool) -> Player {
        Player::start(PlayerConfig {
            url: "https://www.facebook.com/x/videos/1".into(),
            latency: LatencyMode::Ultra,
            wav: Some(f.wav.clone()),
            source: Source::YtDlp(YtDlp { path: Some(f.tool.clone()), args: vec![], auto_update, ffmpeg_live: false }),
            live: false, // các test dùng nội dung ngắn đã kết thúc: phải phát đủ
        })
    }

    /// Chạy tới khi kết thúc; trả (trạng thái cuối, số giây âm thanh trong WAV).
    fn play(f: &Fake, auto_update: bool) -> (Status, f64) {
        let p = start(f, auto_update);
        assert!(p.wait_finished(Duration::from_secs(25)), "player không kết thúc");
        let st = p.status();
        drop(p);
        let secs = std::fs::metadata(&f.wav).map(|m| (m.len().saturating_sub(44)) as f64 / (48_000.0 * 4.0)).unwrap_or(0.0);
        (st, secs)
    }

    #[test]
    fn vod_through_fake_ytdlp_plays_in_full_with_title_and_codec() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("vod", r#"printf '%s\n' 'FBAUDIO_TITLE="Phi\u00ean th\u1eed nghi\u1ec7m"' >&2; cat "$D/audio.adts""#);
        let (st, secs) = play(&f, false);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        assert!((2.0..2.6).contains(&secs), "WAV dài {secs:.2} s, cần ≈ 2 s (nội dung đã kết thúc phải phát đủ)");
        assert_eq!(st.title.as_deref(), Some("Phiên thử nghiệm"));
        assert_eq!(st.codec, "AAC-LC 44.1 kHz");
        assert!(st.received_kb >= 16, "đã đọc {} KB", st.received_kb);
    }

    /// Hồi quy từ phép đo: ffmpeg (dashdec live) có thể phát mỗi đoạn hai lần → phải được loại, không phát lặp.
    #[test]
    fn replayed_segment_is_dropped_so_audio_is_not_doubled() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("dup", r#"cat "$D/audio.adts" "$D/audio.adts""#);
        let (st, secs) = play(&f, false);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        assert!((2.0..2.6).contains(&secs), "WAV dài {secs:.2} s: bản lặp chưa bị loại (4 s nghĩa là phát lặp)");
    }

    const NOISE_ADTS: &[u8] = include_bytes!("../tests/data/noise.adts"); // 6 s, 260 AU đều khác nhau

    /// Tách ADTS thành từng khung (gồm cả tiêu đề).
    fn split_adts(b: &[u8]) -> Vec<&[u8]> {
        let (mut out, mut i) = (Vec::new(), 0);
        while i + 7 <= b.len() {
            let len = (((b[i + 3] & 3) as usize) << 11) | ((b[i + 4] as usize) << 3) | ((b[i + 5] as usize) >> 5);
            out.push(&b[i..i + len]);
            i += len;
        }
        out
    }

    /// Chạy ở chế độ LIVE tới khi kết thúc.
    fn play_live(f: &Fake, ffmpeg_live: bool) -> (Status, f64) {
        let p = Player::start(PlayerConfig {
            url: "https://www.facebook.com/x/videos/1".into(),
            latency: LatencyMode::Ultra,
            wav: Some(f.wav.clone()),
            source: Source::YtDlp(YtDlp { path: Some(f.tool.clone()), args: vec![], auto_update: false, ffmpeg_live }),
            live: true,
        });
        assert!(p.wait_finished(Duration::from_secs(40)), "player không kết thúc");
        let st = p.status();
        drop(p);
        let secs = std::fs::metadata(&f.wav).map(|m| (m.len().saturating_sub(44)) as f64 / (48_000.0 * 4.0)).unwrap_or(0.0);
        (st, secs)
    }

    /// Hành vi THẬT của Facebook (đã gặp): yt-dlp chỉ tải các đoạn trong bản manifest chụp lúc tải trang (11 đoạn)
    /// rồi THOÁT, không bám live. App phải chạy lại và nối tiếp: phần chồng lấn giữa các lượt không được phát lặp, và
    /// khi không còn gì mới thì kết thúc.
    #[test]
    fn live_window_that_ends_each_run_is_restarted_and_stitched_without_duplicates() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake(
            "slide",
            r#"n=$(cat "$D/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$D/n"
f="$D/w$n.adts"; [ -f "$f" ] || f="$D/w3.adts"; cat "$f""#,
        );
        let frames = split_adts(NOISE_ADTS);
        assert_eq!(frames.len(), 260);
        // cửa sổ trượt: 0..120, 60..180, 120..260 (lượt sau chồng lấn nửa cửa sổ trước), rồi lặp lại cửa sổ cuối
        for (i, (a, b)) in [(0, 120), (60, 180), (120, 260)].into_iter().enumerate() {
            std::fs::write(f.dir.join(format!("w{}.adts", i + 1)), frames[a..b].concat()).unwrap();
        }
        let (st, secs) = play_live(&f, false);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        let expect = 260.0 * 1024.0 / 44_100.0; // 6,04 s
        // WAV tính theo đồng hồ thật nên có thêm khoảng lặng của đệm sàn chế độ theo đợt (Siêu thấp: 1,0 s) và khe giữa
        // các lượt; nếu phần chồng lấn bị phát lặp thì sẽ ≥ 10 s
        assert!(
            (expect - 0.3..expect + 2.5).contains(&secs),
            "WAV dài {secs:.2} s, cần ≈ {expect:.2} s + đệm sàn (≥ 10 s nghĩa là phần chồng lấn bị phát lặp)"
        );
        let runs: u32 = std::fs::read_to_string(f.dir.join("n")).unwrap().trim().parse().unwrap();
        assert!(runs >= 4, "phải chạy lại nhiều lượt cho tới khi hết mới, chỉ chạy {runs}");
    }

    /// ffmpeg không dùng được (thiếu ffmpeg, hoặc trang không cung cấp URL manifest) → lượt đầu lỗi, các lượt sau phải đi
    /// bằng bộ tải gốc và phát được bình thường.
    #[test]
    fn ffmpeg_strategy_failure_falls_back_to_the_native_downloader() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake(
            "fb",
            r#"echo "$*" >> "$D/calls"
case "$*" in *dash:ffmpeg*) echo "ERROR: ffmpeg not found" >&2; exit 1;; esac
cat "$D/audio.adts""#,
        );
        let (st, secs) = play_live(&f, true);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        let calls = std::fs::read_to_string(f.dir.join("calls")).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert!(lines[0].contains("dash:ffmpeg"), "lượt đầu phải thử ffmpeg: {}", lines[0]);
        assert!(
            lines.len() >= 3 && lines[1..].iter().all(|l| !l.contains("dash:ffmpeg")),
            "các lượt sau phải dùng bộ tải gốc: {lines:?}"
        );
        // 2 s âm thanh + đệm sàn 1,0 s của chế độ theo đợt; nếu nhân đôi thì ≥ 5 s
        assert!((2.0..4.4).contains(&secs), "âm thanh không được nhân đôi hay mất: {secs:.2} s");
    }

    /// ffmpeg TREO (không lỗi, không ra byte nào: ví dụ chờ URL manifest không dùng được) → người gác diệt nó sớm và app
    /// lùi về bộ tải gốc, không đợi 90 s và không dừng cả phiên.
    #[test]
    fn hung_ffmpeg_strategy_is_killed_early_and_falls_back() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake(
            "hang",
            r#"echo "$*" >> "$D/calls"
case "$*" in *dash:ffmpeg*) sleep 30; exit 0;; esac
cat "$D/audio.adts""#,
        );
        let t0 = Instant::now();
        let (st, secs) = play_live(&f, true);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        assert!(t0.elapsed() < Duration::from_secs(20), "đợi quá lâu: {:?}", t0.elapsed());
        let calls = std::fs::read_to_string(f.dir.join("calls")).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        assert!(lines[0].contains("dash:ffmpeg") && lines[1..].iter().all(|l| !l.contains("dash:ffmpeg")), "{lines:?}");
        // WAV tính theo đồng hồ thật nên có cả ~1,5 s im lặng lúc đợi người gác diệt ffmpeg treo; nếu âm thanh bị nhân
        // đôi thì sẽ ≥ 5,5 s
        assert!((2.0..5.0).contains(&secs), "phải phát được sau khi lùi, không nhân đôi: {secs:.2} s");
    }

    /// Phiên live thật sự đã hết: công cụ cứ báo lỗi sau khi đã phát được một lượt → báo đúng lỗi đó (sau khi phát nốt).
    #[test]
    fn live_tool_that_keeps_failing_after_playing_ends_with_its_error() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake(
            "gone",
            r#"n=$(cat "$D/n" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$D/n"
if [ "$n" = 1 ]; then cat "$D/audio.adts"; else echo "ERROR: [facebook] 1: Video unavailable" >&2; exit 1; fi"#,
        );
        let (st, _) = play_live(&f, false);
        assert_eq!((st.state, st.message.as_str()), (State::Error, "[facebook] 1: Video unavailable"));
    }

    /// yt-dlp bộ tải gốc để tệp tạm `-Frag<N>` ở thư mục làm việc: phải là thư mục riêng và bị xoá khi xong.
    #[test]
    fn ytdlp_runs_in_a_private_dir_that_is_removed_afterwards() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("cwd", r#"pwd > "$D/cwd_used"; echo junk > ./--Frag7; cat "$D/audio.adts""#);
        let (st, _) = play(&f, false);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        let used = std::fs::read_to_string(f.dir.join("cwd_used")).unwrap();
        let used = std::path::Path::new(used.trim());
        assert_ne!(used, std::env::current_dir().unwrap().as_path(), "yt-dlp chạy ở thư mục làm việc của người dùng");
        assert!(used.to_string_lossy().contains("fbaudio-"), "thư mục riêng phải có tên fbaudio-…: {used:?}");
        assert!(!used.exists(), "thư mục tạm chưa được dọn: {used:?}");
    }

    #[test]
    fn tool_failure_surfaces_the_ytdlp_error_line() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("err", r#"echo "WARNING: x" >&2; echo "ERROR: [facebook] 1: Video unavailable" >&2; exit 1"#);
        let (st, _) = play(&f, false);
        assert_eq!((st.state, st.message.as_str()), (State::Error, "[facebook] 1: Video unavailable"));
    }

    #[test]
    fn missing_ffmpeg_and_unknown_format_give_actionable_messages() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("ff", r#"echo "ERROR: ffmpeg exited with code 234" >&2; exit 1"#);
        let (st, _) = play(&f, false);
        assert!(st.message.contains("winget install Gyan.FFmpeg") && st.message.contains("ffmpeg exited"), "{}", st.message);

        let f = fake("fmt", r#"echo "<html>da bi chan</html>""#);
        let (st, _) = play(&f, false);
        assert_eq!(st.state, State::Error);
        assert!(st.message.contains("Không nhận ra định dạng"), "{}", st.message);
    }

    #[test]
    fn missing_ytdlp_is_reported_as_not_installed() {
        let p = Player::start(PlayerConfig {
            url: "https://fb.watch/x".into(),
            latency: LatencyMode::Ultra,
            wav: Some(std::env::temp_dir().join(format!("fbaudio-none-{}.wav", std::process::id()))),
            source: Source::YtDlp(YtDlp { path: Some("/không/tồn/tại/yt-dlp".into()), args: vec![], auto_update: false, ffmpeg_live: true }),
            live: true,
        });
        assert!(p.wait_finished(Duration::from_secs(10)));
        let st = p.status();
        assert_eq!(st.state, State::Error);
        assert!(st.message.contains("chưa được cài"), "{}", st.message);
    }

    /// Nút Dừng phải diệt CẢ cây tiến trình (yt-dlp và ffmpeg con của nó), không chỉ tiến trình đầu.
    #[test]
    fn stop_kills_the_whole_process_tree_and_returns_promptly() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("stop", r#"echo $$ > "$D/pid_parent"; sleep 60 & echo $! > "$D/pid_child"; wait"#);
        let p = start(&f, false);
        let t0 = Instant::now();
        while !f.dir.join("pid_child").exists() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = |n: &str| std::fs::read_to_string(f.dir.join(n)).unwrap().trim().to_string();
        let (parent, child) = (pid("pid_parent"), pid("pid_child"));
        let alive = |p: &str| {
            // zombie (đã chết nhưng chưa thu dọn) không tính là còn sống
            std::fs::read_to_string(format!("/proc/{p}/stat")).map_or(false, |s| !s.contains(") Z"))
        };
        assert!(alive(&parent) && alive(&child), "tiến trình giả phải đang chạy trước khi dừng");

        let t1 = Instant::now();
        p.stop();
        assert!(p.wait_finished(Duration::from_secs(5)), "Dừng phải trả về nhanh dù read đang chặn");
        assert!(t1.elapsed() < Duration::from_secs(3), "dừng mất {:?}", t1.elapsed());
        assert_eq!(p.status().state, State::Idle);
        for _ in 0..50 {
            if !alive(&parent) && !alive(&child) {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(!alive(&parent), "tiến trình cha còn sống sau Dừng");
        assert!(!alive(&child), "tiến trình CON còn sống sau Dừng (yt-dlp thật sẽ để lại ffmpeg mồ côi)");
    }

    /// Facebook vừa đổi → yt-dlp báo "Cannot parse data" → app chạy `-U` rồi thử lại và phát được.
    /// (Test DUY NHẤT dùng auto_update, vì cờ "một lần mỗi lần chạy" là biến toàn cục.)
    #[test]
    fn extractor_break_triggers_one_update_then_retry() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = fake("upd", r#"if [ "$1" = "-U" ]; then touch "$D/updated"; echo "Updated yt-dlp"; exit 0; fi
if [ -f "$D/updated" ]; then cat "$D/audio.adts"; exit 0; fi
echo "ERROR: [facebook] 1: Cannot parse data; please report this issue on https://github.com/yt-dlp/yt-dlp/issues" >&2; exit 1"#);
        let (st, secs) = play(&f, true);
        assert_eq!(st.state, State::Ended, "{}", st.message);
        assert!(f.dir.join("updated").exists(), "chưa chạy yt-dlp -U");
        assert!((2.0..2.6).contains(&secs), "sau khi cập nhật phải phát được: {secs:.2} s");
    }
}
