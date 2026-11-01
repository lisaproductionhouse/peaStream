//! Lấy luồng bằng yt-dlp (https://github.com/yt-dlp/yt-dlp).
//!
//! Facebook đổi cấu trúc trang liên tục; yt-dlp được cộng đồng cập nhật theo trong vài ngày, nên đây
//! là nguồn chính để lấy manifest. Mình chỉ nhờ yt-dlp tìm ra **URL manifest DASH** (cùng header
//! cần dùng khi tải media); việc tải riêng luồng audio với độ trễ thấp vẫn do engine của dự án làm.
//!
//! Hai cách gọi:
//! * **tại chỗ** — chạy tiến trình `yt-dlp -J …` (Windows / Linux / macOS);
//! * **máy chủ** — gọi một dịch vụ nhỏ bọc yt-dlp (`scripts/ytdlp_helper.py`), dùng khi thiết bị không
//!   chạy được yt-dlp. Android không có Python nên phải theo cách này: chạy dịch vụ ngay trên điện
//!   thoại bằng Termux, hoặc trên máy tính trong cùng mạng.

use crate::extract::{Found, Source};
use crate::http::{pct_encode, Http};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Không tìm thấy yt-dlp trên máy (khác với: có yt-dlp nhưng nó báo lỗi).
#[derive(Debug)]
pub struct NotInstalled;

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("yt-dlp chưa được cài trên máy này")
    }
}
impl std::error::Error for NotInstalled {}

/// Chạy yt-dlp tại chỗ.
#[derive(Clone, Debug)]
pub struct Local {
    /// Đường dẫn tới yt-dlp. Mặc định tự tìm: biến `FBAUDIO_YTDLP`, cạnh file chạy của app, PATH,
    /// rồi `python -m yt_dlp`.
    pub path: Option<PathBuf>,
    /// Tham số thêm, ví dụ `--cookies-from-browser firefox` cho video cần đăng nhập.
    /// Mặc định đọc từ biến môi trường `FBAUDIO_YTDLP_ARGS`.
    pub args: Vec<String>,
    /// Khi yt-dlp báo trích xuất hỏng (Facebook vừa đổi): chạy `yt-dlp -U` rồi thử lại, tối đa một
    /// lần mỗi lần chạy ứng dụng. Tắt bằng biến môi trường `FBAUDIO_NO_UPDATE=1`.
    pub auto_update: bool,
    pub timeout: Duration,
}

impl Default for Local {
    fn default() -> Self {
        Self {
            path: None,
            args: std::env::var("FBAUDIO_YTDLP_ARGS").map(|s| split_args(&s)).unwrap_or_default(),
            auto_update: std::env::var_os("FBAUDIO_NO_UPDATE").is_none(),
            timeout: Duration::from_secs(60),
        }
    }
}

/// Dịch vụ yt-dlp từ xa (xem `scripts/ytdlp_helper.py`).
#[derive(Clone, Debug)]
pub struct Remote {
    pub endpoint: String,
    pub token: Option<String>,
}

impl Remote {
    /// `None` nếu địa chỉ trống. Tự thêm `http://` nếu người dùng chỉ gõ `192.168.1.5:8787`.
    pub fn new(endpoint: &str, token: Option<&str>) -> Option<Self> {
        let e = endpoint.trim().trim_end_matches('/');
        if e.is_empty() {
            return None;
        }
        let endpoint = if e.contains("://") { e.to_string() } else { format!("http://{e}") };
        let token = token.map(str::trim).filter(|t| !t.is_empty()).map(String::from);
        Some(Self { endpoint, token })
    }
}

// ------------------------------------------------------------------------------ tại chỗ

struct Cmd {
    program: OsString,
    /// Tham số đứng trước (ví dụ `-m yt_dlp` khi chạy qua Python).
    pre: Vec<&'static str>,
}

impl Cmd {
    fn bin(p: impl Into<OsString>) -> Self {
        Self { program: p.into(), pre: Vec::new() }
    }
    fn python(py: &str) -> Self {
        Self { program: py.into(), pre: vec!["-m", "yt_dlp"] }
    }
    /// Bản chạy độc lập / zipapp tự cập nhật được bằng `-U`; bản cài qua pip thì không.
    fn can_self_update(&self) -> bool {
        self.pre.is_empty()
    }
}

fn candidates(cfg: &Local) -> Vec<Cmd> {
    if let Some(p) = &cfg.path {
        return vec![Cmd::bin(p)]; // người dùng chỉ định rõ → chỉ dùng đúng cái đó
    }
    if let Some(p) = std::env::var_os("FBAUDIO_YTDLP").filter(|p| !p.is_empty()) {
        return vec![Cmd::bin(p)];
    }
    let exe = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let p = dir.join(exe);
        if p.is_file() {
            v.push(Cmd::bin(p));
        }
    }
    v.push(Cmd::bin(exe));
    for py in ["python3", "python", "py"] {
        v.push(Cmd::python(py));
    }
    v
}

#[derive(PartialEq)]
enum Ended {
    Exit,
    Stopped,
    TimedOut,
}

struct Run {
    ok: bool,
    ended: Ended,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Windows: không nháy cửa sổ console khi chạy yt-dlp từ giao diện (CREATE_NO_WINDOW). Nơi khác: không làm gì.
fn hide_console(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = c;
}

/// Diệt cả cây tiến trình, không chỉ tiến trình đầu. `yt-dlp.exe` (PyInstaller) là bộ khởi động
/// sinh ra tiến trình Python thật; chỉ `kill` cha thì con vẫn chạy và giữ đầu ống.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let mut k = Command::new("taskkill");
        k.args(["/PID", pid.as_str(), "/T", "/F"]);
        hide_console(&mut k);
        let _ = k.output();
    }
    #[cfg(unix)]
    {
        // Gọi thẳng syscall chứ KHÔNG chạy lệnh `kill` bên ngoài với PID âm: cách phân tích tham số
        // của từng bản `kill` khác nhau, và hiểu nhầm thành `-1` là giết mọi tiến trình của người dùng.
        // Chỉ diệt nhóm khi chắc chắn tiến trình con là trưởng nhóm riêng của nó (xem `run_cmd`).
        let pid = child.id() as libc::pid_t;
        // SAFETY: getpgid/killpg chỉ đọc/gửi tín hiệu theo PID, không đụng bộ nhớ của ta.
        unsafe {
            if pid > 1 && libc::getpgid(pid) == pid {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

fn run_cmd(cmd: &Cmd, args: &[String], timeout: Duration, stop: &AtomicBool) -> std::io::Result<Run> {
    let mut c = Command::new(&cmd.program);
    c.args(&cmd.pre)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1");
    hide_console(&mut c);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0); // nhóm riêng để diệt được cả cây khi hết giờ / người dùng dừng
    }
    let mut child = c.spawn()?;
    let mut so = child.stdout.take().expect("stdout piped");
    let mut se = child.stderr.take().expect("stderr piped");
    // đọc ở luồng riêng để ống không bị đầy làm treo tiến trình con
    let (tx_o, rx_o) = std::sync::mpsc::channel();
    let (tx_e, rx_e) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        let _ = tx_o.send(b);
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        let _ = tx_e.send(b);
    });
    let t0 = Instant::now();
    let (ok, ended) = loop {
        if let Some(st) = child.try_wait()? {
            break (st.success(), Ended::Exit);
        }
        let ended = if stop.load(Relaxed) {
            Some(Ended::Stopped)
        } else if t0.elapsed() > timeout {
            Some(Ended::TimedOut)
        } else {
            None
        };
        if let Some(e) = ended {
            kill_tree(&mut child);
            let _ = child.wait();
            break (false, e);
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    // Chờ đọc có giới hạn: nếu còn tiến trình mồ côi giữ đầu ống thì bỏ qua, không treo.
    let grace = if ended == Ended::Exit { Duration::from_secs(3) } else { Duration::from_millis(300) };
    Ok(Run {
        ok,
        ended,
        stdout: rx_o.recv_timeout(grace).unwrap_or_default(),
        stderr: rx_e.recv_timeout(grace).unwrap_or_default(),
    })
}

/// Chạy với ứng viên đầu tiên thực sự chạy được. `Err(NotInstalled)` nếu không có cái nào.
fn run_first(cfg: &Local, args: &[String], stop: &AtomicBool) -> Result<(Cmd, Run)> {
    for cmd in candidates(cfg) {
        match run_cmd(&cmd, args, cfg.timeout, stop) {
            Ok(run) => {
                // `python -m yt_dlp` khi Python có nhưng chưa cài module → coi như chưa cài
                if !run.ok && !cmd.pre.is_empty() && String::from_utf8_lossy(&run.stderr).contains("No module named") {
                    continue;
                }
                return Ok((cmd, run));
            }
            Err(e) => log::debug!("không chạy được {:?}: {e}", cmd.program),
        }
    }
    Err(NotInstalled.into())
}

/// Chỉ cho cập nhật một lần mỗi lần chạy ứng dụng.
static UPDATED: AtomicBool = AtomicBool::new(false);

pub fn run_local(url: &str, cfg: &Local, stop: &AtomicBool, progress: &dyn Fn(&str)) -> Result<Found> {
    let mut args: Vec<String> = ["-J", "--no-playlist", "--no-warnings", "--ignore-config", "--socket-timeout", "15"]
        .into_iter()
        .map(String::from)
        .collect();
    args.extend(cfg.args.iter().cloned());
    args.push("--".into()); // sau dấu này là URL, không bao giờ bị hiểu thành tuỳ chọn
    args.push(url.into());

    let mut updated_now = false;
    loop {
        let (cmd, run) = run_first(cfg, &args, stop)?;
        match run.ended {
            Ended::Stopped => bail!("Đã dừng"),
            Ended::TimedOut => bail!("yt-dlp không phản hồi sau {} giây", cfg.timeout.as_secs()),
            Ended::Exit => {}
        }
        if run.ok {
            return parse_info(&run.stdout).map(|mut f| {
                f.via = "yt-dlp";
                f
            });
        }
        let msg = error_line(&run.stderr);
        if !updated_now
            && cfg.auto_update
            && cmd.can_self_update()
            && looks_like_extractor_break(&msg)
            && !UPDATED.swap(true, Relaxed)
        {
            log::info!("yt-dlp báo trích xuất hỏng ({msg}) → thử cập nhật");
            progress("Đang cập nhật yt-dlp");
            let r = run_cmd(&cmd, &["-U".to_string()], Duration::from_secs(180), stop);
            log::info!("kết quả cập nhật: {:?}", r.as_ref().map(|r| r.ok));
            updated_now = true;
            progress("Đang hỏi yt-dlp");
            continue;
        }
        bail!("{msg}");
    }
}

/// `yt-dlp -U` theo yêu cầu người dùng; trả về thông báo của yt-dlp.
pub fn update(cfg: &Local) -> Result<String> {
    let stop = AtomicBool::new(false);
    let (cmd, run) = run_first(cfg, &["-U".to_string()], &stop)?;
    if !cmd.can_self_update() {
        bail!("yt-dlp được cài qua pip: hãy chạy  pip install -U yt-dlp");
    }
    let text = String::from_utf8_lossy(&[run.stdout, run.stderr].concat()).trim().to_string();
    if run.ok {
        Ok(text)
    } else {
        Err(anyhow!("{}", text.lines().last().unwrap_or("cập nhật thất bại")))
    }
}

fn looks_like_extractor_break(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["cannot parse data", "unable to extract", "please report this issue", "no video formats"]
        .iter()
        .any(|p| m.contains(p))
}

/// Dòng lỗi có ích nhất từ stderr của yt-dlp.
fn error_line(stderr: &[u8]) -> String {
    let text = strip_ansi(&String::from_utf8_lossy(stderr));
    let line = text
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("ERROR:"))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp thoát với lỗi không rõ");
    let line = line.trim().trim_start_matches("ERROR:").trim();
    // bỏ đoạn cầu kỳ "; please report this issue on https://github.com/… Confirm you are on the latest version…"
    let line = line.split("; please report").next().unwrap_or(line).trim_end_matches(['.', ',', ' ']);
    line.chars().take(240).collect()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Tách tham số kiểu shell đơn giản: khoảng trắng phân cách, nháy đơn/kép gom nhóm.
pub fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ------------------------------------------------------------------------------ máy chủ

pub fn run_remote(url: &str, cfg: &Remote) -> Result<Found> {
    // yt-dlp có thể mất vài chục giây → thời gian chờ đọc dài hơn mặc định
    let mut http = Http::with_timeouts(Duration::from_secs(8), Duration::from_secs(120));
    if let Some(t) = &cfg.token {
        http = http.with_headers(&[("Authorization".to_string(), format!("Bearer {t}"))]);
    }
    let req = format!("{}/info?url={}", cfg.endpoint, pct_encode(url));
    let (code, body) = http
        .get_raw(&req, 32 << 20)
        .with_context(|| format!("không kết nối được máy chủ yt-dlp {}", cfg.endpoint))?;
    if code != 200 {
        let text = String::from_utf8_lossy(&body);
        bail!("máy chủ yt-dlp trả HTTP {code}: {}", text.lines().next().unwrap_or("").chars().take(200).collect::<String>());
    }
    parse_info(&body).map(|mut f| {
        f.via = "máy chủ yt-dlp";
        f
    })
}

// ------------------------------------------------------------------------------ JSON → Found

/// Đọc JSON của `yt-dlp -J` và trích ra manifest DASH + header cần dùng khi tải media.
pub fn parse_info(json: &[u8]) -> Result<Found> {
    let root: Value = serde_json::from_slice(json).context("yt-dlp trả về JSON không hợp lệ")?;
    let info = match root["_type"].as_str() {
        Some("playlist") => root["entries"]
            .as_array()
            .and_then(|e| e.iter().find(|x| x.is_object()))
            .ok_or_else(|| anyhow!("yt-dlp trả về danh sách rỗng"))?,
        _ => &root,
    };
    let formats: Vec<&Value> = info["formats"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let title = info["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from);

    let is_audio = |f: &Value| {
        f["vcodec"].as_str() == Some("none") && f["acodec"].as_str().is_some_and(|a| a != "none")
    };
    let proto = |f: &Value| f["protocol"].as_str().unwrap_or("").to_string();
    let http_url = |v: &Value| v.as_str().filter(|u| u.starts_with("http")).map(String::from);

    let mut sources: Vec<Source> = Vec::new();
    let mut headers: Option<&Value> = None;

    // 1) Manifest DASH: của các định dạng audio-only trước, rồi của phần còn lại (cùng một MPD thì
    //    trùng URL và bị loại). Engine của dự án tự chọn Representation audio trong MPD.
    for audio_pass in [true, false] {
        for f in formats.iter().filter(|f| is_audio(f) == audio_pass) {
            if !proto(f).starts_with("http_dash_segments") {
                continue; // HLS (m3u8) và tệp liền không phải MPD
            }
            let Some(u) = http_url(&f["manifest_url"]).or_else(|| http_url(&f["url"])) else { continue };
            if !sources.iter().any(|s| s.url.as_deref() == Some(&u)) {
                sources.push(Source { url: Some(u.clone()), xml: None, base: u });
                headers.get_or_insert(&f["http_headers"]);
            }
        }
    }

    // 2) Tệp audio fMP4 liền (nội dung đã kết thúc, MPD dạng SegmentBase): dựng MPD một-BaseURL.
    for f in formats.iter().filter(|f| is_audio(f)) {
        let direct = matches!(proto(f).as_str(), "https" | "http");
        let dash_file = f["container"].as_str().is_some_and(|c| c.ends_with("_dash"));
        if let (true, true, Some(u)) = (direct, dash_file, http_url(&f["url"])) {
            let kbps = f["abr"].as_f64().or_else(|| f["tbr"].as_f64()).unwrap_or(64.0);
            let xml = single_file_mpd(&u, f["acodec"].as_str().unwrap_or("mp4a.40.2"), (kbps * 1000.0) as u64);
            sources.push(Source { url: None, xml: Some(xml), base: u });
            headers.get_or_insert(&f["http_headers"]);
        }
    }

    if sources.is_empty() {
        let kinds: Vec<String> = formats.iter().map(|f| proto(f)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        bail!(
            "yt-dlp thấy video nhưng không có luồng âm thanh DASH dùng được (giao thức: {}). \
             Có thể video chỉ có định dạng ghép sẵn hình + tiếng, hoặc chỉ có HLS.",
            if kinds.is_empty() { "không có định dạng nào".into() } else { kinds.join(", ") }
        );
    }

    let hdr = |v: &Value| -> Vec<(String, String)> {
        v.as_object()
            .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default()
    };
    let headers = headers.map(hdr).filter(|h| !h.is_empty()).unwrap_or_else(|| hdr(&info["http_headers"]));
    Ok(Found { title, sources, headers, via: "yt-dlp" })
}

fn single_file_mpd(url: &str, codecs: &str, bandwidth: u64) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    format!(
        r#"<MPD type="static"><Period><AdaptationSet contentType="audio"><Representation id="a" mimeType="audio/mp4" codecs="{}" bandwidth="{}"><BaseURL>{}</BaseURL><SegmentBase/></Representation></AdaptationSet></Period></MPD>"#,
        esc(codecs),
        bandwidth,
        esc(url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// JSON thật do `yt-dlp -J` (2026.08) in ra cho một MPD có cả video lẫn audio (rút gọn).
    const REAL: &[u8] = include_bytes!("../tests/data/ytdlp-dash.json");

    #[test]
    fn real_ytdlp_json_yields_the_mpd_and_headers() {
        let f = parse_info(REAL).unwrap();
        assert_eq!(f.sources.len(), 1, "audio và video cùng một MPD → chỉ một nguồn");
        assert_eq!(f.sources[0].url.as_deref(), Some("http://127.0.0.1:8770/manifest.mpd"));
        assert!(f.headers.iter().any(|(k, v)| k == "User-Agent" && v.contains("Mozilla")));
        assert_eq!(f.via, "yt-dlp");
        assert!(f.title.is_some());
    }

    /// Dựng theo đúng những gì extractor Facebook của yt-dlp tạo ra: định dạng DASH có
    /// `manifest_url` trên fbcdn và http_headers `facebookexternalhit/1.1`, kèm hd/sd ghép sẵn và HLS.
    #[test]
    fn facebook_like_formats_prefer_audio_manifest_and_keep_headers() {
        let fb_ua = json!({"User-Agent": "facebookexternalhit/1.1", "Accept": "*/*"});
        let mpd = "https://video-sin6-1.xx.fbcdn.net/o1/v/t2/f2/m69/abc.mpd?oh=1&oe=2";
        let v = json!({
            "id": "123", "title": "Phiên live tối nay", "extractor": "facebook", "is_live": true,
            "formats": [
                {"format_id": "hd", "url": "https://video.xx.fbcdn.net/hd.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "mp4a.40.2", "http_headers": fb_ua},
                {"format_id": "hls_sd", "manifest_url": "https://x.fbcdn.net/a.m3u8", "url": "https://x.fbcdn.net/a.m3u8", "protocol": "m3u8_native", "vcodec": "avc1", "acodec": "mp4a.40.2"},
                {"format_id": "v1", "manifest_url": mpd, "url": mpd, "protocol": "http_dash_segments", "vcodec": "avc1.4d401f", "acodec": "none", "http_headers": fb_ua},
                {"format_id": "a1", "manifest_url": mpd, "url": mpd, "protocol": "http_dash_segments", "vcodec": "none", "acodec": "mp4a.40.5", "abr": 48.0, "http_headers": fb_ua},
            ]
        });
        let f = parse_info(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(f.sources.len(), 1); // HLS bị bỏ, MPD không bị lặp
        assert_eq!(f.sources[0].url.as_deref(), Some(mpd));
        assert_eq!(f.title.as_deref(), Some("Phiên live tối nay"));
        assert!(f.headers.contains(&("User-Agent".to_string(), "facebookexternalhit/1.1".to_string())));
    }

    #[test]
    fn dash_audio_file_without_manifest_becomes_a_single_file_mpd() {
        let u = "https://video.xx.fbcdn.net/o1/v/t2/audio.mp4?a=1&b=2";
        let v = json!({"title": "VOD", "formats": [
            {"format_id": "a", "url": u, "protocol": "https", "container": "m4a_dash", "vcodec": "none", "acodec": "mp4a.40.5", "abr": 48.0}
        ]});
        let f = parse_info(&serde_json::to_vec(&v).unwrap()).unwrap();
        let xml = f.sources[0].xml.as_deref().expect("MPD tổng hợp");
        assert!(xml.contains("&amp;b=2"), "phải thoát ký tự & trong XML: {xml}");
        let mpd = crate::dash::parse(xml, &f.sources[0].base).unwrap();
        assert!(matches!(&mpd.audio.source, crate::dash::SegSource::Single(s) if s == u), "{:?}", mpd.audio.source);
        assert_eq!(mpd.audio.codecs, "mp4a.40.5");
    }

    #[test]
    fn playlist_wrapper_and_no_audio_cases() {
        let inner = json!({"title": "x", "formats": [{"protocol": "http_dash_segments", "manifest_url": "http://h/a.mpd", "vcodec": "none", "acodec": "mp4a.40.2"}]});
        let v = json!({"_type": "playlist", "entries": [null, inner]});
        assert_eq!(parse_info(&serde_json::to_vec(&v).unwrap()).unwrap().sources.len(), 1);

        let only_muxed = json!({"formats": [{"url": "https://x/hd.mp4", "protocol": "https", "vcodec": "avc1", "acodec": "mp4a"}]});
        let e = parse_info(&serde_json::to_vec(&only_muxed).unwrap()).unwrap_err().to_string();
        assert!(e.contains("không có luồng âm thanh"), "{e}");
        assert!(parse_info(b"khong phai json").is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(split_args(r#"--cookies-from-browser firefox --x "a b" 'c d' """#), ["--cookies-from-browser", "firefox", "--x", "a b", "c d", ""]);
        assert_eq!(error_line(b"WARNING: x\n\x1b[0;31mERROR:\x1b[0m [facebook] 1: Cannot parse data\n"), "[facebook] 1: Cannot parse data");
        assert_eq!(
            error_line(b"ERROR: [facebook] 1: Cannot parse data; please report this issue on https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the template\n"),
            "[facebook] 1: Cannot parse data"
        );
        assert!(looks_like_extractor_break("[facebook] 1: Cannot parse data; please report"));
        assert!(!looks_like_extractor_break("Video unavailable"));
        let r = Remote::new(" 192.168.1.5:8787/ ", Some(" tok ")).unwrap();
        assert_eq!((r.endpoint.as_str(), r.token.as_deref()), ("http://192.168.1.5:8787", Some("tok")));
        assert!(Remote::new("  ", None).is_none());
    }
}

/// Thử với một "yt-dlp giả" là shell script (chỉ Unix). Các test này tạo file thực thi rồi chạy ngay,
/// nên xếp hàng qua một khoá để tránh lỗi "Text file busy" khi nhiều test fork tiến trình cùng lúc.
#[cfg(all(test, unix))]
mod fake {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());
    const OK_JSON: &str = r#"{"title":"T","formats":[{"protocol":"http_dash_segments","manifest_url":"http://h/live.mpd","vcodec":"none","acodec":"mp4a.40.5"}]}"#;

    fn script(name: &str, body: &str) -> (PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("fbaudio-fake-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("yt-dlp");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (p, dir)
    }

    fn cfg(p: &Path) -> Local {
        Local { path: Some(p.into()), args: vec![], auto_update: false, timeout: Duration::from_secs(10) }
    }

    fn go(c: &Local) -> Result<Found> {
        run_local("https://www.facebook.com/x/videos/1", c, &AtomicBool::new(false), &|_| {})
    }

    #[test]
    fn runs_script_and_parses_output() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (p, _) = script("ok", &format!("echo '{OK_JSON}'"));
        let f = go(&cfg(&p)).unwrap();
        assert_eq!(f.sources[0].url.as_deref(), Some("http://h/live.mpd"));
        assert_eq!(f.title.as_deref(), Some("T"));
    }

    #[test]
    fn url_is_passed_after_double_dash_and_args_are_forwarded() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // script chỉ in JSON hợp lệ nếu đối số cuối là URL, ngay trước nó là "--" và có cờ người dùng thêm
        let (p, _) = script("args", &format!(
            r#"for a in "$@"; do last2="$last"; last="$a"; [ "$a" = "--cookies-from-browser" ] && seen=1; done
[ "$last2" = "--" ] && [ "$last" = "https://www.facebook.com/x/videos/1" ] && [ "$seen" = 1 ] && echo '{OK_JSON}' && exit 0
echo "ERROR: đối số sai: $*" >&2; exit 1"#));
        let mut c = cfg(&p);
        c.args = vec!["--cookies-from-browser".into(), "firefox".into()];
        assert!(go(&c).is_ok());
    }

    #[test]
    fn reports_the_error_line_of_ytdlp() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (p, _) = script("err", r#"echo "WARNING: x" >&2; echo "ERROR: [facebook] 1: Video unavailable" >&2; exit 1"#);
        let e = go(&cfg(&p)).unwrap_err().to_string();
        assert_eq!(e, "[facebook] 1: Video unavailable");
    }

    #[test]
    fn missing_binary_is_reported_as_not_installed() {
        let c = Local { path: Some("/không/tồn/tại/yt-dlp".into()), ..cfg(Path::new("x")) };
        let e = go(&c).unwrap_err();
        assert!(e.downcast_ref::<NotInstalled>().is_some(), "{e:#}");
    }

    #[test]
    fn timeout_and_stop_kill_the_process() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (p, _) = script("sleep", "sleep 30");
        let mut c = cfg(&p);
        c.timeout = Duration::from_millis(400);
        let t0 = Instant::now();
        assert!(go(&c).unwrap_err().to_string().contains("không phản hồi"));
        assert!(t0.elapsed() < Duration::from_secs(5));

        let stop = AtomicBool::new(true); // người dùng bấm dừng ngay khi đang chờ
        let t0 = Instant::now();
        let e = run_local("https://fb.watch/a", &cfg(&p), &stop, &|_| {}).unwrap_err();
        assert_eq!(e.to_string(), "Đã dừng");
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    /// Lần đầu yt-dlp báo "Cannot parse data" → app chạy `-U` → thử lại và thành công.
    #[test]
    fn auto_update_then_retry_succeeds_once() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (p, dir) = script("upd", &format!(r#"D="$(dirname "$0")"
if [ "$1" = "-U" ]; then touch "$D/updated"; echo "Updated yt-dlp"; exit 0; fi
if [ -f "$D/updated" ]; then echo '{OK_JSON}'; exit 0; fi
echo "ERROR: [facebook] 1: Cannot parse data; please report this issue on https://github.com/yt-dlp/yt-dlp/issues" >&2; exit 1"#));
        let mut c = cfg(&p);
        c.auto_update = true;
        let msgs = std::cell::RefCell::new(Vec::new());
        let f = run_local("https://fb.watch/a", &c, &AtomicBool::new(false), &|m| msgs.borrow_mut().push(m.to_string())).unwrap();
        assert_eq!(f.sources.len(), 1);
        assert!(dir.join("updated").exists());
        assert!(msgs.borrow().iter().any(|m| m.contains("cập nhật")));
    }
}
