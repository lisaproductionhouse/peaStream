//! HTTP đồng bộ, giữ kết nối (keep-alive) để các đoạn liên tiếp không phải bắt tay TLS lại,
//! và đọc theo luồng để nhận dữ liệu ngay khi server đẩy từng chunk (CMAF low-latency).

use anyhow::{anyhow, Result};
use std::io::Read;
use std::time::{Duration, SystemTime};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

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
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(UA)
            .timeout_connect(Duration::from_secs(6))
            .timeout_read(Duration::from_secs(10))
            .max_idle_connections_per_host(4)
            .redirects(5)
            .build();
        Self { agent }
    }

    fn open(&self, url: &str) -> Result<ureq::Response> {
        let req = self
            .agent
            .get(url)
            .set("Accept", "*/*")
            .set("Accept-Language", "en-US,en;q=0.9");
        match req.call() {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, _)) => Err(StatusError(code).into()),
            Err(e) => Err(anyhow!(e).context("không kết nối được máy chủ")),
        }
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
    use super::join_url;

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
