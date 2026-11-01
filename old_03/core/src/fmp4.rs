//! Bộ phân tích fMP4/CMAF dạng luồng: nhận byte tới đâu, nhả từng AAC frame tới đó.
//! Với CMAF low-latency (mỗi chunk là một cặp moof+mdat nhỏ) âm thanh được giải mã ngay
//! khi chunk đầu tiên về, không cần chờ hết đoạn.

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub track_id: u32,
    pub timescale: u32,
    /// AudioSpecificConfig (từ hộp esds).
    pub asc: Vec<u8>,
    /// Tần số lấy mẫu của phần lõi AAC. Với HE-AAC đây là NỬA tần số đầu ra sau SBR.
    pub sample_rate: u32,
    pub channels: u16,
    /// 2 = AAC-LC, 5 = HE-AAC (SBR), 29 = HE-AAC v2 (SBR + PS) …
    pub object_type: u8,
    /// Tần số đầu ra sau SBR (chỉ có khi ASC báo hiệu tường minh, ví dụ luồng Facebook).
    pub ext_rate: Option<u32>,
}

impl TrackInfo {
    pub fn is_he(&self) -> bool {
        matches!(self.object_type, 5 | 29)
    }
}

#[allow(dead_code)] // dts/dur: dành cho ước lượng độ trễ theo mốc thời gian media
pub enum Event<'a> {
    Init(&'a TrackInfo),
    Sample { data: &'a [u8], dts: u64, dur: u32 },
}

#[derive(Default, Clone, Copy)]
struct Trex {
    dur: u32,
    size: u32,
}

struct SampleRef {
    /// Vị trí mẫu, tính từ byte đầu tiên của hộp moof.
    off: usize,
    size: usize,
    dur: u32,
}

struct Frag {
    base: u64,
    samples: Vec<SampleRef>,
}

#[derive(Default)]
pub struct Fmp4Parser {
    buf: Vec<u8>,
    /// Vị trí tuyệt đối của buf[0] trong luồng.
    consumed: u64,
    track: Option<TrackInfo>,
    trex: Trex,
    moof: Option<(u64, Frag)>,
}

impl Fmp4Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bỏ dữ liệu dở dang khi bắt đầu tải đoạn mới; giữ thông tin track (init segment).
    pub fn reset_stream(&mut self) {
        self.buf.clear();
        self.consumed = 0;
        self.moof = None;
    }

    pub fn feed(
        &mut self,
        data: &[u8],
        sink: &mut dyn FnMut(Event<'_>) -> Result<()>,
    ) -> Result<()> {
        self.buf.extend_from_slice(data);
        let buf = std::mem::take(&mut self.buf);
        let mut pos = 0usize;
        let res = self.run(&buf, &mut pos, sink);
        if res.is_ok() {
            self.consumed += pos as u64;
            self.buf = buf[pos..].to_vec();
        } else {
            // dữ liệu hỏng: bỏ hết, đợi đoạn kế
            self.consumed += buf.len() as u64;
            self.moof = None;
        }
        res
    }

    fn run(
        &mut self,
        buf: &[u8],
        pos: &mut usize,
        sink: &mut dyn FnMut(Event<'_>) -> Result<()>,
    ) -> Result<()> {
        loop {
            let rest = &buf[*pos..];
            if rest.len() < 8 {
                return Ok(());
            }
            let mut size = u32_at(rest, 0)? as usize;
            let mut hdr = 8usize;
            if size == 1 {
                if rest.len() < 16 {
                    return Ok(());
                }
                size = u64_at(rest, 8)? as usize;
                hdr = 16;
            } else if size == 0 {
                bail!("hộp MP4 không xác định kích thước (size=0) chưa được hỗ trợ");
            }
            if size < hdr {
                bail!("hộp MP4 hỏng");
            }
            if rest.len() < size {
                return Ok(()); // chờ thêm byte
            }
            let typ = [rest[4], rest[5], rest[6], rest[7]];
            let body = &rest[hdr..size];
            let abs = self.consumed + *pos as u64;
            match &typ {
                b"moov" => {
                    let (info, trex) = parse_moov(body)?;
                    self.trex = trex;
                    self.track = Some(info);
                    if let Some(t) = self.track.as_ref() {
                        sink(Event::Init(t))?;
                    }
                }
                b"moof" => {
                    if let Some(t) = &self.track {
                        self.moof = parse_moof(body, t.track_id, self.trex)?.map(|f| (abs, f));
                    }
                }
                b"mdat" => {
                    if let Some((moof_abs, frag)) = self.moof.take() {
                        let body_abs = abs + hdr as u64;
                        let mut dts = frag.base;
                        for s in &frag.samples {
                            let start = moof_abs + s.off as u64;
                            if start < body_abs {
                                bail!("mẫu nằm ngoài mdat");
                            }
                            let a = (start - body_abs) as usize;
                            let b = a + s.size;
                            if b > body.len() {
                                bail!("mẫu vượt quá mdat");
                            }
                            if s.size > 0 {
                                sink(Event::Sample { data: &body[a..b], dts, dur: s.dur })?;
                            }
                            dts += s.dur as u64;
                        }
                    }
                }
                _ => {}
            }
            *pos += size;
        }
    }
}

fn parse_moof(moof: &[u8], track_id: u32, trex: Trex) -> Result<Option<Frag>> {
    for (t, traf) in boxes(moof)? {
        if &t != b"traf" {
            continue;
        }
        let mut tid = 0u32;
        let (mut def_dur, mut def_size) = (trex.dur, trex.size);
        let mut base = 0u64;
        let mut samples = Vec::new();
        let mut cursor: Option<i64> = None;
        for (t2, b) in boxes(traf)? {
            match &t2 {
                b"tfhd" => {
                    let flags = u32_at(b, 0)? & 0x00ff_ffff;
                    tid = u32_at(b, 4)?;
                    let mut p = 8;
                    if flags & 0x01 != 0 {
                        p += 8; // base_data_offset (không dùng được khi stream)
                    }
                    if flags & 0x02 != 0 {
                        p += 4;
                    }
                    if flags & 0x08 != 0 {
                        def_dur = u32_at(b, p)?;
                        p += 4;
                    }
                    if flags & 0x10 != 0 {
                        def_size = u32_at(b, p)?;
                    }
                }
                b"tfdt" => {
                    base = if b.first() == Some(&1) { u64_at(b, 4)? } else { u32_at(b, 4)? as u64 };
                }
                b"trun" => {
                    let flags = u32_at(b, 0)? & 0x00ff_ffff;
                    let count = u32_at(b, 4)? as usize;
                    let mut p = 8;
                    let mut off = if flags & 0x01 != 0 {
                        let v = u32_at(b, p)? as i32 as i64;
                        p += 4;
                        v
                    } else {
                        cursor.unwrap_or(0)
                    };
                    if flags & 0x04 != 0 {
                        p += 4;
                    }
                    for _ in 0..count {
                        let dur = if flags & 0x100 != 0 {
                            let v = u32_at(b, p)?;
                            p += 4;
                            v
                        } else {
                            def_dur
                        };
                        let size = if flags & 0x200 != 0 {
                            let v = u32_at(b, p)?;
                            p += 4;
                            v
                        } else {
                            def_size
                        };
                        if flags & 0x400 != 0 {
                            p += 4;
                        }
                        if flags & 0x800 != 0 {
                            p += 4;
                        }
                        if off < 0 {
                            bail!("data_offset âm");
                        }
                        samples.push(SampleRef { off: off as usize, size: size as usize, dur });
                        off += size as i64;
                    }
                    cursor = Some(off);
                }
                _ => {}
            }
        }
        if tid == track_id {
            return Ok(Some(Frag { base, samples }));
        }
    }
    Ok(None)
}

fn parse_moov(moov: &[u8]) -> Result<(TrackInfo, Trex)> {
    let mut trex = Trex::default();
    if let Some(mvex) = find(moov, b"mvex")? {
        for (t, b) in boxes(mvex)? {
            if &t == b"trex" {
                trex = Trex { dur: u32_at(b, 12)?, size: u32_at(b, 16)? };
            }
        }
    }
    for (t, trak) in boxes(moov)? {
        if &t != b"trak" {
            continue;
        }
        let Some(mdia) = find(trak, b"mdia")? else { continue };
        let Some(hdlr) = find(mdia, b"hdlr")? else { continue };
        if hdlr.get(8..12) != Some(b"soun".as_slice()) {
            continue;
        }
        let tkhd = find(trak, b"tkhd")?.ok_or_else(|| anyhow!("thiếu tkhd"))?;
        let track_id = u32_at(tkhd, if tkhd.first() == Some(&1) { 20 } else { 12 })?;
        let mdhd = find(mdia, b"mdhd")?.ok_or_else(|| anyhow!("thiếu mdhd"))?;
        let timescale = u32_at(mdhd, if mdhd.first() == Some(&1) { 20 } else { 12 })?;
        let stsd = path(mdia, &[b"minf", b"stbl", b"stsd"])?.ok_or_else(|| anyhow!("thiếu stsd"))?;
        let entries = boxes(stsd.get(8..).ok_or_else(|| anyhow!("stsd ngắn"))?)?;
        let (etype, entry) = entries.into_iter().next().ok_or_else(|| anyhow!("stsd rỗng"))?;
        if &etype != b"mp4a" {
            bail!("codec âm thanh “{}” chưa được hỗ trợ (cần AAC)", String::from_utf8_lossy(&etype));
        }
        let version = u16_at(entry, 8)?;
        let entry_channels = u16_at(entry, 16)?;
        let entry_rate = u32_at(entry, 24)? >> 16;
        let skip = 28 + if version == 1 { 16 } else { 0 };
        let esds = find(entry.get(skip..).ok_or_else(|| anyhow!("mp4a ngắn"))?, b"esds")?
            .ok_or_else(|| anyhow!("thiếu esds"))?;
        let asc = parse_esds(esds)?;
        let (object_type, sample_rate, channels, ext_rate) = match parse_asc(&asc) {
            Some(a) => (a.aot, a.rate, if a.channels == 7 { 8 } else { a.channels }, a.ext_rate),
            None => (2, entry_rate, entry_channels, None),
        };
        let info =
            TrackInfo { track_id, timescale, asc, sample_rate, channels, object_type, ext_rate };
        return Ok((info, trex));
    }
    bail!("không tìm thấy track âm thanh trong init segment")
}

fn read_desc(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *d.first()?;
    let mut i = 1;
    let mut len = 0usize;
    for _ in 0..4 {
        let b = *d.get(i)?;
        i += 1;
        len = (len << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    Some((tag, d.get(i..i + len)?, d.get(i + len..)?))
}

fn parse_esds(esds: &[u8]) -> Result<Vec<u8>> {
    let bad = || anyhow!("esds không hợp lệ");
    let (tag, es, _) = read_desc(esds.get(4..).ok_or_else(bad)?).ok_or_else(bad)?;
    if tag != 0x03 {
        return Err(bad());
    }
    let flags = *es.get(2).ok_or_else(bad)?;
    let mut p = 3;
    if flags & 0x80 != 0 {
        p += 2;
    }
    if flags & 0x40 != 0 {
        p += 1 + *es.get(p).ok_or_else(bad)? as usize;
    }
    if flags & 0x20 != 0 {
        p += 2;
    }
    let mut rest = es.get(p..).ok_or_else(bad)?;
    while let Some((t, body, r)) = read_desc(rest) {
        if t == 0x04 {
            let mut inner = body.get(13..).ok_or_else(bad)?;
            while let Some((t2, b2, r2)) = read_desc(inner) {
                if t2 == 0x05 {
                    return Ok(b2.to_vec());
                }
                inner = r2;
            }
        }
        rest = r;
    }
    Err(anyhow!("esds không có AudioSpecificConfig"))
}

const RATES: [u32; 13] =
    [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

struct Asc {
    aot: u8,
    rate: u32,
    channels: u16,
    ext_rate: Option<u32>,
}

fn parse_asc(asc: &[u8]) -> Option<Asc> {
    struct Bits<'a>(&'a [u8], usize);
    impl Bits<'_> {
        fn read(&mut self, n: usize) -> Option<u32> {
            let mut v = 0u32;
            for _ in 0..n {
                let byte = *self.0.get(self.1 / 8)?;
                v = (v << 1) | ((byte >> (7 - self.1 % 8)) & 1) as u32;
                self.1 += 1;
            }
            Some(v)
        }
    }
    let mut b = Bits(asc, 0);
    let mut aot = b.read(5)? as u8;
    if aot == 31 {
        aot = 32 + b.read(6)? as u8;
    }
    let idx = b.read(4)? as usize;
    let rate = if idx == 15 { b.read(24)? } else { *RATES.get(idx)? };
    let channels = b.read(4)? as u16;
    // HE-AAC báo hiệu tường minh phân cấp: tần số ở trên là của phần lõi, kế tiếp là tần số sau SBR
    let ext_rate = if aot == 5 || aot == 29 {
        let i = b.read(4)? as usize;
        Some(if i == 15 { b.read(24)? } else { *RATES.get(i)? })
    } else {
        None
    };
    Some(Asc { aot, rate, channels, ext_rate })
}

// ---- tiện ích đọc hộp ----

fn boxes(mut d: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut v = Vec::new();
    while d.len() >= 8 {
        let mut size = u32_at(d, 0)? as usize;
        let mut hdr = 8;
        if size == 1 {
            size = u64_at(d, 8)? as usize;
            hdr = 16;
        } else if size == 0 {
            size = d.len();
        }
        if size < hdr || size > d.len() {
            bail!("hộp MP4 con bị cắt hoặc hỏng");
        }
        v.push(([d[4], d[5], d[6], d[7]], &d[hdr..size]));
        d = &d[size..];
    }
    Ok(v)
}

fn find<'a>(d: &'a [u8], typ: &[u8; 4]) -> Result<Option<&'a [u8]>> {
    Ok(boxes(d)?.into_iter().find(|(t, _)| t == typ).map(|(_, b)| b))
}

fn path<'a>(d: &'a [u8], p: &[&[u8; 4]]) -> Result<Option<&'a [u8]>> {
    let mut cur = d;
    for t in p {
        match find(cur, t)? {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

fn u16_at(b: &[u8], p: usize) -> Result<u16> {
    b.get(p..p + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or_else(|| anyhow!("MP4: đọc vượt biên"))
}
fn u32_at(b: &[u8], p: usize) -> Result<u32> {
    b.get(p..p + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| anyhow!("MP4: đọc vượt biên"))
}
fn u64_at(b: &[u8], p: usize) -> Result<u64> {
    b.get(p..p + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or_else(|| anyhow!("MP4: đọc vượt biên"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::AacDec;

    // 440 Hz, tạo bằng ffmpeg `-f dash` (LC) hoặc bộ mã hóa FDK + ffmpeg (HE-AAC; ASC đã được vá sang
    // báo hiệu tường minh AOT 5 / 29 giống luồng Facebook). Mỗi bộ: init + hai đoạn fMP4 dài 1 s.
    const LC: [&[u8]; 3] = [
        include_bytes!("../tests/data/init.m4s"),
        include_bytes!("../tests/data/seg-1.m4s"),
        include_bytes!("../tests/data/seg-2.m4s"),
    ];
    const HE1: [&[u8]; 3] = [
        include_bytes!("../tests/data/he1-init.m4s"),
        include_bytes!("../tests/data/he1-seg-1.m4s"),
        include_bytes!("../tests/data/he1-seg-2.m4s"),
    ];
    const HE2: [&[u8]; 3] = [
        include_bytes!("../tests/data/he2-init.m4s"),
        include_bytes!("../tests/data/he2-seg-1.m4s"),
        include_bytes!("../tests/data/he2-seg-2.m4s"),
    ];

    struct Out {
        frames: usize,
        pcm: Vec<f32>,
        info: TrackInfo,
        rate: u32,
        channels: usize,
        core_only: bool,
    }

    /// Nạp init + hai đoạn theo từng mẩu `step` byte (như dữ liệu mạng đến ở ranh giới bất kỳ).
    fn run_parts(parts: &[&[u8]; 3], step: usize) -> Out {
        let mut parser = Fmp4Parser::new();
        let mut dec: Option<AacDec> = None;
        let (mut frames, mut pcm, mut info) = (0usize, Vec::new(), None);
        for (i, data) in parts.iter().enumerate() {
            if i > 0 {
                parser.reset_stream(); // giống player: mỗi đoạn là một response mới
            }
            for chunk in data.chunks(step) {
                parser
                    .feed(chunk, &mut |ev| {
                        match ev {
                            Event::Init(t) => {
                                info = Some(t.clone());
                                dec = Some(AacDec::new(t, false)?);
                            }
                            Event::Sample { data, .. } => {
                                frames += 1;
                                dec.as_mut().unwrap().decode(data, &mut pcm)?;
                            }
                        }
                        Ok(())
                    })
                    .unwrap();
            }
        }
        let d = dec.unwrap();
        Out { frames, pcm, info: info.unwrap(), rate: d.rate, channels: d.channels, core_only: d.core_only }
    }

    fn run(step: usize) -> Out {
        run_parts(&LC, step)
    }

    /// (RMS, tần số đỉnh phổ) của kênh đầu, bỏ phần khởi động. Dùng đỉnh phổ (Goertzel, bước 1 Hz)
    /// thay vì đếm điểm cắt không vì SBR/PS thêm nhiễu và hài ở dải cao làm đếm điểm cắt bị lệch.
    fn tone_of(o: &Out) -> (f32, f32) {
        let left: Vec<f32> = o.pcm.iter().step_by(o.channels).copied().skip(4096).collect();
        let rms = (left.iter().map(|v| v * v).sum::<f32>() / left.len() as f32).sqrt();
        let seg = &left[..left.len().min(32_768)];
        let power = |hz: f32| {
            let w = std::f32::consts::TAU * hz / o.rate as f32;
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in seg {
                let s0 = x + 2.0 * w.cos() * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2
        };
        let peak = (380..=500).map(|h| h as f32).max_by(|a, b| power(*a).total_cmp(&power(*b))).unwrap();
        (rms, peak)
    }

    #[test]
    fn init_segment_is_understood() {
        let o = run(usize::MAX);
        let info = &o.info;
        assert_eq!((info.sample_rate, info.channels, info.object_type), (44_100, 2, 2));
        assert!(!info.is_he() && info.ext_rate.is_none());
        assert_eq!(info.timescale, 44_100);
    }

    #[test]
    fn identical_output_for_any_chunking() {
        let o = run(usize::MAX);
        assert!((84..=90).contains(&o.frames), "frames = {}", o.frames); // 2 s ≈ 86 frame × 1024 mẫu
        assert_eq!(o.pcm.len(), o.frames * 1024 * 2);
        // dữ liệu có thể đến ở ranh giới bất kỳ (CMAF chunked): kết quả phải bit-exact
        for step in [1, 3, 7, 100, 1000, 4096] {
            let p = run(step);
            assert_eq!(p.frames, o.frames, "step {step}");
            assert!(p.pcm == o.pcm, "PCM khác nhau với step {step}");
        }
    }

    #[test]
    fn decoded_audio_is_the_440hz_tone() {
        let (rms, hz) = tone_of(&run(usize::MAX));
        // ffmpeg sine: biên độ 0,125; mono→stereo −3 dB ⇒ RMS lý thuyết = 0,125/√2/√2 = 0,0625
        assert!((rms - 0.0625).abs() < 0.01, "rms = {rms}");
        assert!((hz - 440.0).abs() <= 3.0, "tần số = {hz} Hz");
    }

    /// Hồi quy từ lỗi thực tế: luồng Facebook dùng HE-AAC (AOT 5, ASC tường minh) và từng bị
    /// từ chối với thông báo "AAC profile 5". Phải giải mã được phần lõi, ĐÚNG cao độ và tốc độ.
    #[test]
    fn he_aac_v1_explicit_signaling_decodes_core_at_right_pitch() {
        let o = run_parts(&HE1, usize::MAX);
        assert_eq!(o.info.object_type, 5);
        assert_eq!((o.info.sample_rate, o.info.ext_rate), (24_000, Some(48_000)));
        assert!(o.info.is_he());
        // mặc định: chỉ phần lõi ở 24 kHz (bộ resample nâng lên); feature `sbr`: đủ SBR ở 48 kHz
        #[cfg(not(feature = "sbr"))]
        assert!(o.core_only && (o.rate, o.channels) == (24_000, 2), "{} Hz {} ch", o.rate, o.channels);
        #[cfg(feature = "sbr")]
        assert!(!o.core_only && (o.rate, o.channels) == (48_000, 2), "{} Hz {} ch", o.rate, o.channels);
        assert!((44..=50).contains(&o.frames), "frames = {}", o.frames); // 2 s × 24000 / 1024 ≈ 47
        let (rms, hz) = tone_of(&o);
        assert!((rms - 0.177).abs() < 0.03, "rms = {rms}"); // 0,25/√2
        assert!((hz - 440.0).abs() <= 3.0, "tần số = {hz} Hz");
        for step in [1, 13, 500] {
            let p = run_parts(&HE1, step);
            assert!(p.pcm == o.pcm, "PCM khác nhau với step {step}");
        }
    }

    #[test]
    fn he_aac_v2_ps_decodes_mono_core_at_right_pitch() {
        let o = run_parts(&HE2, usize::MAX);
        assert_eq!(o.info.object_type, 29);
        assert_eq!((o.info.sample_rate, o.info.ext_rate), (24_000, Some(48_000)));
        // mặc định: lõi đơn kênh 24 kHz (PS bị bỏ); feature `sbr`: SBR + PS → stereo 48 kHz
        #[cfg(not(feature = "sbr"))]
        assert_eq!((o.rate, o.channels), (24_000, 1));
        #[cfg(feature = "sbr")]
        assert_eq!((o.rate, o.channels), (48_000, 2));
        let (rms, hz) = tone_of(&o);
        assert!(rms > 0.1, "rms = {rms}");
        assert!((hz - 440.0).abs() <= 3.0, "tần số = {hz} Hz");
    }

    #[test]
    fn unsupported_profile_is_a_clear_error() {
        let mut info = run(usize::MAX).info;
        info.object_type = 23; // ER AAC LD
        let e = AacDec::new(&info, false).err().unwrap().to_string();
        assert!(e.contains("23") && e.contains("HE-AAC"), "{e}");
    }

    #[test]
    fn garbage_does_not_panic_and_recovers() {
        let mut p = Fmp4Parser::new();
        let mut n = 0;
        let mut sink = |ev: Event<'_>| {
            if matches!(ev, Event::Sample { .. }) {
                n += 1;
            }
            Ok(())
        };
        p.feed(LC[0], &mut sink).unwrap();
        assert!(p.feed(&[0, 0, 0, 0, b'x', b'x', b'x', b'x'], &mut sink).is_err()); // size=0
        p.reset_stream();
        p.feed(LC[1], &mut sink).unwrap(); // vẫn dùng được sau lỗi
        assert!(n > 40);
    }
}
