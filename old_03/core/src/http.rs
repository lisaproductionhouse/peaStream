//! HTTP đồng bộ, giữ kết nối (keep-alive) để các đoạn liên tiếp không phải bắt tay TLS lại,
//! và đọc theo luồng để nhận dữ liệu ngay khi server đẩy từng chunk (CMAF low-latency).

use anyhow::{anyhow, Result};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// UA trình duyệt (cùng loại yt-dlp dùng): để tải trang và manifest.
const UA_BROWSER: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
/// Facebook giới hạn tốc độ tải media nếu dùng UA trình duyệt; yt-dlp phải dùng UA này cho mọi định
/// dạng. Với phát trực tiếp, bị giới hạn tốc độ nghĩa là đoạn về chậm → giật.
const UA_FB_MEDIA: &str = "facebookexternalhit/1.1";

/// Lỗi theo mã trạng thái HTTP, để phía trên phân biệt "đoạn chưa có" (404) với "mất mạng".
#[derive(Debug)]
pub struct StatusError(pub u16);

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}
impl std::error::Error for StatusError {}

pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<StatusError>().map(|s| s.0)
}

#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    /// Header bổ sung cho MỌI request (ví dụ các header yt-dlp yêu cầu khi tải media).
    headers: Arc<Vec<(String, String)>>,
}

impl Http {
    pub fn new() -> Self {
        Self::with_timeouts(Duration::from_secs(6), Duration::from_secs(10))
    }

    pub fn with_timeouts(connect: Duration, read: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(UA_BROWSER)
            .timeout_connect(connect)
            .timeout_read(read)
            .max_idle_connections_per_host(4)
            .redirects(5)
            .build();
        Self { agent, headers: Arc::new(Vec::new()) }
    }

    /// Bản sao dùng chung kết nối, thêm các header này vào mọi request.
    ///
    /// Bỏ những header do tầng HTTP tự quản lý: `Accept-Encoding` (yt-dlp có thể khai báo `br`/`zstd`
    /// mà ta không giải nén được → nội dung hỏng), `Host`, `Connection`, `Content-Length`, `Range`…
    pub fn with_headers(&self, headers: &[(String, String)]) -> Self {
        const SKIP: [&str; 7] =
            ["accept-encoding", "host", "connection", "content-length", "transfer-encoding", "range", "te"];
        let kept = headers
            .iter()
            .filter(|(k, _)| !SKIP.iter().any(|s| k.eq_ignore_ascii_case(s)))
            .cloned()
            .collect();
        Self { agent: self.agent.clone(), headers: Arc::new(kept) }
    }

    fn request(&self, url: &str) -> ureq::Request {
        let mut req = self
            .agent
            .get(url)
            .set("Accept", "*/*")
            .set("Accept-Language", "en-US,en;q=0.9");
        let has_ua = self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
        if !has_ua && is_fb_media_host(url) {
            req = req.set("User-Agent", UA_FB_MEDIA);
        }
        for (k, v) in self.headers.iter() {
            req = req.set(k, v);
        }
        req
    }

    fn open(&self, url: &str) -> Result<ureq::Response> {
        let req = self.request(url);
        match req.call() {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, _)) => Err(StatusError(code).into()),
            Err(e) => Err(anyhow!(e).context("không kết nối được máy chủ")),
        }
    }

    /// Như `get` nhưng KHÔNG coi mã lỗi HTTP là lỗi: trả (mã, nội dung) để đọc được thông báo của máy chủ.
    pub fn get_raw(&self, url: &str, max: u64) -> Result<(u16, Vec<u8>)> {
        let (code, resp) = match self.request(url).call() {
            Ok(r) => (r.status(), r),
            Err(ureq::Error::Status(code, r)) => (code, r),
            Err(e) => return Err(anyhow!(e).context("không kết nối được máy chủ")),
        };
        let mut body = Vec::new();
        resp.into_reader().take(max).read_to_end(&mut body)?;
        Ok((code, body))
    }

    /// Tải trọn nội dung (tối đa `max` byte) + thời gian máy chủ từ header `Date`.
    pub fn get(&self, url: &str, max: u64) -> Result<(Vec<u8>, Option<SystemTime>)> {
        let resp = self.open(url)?;
        let date = resp
            .header("Date")
            .and_then(|d| httpdate::parse_http_date(d).ok());
        let mut body = Vec::new();
        resp.into_reader().take(max).read_to_end(&mut body)?;
        Ok((body, date))
    }

    /// Đọc theo luồng: gọi `f` ngay khi có byte. `f` trả `false` để dừng sớm.
    pub fn stream(&self, url: &str, mut f: impl FnMut(&[u8]) -> Result<bool>) -> Result<()> {
        let resp = self.open(url)?;
        let mut r = resp.into_reader();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 || !f(&buf[..n])? {
                return Ok(());
            }
        }
    }
}

/// CDN media của Facebook (video-*.fbcdn.net, scontent-*.fbcdn.net…).
fn is_fb_media_host(url: &str) -> bool {
    let host = url.split_once("://").map_or(url, |(_, r)| r).split('/').next().unwrap_or("");
    host.ends_with("fbcdn.net")
}

/// Mã hoá phần trăm cho giá trị tham số query.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Giải URL tương đối theo `base` (đủ dùng cho BaseURL / template của MPD).
pub fn join_url(base: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let (host, path_q) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if let Some(r) = rel.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    if rel.starts_with('/') {
        return format!("{scheme}://{host}{}", dots(rel));
    }
    let path = path_q.split(['?', '#']).next().unwrap_or("/");
    let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
    format!("{scheme}://{host}{}", dots(&format!("{dir}{rel}")))
}

/// Chuẩn hoá "./" và "../" trong phần path (giữ nguyên query).
fn dots(p: &str) -> String {
    let (path, q) = match p.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (p, None),
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s),
        }
    }
    let mut s = out.join("/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    if let Some(q) = q {
        s.push('?');
        s.push_str(q);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{is_fb_media_host, join_url, pct_encode};

    #[test]
    fn unsafe_headers_from_ytdlp_are_dropped() {
        let h = super::Http::new().with_headers(&[
            ("User-Agent".into(), "facebookexternalhit/1.1".into()),
            ("Accept-Encoding".into(), "gzip, deflate, br".into()),
            ("range".into(), "bytes=0-".into()),
        ]);
        assert_eq!(h.headers.len(), 1);
        assert_eq!(h.headers[0].0, "User-Agent");
    }

    #[test]
    fn encodes_and_detects_hosts() {
        assert_eq!(pct_encode("https://fb.watch/a b?x=1&y=é"), "https%3A%2F%2Ffb.watch%2Fa%20b%3Fx%3D1%26y%3D%C3%A9");
        assert!(is_fb_media_host("https://video-sin6-1.xx.fbcdn.net/o1/v/t2/x.mpd?a=1"));
        assert!(!is_fb_media_host("https://www.facebook.com/x/videos/1"));
    }

    #[test]
    fn joins() {
        let b = "https://cdn.x.com/live/a/manifest.mpd?sig=1";
        assert_eq!(join_url(b, "seg-1.m4s"), "https://cdn.x.com/live/a/seg-1.m4s");
        assert_eq!(join_url(b, "../v/seg.m4s?x=a/b"), "https://cdn.x.com/live/v/seg.m4s?x=a/b");
        assert_eq!(join_url(b, "/root/seg.m4s"), "https://cdn.x.com/root/seg.m4s");
        assert_eq!(join_url(b, "//other.com/s.m4s"), "https://other.com/s.m4s");
        assert_eq!(join_url(b, "https://abs.com/s"), "https://abs.com/s");
    }
}
