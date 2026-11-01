//! Điều phối: mở nguồn byte thô (yt-dlp…) → bóc container → loại AU lặp → giải mã AAC → loa.
//!
//! Ứng dụng KHÔNG tự tải hay phân tích manifest: yt-dlp lo từ link Facebook tới luồng âm thanh và ghi ra
//! stdout; phía ta chỉ liên tục đọc dòng byte đó. Phần ta vẫn tự làm là phần quyết định độ trễ nghe được:
//! cắt bỏ phần dồn cũ để bám live, đệm chống giật, điều tốc.

use crate::decode::AacDec;
use crate::demux::{Dedupe, Demux, Ev};
use crate::fmp4::TrackInfo;
use crate::output::{Meter, Output, SinkKind, Tuning};
use crate::source::{self, looks_like_extractor_break, Killer, NotInstalled, Opened, Source};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
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
    fn start(killer: Killer) -> Self {
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
            let limit = if first.load(Relaxed) { STALL_LIMIT_MS } else { FIRST_BYTE_LIMIT_MS };
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
                "Không nhận được dữ liệu sau 90 giây — công cụ có thể bị treo hoặc video không phát được."
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
    Done,
    /// Công cụ kết thúc mà chưa phát ra âm thanh nào (kèm thông báo lỗi của nó).
    NoAudio(String),
}

/// Chỉ cho tự cập nhật yt-dlp một lần mỗi lần chạy ứng dụng.
static UPDATED: AtomicBool = AtomicBool::new(false);

fn run(sh: &Shared, cfg: &PlayerConfig) -> Result<()> {
    let kind = match &cfg.wav {
        Some(p) => SinkKind::Wav(p.clone()),
        None => SinkKind::Device,
    };
    // Mở thiết bị âm thanh ngay, song song với lúc yt-dlp khởi động: không thêm độ trễ vào tiếng đầu tiên.
    let mut out = Output::new(cfg.latency.tuning(), kind, sh.stop.clone())?;
    *sh.meter.lock().unwrap() = Some(out.meter.clone());

    let mut updated = false;
    loop {
        sh.set(State::Resolving, waiting_message(&cfg.source));
        let opened = source::open(&cfg.source, &cfg.url, cfg.live).map_err(|e| {
            if e.downcast_ref::<NotInstalled>().is_some() && cfg!(target_os = "android") {
                anyhow!("{e}. Android không chạy được yt-dlp: nhập địa chỉ máy chủ yt-dlp ở mục Nâng cao.")
            } else {
                e
            }
        })?;
        *sh.killer.lock().unwrap() = opened.killer.clone();
        if sh.stopped() {
            opened.killer.kill(); // bấm Dừng ngay lúc đang mở
            return Ok(());
        }
        match pump(sh, opened, &mut out, cfg.live)? {
            Outcome::Done => {
                out.drain(&sh.stop);
                return Ok(());
            }
            Outcome::NoAudio(msg) => {
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
fn report(sh: &Shared, info: &source::Info, total: u64, au_bytes: u64, au_frames: u64, core_rate: u32) {
    let secs = au_frames as f64 * 1024.0 / core_rate.max(1) as f64;
    sh.update(|s| {
        s.received_kb = total / 1024;
        if secs > 1.0 {
            s.bitrate_kbps = (au_bytes as f64 * 8.0 / secs / 1000.0).round() as u32;
        }
        if s.title.is_none() {
            s.title = info.title();
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

fn pump(sh: &Shared, opened: Opened, out: &mut Output, live: bool) -> Result<Outcome> {
    let Opened { stream, info, killer } = opened;
    let mon = Monitor::start(killer);
    let rx = spawn_reader(stream);

    let mut demux = Demux::new();
    let mut dedupe = Dedupe::new();
    let mut dec: Option<AacDec> = None;
    let mut pcm: Vec<f32> = Vec::new();
    let mut gate: Option<Gate> = live.then(Gate::new); // chỉ có với luồng live
    let mut playing = false;
    out.set_live(live);
    log::info!("chế độ {}", if live { "live: bám live, bỏ phần lịch sử/dồn" } else { "phát lại: phát đủ từ đầu" });
    let (mut total, mut au_bytes, mut au_frames, mut core_rate) = (0u64, 0u64, 0u64, 48_000u32);
    let mut last_report = Instant::now();
    let mut eof: Option<Result<()>> = None;

    loop {
        if sh.stopped() {
            return Ok(Outcome::Done);
        }
        let data = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Msg::Data(d)) => d,
            Ok(Msg::Eof(r)) => {
                eof = Some(r);
                break;
            }
            Ok(Msg::Err(e)) => {
                if sh.stopped() {
                    return Ok(Outcome::Done);
                }
                if let Some(m) = mon.fired() {
                    bail!("{m}");
                }
                return Err(anyhow!(e).context("đọc luồng thất bại"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // công cụ đang im lặng: nếu đợt giao đầu tiên đã kết thúc thì nhả phần đang giữ
                if let Some(g) = gate.as_mut() {
                    if g.tick(Instant::now()) && !g.held.is_empty() {
                        let (rate, ch) = g.held_fmt;
                        out.push(&g.held, rate, ch);
                        g.held.clear();
                        if !playing {
                            playing = true;
                            sh.set(State::Playing, "Đang phát");
                        }
                    }
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        mon.touch();
        total += data.len() as u64;

        if total == data.len() as u64 {
            sh.update(|s| s.title = s.title.take().or_else(|| info.title()));
            if live {
                sh.set(State::Connecting, "Đã nhận dữ liệu, đang vào live");
            }
        }

        demux.feed(&data, &mut |ev| {
            match ev {
                Ev::Config(t) => {
                    // HE-AAC trong ADTS báo "ngầm" (tiêu đề ghi LC ở tần số lõi): tần số lõi thấp là dấu hiệu.
                    let he_hint = t.is_he() || (t.ext_rate.is_none() && matches!(t.sample_rate, 24_000 | 22_050 | 16_000));
                    if dec.as_ref().map_or(true, |d| d.asc != t.asc) {
                        let d = AacDec::new(t, he_hint)?;
                        core_rate = t.sample_rate;
                        sh.update(|s| {
                            s.codec = codec_label(t, &d);
                            if d.core_only {
                                s.note = Some(HE_NOTE.into());
                            }
                        });
                        dec = Some(d);
                    }
                }
                Ev::Frame(f) => {
                    if dedupe.is_dup(f) {
                        return Ok(());
                    }
                    au_bytes += f.len() as u64;
                    au_frames += 1;
                    if let Some(d) = dec.as_mut() {
                        d.decode(f, &mut pcm)?;
                    }
                }
            }
            Ok(())
        })?;

        if !pcm.is_empty() {
            let (rate, ch) = dec.as_ref().map_or((48_000, 2), |d| (d.rate, d.channels));
            let ms = pcm.len() as f64 / ch.max(1) as f64 / rate.max(1) as f64 * 1000.0;
            let verdict = match gate.as_mut() {
                Some(g) => g.feed(Instant::now(), ms),
                None => Verdict::PushAll,
            };
            match verdict {
                Verdict::PushAll => {
                    if let Some(g) = gate.as_mut().filter(|g| !g.held.is_empty()) {
                        let (hr, hc) = g.held_fmt;
                        out.push(&g.held, hr, hc);
                        g.held.clear();
                    }
                    out.push(&pcm, rate, ch);
                    if !playing {
                        playing = true;
                        sh.set(State::Playing, "Đang phát");
                    }
                }
                Verdict::Hold => {
                    let g = gate.as_mut().expect("Hold chỉ có khi có cổng");
                    g.held_fmt = (rate, ch);
                    g.held.extend_from_slice(&pcm);
                }
                Verdict::DiscardAll => {
                    if let Some(g) = gate.as_mut() {
                        g.held.clear();
                    }
                    if !playing {
                        sh.set(State::Connecting, "Đang bỏ qua phần lịch sử để vào live");
                    }
                    log::debug!("đang dồn lịch sử: bỏ im lặng tới khi về sát live");
                }
                Verdict::Discard => {}
            }
            pcm.clear();
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            report(sh, &info, total, au_bytes, au_frames, core_rate);
        }
    }

    if sh.stopped() {
        return Ok(Outcome::Done);
    }
    if let Some(m) = mon.fired() {
        bail!("{m}");
    }
    // luồng ngắn kết thúc khi cổng còn đang giữ → nhả nốt
    if let Some(g) = gate.as_mut().filter(|g| !g.held.is_empty() && !g.closed) {
        let (rate, ch) = g.held_fmt;
        out.push(&g.held, rate, ch);
        g.held.clear();
        playing = true;
    }
    report(sh, &info, total, au_bytes, au_frames, core_rate); // lần báo cuối (luồng ngắn kết thúc trước chu kỳ 1 s)
    if dedupe.dropped > 0 {
        log::info!("đã loại {} access unit bị phát lặp (ffmpeg phát lặp đoạn live)", dedupe.dropped);
    }
    match (eof.unwrap_or(Ok(())), playing) {
        (Ok(()), true) => Ok(Outcome::Done),
        (Ok(()), false) => Ok(Outcome::NoAudio("Công cụ kết thúc mà không xuất ra âm thanh nào.".into())),
        (Err(e), true) => Err(e), // đang phát thì công cụ chết giữa chừng: báo lỗi của nó
        (Err(e), false) => Ok(Outcome::NoAudio(format!("{e:#}"))),
    }
}

// ---------------------------------------------------------------- cổng bỏ phần dồn lúc vào live

/// Im lặng ngần này (giữa hai lần dữ liệu về) = một "đợt giao" đã kết thúc: công cụ đang chờ dữ liệu live mới.
const GATE_IDLE: Duration = Duration::from_millis(150);
/// Lúc bắt đầu: âm thanh về nhiều hơn thời gian thực chừng này (ms) trong cùng một đợt giao → đang dồn lịch sử.
const GATE_START_SURPLUS_MS: f64 = 4_500.0;
/// Về sau: chỉ các đợt dồn rất lớn (sau khi mạng nghẽn dài) mới bị bỏ hẳn; mức vừa phải do `Output` tự cắt.
const GATE_BURST_SURPLUS_MS: f64 = 12_000.0;

#[derive(Debug, PartialEq)]
enum Verdict {
    /// Phát (nhả cả phần đang giữ trước).
    PushAll,
    /// Giữ lại, chưa biết có phải dồn lịch sử không (chỉ ở đợt giao đầu tiên).
    Hold,
    /// Bỏ phần này.
    Discard,
    /// Đang dồn lịch sử: bỏ phần này và cả phần đang giữ.
    DiscardAll,
}

/// Vào live giữa chừng một luồng có lịch sử (cửa sổ DVR), công cụ như yt-dlp+ffmpeg có thể đổ cả lịch sử ra nhanh
/// gấp nhiều lần thời gian thực rồi mới tới mép live. Nếu phát hết thì nghe thấy vài giây âm thanh CŨ, sau đó là
/// khoảng cắt vụn. Cổng này ở đợt giao đầu tiên giữ PCM lại đợi đợt giao kết thúc: kết thúc sớm (≤ vài giây
/// dư) = luồng thật sự đang live → phát; còn tiếp tục đổ dồn → bỏ im lặng tới khi luồng chững lại ở mép live.
struct Gate {
    decided: bool,
    closed: bool,
    started: bool,
    last: Option<Instant>,
    ep_start: Instant,
    ep_media: f64,
    held: Vec<f32>,
    held_fmt: (u32, usize),
}

impl Gate {
    fn new() -> Self {
        Gate {
            decided: false,
            closed: false,
            started: false,
            last: None,
            ep_start: Instant::now(),
            ep_media: 0.0,
            held: Vec::new(),
            held_fmt: (48_000, 2),
        }
    }

    /// Có `ms` âm thanh (đã giải mã, đã lọc lặp) vừa về lúc `now`.
    fn feed(&mut self, now: Instant, ms: f64) -> Verdict {
        let new_episode = self.last.map_or(true, |l| now.duration_since(l) >= GATE_IDLE);
        self.last = Some(now);
        if new_episode {
            // đợt giao trước (nếu có) đã kết thúc mà chưa vượt ngưỡng → đó là live thật
            if !self.decided && self.started {
                self.decided = true;
            }
            self.ep_start = now;
            self.ep_media = 0.0;
            self.closed = false;
        }
        self.started = true;
        self.ep_media += ms;
        let surplus = self.ep_media - now.duration_since(self.ep_start).as_secs_f64() * 1000.0;
        if !self.decided {
            if surplus > GATE_START_SURPLUS_MS {
                self.decided = true;
                self.closed = true;
                return Verdict::DiscardAll;
            }
            return Verdict::Hold;
        }
        if surplus > GATE_BURST_SURPLUS_MS {
            self.closed = true;
        }
        if self.closed {
            Verdict::Discard
        } else {
            Verdict::PushAll
        }
    }

    /// Gọi định kỳ khi không có dữ liệu mới. `true` = đợt giao đầu tiên vừa kết thúc → nhả phần đang giữ.
    fn tick(&mut self, now: Instant) -> bool {
        if !self.decided && self.started && self.last.is_some_and(|l| now.duration_since(l) >= GATE_IDLE) {
            self.decided = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn realtime_stream_with_2s_bursts_is_released_after_idle_and_never_discarded() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        // đoạn đầu 2 s về trong 40 ms (3 lần đọc), rồi im
        assert_eq!(g.feed(at(t0, 0), 700.0), Verdict::Hold);
        assert_eq!(g.feed(at(t0, 20), 700.0), Verdict::Hold);
        assert_eq!(g.feed(at(t0, 40), 600.0), Verdict::Hold);
        assert!(!g.tick(at(t0, 100)), "chưa đủ im lặng");
        assert!(g.tick(at(t0, 200)), "im ≥150 ms → nhả phần đang giữ");
        // các đợt sau: phát ngay, kể cả đợt 4 s
        for k in 1..6u64 {
            assert_eq!(g.feed(at(t0, 2000 * k), 1000.0), Verdict::PushAll);
            assert_eq!(g.feed(at(t0, 2000 * k + 30), 1000.0), Verdict::PushAll);
        }
    }

    #[test]
    fn history_flood_is_discarded_silently_then_live_resumes_after_the_first_idle() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        // lịch sử 48 s đổ ra ở tốc độ 8×: mỗi 100 ms về 800 ms âm thanh
        let mut verdicts = Vec::new();
        for i in 0..60u64 {
            verdicts.push(g.feed(at(t0, i * 100), 800.0));
        }
        assert!(verdicts[..5].iter().all(|v| *v == Verdict::Hold), "vài lần đầu còn giữ: {verdicts:?}");
        assert_eq!(verdicts.iter().filter(|v| **v == Verdict::DiscardAll).count(), 1);
        assert!(verdicts.iter().skip(8).all(|v| *v == Verdict::Discard), "sau đó bỏ hết: {verdicts:?}");
        // lịch sử hết, luồng chững lại ở mép live (đợt kế tiếp sau 1,9 s im lặng) → phát lại
        assert_eq!(g.feed(at(t0, 5900 + 1900), 2000.0), Verdict::PushAll);
        assert_eq!(g.feed(at(t0, 5900 + 1900 + 20), 2000.0), Verdict::PushAll);
    }

    #[test]
    fn a_huge_burst_after_startup_is_dropped_but_moderate_ones_are_kept() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        assert_eq!(g.feed(at(t0, 0), 2000.0), Verdict::Hold);
        assert!(g.tick(at(t0, 300)));
        // đợt dồn vừa phải sau khi mạng chậm (6 s về cùng lúc): giữ, để Output tự cắt
        assert_eq!(g.feed(at(t0, 5000), 3000.0), Verdict::PushAll);
        assert_eq!(g.feed(at(t0, 5010), 3000.0), Verdict::PushAll);
        // đợt khổng lồ (30 s ào về): từ khi vượt ngưỡng thì bỏ hết phần còn lại của đợt
        let mut t = 20_000;
        let mut dropped = 0;
        for _ in 0..12 {
            if g.feed(at(t0, t), 3000.0) == Verdict::Discard {
                dropped += 1;
            }
            t += 10;
        }
        assert!(dropped >= 6, "phải bỏ phần lớn đợt khổng lồ, bỏ {dropped}");
    }

    #[test]
    fn short_stream_finishing_inside_the_hold_is_not_lost() {
        let t0 = Instant::now();
        let mut g = Gate::new();
        assert_eq!(g.feed(at(t0, 0), 1500.0), Verdict::Hold);
        // luồng kết thúc trước khi tick: `pump` nhả phần đang giữ ở nhánh EOF vì cổng chưa đóng
        assert!(!g.closed);
    }
}

/// Thử cả đường ống với "yt-dlp giả" (shell script, chỉ Unix): script in `FBAUDIO_*` ra stderr và ghi dữ liệu ra
/// stdout y như yt-dlp thật. Tạo file thực thi rồi chạy ngay nên xếp hàng qua một khoá (tránh "Text file busy").
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
            source: Source::YtDlp(YtDlp { path: Some(f.tool.clone()), args: vec![], auto_update }),
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
        let f = fake("vod", r#"echo "FBAUDIO_TITLE=Phiên thử nghiệm" >&2; cat "$D/audio.adts""#);
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
            source: Source::YtDlp(YtDlp { path: Some("/không/tồn/tại/yt-dlp".into()), args: vec![], auto_update: false }),
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
