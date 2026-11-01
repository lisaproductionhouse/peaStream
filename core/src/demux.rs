//! Bóc container của luồng byte thô để lấy các access unit AAC.
//!
//! Công cụ ngoài có thể ghi ra hai dạng (tự nhận biết qua vài byte đầu):
//!  * **ADTS** — AAC thô; yt-dlp + ffmpeg cho live DASH (ta ép `-f adts`);
//!  * **fMP4** — yt-dlp với bộ tải gốc cho nội dung đã kết thúc (init + các fragment nối tiếp).
//!
//! Cả hai đều xử lý theo luồng: nhận byte tới đâu nhả access unit tới đó, đúng với mọi ranh giới chia mẩu.

use crate::fmp4::{Event, Fmp4Parser, TrackInfo, RATES};
use anyhow::{bail, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

pub enum Ev<'a> {
    /// Tham số codec (lần đầu, hoặc khi thay đổi).
    Config(&'a TrackInfo),
    /// Một access unit AAC thô.
    Frame(&'a [u8]),
}

enum Kind {
    Sniffing,
    Adts,
    Fmp4,
}

pub struct Demux {
    kind: Kind,
    head: Vec<u8>,
    fmp4: Fmp4Parser,
    adts: Adts,
}

impl Demux {
    pub fn new() -> Self {
        Self { kind: Kind::Sniffing, head: Vec::new(), fmp4: Fmp4Parser::new(), adts: Adts::default() }
    }

    pub fn feed(&mut self, data: &[u8], sink: &mut dyn FnMut(Ev<'_>) -> Result<()>) -> Result<()> {
        if matches!(self.kind, Kind::Sniffing) {
            self.head.extend_from_slice(data);
            if self.head.len() < 8 {
                return Ok(());
            }
            self.kind = sniff(&self.head)?;
            let buffered = std::mem::take(&mut self.head);
            return self.feed_known(&buffered, sink);
        }
        self.feed_known(data, sink)
    }

    fn feed_known(&mut self, data: &[u8], sink: &mut dyn FnMut(Ev<'_>) -> Result<()>) -> Result<()> {
        match self.kind {
            Kind::Adts => self.adts.feed(data, sink),
            Kind::Fmp4 => self.fmp4.feed(data, &mut |ev| match ev {
                Event::Init(t) => sink(Ev::Config(t)),
                Event::Sample { data, .. } => sink(Ev::Frame(data)),
            }),
            Kind::Sniffing => unreachable!("đã nhận dạng ở feed()"),
        }
    }
}

fn sniff(b: &[u8]) -> Result<Kind> {
    if b[0] == 0xFF && (b[1] & 0xF6) == 0xF0 {
        return Ok(Kind::Adts);
    }
    if matches!(&b[4..8], b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx" | b"free" | b"skip" | b"mdat" | b"emsg") {
        return Ok(Kind::Fmp4);
    }
    if b[0] == 0x47 {
        bail!("Công cụ ghi ra MPEG-TS (đây là định dạng yt-dlp dùng cho HLS) — chưa hỗ trợ; cần AAC dạng ADTS hoặc fMP4");
    }
    if b[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        bail!("Công cụ ghi ra Matroska/WebM — chưa hỗ trợ; cần AAC dạng ADTS hoặc fMP4");
    }
    bail!(
        "Không nhận ra định dạng luồng (8 byte đầu: {}). Cần AAC dạng ADTS hoặc fMP4.",
        b[..8].iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
    )
}

// ------------------------------------------------------------------------------------------ ADTS

#[derive(Default)]
struct Adts {
    buf: Vec<u8>,
    cfg: Option<(u8, u8, u8)>, // (profile, chỉ số tần số, kênh)
}

impl Adts {
    fn feed(&mut self, data: &[u8], sink: &mut dyn FnMut(Ev<'_>) -> Result<()>) -> Result<()> {
        self.buf.extend_from_slice(data);
        let mut pos = 0usize;
        loop {
            let b = &self.buf[pos..];
            if b.len() < 7 {
                break;
            }
            // đồng bộ 0xFFF, layer 00
            if !(b[0] == 0xFF && (b[1] & 0xF6) == 0xF0) {
                pos += 1;
                continue;
            }
            let hdr = if b[1] & 1 == 1 { 7 } else { 9 }; // protection_absent
            let (profile, idx) = ((b[2] >> 6) & 3, (b[2] >> 2) & 0xF);
            let ch = ((b[2] & 1) << 2) | (b[3] >> 6);
            let len = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | ((b[5] as usize) >> 5);
            if len <= hdr || idx as usize >= 13 {
                pos += 1; // tiêu đề giả
                continue;
            }
            if b.len() < len {
                break; // chờ đủ khung
            }
            // khung kế tiếp (nếu đã có) phải cũng bắt đầu bằng đồng bộ, nếu không đây chỉ là trùng ngẫu nhiên
            if b.len() >= len + 2 && !(b[len] == 0xFF && (b[len + 1] & 0xF6) == 0xF0) {
                pos += 1;
                continue;
            }
            if self.cfg != Some((profile, idx, ch)) {
                self.cfg = Some((profile, idx, ch));
                sink(Ev::Config(&track_info(profile, idx, ch)))?;
            }
            sink(Ev::Frame(&b[hdr..len]))?;
            pos += len;
        }
        self.buf.drain(..pos);
        Ok(())
    }
}

/// Dựng AudioSpecificConfig 2 byte từ tiêu đề ADTS. HE-AAC trong ADTS luôn báo kiểu "ngầm": tiêu đề ghi
/// AAC-LC ở tần số LÕI, phần SBR nằm trong access unit.
fn track_info(profile: u8, idx: u8, ch: u8) -> TrackInfo {
    let aot = profile + 1; // 1 = Main, 2 = LC, 3 = SSR, 4 = LTP
    let asc = vec![(aot << 3) | (idx >> 1), ((idx & 1) << 7) | (ch << 3)];
    let rate = RATES[idx as usize];
    TrackInfo { track_id: 1, timescale: rate, asc, sample_rate: rate, channels: ch as u16, object_type: aot, ext_rate: None }
}

// ------------------------------------------------------------------------------------- lọc lặp

/// Loại các access unit bị phát lặp.
///
/// Demuxer DASH live của ffmpeg (đã quan sát trên 6.1.1) đôi khi phát RA HAI LẦN mỗi đoạn live. Một AU bị
/// coi là lặp khi nó giống hệt một AU trước đó mà:
///  * cách đây 16..16384 AU (đoạn lặp cách nhau cả chục AU; thấp hơn là khung giống nhau liên tiếp),
///  * mới xuất hiện đúng một lần (khung im lặng CBR giống hệt nhau xuất hiện rất nhiều lần nên không bao giờ bị loại),
///  * dài ≥ 64 byte (khung ngắn quá dễ trùng).
pub struct Dedupe {
    seen: HashMap<u64, (u32, u64)>, // hash → (số lần, chỉ số AU gần nhất)
    order: VecDeque<(u64, u64)>,    // (chỉ số, hash) để quên dần
    idx: u64,
    pub dropped: u64,
}

impl Dedupe {
    pub fn new() -> Self {
        Self { seen: HashMap::new(), order: VecDeque::new(), idx: 0, dropped: 0 }
    }

    pub fn is_dup(&mut self, au: &[u8]) -> bool {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        au.hash(&mut h);
        let h = h.finish();
        let i = self.idx;
        self.idx += 1;
        let prev = self.seen.get(&h).copied();
        let e = self.seen.entry(h).or_insert((0, i));
        e.0 += 1;
        e.1 = i;
        self.order.push_back((i, h));
        while self.order.front().is_some_and(|(j, _)| i - *j > 16_384) {
            let (j, hh) = self.order.pop_front().unwrap();
            if self.seen.get(&hh).is_some_and(|e| e.1 == j) {
                self.seen.remove(&hh);
            }
        }
        let dup = au.len() >= 64 && matches!(prev, Some((1, p)) if (16..=16_384).contains(&(i - p)));
        if dup {
            self.dropped += 1;
        }
        dup
    }
}

// ---------------------------------------------------------------------------------- nối tiếp các lượt

/// Số access unit nhớ lại để nhận ra phần chồng lấn giữa các lượt (~3 phút âm thanh).
const STITCH_CAP: usize = 8192;

/// Nối tiếp giữa các lượt chạy của công cụ.
///
/// Với Facebook, yt-dlp chỉ tải các đoạn có trong bản manifest chụp lúc tải trang rồi thoát (đã gặp: 11 đoạn),
/// không bám live. Để nghe tiếp, app chạy lại công cụ; lượt sau luôn bắt đầu bằng đoạn CŨ NHẤT của cửa sổ mới,
/// tức là một phần chồng lấn với lượt trước. Bộ này bỏ phần đầu đó (mọi AU đã thấy ở lượt trước), và chỉ giữ từ
/// access unit mới đầu tiên trở đi.
///
/// Chỉ xét ở ĐẦU lượt (tiền tố): sau access unit mới đầu tiên thì giữ hết, kể cả khi có AU trùng ngẫu nhiên.
/// AU dưới 64 byte (khung im lặng giống hệt nhau) trong tiền tố được coi là thuộc tiền tố.
pub struct Stitch {
    hist: HashSet<u64>,
    order: VecDeque<u64>,
    in_prefix: bool,
    /// Số access unit MỚI (chưa từng thấy) trong lượt hiện tại.
    pub new_aus: u64,
}

impl Stitch {
    pub fn new() -> Self {
        Self { hist: HashSet::new(), order: VecDeque::new(), in_prefix: false, new_aus: 0 }
    }

    /// Bắt đầu một lượt chạy mới của công cụ.
    pub fn begin_run(&mut self) {
        self.in_prefix = !self.hist.is_empty();
        self.new_aus = 0;
    }

    /// `true` = access unit này là nội dung mới, giữ lại.
    pub fn accept(&mut self, au: &[u8]) -> bool {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        au.hash(&mut h);
        let h = h.finish();
        if self.in_prefix {
            if au.len() < 64 || self.hist.contains(&h) {
                return false;
            }
            self.in_prefix = false;
        }
        if self.hist.insert(h) {
            self.order.push_back(h);
            if self.order.len() > STITCH_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.hist.remove(&old);
                }
            }
        }
        self.new_aus += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LC_ADTS: &[u8] = include_bytes!("../tests/data/lc.adts");
    const HE_ADTS: &[u8] = include_bytes!("../tests/data/he1.adts");
    const INIT: &[u8] = include_bytes!("../tests/data/init.m4s");
    const SEG1: &[u8] = include_bytes!("../tests/data/seg-1.m4s");
    const SEG2: &[u8] = include_bytes!("../tests/data/seg-2.m4s");

    /// (cấu hình đã nhận, các access unit) khi nạp `data` theo từng mẩu `step` byte.
    fn run(data: &[u8], step: usize) -> (Vec<TrackInfo>, Vec<Vec<u8>>) {
        let mut d = Demux::new();
        let (mut cfgs, mut frames) = (Vec::new(), Vec::new());
        for chunk in data.chunks(step) {
            d.feed(chunk, &mut |ev| {
                match ev {
                    Ev::Config(t) => cfgs.push(t.clone()),
                    Ev::Frame(f) => frames.push(f.to_vec()),
                }
                Ok(())
            })
            .unwrap();
        }
        (cfgs, frames)
    }

    #[test]
    fn adts_is_parsed_with_correct_config_and_any_chunking() {
        let (cfgs, frames) = run(LC_ADTS, usize::MAX);
        assert_eq!(cfgs.len(), 1);
        let c = &cfgs[0];
        assert_eq!((c.sample_rate, c.channels, c.object_type, c.asc.as_slice()), (44_100, 2, 2, [0x12, 0x10].as_slice()));
        assert!((84..=90).contains(&frames.len()), "{} khung", frames.len()); // 2 s ≈ 86 khung
        for step in [1, 3, 7, 100, 4096] {
            let (c2, f2) = run(LC_ADTS, step);
            assert_eq!(c2.len(), 1, "step {step}");
            assert!(f2 == frames, "AU khác nhau với step {step}");
        }
    }

    #[test]
    fn he_aac_in_adts_reports_core_rate() {
        let (cfgs, frames) = run(HE_ADTS, usize::MAX);
        // ADTS không báo SBR tường minh: tiêu đề ghi LC ở tần số lõi 24 kHz
        assert_eq!((cfgs[0].sample_rate, cfgs[0].object_type, cfgs[0].ext_rate), (24_000, 2, None));
        assert!(frames.len() > 40);
    }

    #[test]
    fn fmp4_gives_the_same_frames_as_the_same_audio_in_adts() {
        let mut mp4 = Vec::new();
        for p in [INIT, SEG1, SEG2] {
            mp4.extend_from_slice(p);
        }
        let (c_mp4, f_mp4) = run(&mp4, 1000);
        let (_, f_adts) = run(LC_ADTS, 1000);
        assert_eq!(c_mp4[0].object_type, 2);
        assert_eq!(f_mp4.len(), f_adts.len());
        assert!(f_mp4 == f_adts, "cùng một âm thanh phải cho cùng các access unit dù nằm trong ADTS hay fMP4");
    }

    #[test]
    fn resyncs_after_garbage_and_never_panics() {
        let mut data = vec![0xFF, 0xF1, 0x00]; // tiêu đề cụt
        data.extend_from_slice(&[0x12; 50]);
        data.extend_from_slice(LC_ADTS);
        // đoạn rác ở đầu khiến không nhận ra định dạng → báo lỗi rõ ràng, không panic
        let mut d = Demux::new();
        assert!(d.feed(&data[3..], &mut |_| Ok(())).is_err());
        // rác chen GIỮA luồng ADTS: bỏ qua rồi đồng bộ lại
        let mid = LC_ADTS.len() / 2;
        let mut spliced = LC_ADTS[..mid].to_vec();
        spliced.extend_from_slice(&[0x55; 37]);
        spliced.extend_from_slice(&LC_ADTS[mid..]);
        let (_, frames) = run(&spliced, 500);
        let (_, clean) = run(LC_ADTS, 500);
        assert!(frames.len() + 3 >= clean.len() && frames.len() <= clean.len(), "{} vs {}", frames.len(), clean.len());
    }

    #[test]
    fn unsupported_containers_get_a_specific_message() {
        for (bytes, expect) in [
            (vec![0x47, 0x40, 0x00, 0x10, 0, 0, 0, 0, 0], "MPEG-TS"),
            (vec![0x1A, 0x45, 0xDF, 0xA3, 0x9F, 0x42, 0x86, 0x81], "Matroska"),
            (b"<html>nope".to_vec(), "Không nhận ra"),
        ] {
            let e = Demux::new().feed(&bytes, &mut |_| Ok(())).unwrap_err().to_string();
            assert!(e.contains(expect), "{e}");
        }
        // dưới 8 byte: chưa đủ để nhận dạng, chưa báo lỗi
        assert!(Demux::new().feed(&[1, 2, 3], &mut |_| Ok(())).is_ok());
    }

    #[test]
    fn dedupe_drops_replayed_segments_but_keeps_silence_and_real_audio() {
        let au = |n: u32| -> Vec<u8> {
            (0..100u32).map(|k| (n.wrapping_mul(2654435761).wrapping_add(k * 97) >> 5) as u8).collect()
        };
        // 3 đoạn 40 AU; mỗi đoạn live được phát ra HAI lần, đúng kiểu ffmpeg
        let mut stream: Vec<Vec<u8>> = Vec::new();
        for seg in 0..3u32 {
            for _ in 0..2 {
                stream.extend((0..40).map(|k| au(seg * 40 + k)));
            }
        }
        let mut d = Dedupe::new();
        let kept: Vec<_> = stream.iter().filter(|a| !d.is_dup(a)).collect();
        assert_eq!(kept.len(), 120, "phải còn đúng 3×40 AU, mất {} bản lặp", stream.len() - kept.len());
        assert_eq!(d.dropped, 120);

        // im lặng CBR: hàng trăm AU giống hệt (kể cả sau một đoạn tiếng nói) không được bị coi là lặp
        let silence = vec![0u8; 278];
        let mut d = Dedupe::new();
        let mut dropped = 0;
        for round in 0..5 {
            for _ in 0..200 {
                dropped += d.is_dup(&silence) as u32;
            }
            for k in 0..60 {
                dropped += d.is_dup(&au(10_000 + round * 100 + k)) as u32;
            }
        }
        assert!(dropped <= 1, "không được loại khung im lặng: loại {dropped}");

        // âm thanh thật không lặp thì giữ nguyên toàn bộ
        let mut d = Dedupe::new();
        assert!((0..2000).all(|k| !d.is_dup(&au(50_000 + k))));
    }

    #[test]
    fn stitch_skips_the_overlap_prefix_of_each_new_run_and_keeps_only_new_audio() {
        let au = |n: u32| -> Vec<u8> {
            (0..100u32).map(|k| (n.wrapping_mul(2654435761).wrapping_add(k * 97) >> 5) as u8).collect()
        };
        let window = |a: u32, b: u32| (a..b).map(au).collect::<Vec<_>>();
        let mut s = Stitch::new();
        let mut run = |s: &mut Stitch, w: Vec<Vec<u8>>| -> Vec<Vec<u8>> {
            s.begin_run();
            w.into_iter().filter(|a| s.accept(a)).collect()
        };
        // lượt 1: cả cửa sổ là mới
        assert_eq!(run(&mut s, window(0, 40)).len(), 40);
        assert_eq!(s.new_aus, 40);
        // lượt 2: cửa sổ trượt (20..60): 20 AU đầu chồng lấn → chỉ 20..40 đã thấy, 40..60 mới
        let kept = run(&mut s, window(20, 60));
        assert!(kept == window(40, 60), "phải giữ đúng 40..60, giữ {} AU", kept.len());
        assert_eq!(s.new_aus, 20);
        // lượt 3: không có gì mới (live chưa ra thêm) → 0 AU mới
        assert_eq!(run(&mut s, window(20, 60)).len(), 0);
        assert_eq!(s.new_aus, 0);
        // lượt 4: có thêm 60..70
        assert!(run(&mut s, window(30, 70)) == window(60, 70));
        // cửa sổ trượt quá xa, không còn chồng lấn (mất một khúc): giữ toàn bộ
        assert_eq!(run(&mut s, window(500, 520)).len(), 20);
    }

    #[test]
    fn stitch_treats_small_silence_frames_in_the_prefix_as_overlap_but_keeps_everything_after() {
        let big = |n: u8| vec![n; 200];
        let silence = vec![0u8; 20];
        let mut s = Stitch::new();
        s.begin_run();
        for a in [big(1), silence.clone(), silence.clone(), big(2)] {
            assert!(s.accept(&a));
        }
        s.begin_run();
        // đầu lượt sau: big(1), im lặng, big(2) (đã thấy) rồi big(3) mới, rồi trùng ngẫu nhiên big(1) giữa lượt
        let seq = [big(1), silence.clone(), silence.clone(), big(2), big(3), big(1), silence.clone()];
        let kept: Vec<bool> = seq.iter().map(|a| s.accept(a)).collect();
        assert_eq!(kept, [false, false, false, false, true, true, true], "{kept:?}");
    }
}
