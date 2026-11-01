//! Phân tích MPD (DASH) và lập lịch đoạn cho phát trực tiếp.
//! Chỉ chọn Representation âm thanh: luồng video không bao giờ được tải.

use crate::http::join_url;
use anyhow::{anyhow, bail, Context, Result};
use roxmltree::Node;

#[derive(Debug, Clone)]
pub struct Seg {
    pub t: u64,
    pub d: u64,
}

#[derive(Debug, Clone)]
pub enum SegSource {
    /// SegmentTemplate ($Number$ hoặc $Time$), có thể kèm SegmentTimeline.
    Template { media: String, start_number: u64, timescale: u64, duration: u64, timeline: Vec<Seg> },
    /// Một file fMP4 liền (BaseURL) — ví dụ video Facebook đã phát xong.
    Single(String),
}

#[derive(Debug, Clone)]
pub struct AudioRep {
    pub id: String,
    pub bandwidth: u64,
    pub codecs: String,
    pub init_url: Option<String>,
    pub source: SegSource,
}

#[derive(Debug, Clone)]
pub struct Mpd {
    pub live: bool,
    pub ast: Option<f64>,
    pub period_start: f64,
    pub audio: AudioRep,
}

/// Vị trí một đoạn: số thứ tự và thời điểm (đơn vị timescale).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub number: u64,
    pub time: u64,
}

pub fn parse(xml: &str, base_url: &str) -> Result<Mpd> {
    let doc = roxmltree::Document::parse(xml).context("MPD không phải XML hợp lệ")?;
    let root = doc.root_element();
    if root.tag_name().name() != "MPD" {
        bail!("Không phải manifest MPD");
    }
    let live = root.attribute("type") == Some("dynamic");
    let ast = root.attribute("availabilityStartTime").and_then(iso_time);
    let base0 = with_base(base_url, root);
    let period = kids(root, "Period").last().ok_or_else(|| anyhow!("MPD không có Period"))?;
    let period_start = period.attribute("start").and_then(duration).unwrap_or(0.0);
    let pbase = with_base(&base0, period);

    let mut best: Option<AudioRep> = None;
    for aset in kids(period, "AdaptationSet") {
        let abase = with_base(&pbase, aset);
        for rep in kids(aset, "Representation") {
            if !is_audio(aset, rep) {
                continue;
            }
            let rbase = with_base(&abase, rep);
            if let Some(r) = build_rep(aset, rep, &rbase) {
                if best.as_ref().map_or(true, |b| r.bandwidth > b.bandwidth) {
                    best = Some(r);
                }
            }
        }
    }
    let audio = best.ok_or_else(|| anyhow!("Manifest không có luồng âm thanh dùng được (không có audio, hoặc kiểu đánh địa chỉ đoạn chưa hỗ trợ)"))?;
    Ok(Mpd { live, ast, period_start, audio })
}

fn build_rep(aset: Node, rep: Node, rbase: &str) -> Option<AudioRep> {
    let id = rep.attribute("id").unwrap_or("").to_string();
    let bandwidth: u64 = num(rep, "bandwidth").unwrap_or(0);
    let codecs = inherit(rep, aset, "codecs").unwrap_or("").to_string();

    let tpl = kids(rep, "SegmentTemplate").next().or_else(|| kids(aset, "SegmentTemplate").next());
    if let Some(t) = tpl {
        let init_url = t
            .attribute("initialization")
            .map(|i| expand(&join_url(rbase, i), &id, bandwidth, None, None));
        let source = SegSource::Template {
            media: join_url(rbase, t.attribute("media")?),
            start_number: num(t, "startNumber").unwrap_or(1),
            timescale: num(t, "timescale").unwrap_or(1).max(1),
            duration: num(t, "duration").unwrap_or(0),
            timeline: timeline(t),
        };
        return Some(AudioRep { id, bandwidth, codecs, init_url, source });
    }
    if kids(rep, "BaseURL").next().is_some() {
        return Some(AudioRep {
            id,
            bandwidth,
            codecs,
            init_url: None,
            source: SegSource::Single(rbase.to_string()),
        });
    }
    None
}

fn timeline(t: Node) -> Vec<Seg> {
    let Some(tl) = kids(t, "SegmentTimeline").next() else { return Vec::new() };
    let mut out = Vec::new();
    let mut cur = 0u64;
    for s in kids(tl, "S") {
        let d = num(s, "d").unwrap_or(0);
        if let Some(t0) = num(s, "t") {
            cur = t0;
        }
        let r: i64 = s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0);
        for _ in 0..(r.max(0) + 1) {
            out.push(Seg { t: cur, d });
            cur += d;
        }
    }
    out
}

impl Mpd {
    /// Đoạn đầu tiên (dùng cho nội dung không phải live).
    pub fn first(&self) -> Cursor {
        match &self.audio.source {
            SegSource::Template { start_number, timeline, .. } => {
                Cursor { number: *start_number, time: timeline.first().map_or(0, |s| s.t) }
            }
            SegSource::Single(_) => Cursor { number: 0, time: 0 },
        }
    }

    /// Đoạn mới nhất đã công bố. `now` = giờ máy chủ ước lượng (epoch, giây).
    pub fn live_edge(&self, now: f64) -> Cursor {
        let SegSource::Template { start_number, timescale, duration, timeline, .. } = &self.audio.source
        else {
            return self.first();
        };
        if let Some(last) = timeline.last() {
            return Cursor { number: start_number + timeline.len() as u64 - 1, time: last.t };
        }
        if let (true, Some(ast)) = (*duration > 0, self.ast) {
            let seg = *duration as f64 / *timescale as f64;
            let n = ((now - ast - self.period_start) / seg).floor();
            if n >= 1.0 {
                let n = n as u64;
                // đoạn thứ n đang được ghi dở → lấy đoạn trước đó
                return Cursor { number: start_number + n - 1, time: (n - 1) * duration };
            }
        }
        self.first()
    }
}

impl AudioRep {
    pub fn url(&self, c: Cursor) -> String {
        match &self.source {
            SegSource::Template { media, .. } => {
                expand(media, &self.id, self.bandwidth, Some(c.number), Some(c.time))
            }
            SegSource::Single(u) => u.clone(),
        }
    }

    fn dur_ticks(&self, c: Cursor) -> u64 {
        let SegSource::Template { duration, timeline, .. } = &self.source else { return 0 };
        timeline
            .iter()
            .find(|s| s.t == c.time)
            .or_else(|| timeline.last())
            .map_or(*duration, |s| s.d)
    }

    /// Độ dài đoạn (giây); 2 s nếu manifest không cho biết.
    pub fn dur_secs(&self, c: Cursor) -> f64 {
        let SegSource::Template { timescale, .. } = &self.source else { return 2.0 };
        match self.dur_ticks(c) {
            0 => 2.0,
            d => d as f64 / *timescale as f64,
        }
    }

    /// Đoạn kế tiếp; nếu vượt phần timeline đã biết thì ngoại suy theo độ dài đoạn cuối.
    pub fn next(&self, c: Cursor) -> Cursor {
        Cursor { number: c.number + 1, time: c.time + self.dur_ticks(c) }
    }
}

/// Thay $RepresentationID$, $Number$, $Time$, $Bandwidth$ (hỗ trợ `$Number%05d$` và `$$`).
pub fn expand(t: &str, rep_id: &str, bw: u64, number: Option<u64>, time: Option<u64>) -> String {
    let mut out = String::with_capacity(t.len() + 16);
    let mut rest = t;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let Some(j) = rest.find('$') else {
            out.push('$');
            break;
        };
        let tok = &rest[..j];
        rest = &rest[j + 1..];
        if tok.is_empty() {
            out.push('$');
            continue;
        }
        let (name, fmt) = match tok.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (tok, None),
        };
        let val = match name {
            "RepresentationID" => {
                out.push_str(rep_id);
                continue;
            }
            "Number" => number,
            "Time" => time,
            "Bandwidth" => Some(bw),
            _ => None,
        };
        match val {
            Some(v) => out.push_str(&fmt_num(v, fmt)),
            None => {
                out.push('$');
                out.push_str(tok);
                out.push('$');
            }
        }
    }
    out.push_str(rest);
    out
}

fn fmt_num(v: u64, fmt: Option<&str>) -> String {
    if let Some(w) = fmt.and_then(|f| f.strip_suffix('d')) {
        let zero = w.starts_with('0');
        let width: usize = w.trim_start_matches('0').parse().unwrap_or(0);
        return if zero { format!("{v:0width$}") } else { format!("{v:width$}") };
    }
    v.to_string()
}

fn kids<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn num(n: Node, a: &str) -> Option<u64> {
    n.attribute(a)?.trim().parse().ok()
}

fn inherit<'a>(rep: Node<'a, '_>, aset: Node<'a, '_>, a: &str) -> Option<&'a str> {
    rep.attribute(a).or_else(|| aset.attribute(a))
}

fn with_base(base: &str, n: Node) -> String {
    match kids(n, "BaseURL").next().and_then(|b| b.text()).map(str::trim) {
        Some(t) if !t.is_empty() => join_url(base, t),
        _ => base.to_string(),
    }
}

fn is_audio(aset: Node, rep: Node) -> bool {
    aset.attribute("contentType") == Some("audio")
        || inherit(rep, aset, "mimeType").unwrap_or("").starts_with("audio")
        || inherit(rep, aset, "codecs").unwrap_or("").starts_with("mp4a")
}

/// ISO-8601 duration ("PT1.5S", "PT0H0M2S") → giây.
fn duration(s: &str) -> Option<f64> {
    let s = s.strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    for (part, is_time) in [(date, false), (time, true)] {
        let mut n = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                n.push(c);
                continue;
            }
            let v: f64 = n.parse().ok()?;
            n.clear();
            total += v * match (c, is_time) {
                ('Y', false) => 31_536_000.0,
                ('M', false) => 2_592_000.0,
                ('W', false) => 604_800.0,
                ('D', false) => 86_400.0,
                ('H', true) => 3600.0,
                ('M', true) => 60.0,
                ('S', true) => 1.0,
                _ => return None,
            };
        }
    }
    Some(total)
}

/// "2024-05-01T10:00:00.250Z" / "+07:00" → epoch giây.
fn iso_time(s: &str) -> Option<f64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let dd: i64 = d.next()?.parse().ok()?;
    let (clock, off) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0i64)
    } else if let Some(i) = time.rfind(['+', '-']) {
        let (c, o) = time.split_at(i);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let (hh, mm) = o[1..].split_once(':').unwrap_or((&o[1..], "0"));
        (c, sign * (hh.parse::<i64>().ok()? * 3600 + mm.parse::<i64>().ok()? * 60))
    } else {
        (time, 0)
    };
    let mut t = clock.split(':');
    let h: f64 = t.next()?.parse().ok()?;
    let mi: f64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    // ngày dân sự → số ngày kể từ 1970-01-01 (thuật toán của Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + h * 3600.0 + mi * 60.0 + sec - off as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" availabilityStartTime="2024-05-01T10:00:00Z">
 <Period id="0" start="PT0S">
  <AdaptationSet contentType="video"><Representation id="v" mimeType="video/mp4" codecs="avc1.4d401f" bandwidth="800000">
    <SegmentTemplate timescale="1000" duration="2000" media="v-$Number$.m4s" startNumber="1"/></Representation></AdaptationSet>
  <AdaptationSet id="1" contentType="audio">
   <Representation id="a1" mimeType="audio/mp4" codecs="mp4a.40.2" bandwidth="96000">
    <SegmentTemplate timescale="48000" initialization="init-$RepresentationID$.m4s"
        media="chunk-$RepresentationID$-$Number%05d$.m4s" startNumber="7">
      <SegmentTimeline><S t="576000" d="96000" r="2"/><S d="95232"/></SegmentTimeline>
    </SegmentTemplate>
   </Representation>
  </AdaptationSet>
 </Period>
</MPD>"#;

    #[test]
    fn parses_audio_only_and_timeline() {
        let m = parse(MPD, "https://cdn.example.com/live/manifest.mpd?sig=1").unwrap();
        assert!(m.live);
        assert_eq!(m.audio.id, "a1");
        assert_eq!(m.audio.init_url.as_deref(), Some("https://cdn.example.com/live/init-a1.m4s"));
        let edge = m.live_edge(0.0);
        assert_eq!(edge, Cursor { number: 10, time: 576_000 + 3 * 96_000 });
        assert_eq!(m.audio.url(edge), "https://cdn.example.com/live/chunk-a1-00010.m4s");
        let n = m.audio.next(edge);
        assert_eq!(n, Cursor { number: 11, time: 864_000 + 95_232 });
        // vượt timeline → ngoại suy bằng độ dài đoạn cuối
        assert_eq!(m.audio.next(n).time, n.time + 95_232);
    }

    #[test]
    fn numbered_template_uses_clock() {
        let xml = r#"<MPD type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z"><Period>
          <AdaptationSet mimeType="audio/mp4"><Representation id="a" bandwidth="64000" codecs="mp4a.40.2">
          <SegmentTemplate timescale="1000" duration="2000" media="s$Number$.m4s" initialization="i.m4s" startNumber="0"/>
          </Representation></AdaptationSet></Period></MPD>"#;
        let m = parse(xml, "https://h/x/m.mpd").unwrap();
        // 101 s sau gốc → đã trọn 50 đoạn, đoạn 50 đang ghi dở → lấy #49
        assert_eq!(m.live_edge(101.0).number, 49);
    }

    #[test]
    fn expand_and_time_parsers() {
        assert_eq!(expand("a-$Number%05d$-$$-$Time$-$RepresentationID$", "r", 5, Some(7), Some(9)), "a-00007-$-9-r");
        assert_eq!(expand("x-$Unknown$", "r", 1, None, None), "x-$Unknown$");
        assert_eq!(duration("PT1.5S"), Some(1.5));
        assert_eq!(duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(iso_time("2000-01-01T00:00:00Z"), Some(946_684_800.0));
        assert_eq!(iso_time("2000-01-01T07:00:00+07:00"), Some(946_684_800.0));
    }

    #[test]
    fn single_file_baseurl() {
        let xml = r#"<MPD type="static"><Period><AdaptationSet contentType="audio"><Representation id="a" codecs="mp4a.40.2" bandwidth="64000">
          <BaseURL>audio.mp4?x=1</BaseURL><SegmentBase indexRange="0-100"/></Representation></AdaptationSet></Period></MPD>"#;
        let m = parse(xml, "https://h/p/m.mpd").unwrap();
        assert!(matches!(&m.audio.source, SegSource::Single(u) if u == "https://h/p/audio.mp4?x=1"));
    }

    #[test]
    fn video_only_is_rejected() {
        let xml = r#"<MPD><Period><AdaptationSet contentType="video"><Representation id="v" mimeType="video/mp4" bandwidth="1">
          <BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;
        assert!(parse(xml, "https://h/m.mpd").is_err());
    }
}
