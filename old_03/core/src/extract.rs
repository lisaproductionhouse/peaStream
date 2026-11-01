//! Từ link Facebook Live (hoặc link .mpd trực tiếp) → danh sách manifest DASH ứng viên.
//!
//! Facebook không có API công khai cho người xem, nên (giống yt-dlp) ta đọc dữ liệu JSON
//! mà trang video nhúng sẵn. Cấu trúc này Facebook đổi thường xuyên → đây là phần dễ gãy nhất;
//! khi gãy, người dùng có thể dán thẳng link .mpd.

use crate::http::Http;
use crate::ytdlp::{self, NotInstalled};
use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Source {
    /// Manifest tải được qua URL (làm mới được → bám live edge tốt hơn).
    pub url: Option<String>,
    /// Manifest nhúng sẵn (từ trang hoặc dựng tổng hợp). Chỉ là ảnh chụp, không làm mới được.
    pub xml: Option<String>,
    /// Gốc để giải URL tương đối.
    pub base: String,
}

#[derive(Clone, Debug, Default)]
pub struct Found {
    pub title: Option<String>,
    pub sources: Vec<Source>,
    /// Header HTTP nên dùng khi tải manifest và media. yt-dlp cho biết; với Facebook là
    /// `User-Agent: facebookexternalhit/1.1` để khỏi bị giới hạn tốc độ.
    pub headers: Vec<(String, String)>,
    /// Link được lấy bằng cách nào (để hiển thị).
    pub via: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolverMode {
    /// yt-dlp (máy chủ nếu có cấu hình, rồi tại chỗ), cuối cùng mới tới bộ cào tích hợp.
    Auto,
    /// Chỉ yt-dlp.
    YtDlp,
    /// Chỉ bộ cào tích hợp (không cần cài gì nhưng dễ gãy khi Facebook đổi cấu trúc).
    Builtin,
}

impl ResolverMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "ytdlp" | "yt-dlp" => Self::YtDlp,
            "builtin" | "tich-hop" => Self::Builtin,
            _ => Self::Auto,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolverConfig {
    pub mode: ResolverMode,
    pub ytdlp: ytdlp::Local,
    /// Máy chủ yt-dlp (cho thiết bị không chạy được yt-dlp, như Android).
    pub remote: Option<ytdlp::Remote>,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self { mode: ResolverMode::Auto, ytdlp: ytdlp::Local::default(), remote: None }
    }
}

#[derive(Clone, Copy)]
enum Step {
    Remote,
    Local,
    Builtin,
}

impl Step {
    fn label(self) -> &'static str {
        match self {
            Step::Remote => "máy chủ yt-dlp",
            Step::Local => "yt-dlp",
            Step::Builtin => "bộ cào tích hợp",
        }
    }
}

// Kết quả lấy link được nhớ vài phút: bấm dừng rồi phát lại (hoặc đổi mức độ trễ, vốn phải phát lại)
// không phải chờ yt-dlp thêm vài giây nữa.
static CACHE: Mutex<Vec<(String, Instant, Found)>> = Mutex::new(Vec::new());
const CACHE_TTL: Duration = Duration::from_secs(180);

fn cache_get(key: &str) -> Option<Found> {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|(_, t, _)| t.elapsed() < CACHE_TTL);
    c.iter().find(|(k, _, _)| k == key).map(|(_, _, f)| f.clone())
}

fn cache_put(key: &str, f: &Found) {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|(k, _, _)| k != key);
    c.push((key.to_string(), Instant::now(), f.clone()));
    if c.len() > 8 {
        c.remove(0);
    }
}

/// Quên kết quả đã nhớ (khi manifest đã nhớ hết hạn / không dùng được).
pub fn forget(input: &str) {
    if let Ok(u) = pick_url(input) {
        let key = normalize(&u);
        CACHE.lock().unwrap_or_else(|e| e.into_inner()).retain(|(k, _, _)| *k != key);
    }
}

/// Từ link Facebook (hoặc link .mpd) → manifest DASH. Trả thêm `true` nếu lấy từ bộ nhớ đệm.
/// `progress` nhận mô tả bước đang làm để hiện lên giao diện.
pub fn resolve(
    http: &Http,
    input: &str,
    cfg: &ResolverConfig,
    stop: &AtomicBool,
    progress: &dyn Fn(&str),
    fresh: bool,
) -> Result<(Found, bool)> {
    let url = pick_url(input)?;
    if is_mpd(&url) {
        let src = Source { url: Some(url.clone()), xml: None, base: url };
        return Ok((Found { sources: vec![src], via: "link .mpd", ..Default::default() }, false));
    }
    let page = normalize(&url);
    if !fresh {
        if let Some(f) = cache_get(&page) {
            log::info!("dùng kết quả đã nhớ ({})", f.via);
            return Ok((f, true));
        }
    }

    let mut steps = Vec::new();
    if cfg.mode != ResolverMode::Builtin {
        if cfg.remote.is_some() {
            steps.push(Step::Remote);
        }
        steps.push(Step::Local);
    }
    if cfg.mode != ResolverMode::YtDlp {
        steps.push(Step::Builtin);
    }

    let (mut why, mut ytdlp_missing) = (Vec::new(), false);
    for step in steps {
        if stop.load(Relaxed) {
            bail!("Đã dừng");
        }
        let r = match step {
            Step::Remote => {
                progress("Đang hỏi máy chủ yt-dlp");
                ytdlp::run_remote(&page, cfg.remote.as_ref().expect("đã kiểm tra"))
            }
            Step::Local => {
                progress("Đang hỏi yt-dlp");
                ytdlp::run_local(&page, &cfg.ytdlp, stop, progress)
            }
            Step::Builtin => {
                progress("Đang đọc trang Facebook");
                builtin(http, &page)
            }
        };
        match r {
            Ok(found) => {
                log::info!("lấy link bằng {}", found.via);
                cache_put(&page, &found);
                return Ok((found, false));
            }
            Err(e) if e.downcast_ref::<NotInstalled>().is_some() => {
                log::info!("{}: {e}", step.label());
                ytdlp_missing = true;
            }
            Err(e) => {
                log::warn!("{}: {e:#}", step.label());
                why.push(format!("{}: {}", step.label(), short(&format!("{e:#}"))));
            }
        }
    }
    bail!(explain(&why, ytdlp_missing, cfg))
}

fn explain(why: &[String], ytdlp_missing: bool, cfg: &ResolverConfig) -> String {
    let mut s = String::from("Không lấy được luồng âm thanh từ link này.");
    for w in why {
        s.push_str("\n• ");
        s.push_str(w);
    }
    if ytdlp_missing && cfg.mode != ResolverMode::Builtin {
        s.push_str("\n• yt-dlp chưa được cài (Windows: winget install yt-dlp.yt-dlp). Android không chạy được yt-dlp: dùng “máy chủ yt-dlp” ở mục Nâng cao.");
    }
    s.push_str("\nThử cập nhật yt-dlp (yt-dlp -U) hoặc dán trực tiếp link .mpd.");
    s
}

fn short(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(220).collect()
}

/// Bộ cào tích hợp: đọc JSON mà trang video nhúng sẵn. Không cần cài gì nhưng dễ gãy khi Facebook
/// đổi cấu trúc — vì vậy yt-dlp là nguồn chính.
fn builtin(http: &Http, page: &str) -> Result<Found> {
    let (bytes, _) = http.get(page, 16 << 20)?;
    scan(&String::from_utf8_lossy(&bytes), page)
}

pub fn scan(html: &str, page_url: &str) -> Result<Found> {
    // Cấu trúc hiện hành (theo extractor Facebook của yt-dlp 2026.08):
    //   videoDeliveryResponseResult.dash_manifest_urls[i].manifest_url  ↔  dash_manifests[i].manifest_xml
    // Cấu trúc cũ: playable_url_dash / dash_manifest_url / dash_manifest.
    let mut urls: Vec<String> = Vec::new();
    for key in ["manifest_url", "playable_url_dash", "dash_manifest_url"] {
        for v in json_strings(html, key) {
            if v.starts_with("http") && !urls.contains(&v) {
                urls.push(v);
            }
        }
    }
    let mut xmls: Vec<String> = Vec::new();
    for key in ["manifest_xml", "dash_manifest", "dash_manifest_xml_string"] {
        for v in json_strings(html, key) {
            let xml = if v.trim_start().starts_with('<') { v } else { pct_decode(&v) };
            if xml.contains("<MPD") && !xmls.contains(&xml) {
                xmls.push(xml);
            }
        }
    }

    // Ghép bản XML nhúng với URL cùng thứ tự (như yt-dlp): XML dùng URL làm gốc cho đường dẫn tương
    // đối, còn URL cho phép làm mới manifest khi nghe live.
    let paired = urls.len() == xmls.len();
    let mut sources: Vec<Source> = Vec::new();
    for (i, u) in urls.iter().enumerate() {
        sources.push(Source { url: Some(u.clone()), xml: paired.then(|| xmls[i].clone()), base: u.clone() });
    }
    if !paired {
        for x in xmls {
            sources.push(Source { url: None, xml: Some(x), base: page_url.to_string() });
        }
    }
    if sources.is_empty() {
        bail!(
            "không thấy dữ liệu video trong trang (video riêng tư / cần đăng nhập, hoặc Facebook đã đổi \
             cấu trúc trang)"
        );
    }
    Ok(Found { title: og_title(html), sources, headers: Vec::new(), via: "bộ cào tích hợp" })
}

/// Người dùng thường dán cả câu chia sẻ; chỉ lấy URL đầu tiên.
fn pick_url(input: &str) -> Result<String> {
    let t = input.trim();
    let start = t.find("https://").or_else(|| t.find("http://"));
    let raw = match start {
        Some(i) => &t[i..],
        None if t.contains("facebook.com") || t.contains("fb.watch") || t.contains("fb.com") => t,
        None => bail!("Hãy dán link Facebook Live (hoặc link .mpd)."),
    };
    let end = raw
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
        .unwrap_or(raw.len());
    let url = raw[..end].trim_end_matches([')', '.', ',', ';']);
    Ok(if url.starts_with("http") { url.to_string() } else { format!("https://{url}") })
}

fn is_mpd(url: &str) -> bool {
    url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase().ends_with(".mpd")
}

fn normalize(url: &str) -> String {
    let mut u = url.to_string();
    for h in ["m.facebook.com", "mbasic.facebook.com", "web.facebook.com", "touch.facebook.com"] {
        u = u.replace(&format!("://{h}/"), "://www.facebook.com/");
    }
    u
}

/// Mọi giá trị chuỗi JSON của khoá `"key":"..."` trong văn bản (đã giải mã \/ \u0026 ...).
fn json_strings(h: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let b = h.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = h[from..].find(&needle) {
        let mut p = from + i + needle.len();
        from = p;
        while p < b.len() && b[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= b.len() || b[p] != b':' {
            continue;
        }
        p += 1;
        while p < b.len() && b[p].is_ascii_whitespace() {
            p += 1;
        }
        if p >= b.len() || b[p] != b'"' {
            continue;
        }
        let start = p + 1;
        let mut q = start;
        let mut esc = false;
        while q < b.len() {
            match b[q] {
                _ if esc => esc = false,
                b'\\' => esc = true,
                b'"' => break,
                _ => {}
            }
            q += 1;
        }
        if q >= b.len() {
            break;
        }
        if let Ok(s) = serde_json::from_str::<String>(&format!("\"{}\"", &h[start..q])) {
            out.push(s);
        }
        from = q + 1;
    }
    out
}

fn og_title(html: &str) -> Option<String> {
    let i = html.find("property=\"og:title\"")?;
    let tag_start = html[..i].rfind('<')?;
    let tag_end = i + html[i..].find('>')?;
    let tag = &html[tag_start..tag_end];
    let c = tag.find("content=\"")? + "content=\"".len();
    let len = tag[c..].find('"')?;
    let t = html_unescape(&tag[c..c + len]);
    (!t.trim().is_empty()).then(|| t.trim().to_string())
}

fn html_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(j) = tail.find(';').filter(|&j| j <= 10) {
            let ent = &tail[1..j];
            let ch = match ent {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "apos" => Some('\''),
                _ => ent
                    .strip_prefix('#')
                    .and_then(|n| match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => n.parse().ok(),
                    })
                    .and_then(char::from_u32),
            };
            if let Some(c) = ch {
                out.push(c);
                rest = &tail[j + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

fn pct_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = "https://www.facebook.com/x/videos/1";

    #[test]
    fn legacy_structure_url_paired_with_inline_xml_and_title() {
        let html = r#"<meta property="og:title" content="Live &amp; vui"/><script>{"video":{"playable_url_dash":"https:\/\/video.xx.fbcdn.net\/v\/live.mpd?a=1\u0026b=2","dash_manifest":"<MPD type=\"dynamic\"><\/MPD>","x":null}}</script>"#;
        let f = scan(html, PAGE).unwrap();
        assert_eq!(f.title.as_deref(), Some("Live & vui"));
        assert_eq!(f.sources.len(), 1, "URL và XML cùng thứ tự được ghép làm một nguồn");
        assert_eq!(f.sources[0].url.as_deref(), Some("https://video.xx.fbcdn.net/v/live.mpd?a=1&b=2"));
        assert!(f.sources[0].xml.as_deref().unwrap().starts_with("<MPD"));
        assert_eq!(f.via, "bộ cào tích hợp");
    }

    /// Cấu trúc mà extractor Facebook của yt-dlp 2026.08 đọc: khoá `manifest_url` nằm trong
    /// `dash_manifest_urls[]`. Bộ cào cũ không biết khoá này nên báo "không tìm thấy luồng".
    #[test]
    fn current_relay_structure_with_manifest_url_key() {
        let html = r#"<script type="application/json" data-sjs>{"require":[["RelayPrefetchedStreamCache","next",[],["x",{"__bbox":{"result":{"data":{"video":{"videoDeliveryResponseFragment":{"videoDeliveryResponseResult":{"dash_manifest_urls":[{"manifest_url":"https:\/\/video-sin6-1.xx.fbcdn.net\/o1\/v\/t2\/a.mpd?oh=1\u0026oe=2"}],"dash_manifests":[{"manifest_xml":"<MPD type=\"dynamic\"><\/MPD>"}],"progressive_urls":[{"progressive_url":"https:\/\/x.fbcdn.net\/a.mp4"}],"hls_playlist_urls":[]}}}}}}]]]}</script>"#;
        let f = scan(html, PAGE).unwrap();
        assert_eq!(f.sources.len(), 1);
        assert_eq!(f.sources[0].url.as_deref(), Some("https://video-sin6-1.xx.fbcdn.net/o1/v/t2/a.mpd?oh=1&oe=2"));
        assert!(f.sources[0].xml.is_some());
        assert_eq!(f.sources[0].base, f.sources[0].url.clone().unwrap(), "XML dùng URL làm gốc");
    }

    #[test]
    fn unpaired_inline_xml_uses_page_as_base_and_empty_page_errors() {
        let html = r#"{"manifest_xml":"<MPD><\/MPD>"}"#;
        let f = scan(html, PAGE).unwrap();
        assert_eq!((f.sources[0].url.as_deref(), f.sources[0].base.as_str()), (None, PAGE));
        assert!(scan("<html>login</html>", PAGE).is_err());
    }

    #[test]
    fn picks_url_from_share_text() {
        let u = pick_url("Xem live nè: https://fb.watch/abc123/ nhé!").unwrap();
        assert_eq!(u, "https://fb.watch/abc123/");
        assert!(pick_url("hello").is_err());
        assert!(is_mpd("https://a/b/live.MPD?x=1"));
    }

    fn quiet(cfg: &ResolverConfig, input: &str) -> Result<(Found, bool)> {
        resolve(&Http::new(), input, cfg, &AtomicBool::new(false), &|_| {}, true)
    }

    #[test]
    fn direct_mpd_skips_all_resolvers() {
        let (f, cached) = quiet(&ResolverConfig::default(), "http://127.0.0.1:1/live.mpd?a=1").unwrap();
        assert_eq!((f.via, cached), ("link .mpd", false));
        assert_eq!(f.sources[0].url.as_deref(), Some("http://127.0.0.1:1/live.mpd?a=1"));
    }

    #[test]
    fn missing_ytdlp_gives_actionable_message() {
        let mut cfg = ResolverConfig::default();
        cfg.mode = ResolverMode::YtDlp;
        cfg.ytdlp.path = Some("/không/tồn/tại/yt-dlp".into());
        let e = quiet(&cfg, PAGE).unwrap_err().to_string();
        assert!(e.contains("yt-dlp chưa được cài") && e.contains("winget") && e.contains(".mpd"), "{e}");
        assert!(!e.contains("bộ cào"), "mode YtDlp không được lặng lẽ rơi về bộ cào: {e}");
    }

    #[cfg(unix)]
    #[test]
    fn result_is_cached_and_forgotten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fbaudio-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let p = dir.join("yt-dlp");
        std::fs::write(&p, format!(r#"#!/bin/sh
echo x >> "{}"
echo '{{"title":"T","formats":[{{"protocol":"http_dash_segments","manifest_url":"http://h/a.mpd","vcodec":"none","acodec":"mp4a.40.2"}}]}}'
"#, counter.display())).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // chờ file ghi xong hẳn rồi mới chạy, tránh "Text file busy" khi test khác đang fork
        std::thread::sleep(Duration::from_millis(100));

        let mut cfg = ResolverConfig::default();
        cfg.mode = ResolverMode::YtDlp;
        cfg.ytdlp.path = Some(p);
        cfg.ytdlp.auto_update = false;
        let url = "https://fb.watch/cache-test-1";
        let run = |fresh| resolve(&Http::new(), url, &cfg, &AtomicBool::new(false), &|_| {}, fresh).unwrap();
        assert!(!run(false).1);
        assert!(run(false).1, "lần hai phải lấy từ bộ nhớ đệm");
        assert_eq!(std::fs::read_to_string(&counter).unwrap().lines().count(), 1, "yt-dlp chỉ được chạy một lần");
        forget(url);
        assert!(!run(false).1, "sau khi quên phải hỏi lại yt-dlp");
        assert_eq!(std::fs::read_to_string(&counter).unwrap().lines().count(), 2);
    }
}
