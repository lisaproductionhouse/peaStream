//! Điều phối toàn bộ: tìm manifest → bám live edge → tải đoạn audio → fMP4 → AAC → loa.

use crate::dash::{Cursor, Mpd, SegSource};
use crate::decode::AacDec;
use crate::extract::{self, Found, ResolverConfig, Source};
use crate::fmp4::{Event, Fmp4Parser};
use crate::http::{status_of, Http};
use crate::output::{Meter, Output, SinkKind, Tuning};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Link Facebook Live, hoặc link .mpd trực tiếp.
    pub url: String,
    pub latency: LatencyMode,
    /// Nếu có: ghi ra WAV thay vì phát ra loa (dùng để kiểm thử).
    pub wav: Option<PathBuf>,
    /// Cách lấy manifest từ link Facebook (yt-dlp, máy chủ yt-dlp, bộ cào tích hợp).
    pub resolver: ResolverConfig,
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
    pub segments: u64,
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
            segments: 0,
            note: None,
        }
    }
}

struct Shared {
    stop: Arc<AtomicBool>,
    st: Mutex<Status>,
    meter: Mutex<Option<Arc<Meter>>>,
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
    fn recover(&self) {
        let mut s = self.st.lock().unwrap();
        if s.state == State::Reconnecting || s.state == State::Connecting {
            s.state = State::Playing;
            s.message = "Đang phát".into();
        }
    }
}

pub struct Player {
    sh: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Player {
    /// Khởi động luồng nền; trả về ngay. Theo dõi tiến trình bằng `status()`.
    pub fn start(cfg: PlayerConfig) -> Player {
        // Trạng thái ban đầu là "đang tìm" (không phải Idle): giao diện có thể hỏi status ngay sau
        // `start`, thấy Idle sẽ tưởng lõi đã tự dừng và tắt dịch vụ nền vừa bật.
        let mut first = Status::idle();
        first.state = State::Resolving;
        first.message = "Đang tìm luồng âm thanh".into();
        let sh = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(false)),
            st: Mutex::new(first),
            meter: Mutex::new(None),
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

    /// Dừng ngay (âm thanh tắt tức thì); luồng nền tự thoát ngay sau đó.
    pub fn stop(&self) {
        self.sh.stop.store(true, Relaxed);
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

    /// Còn đang tìm/kết nối/phát? (dùng cho dịch vụ nền Android tự dọn khi không còn phát)
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

// ---------------------------------------------------------------- đường ống xử lý

const HE_NOTE: &str =
    "Luồng HE-AAC: đang phát phần lõi, âm thanh có thể kém sáng hơn bản gốc (thiếu dải cao).";

struct Pipe<'a> {
    parser: Fmp4Parser,
    dec: Option<AacDec>,
    pcm: Vec<f32>,
    out: Output,
    sh: &'a Shared,
    /// Manifest khai báo HE-AAC (kể cả khi ASC báo hiệu ngầm).
    he_hint: bool,
}

impl Pipe<'_> {
    /// Nạp byte vào; trả `true` nếu đã đẩy PCM mới ra hàng đợi phát.
    fn feed(&mut self, data: &[u8]) -> Result<bool> {
        let Pipe { parser, dec, pcm, sh, he_hint, .. } = self;
        parser.feed(data, &mut |ev| {
            match ev {
                Event::Init(info) => {
                    if dec.as_ref().map_or(true, |d| d.asc != info.asc) {
                        let d = AacDec::new(info, *he_hint)?;
                        if d.core_only {
                            sh.update(|s| s.note = Some(HE_NOTE.into()));
                        }
                        *dec = Some(d);
                    }
                }
                Event::Sample { data, .. } => {
                    if let Some(d) = dec.as_mut() {
                        d.decode(data, pcm)?;
                    }
                }
            }
            Ok(())
        })?;
        if pcm.is_empty() {
            return Ok(false);
        }
        let (rate, ch) = dec.as_ref().map_or((48_000, 2), |d| (d.rate, d.channels));
        self.out.push(&self.pcm, rate, ch);
        self.pcm.clear();
        Ok(true)
    }
}

fn epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Tải + phân tích manifest. Trả thêm độ lệch đồng hồ máy chủ (từ header Date), nếu có.
/// Nguồn vừa có URL vừa có bản nhúng sẵn: ưu tiên URL (luôn mới), hỏng thì dùng bản nhúng.
fn load(http: &Http, src: &Source) -> Result<(Mpd, Option<f64>)> {
    if let Some(u) = &src.url {
        let fetched = http.get(u, 4 << 20).and_then(|(body, date)| {
            let off = date
                .and_then(|d| d.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64() - epoch());
            Ok((crate::dash::parse(&String::from_utf8_lossy(&body), u)?, off))
        });
        return match (fetched, &src.xml) {
            (Ok(r), _) => Ok(r),
            (Err(e), Some(x)) => {
                log::warn!("không tải được manifest ({e:#}) → dùng bản nhúng sẵn");
                Ok((crate::dash::parse(x, &src.base)?, None))
            }
            (Err(e), None) => Err(e),
        };
    }
    match &src.xml {
        Some(x) => Ok((crate::dash::parse(x, &src.base)?, None)),
        None => bail!("nguồn manifest rỗng"),
    }
}

/// Thử lần lượt các manifest ứng viên, lấy cái đầu tiên có luồng audio dùng được.
fn open_manifest(http: &Http, found: &Found) -> Result<(Source, Mpd, f64)> {
    let mut last_err = None;
    for src in &found.sources {
        match load(http, src) {
            Ok((mpd, off)) => return Ok((src.clone(), mpd, off.unwrap_or(0.0))),
            Err(e) => {
                log::warn!("bỏ qua manifest: {e:#}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("Không có manifest nào dùng được")))
}

fn run(sh: &Shared, cfg: &PlayerConfig) -> Result<()> {
    let base_http = Http::new();
    sh.set(State::Resolving, "Đang tìm luồng âm thanh");

    // Lấy link → mở manifest. Nếu kết quả lấy từ bộ nhớ đệm mà không dùng được nữa (hết hạn, phiên live
    // đã đổi) thì quên đi và lấy lại một lần.
    let mut fresh = false;
    let (found, http, src, mut mpd, mut clock) = loop {
        let progress = |m: &str| sh.set(State::Resolving, m);
        let (found, cached) = extract::resolve(&base_http, &cfg.url, &cfg.resolver, &sh.stop, &progress, fresh)?;
        if sh.stopped() {
            return Ok(());
        }
        // header do yt-dlp yêu cầu (Facebook: facebookexternalhit/1.1) dùng cho mọi request về sau
        let http = base_http.with_headers(&found.headers);
        match open_manifest(&http, &found) {
            Ok((src, mpd, off)) => break (found, http, src, mpd, off),
            Err(e) if cached => {
                log::info!("kết quả đã nhớ không còn dùng được ({e:#}) → lấy lại");
                extract::forget(&cfg.url);
                fresh = true;
            }
            Err(e) => return Err(e),
        }
    };
    sh.update(|s| s.title = found.title.clone());

    let rep = mpd.audio.clone();
    sh.update(|s| {
        s.bitrate_kbps = (rep.bandwidth / 1000) as u32;
        s.codec = rep.codecs.clone();
    });
    sh.set(State::Connecting, format!("Đã lấy link bằng {}", found.via));
    log::info!(
        "audio: id={} {} kbps {} live={}",
        rep.id,
        rep.bandwidth / 1000,
        rep.codecs,
        mpd.live
    );

    let kind = match &cfg.wav {
        Some(p) => SinkKind::Wav(p.clone()),
        None => SinkKind::Device,
    };
    let mut out = Output::new(cfg.latency.tuning(), kind, sh.stop.clone())?;
    // Nội dung đã kết thúc (VOD / file liền): phát đủ, không đuổi live.
    out.set_live(mpd.live && !matches!(rep.source, SegSource::Single(_)));
    *sh.meter.lock().unwrap() = Some(out.meter.clone());
    let he_hint = rep.codecs.starts_with("mp4a.40.5") || rep.codecs.starts_with("mp4a.40.29");
    let mut pipe = Pipe { parser: Fmp4Parser::new(), dec: None, pcm: Vec::new(), out, sh, he_hint };

    if let Some(u) = &rep.init_url {
        let (b, _) = http.get(u, 1 << 20)?;
        pipe.feed(&b)?;
    }
    let mut init_url = rep.init_url.clone();

    if let SegSource::Single(url) = &rep.source {
        // File fMP4 liền (nội dung đã kết thúc): phát thẳng từ đầu.
        let mut playing = false;
        http.stream(url, |c| {
            if pipe.feed(c)? && !playing {
                playing = true;
                sh.set(State::Playing, "Đang phát");
            }
            Ok(!sh.stopped())
        })?;
        pipe.out.drain(&sh.stop);
        return Ok(());
    }

    segment_loop(sh, &http, &mut pipe, &src, &mut mpd, &mut clock, &mut init_url)?;
    pipe.out.drain(&sh.stop);
    Ok(())
}

/// Vòng lặp chính: tải liên tiếp các đoạn audio, luôn bám đoạn mới nhất.
fn segment_loop(
    sh: &Shared,
    http: &Http,
    pipe: &mut Pipe<'_>,
    src: &Source,
    mpd: &mut Mpd,
    clock: &mut f64,
    init_url: &mut Option<String>,
) -> Result<()> {
    let live = mpd.live;
    // Vào thẳng đoạn mới nhất đã công bố (không lùi 3 đoạn như player thông thường).
    let mut cur = if live { mpd.live_edge(epoch() + *clock) } else { mpd.first() };
    let mut last_ok = Instant::now();
    let mut last_refresh = Instant::now();
    let mut miss_since: Option<Instant> = None;
    let mut played_any = false;
    let mut playing = false;
    // Manifest đã chuyển sang `static` (hoặc biến mất): phiên live đã kết thúc → hết đoạn thì dừng.
    let mut source_ended = false;
    let mut not_before = Instant::now();

    while !sh.stopped() {
        let now = Instant::now();
        if now < not_before {
            std::thread::sleep((not_before - now).min(Duration::from_millis(20)));
            continue;
        }

        let url = mpd.audio.url(cur);
        let t0 = Instant::now();
        pipe.parser.reset_stream();
        let mut got = 0usize;
        let res = http.stream(&url, |c| {
            got += c.len();
            if pipe.feed(c)? && !playing {
                playing = true;
                sh.set(State::Playing, "Đang phát");
            }
            Ok(!sh.stopped())
        });
        let seg_dur = mpd.audio.dur_secs(cur);

        match res {
            Ok(()) => {
                if sh.stopped() {
                    break;
                }
                let had_miss = miss_since.take().is_some();
                last_ok = Instant::now();
                played_any = true;
                sh.recover();
                sh.update(|s| s.segments += 1);
                // Đã thấy chuyển 404→200: t0 ≈ lúc đoạn được xuất bản → hỏi đoạn kế ngay trước lúc
                // nó xuất hiện (ít request nhất mà vẫn nhận sớm nhất).
                // Thành công ngay lần hỏi đầu = đoạn đã có sẵn: mới vào live, hoặc đang tụt lại sau
                // khi mạng/nguồn khựng → hỏi tiếp NGAY (tầng đầu ra tự cắt nhảy tới live); nếu
                // đoạn kế chưa có thì thăm dò dày để bắt lại đúng nhịp xuất bản.
                let wait = if had_miss { (seg_dur - 0.12).max(0.0) } else { 0.0 };
                not_before = t0 + Duration::from_secs_f64(wait);
                cur = mpd.audio.next(cur);
            }
            Err(e) => {
                let code = status_of(&e);
                let not_found = matches!(code, Some(404) | Some(410));
                if got > 0 {
                    // đã nhận dở đoạn này: bỏ phần còn lại, sang đoạn kế (tránh phát lặp)
                    log::warn!("đoạn bị ngắt giữa chừng: {e:#}");
                    cur = mpd.audio.next(cur);
                    last_ok = Instant::now();
                    continue;
                }
                if not_found && (!live || source_ended) {
                    if played_any {
                        return Ok(());
                    }
                    bail!("Không tải được đoạn âm thanh đầu tiên (HTTP {})", code.unwrap_or(0));
                }
                let missing_for = miss_since.get_or_insert_with(Instant::now).elapsed();
                let wait = if !not_found {
                    Duration::from_millis(400)
                } else if missing_for < Duration::from_secs(3) {
                    Duration::from_millis(50)
                } else if missing_for < Duration::from_secs(10) {
                    Duration::from_millis(250)
                } else {
                    Duration::from_secs(1)
                };
                not_before = Instant::now() + wait;
                if !not_found {
                    log::warn!("lỗi tải đoạn: {e:#}");
                    sh.set(State::Reconnecting, "Mạng chập chờn, đang thử lại");
                }
                // Mất đoạn lâu hoặc lỗi lạ (403 hết hạn chữ ký…) → làm mới manifest, nhảy tới live.
                let stale = Duration::from_secs_f64((seg_dur * 2.0).max(3.0));
                if (!not_found || missing_for > stale) && last_refresh.elapsed() > Duration::from_secs(2) {
                    last_refresh = Instant::now();
                    source_ended |= refresh(http, pipe, src, mpd, clock, init_url, &mut cur);
                }
                if last_ok.elapsed() > Duration::from_secs(45) {
                    bail!("Mất tín hiệu hơn 45 giây — có thể phiên live đã kết thúc.");
                }
            }
        }
    }
    Ok(())
}

fn refresh(
    http: &Http,
    pipe: &mut Pipe<'_>,
    src: &Source,
    mpd: &mut Mpd,
    clock: &mut f64,
    init_url: &mut Option<String>,
    cur: &mut Cursor,
) -> bool {
    if src.url.is_none() {
        return false; // manifest nhúng sẵn trong trang: không làm mới được
    }
    match load(http, src) {
        Ok((new, off)) => {
            if let Some(o) = off {
                *clock = o;
            }
            if new.audio.init_url != *init_url {
                if let Some(u) = &new.audio.init_url {
                    if let Ok((b, _)) = http.get(u, 1 << 20) {
                        let _ = pipe.feed(&b);
                    }
                }
                *init_url = new.audio.init_url.clone();
            }
            let ended = !new.live;
            if ended {
                log::info!("manifest đã chuyển sang static: phiên live kết thúc");
            }
            *mpd = new;
            let edge = mpd.live_edge(epoch() + *clock);
            if edge.number > cur.number {
                log::info!("nhảy tới live edge #{}", edge.number);
                *cur = edge;
            }
            ended
        }
        Err(e) => {
            log::warn!("không làm mới được manifest: {e:#}");
            // manifest không còn nữa (404/410) cộng với đoạn cũng hết → coi như đã kết thúc
            matches!(status_of(&e), Some(404) | Some(410))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hồi quy: ngay sau `start` trạng thái không bao giờ là Idle (giao diện sẽ tưởng lõi đã
    /// tự dừng); link sai phải dẫn tới Error có thông báo, và `is_busy` phải về false.
    #[test]
    fn never_idle_after_start_and_bad_link_errors() {
        for _ in 0..30 {
            let p = Player::start(PlayerConfig {
                url: "không phải link".into(),
                latency: LatencyMode::Ultra,
                wav: None,
                resolver: ResolverConfig::default(),
            });
            assert_ne!(p.status().state, State::Idle);
            assert!(p.wait_finished(Duration::from_secs(2)));
            let st = p.status();
            assert_eq!(st.state, State::Error);
            assert!(st.message.contains("link"), "message = {}", st.message);
            assert!(!p.is_busy());
        }
    }

    #[test]
    fn stop_sets_idle_and_parse_modes() {
        let p = Player::start(PlayerConfig {
            url: "https://127.0.0.1:1/x.mpd".into(), // cổng đóng → lỗi kết nối nhanh
            latency: LatencyMode::parse("stable"),
            wav: None,
            resolver: ResolverConfig::default(),
        });
        p.stop();
        assert_eq!(p.status().state, State::Idle);
        assert!(p.wait_finished(Duration::from_secs(15)));
        assert_eq!(p.status().state, State::Idle); // đã stop thì không bị ghi đè bởi Error
        assert_eq!(LatencyMode::parse("balanced"), LatencyMode::Balanced);
        assert_eq!(LatencyMode::parse("xyz"), LatencyMode::Ultra);
    }
}
