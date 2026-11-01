//! Nguồn byte thô: ứng dụng KHÔNG tự tải hay phân tích manifest. Một công cụ ngoài (yt-dlp) làm hết
//! việc từ link Facebook tới luồng âm thanh và ghi ra stdout; ta chỉ đọc dòng byte đó.
//!
//! * `YtDlp`   — chạy `yt-dlp … -o -` tại chỗ (Windows / Linux / macOS);
//! * `Remote`  — đọc cùng luồng đó qua HTTP từ `scripts/ytdlp_helper.py`. Android không có Python nên
//!               phải chạy yt-dlp ở nơi khác (Termux trên chính điện thoại, hoặc máy tính cùng mạng);
//! * `Command` — lệnh tuỳ ý in AAC (ADTS hoặc fMP4) ra stdout, `{url}` được thay bằng link;
//! * `Stdin`   — `yt-dlp … -o - | fbaudio -`.
//!
//! Tiêu đề video (yt-dlp biết mà luồng byte không có) được lấy từ stderr nhờ `--print before_dl:…`
//! (khi `-o -`, yt-dlp dẫn mọi thông báo ra stderr nên stdout vẫn sạch).

use anyhow::{anyhow, bail, Result};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Không tìm thấy công cụ ngoài (khác với: có chạy nhưng báo lỗi).
#[derive(Debug)]
pub struct NotInstalled(pub &'static str);

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} chưa được cài trên máy này (Windows: winget install yt-dlp.yt-dlp)", self.0)
    }
}
impl std::error::Error for NotInstalled {}

#[derive(Clone, Debug)]
pub struct YtDlp {
    /// Mặc định tự tìm: biến `FBAUDIO_YTDLP`, `yt-dlp(.exe)` cạnh file chạy của app, PATH, `python -m yt_dlp`.
    pub path: Option<PathBuf>,
    /// Tham số thêm, ví dụ `--cookies-from-browser firefox` (video cần đăng nhập). Mặc định đọc từ `FBAUDIO_YTDLP_ARGS`.
    pub args: Vec<String>,
    /// yt-dlp báo trích xuất hỏng (Facebook vừa đổi) → chạy `yt-dlp -U` rồi thử lại, một lần mỗi lần chạy app.
    /// Tắt bằng biến môi trường `FBAUDIO_NO_UPDATE=1`.
    pub auto_update: bool,
    /// Live: thử ép yt-dlp giao DASH cho ffmpeg trước (độ trễ thấp nhất nếu manifest là manifest "sống"), không được
    /// thì lùi về bộ tải gốc chạy lặp từng lượt. Tắt (đi thẳng bộ tải gốc) bằng `FBAUDIO_NO_FFMPEG=1`.
    pub ffmpeg_live: bool,
}

impl Default for YtDlp {
    fn default() -> Self {
        Self {
            path: None,
            args: std::env::var("FBAUDIO_YTDLP_ARGS").map(|s| split_args(&s)).unwrap_or_default(),
            auto_update: std::env::var_os("FBAUDIO_NO_UPDATE").is_none(),
            ffmpeg_live: std::env::var_os("FBAUDIO_NO_FFMPEG").is_none(),
        }
    }
}

/// Máy chủ yt-dlp (`scripts/ytdlp_helper.py`).
#[derive(Clone, Debug)]
pub struct Remote {
    pub endpoint: String,
    pub token: Option<String>,
}

impl Remote {
    /// `None` nếu địa chỉ trống. Tự thêm `http://` nếu chỉ gõ `192.168.1.5:8787`.
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

#[derive(Clone, Debug)]
pub enum Source {
    YtDlp(YtDlp),
    Remote(Remote),
    /// `template` tách theo kiểu shell đơn giản (không dùng shell), `{url}` được thay bằng link.
    Command { template: String },
    Stdin,
}

impl Default for Source {
    fn default() -> Self {
        Source::YtDlp(YtDlp::default())
    }
}

/// Thông tin công cụ báo kèm theo luồng byte (hiện chỉ có tiêu đề).
///
/// Cố ý KHÔNG lấy `is_live` từ yt-dlp: extractor Facebook không đặt trường này (chỉ extractor Generic đặt khi MPD
/// là `dynamic`), nên với Facebook nó luôn trống dù đang live. Live hay phát lại do người dùng chọn.
#[derive(Default)]
pub struct Info {
    title: Mutex<Option<String>>,
}

impl Info {
    fn set_title(&self, t: &str) {
        if !t.is_empty() && t != "NA" {
            *self.title.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.to_string());
        }
    }
    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Luồng byte đang đọc. `finish()` gọi sau khi gặp EOF để biết công cụ kết thúc bình thường hay báo lỗi.
pub trait Stream: Read + Send {
    fn finish(&mut self) -> Result<()>;
}

pub struct Opened {
    pub stream: Box<dyn Stream>,
    pub info: Arc<Info>,
    pub killer: Killer,
}

/// Dừng tiến trình công cụ từ luồng khác (nút Dừng): `read` đang chặn sẽ trả EOF ngay.
#[derive(Clone)]
pub struct Killer {
    pid: u32,
    alive: Arc<AtomicBool>,
}

impl Killer {
    pub fn none() -> Self {
        Self { pid: 0, alive: Arc::new(AtomicBool::new(false)) }
    }
    pub fn kill(&self) {
        // `alive` được tắt trước khi tiến trình bị thu dọn, để không bắn nhầm vào PID đã được tái sử dụng
        if self.pid != 0 && self.alive.load(Relaxed) {
            kill_group(self.pid);
        }
    }
}

/// Diệt cả nhóm tiến trình. `yt-dlp` còn sinh `ffmpeg` (và bản `.exe` sinh tiến trình Python thật);
/// chỉ diệt cha thì con vẫn chạy và tiếp tục giữ đầu ống.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        // Gọi thẳng syscall chứ KHÔNG chạy lệnh `kill` ngoài với PID âm: mỗi bản `kill` phân tích tham số
        // một kiểu, và hiểu nhầm thành `-1` là giết mọi tiến trình của người dùng (đã gặp khi thử nghiệm).
        // Chỉ diệt nhóm khi chắc chắn tiến trình con là trưởng nhóm riêng của nó (xem `spawn`).
        let pid = pid as libc::pid_t;
        // SAFETY: getpgid/killpg chỉ đọc/gửi tín hiệu theo PID, không đụng bộ nhớ của ta.
        unsafe {
            if pid > 1 && libc::getpgid(pid) == pid {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let mut k = Command::new("taskkill");
        k.args(["/PID", pid.as_str(), "/T", "/F"]);
        hide_console(&mut k);
        let _ = k.output();
    }
}

fn kill_tree(child: &mut Child) {
    kill_group(child.id());
    let _ = child.kill();
}

/// Windows: không nháy cửa sổ console khi chạy công cụ từ giao diện (CREATE_NO_WINDOW). Nơi khác: không làm gì.
fn hide_console(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = c;
}

// ---------------------------------------------------------------------------------- tiến trình

struct ProcStream {
    stdout: ChildStdout,
    child: Child,
    tail: Arc<Mutex<VecDeque<String>>>,
    alive: Arc<AtomicBool>,
    name: &'static str,
    /// Thư mục làm việc riêng của công cụ, xoá khi xong (xem `private_dir`).
    tmp: Option<PathBuf>,
}

impl Read for ProcStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Stream for ProcStream {
    fn finish(&mut self) -> Result<()> {
        // sau EOF tiến trình thường thoát ngay; chờ tối đa 3 s rồi diệt hẳn
        let t0 = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait()? {
                break Some(s);
            }
            if t0.elapsed() > Duration::from_secs(3) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.alive.store(false, Relaxed);
        match status {
            Some(s) if s.success() => Ok(()),
            Some(s) => {
                std::thread::sleep(Duration::from_millis(60)); // cho luồng đọc stderr kịp nhận dòng cuối
                let msg = error_line(&self.tail.lock().unwrap_or_else(|e| e.into_inner()));
                Err(anyhow!(
                    "{}",
                    if msg.is_empty() { format!("{} thoát với mã {}", self.name, s.code().unwrap_or(-1)) } else { msg }
                ))
            }
            None => {
                kill_tree(&mut self.child);
                bail!("{} không thoát sau khi đóng luồng", self.name)
            }
        }
    }
}

impl Drop for ProcStream {
    fn drop(&mut self) {
        self.alive.store(false, Relaxed);
        if self.child.try_wait().ok().flatten().is_none() {
            kill_tree(&mut self.child);
        }
        let _ = self.child.wait();
        if let Some(d) = &self.tmp {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Dòng lỗi có ích nhất trong phần đuôi stderr của công cụ.
fn error_line(tail: &VecDeque<String>) -> String {
    let line = tail
        .iter()
        .rev()
        .find(|l| l.trim_start().starts_with("ERROR:"))
        .or_else(|| tail.iter().rev().find(|l| !l.trim().is_empty()))
        .map(String::as_str)
        .unwrap_or("");
    let line = line.trim().trim_start_matches("ERROR:").trim();
    // bỏ đoạn cầu kỳ "; please report this issue on https://github.com/… Confirm you are on the latest version…"
    let line = line.split("; please report").next().unwrap_or(line).trim_end_matches(['.', ',', ' ']);
    line.chars().take(240).collect()
}

/// yt-dlp (bộ tải gốc) ghi tệp tạm `-Frag<N>` vào THƯ MỤC LÀM VIỆC rồi để lại nếu bị ngắt giữa chừng (đã gặp: bấm
/// Dừng khi phát lại). Cho nó một thư mục riêng, xoá khi công cụ kết thúc, để không rải rác vào thư mục của người dùng.
fn private_dir() -> Option<PathBuf> {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!("fbaudio-{}-{}", std::process::id(), SEQ.fetch_add(1, Relaxed)));
    std::fs::create_dir_all(&d).ok().map(|_| d)
}

fn spawn(program: &OsStr, pre: &[&str], args: &[String], name: &'static str, own_dir: bool) -> io::Result<Opened> {
    let tmp = if own_dir { private_dir() } else { None };
    let mut c = Command::new(program);
    if let Some(d) = &tmp {
        c.current_dir(d);
    }
    c.args(pre)
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
        c.process_group(0); // nhóm riêng → diệt được cả cây (yt-dlp + ffmpeg) khi bấm Dừng
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            if let Some(d) = &tmp {
                let _ = std::fs::remove_dir_all(d);
            }
            return Err(e);
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let info = Arc::new(Info::default());
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    {
        let (info, tail) = (info.clone(), tail.clone());
        std::thread::spawn(move || {
            let mut r = io::BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                if !matches!(r.read_until(b'\n', &mut line), Ok(n) if n > 0) {
                    break;
                }
                let s = strip_ansi(&String::from_utf8_lossy(&line));
                let s = s.trim_end();
                if let Some(v) = s.strip_prefix("FBAUDIO_TITLE=") {
                    info.set_title(v.trim());
                } else if !s.trim().is_empty() {
                    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                    t.push_back(s.to_string());
                    if t.len() > 16 {
                        t.pop_front();
                    }
                }
            }
        });
    }
    let alive = Arc::new(AtomicBool::new(true));
    let killer = Killer { pid: child.id(), alive: alive.clone() };
    Ok(Opened { stream: Box::new(ProcStream { stdout, child, tail, alive, name, tmp }), info, killer })
}

// ---------------------------------------------------------------------------------- yt-dlp

struct Cand {
    program: OsString,
    pre: Vec<&'static str>,
}

fn candidates(cfg: &YtDlp) -> Vec<Cand> {
    let bin = |p: OsString| Cand { program: p, pre: Vec::new() };
    if let Some(p) = &cfg.path {
        return vec![bin(p.clone().into_os_string())]; // người dùng chỉ định rõ → chỉ dùng đúng cái đó
    }
    if let Some(p) = std::env::var_os("FBAUDIO_YTDLP").filter(|p| !p.is_empty()) {
        return vec![bin(p)];
    }
    let exe = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        let p = dir.join(exe);
        if p.is_file() {
            v.push(bin(p.into_os_string()));
        }
    }
    v.push(bin(exe.into()));
    for py in ["python3", "python", "py"] {
        v.push(Cand { program: py.into(), pre: vec!["-m", "yt_dlp"] });
    }
    v
}

fn python_has_ytdlp(py: &OsStr) -> bool {
    let mut c = Command::new(py);
    c.args(["-c", "import yt_dlp"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    hide_console(&mut c);
    c.status().map(|s| s.success()).unwrap_or(false)
}

/// ffmpeg đọc nhanh bằng chừng này lần thời gian thực để tiêu hết lịch sử (cửa sổ DVR) khi vào live; app bỏ
/// im lặng phần lịch sử đó. Đã đo: lịch sử 48 s → ~7 s im lặng lúc vào, rồi liền mạch sát live. Thấp hơn (`-re`,
/// tức ×1) thì liền mạch nhưng LÙI bằng cả cửa sổ lịch sử (đo: 15 s → trễ 21 s; 40 s → trễ 46 s); cao hơn
/// nhiều (×100) thì ffmpeg phát đi phát lại cả lịch sử rồi kẹt.
const LIVE_CATCHUP_RATE: u32 = 8;

/// Dòng lệnh yt-dlp. Mỗi tham số đều có lý do (xem README, phần "Đo thực tế").
///
/// `ffmpeg`: ép yt-dlp giao DASH cho ffmpeg (chỉ dùng cho live). `false` = bộ tải gốc: tải đúng các đoạn có trong
/// manifest rồi thoát, không cần ffmpeg (dùng cho phát lại, và cho live khi ffmpeg không dùng được).
fn ytdlp_args(cfg: &YtDlp, url: &str, ffmpeg: bool) -> Vec<String> {
    let mut a: Vec<String> = [
        "-f", "ba", // chỉ luồng audio tốt nhất: không bao giờ tải video
        "-o", "-", // ghi dữ liệu ra stdout
        "--no-playlist", "--no-warnings", "--ignore-config", "--no-mtime", "--socket-timeout", "15",
        // tiêu đề: khi `-o -` yt-dlp dẫn `--print` ra stderr nên stdout vẫn chỉ là dữ liệu
        "--print", "before_dl:FBAUDIO_TITLE=%(title)s",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if ffmpeg {
        // Với Facebook, extractor phân tích bản manifest XML NHÚNG trong trang (một ảnh chụp tại lúc tải trang) và
        // không đặt `is_live`: bộ tải gốc chỉ tải đúng các đoạn trong ảnh chụp đó (đã gặp: 11 đoạn) rồi thoát.
        // Ép ffmpeg cho DASH: ffmpeg được đưa URL manifest (`mpd_url`, nếu trang có cung cấp) và tự bám theo nếu
        // đó là manifest sống. Nếu không có URL, hoặc không có ffmpeg, lượt này lỗi và app lùi về bộ tải gốc.
        a.extend(["--downloader", "dash:ffmpeg"].map(String::from));
        // yt-dlp chỉ tự thêm `-re` khi nó coi luồng là live; ta chỉ định tốc độ đọc rõ ràng (xem LIVE_CATCHUP_RATE).
        a.extend(["--downloader-args".to_string(), format!("ffmpeg_i:-readrate {LIVE_CATCHUP_RATE}")]);
        // stdout không tua được: muxer mặc định `ipod` (đuôi .m4a) lỗi "Could not write header" → ép `-f adts`
        // (AAC thô, đơn giản nhất để đọc) và flush từng gói.
        a.extend(["--downloader-args", "ffmpeg_o:-f adts -flush_packets 1"].map(String::from));
    }
    a.extend(cfg.args.iter().cloned());
    a.push("--".into()); // sau dấu này là URL, không bao giờ bị hiểu thành tuỳ chọn
    a.push(url.into());
    a
}

fn open_ytdlp(cfg: &YtDlp, url: &str, ffmpeg: bool) -> Result<Opened> {
    let args = ytdlp_args(cfg, url, ffmpeg);
    for cand in candidates(cfg) {
        if !cand.pre.is_empty() && !python_has_ytdlp(&cand.program) {
            continue;
        }
        match spawn(&cand.program, &cand.pre, &args, "yt-dlp", true) {
            Ok(o) => return Ok(o),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow!(e).context("không chạy được yt-dlp")),
        }
    }
    Err(NotInstalled("yt-dlp").into())
}

/// Chạy một lệnh ngắn tới khi xong (có thời hạn), trả (thành công?, đầu ra gộp).
fn run_to_end(program: &OsStr, pre: &[&str], args: &[&str], timeout: Duration) -> io::Result<(bool, String)> {
    let mut c = Command::new(program);
    c.args(pre).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console(&mut c);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    let mut child = c.spawn()?;
    let (mut so, mut se) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    std::thread::spawn(move || {
        let mut b = String::new();
        let _ = so.read_to_string(&mut b);
        let _ = tx.send(b);
    });
    std::thread::spawn(move || {
        let mut b = String::new();
        let _ = se.read_to_string(&mut b);
        let _ = tx2.send(b);
    });
    let t0 = Instant::now();
    let ok = loop {
        if let Some(s) = child.try_wait()? {
            break s.success();
        }
        if t0.elapsed() > timeout {
            kill_tree(&mut child);
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let mut text = String::new();
    for _ in 0..2 {
        if let Ok(s) = rx.recv_timeout(Duration::from_millis(500)) {
            text.push_str(&s);
        }
    }
    Ok((ok, strip_ansi(&text).trim().to_string()))
}

/// `yt-dlp -U`. Bản chạy độc lập (`yt-dlp(.exe)`) tự cập nhật được; bản pip thì phải dùng pip.
pub fn update_ytdlp(cfg: &YtDlp) -> Result<String> {
    for cand in candidates(cfg) {
        if !cand.pre.is_empty() {
            if python_has_ytdlp(&cand.program) {
                bail!("yt-dlp được cài qua pip: hãy chạy  pip install -U yt-dlp");
            }
            continue;
        }
        match run_to_end(&cand.program, &cand.pre, &["-U"], Duration::from_secs(180)) {
            Ok((true, text)) => return Ok(text),
            Ok((false, text)) => bail!("{}", text.lines().last().unwrap_or("cập nhật thất bại")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow!(e).context("không chạy được yt-dlp")),
        }
    }
    Err(NotInstalled("yt-dlp").into())
}

/// Lỗi yt-dlp kiểu "Facebook vừa đổi cấu trúc, extractor hỏng" (cập nhật yt-dlp có thể sửa được).
pub fn looks_like_extractor_break(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["cannot parse data", "unable to extract", "please report this issue", "no video formats"]
        .iter()
        .any(|p| m.contains(p))
}

// ---------------------------------------------------------------------------------- lệnh tuỳ ý / stdin

fn open_command(template: &str, url: &str) -> Result<Opened> {
    let toks = split_args(template);
    let Some((prog, rest)) = toks.split_first() else { bail!("lệnh tuỳ ý đang trống") };
    let args: Vec<String> = rest.iter().map(|t| t.replace("{url}", url)).collect();
    spawn(OsStr::new(&prog.replace("{url}", url)), &[], &args, "lệnh", false)
        .map_err(|e| anyhow!(e).context(format!("không chạy được “{prog}”")))
}

struct StdinStream;
impl Read for StdinStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::stdin().lock().read(buf)
    }
}
impl Stream for StdinStream {
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------- máy chủ yt-dlp

struct HttpStream(Box<dyn Read + Send + Sync>);
impl Read for HttpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}
impl Stream for HttpStream {
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

fn open_remote(r: &Remote, url: &str, ffmpeg: bool) -> Result<Opened> {
    // Chờ đọc dài: máy chủ im lặng tới khi yt-dlp có byte đầu tiên (có thể vài chục giây); sau đó dữ liệu về đều.
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(120))
        .build();
    let mut req = agent.get(&format!("{}/stream?url={}&ffmpeg={}", r.endpoint, pct_encode(url), ffmpeg as u8));
    if let Some(t) = &r.token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(resp) => {
            let info = Arc::new(Info::default());
            if let Some(t) = resp.header("X-Fbaudio-Title") {
                info.set_title(&pct_decode(t));
            }
            Ok(Opened { stream: Box::new(HttpStream(resp.into_reader())), info, killer: Killer::none() })
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            bail!(
                "máy chủ yt-dlp trả HTTP {code}: {}",
                body.lines().next().unwrap_or("").chars().take(200).collect::<String>()
            )
        }
        Err(e) => Err(anyhow!(e).context(format!("không kết nối được máy chủ yt-dlp {}", r.endpoint))),
    }
}

// ---------------------------------------------------------------------------------- mở nguồn

/// `ffmpeg`: ép yt-dlp giao DASH cho ffmpeg (chiến lược live độ trễ thấp); `false` = bộ tải gốc.
/// Chỉ `Source::YtDlp` và `Source::Remote` dùng tới; các nguồn khác ghi sẵn dữ liệu.
pub fn open(src: &Source, url: &str, ffmpeg: bool) -> Result<Opened> {
    match src {
        Source::YtDlp(cfg) => open_ytdlp(cfg, url, ffmpeg),
        Source::Remote(r) => open_remote(r, url, ffmpeg),
        Source::Command { template } => open_command(template, url),
        Source::Stdin => Ok(Opened { stream: Box::new(StdinStream), info: Arc::new(Info::default()), killer: Killer::none() }),
    }
}

// ---------------------------------------------------------------------------------- tiện ích

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

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(
            split_args(r#"--cookies-from-browser firefox "a b" 'c d' """#),
            ["--cookies-from-browser", "firefox", "a b", "c d", ""]
        );
        assert_eq!(pct_encode("https://fb.watch/a b?x=1&y=é"), "https%3A%2F%2Ffb.watch%2Fa%20b%3Fx%3D1%26y%3D%C3%A9");
        assert_eq!(pct_decode("Phi%C3%AAn%20live"), "Phiên live");
        let r = Remote::new(" 192.168.1.5:8787/ ", Some(" tok ")).unwrap();
        assert_eq!((r.endpoint.as_str(), r.token.as_deref()), ("http://192.168.1.5:8787", Some("tok")));
        assert!(Remote::new("  ", None).is_none());
        assert!(looks_like_extractor_break("[facebook] 1: Cannot parse data; please report"));
        assert!(!looks_like_extractor_break("Video unavailable"));
    }

    #[test]
    fn error_line_prefers_ytdlp_error_and_trims_boilerplate() {
        let t: VecDeque<String> = [
            "WARNING: x",
            "ERROR: [facebook] 1: Cannot parse data; please report this issue on https://github.com/yt-dlp/yt-dlp/issues?q= , filling out the template",
            "trailing line",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(error_line(&t), "[facebook] 1: Cannot parse data");
        assert_eq!(error_line(&["chỉ có dòng này".to_string()].into_iter().collect()), "chỉ có dòng này");
        assert_eq!(error_line(&VecDeque::new()), "");
    }

    #[test]
    fn ytdlp_command_line_is_safe_and_audio_only() {
        let cfg = YtDlp { path: None, args: vec!["--cookies".into(), "cookies.txt".into()], auto_update: false, ffmpeg_live: true };
        let a = ytdlp_args(&cfg, "https://fb.watch/x", true);
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap_or_else(|| panic!("thiếu {s}"));
        assert_eq!((a[pos("-f") + 1].as_str(), a[pos("-o") + 1].as_str()), ("ba", "-"));
        assert!(pos("--cookies") < pos("--"), "tham số người dùng phải đứng trước --");
        assert_eq!(&a[a.len() - 2..], ["--", "https://fb.watch/x"], "URL luôn đứng cuối, sau --");
        assert!(a.iter().any(|x| x == "ffmpeg_o:-f adts -flush_packets 1"));
        // live: ép ffmpeg cho DASH (extractor Facebook không đặt is_live) và đọc nhanh vừa phải để tiêu hết lịch sử
        assert!(a.windows(2).any(|w| w == ["--downloader", "dash:ffmpeg"]));
        assert!(a.iter().any(|x| x == "ffmpeg_i:-readrate 8"));
        // phát lại: bộ tải gốc, không cần ffmpeg
        let r = ytdlp_args(&cfg, "https://fb.watch/x", false);
        assert!(!r.iter().any(|x| x.contains("ffmpeg") || x == "--downloader"), "phát lại không được phụ thuộc ffmpeg: {r:?}");
        assert_eq!(&r[r.len() - 2..], ["--", "https://fb.watch/x"]);
    }
}
