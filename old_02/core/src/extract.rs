//! Từ link Facebook Live (hoặc link .mpd trực tiếp) → danh sách manifest DASH ứng viên.
//!
//! Facebook không có API công khai cho người xem, nên (giống yt-dlp) ta đọc dữ liệu JSON
//! mà trang video nhúng sẵn. Cấu trúc này Facebook đổi thường xuyên → đây là phần dễ gãy nhất;
//! khi gãy, người dùng có thể dán thẳng link .mpd.

use crate::http::Http;
use anyhow::{bail, Result};

#[derive(Clone, Debug)]
pub struct Source {
    /// Manifest tải được qua URL (làm mới được → bám live edge tốt hơn).
    pub url: Option<String>,
    /// Manifest nhúng sẵn trong trang (chỉ là ảnh chụp, không làm mới được).
    pub xml: Option<String>,
    /// Gốc để giải URL tương đối.
    pub base: String,
}

pub struct Found {
    pub title: Option<String>,
    pub sources: Vec<Source>,
}

pub fn resolve(http: &Http, input: &str) -> Result<Found> {
    let url = pick_url(input)?;
    if is_mpd(&url) {
        let src = Source { url: Some(url.clone()), xml: None, base: url };
        return Ok(Found { title: None, sources: vec![src] });
    }
    let page = normalize(&url);
    let (bytes, _) = http.get(&page, 16 << 20)?;
    scan(&String::from_utf8_lossy(&bytes), &page)
}

pub fn scan(html: &str, page_url: &str) -> Result<Found> {
    let mut sources: Vec<Source> = Vec::new();

    for key in ["playable_url_dash", "dash_manifest_url"] {
        for v in json_strings(html, key) {
            if v.starts_with("http") && !sources.iter().any(|s| s.url.as_deref() == Some(&v)) {
                sources.push(Source { url: Some(v.clone()), xml: None, base: v });
            }
        }
    }
    for key in ["manifest_xml", "dash_manifest", "dash_manifest_xml_string"] {
        for v in json_strings(html, key) {
            let xml = if v.trim_start().starts_with('<') { v } else { pct_decode(&v) };
            if xml.contains("<MPD") && !sources.iter().any(|s| s.xml.as_deref() == Some(&xml)) {
                sources.push(Source { url: None, xml: Some(xml), base: page_url.to_string() });
            }
        }
    }
    if sources.is_empty() {
        bail!(
            "Không tìm thấy luồng âm thanh trong trang Facebook (video riêng tư, cần đăng nhập, \
             hoặc Facebook đã đổi cấu trúc). Hãy thử dán trực tiếp link .mpd."
        );
    }
    Ok(Found { title: og_title(html), sources })
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

    #[test]
    fn finds_url_inline_xml_and_title() {
        let html = r#"<meta property="og:title" content="Live &amp; vui"/><script>{"video":{"playable_url_dash":"https:\/\/video.xx.fbcdn.net\/v\/live.mpd?a=1\u0026b=2","dash_manifest":"<MPD type=\"dynamic\"><\/MPD>","x":null}}</script>"#;
        let f = scan(html, "https://www.facebook.com/x/videos/1").unwrap();
        assert_eq!(f.title.as_deref(), Some("Live & vui"));
        assert_eq!(f.sources[0].url.as_deref(), Some("https://video.xx.fbcdn.net/v/live.mpd?a=1&b=2"));
        assert!(f.sources[1].xml.as_deref().unwrap().starts_with("<MPD"));
    }

    #[test]
    fn picks_url_from_share_text() {
        let u = pick_url("Xem live nè: https://fb.watch/abc123/ nhé!").unwrap();
        assert_eq!(u, "https://fb.watch/abc123/");
        assert!(pick_url("hello").is_err());
        assert!(is_mpd("https://a/b/live.MPD?x=1"));
    }

    #[test]
    fn empty_page_errors() {
        assert!(scan("<html>login</html>", "https://www.facebook.com/").is_err());
    }
}
