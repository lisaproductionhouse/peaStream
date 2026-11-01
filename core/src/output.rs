//! Đầu ra âm thanh + kiểm soát độ trễ.
//!
//! * Vòng đệm SPSC không khoá giữa luồng giải mã (producer) và callback âm thanh (consumer).
//! * Callback chạy theo thời gian thực; thiếu dữ liệu thì xuất khoảng lặng, không bao giờ chặn.
//! * Mỗi lần có PCM mới, producer:
//!     1. nếu bộ đệm cạn (đầu phiên / sau underrun) → chèn `margin_ms` khoảng lặng để tạo
//!        lại đệm chống giật,
//!     2. nếu mức đệm thấp nhất gần đây cao hơn mong muốn → phát nhanh hơn tối đa 6%
//!        (resample điều tốc) để thu hẹp độ trễ mà gần như không nghe ra,
//!     3. nếu tụt quá xa live (> `slack_ms`) → bỏ phần cũ, nhảy thẳng tới live.

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const RING_SECONDS: usize = 12;
/// Cửa sổ tìm đáy mức đệm: đủ dài để chứa ít nhất một chu kỳ giao dữ liệu (đoạn DASH ~2 s).
const LOW_WINDOW: Duration = Duration::from_secs(3);
/// Vòng đệm chứa quá chừng này thì chắc chắn đang bị dồn (backlog): cắt cứng, giữ lại đúng `margin`.
const BACKLOG_CAP_MS: f64 = 9_000.0;

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// Đệm chống giật (ms) giữ phía trước điểm phát: nhỏ → trễ thấp hơn nhưng dễ giật hơn.
    pub margin_ms: u32,
    /// Dư bao nhiêu ms so với mức lý tưởng thì cắt nhảy tới live.
    pub slack_ms: u32,
}

pub enum SinkKind {
    /// Loa / tai nghe mặc định.
    Device,
    /// Ghi ra file WAV (kiểm thử không cần thiết bị âm thanh).
    Wav(PathBuf),
}

/// Số liệu chia sẻ giữa callback âm thanh và giao diện.
#[derive(Default)]
pub struct Meter {
    pub fill_frames: AtomicUsize,
    pub underruns: AtomicU32,
    pub started: AtomicBool,
    pub rate: AtomicU32,
}

struct Callback {
    cons: HeapCons<f32>,
    skip: Arc<AtomicUsize>,
    meter: Arc<Meter>,
    stop: Arc<AtomicBool>,
    in_underrun: bool,
}

impl Callback {
    /// Điền `out` (stereo xen kẽ). Không cấp phát, không khoá.
    fn fill(&mut self, out: &mut [f32]) {
        if self.stop.load(Relaxed) {
            out.fill(0.0);
            return;
        }
        let s = self.skip.swap(0, Relaxed);
        if s > 0 {
            self.cons.skip(s * 2);
        }
        let got = self.cons.pop_slice(out);
        if got < out.len() {
            out[got..].fill(0.0);
            if self.meter.started.load(Relaxed) && !self.in_underrun {
                self.meter.underruns.fetch_add(1, Relaxed);
                self.in_underrun = true;
            }
        } else {
            self.in_underrun = false;
        }
        self.meter.fill_frames.store(self.cons.occupied_len() / 2, Relaxed);
    }
}

pub struct Output {
    prod: HeapProd<f32>,
    skip: Arc<AtomicUsize>,
    pub meter: Arc<Meter>,
    dev_rate: u32,
    tuning: Tuning,
    rs: Resampler,
    src_rate: u32,
    speed: f64,
    /// (lúc, mức đệm ms ngay trước khi nhận dữ liệu mới) trong LOW_WINDOW gần nhất → đáy mức đệm.
    lows: VecDeque<(Instant, f64)>,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
    closing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// false = nội dung đã kết thúc (VOD): không có live edge để đuổi theo → chặn nguồn lại thay vì bỏ dữ liệu.
    live: bool,
    /// Dữ liệu về theo từng lượt lớn (vài giây một lần, không đều): giữ liền mạch thay vì đuổi theo live.
    burst_mode: bool,
    // `Stream` không Send → Output phải sống trọn đời trong luồng player.
    _stream: Option<cpal::Stream>,
    sim: Option<JoinHandle<()>>,
}

impl Output {
    pub fn new(tuning: Tuning, kind: SinkKind, stop: Arc<AtomicBool>) -> Result<Self> {
        let meter = Arc::new(Meter::default());
        let skip = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        let stop_out = stop.clone();

        match kind {
            SinkKind::Device => {
                let device = cpal::default_host()
                    .default_output_device()
                    .ok_or_else(|| anyhow!("Không tìm thấy thiết bị phát âm thanh"))?;
                let supported = device.default_output_config()?;
                let fmt = supported.sample_format();
                let cfg: cpal::StreamConfig = supported.config();
                let rate = cfg.sample_rate.0;
                let (prod, cons) = HeapRb::<f32>::new(rate as usize * 2 * RING_SECONDS).split();
                let cb = Callback { cons, skip: skip.clone(), meter: meter.clone(), stop, in_underrun: false };
                let stream = build_stream(&device, &cfg, fmt, cb)?;
                stream.play()?;
                Ok(Self::assemble(prod, skip, meter, rate, tuning, closing, stop_out, Some(stream), None))
            }
            SinkKind::Wav(path) => {
                let rate = 48_000u32;
                let (prod, cons) = HeapRb::<f32>::new(rate as usize * 2 * RING_SECONDS).split();
                let cb = Callback { cons, skip: skip.clone(), meter: meter.clone(), stop, in_underrun: false };
                let wav = Wav::create(&path, rate)?;
                let sim = spawn_wav(wav, rate, cb, closing.clone());
                Ok(Self::assemble(prod, skip, meter, rate, tuning, closing, stop_out, None, Some(sim)))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        prod: HeapProd<f32>,
        skip: Arc<AtomicUsize>,
        meter: Arc<Meter>,
        dev_rate: u32,
        tuning: Tuning,
        closing: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        stream: Option<cpal::Stream>,
        sim: Option<JoinHandle<()>>,
    ) -> Self {
        meter.rate.store(dev_rate, Relaxed);
        Self {
            prod,
            skip,
            meter,
            dev_rate,
            tuning,
            rs: Resampler::new(),
            src_rate: 0,
            speed: 1.0,
            lows: VecDeque::new(),
            stereo: Vec::new(),
            resampled: Vec::new(),
            closing,
            stop,
            burst_mode: false,
            live: true,
            _stream: stream,
            sim,
        }
    }

    /// `false` cho nội dung đã kết thúc (VOD): phát đủ, không cắt nhảy, nguồn bị chặn khi đệm đầy.
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Dữ liệu về theo từng lượt lớn, không đều (công cụ ngoài chạy lại sau mỗi cửa sổ hữu hạn): độ trễ vốn đã bằng cỡ
    /// một lượt, nên ưu tiên liền mạch. Tắt điều tốc (phát nhanh cả lượt nghe lệch cao độ) và chỉ cắt khi đệm quá
    /// dày (> 8 s), thay vì cắt hễ đáy cao hơn `slack`.
    pub fn set_burst_mode(&mut self, on: bool) {
        self.burst_mode = on;
    }

    /// Đẩy PCM float xen kẽ kênh (từ bộ giải mã) vào hàng đợi phát.
    pub fn push(&mut self, pcm: &[f32], src_rate: u32, src_ch: usize) {
        if pcm.is_empty() || src_ch == 0 || src_rate == 0 {
            return;
        }
        if src_rate != self.src_rate {
            self.src_rate = src_rate;
            self.rs = Resampler::new();
        }

        // 1) về stereo
        self.stereo.clear();
        if src_ch == 1 {
            for &s in pcm {
                self.stereo.push(s);
                self.stereo.push(s);
            }
        } else {
            for fr in pcm.chunks_exact(src_ch) {
                self.stereo.push(fr[0]);
                self.stereo.push(fr[1]);
            }
        }

        let dev = self.dev_rate as f64;
        let ms = |frames: usize| frames as f64 * 1000.0 / dev;
        let margin_ms = self.tuning.margin_ms as f64;
        let slack_ms = if self.burst_mode { (self.tuning.slack_ms as f64).max(8_000.0) } else { self.tuning.slack_ms as f64 };

        if !self.live {
            // VOD: tốc độ 1,0, không đuổi live; chỉ dựng đệm lúc khởi động rồi chặn nguồn theo nhịp phát.
            self.resampled.clear();
            self.rs.process(&self.stereo, src_rate as f64 / dev, &mut self.resampled);
            if ms(self.prod.occupied_len() / 2) < 8.0 {
                self.push_silence((margin_ms * dev / 1000.0) as usize);
            }
            self.push_blocking();
            return;
        }

        // 2) đầu phiên / sau underrun: dựng lại đệm chống giật bằng khoảng lặng
        // Phần đã yêu cầu bỏ nhưng callback chưa kịp xử lý vẫn nằm trong vòng đệm: phải trừ đi, nếu không các lần
        // đẩy liên tiếp trong cùng một đợt sẽ tính TRÙNG phần cần bỏ và callback bỏ luôn cả âm thanh mới đẩy vào.
        let pending_skip = self.skip.load(Relaxed);
        let mut fill = (self.prod.occupied_len() / 2).saturating_sub(pending_skip);
        if ms(fill) < 8.0 {
            let n = (margin_ms * dev / 1000.0) as usize;
            self.push_silence(n);
            fill += n;
        }

        // 3) Đáy mức đệm: mức thấp nhất (ngay TRƯỚC khi nhận dữ liệu mới) trong LOW_WINDOW gần nhất. Đo ở phía tiêu
        //    thụ nên không phụ thuộc việc công cụ ngoài chia một đợt dữ liệu thành mấy lần đẩy hay lặp bớt phần
        //    nào. (Cách "gom đợt theo khoảng cách giữa các lần đẩy" từng thử đã bị chia vụn sau khi lọc lặp: mỗi
        //    lần đọc chỉ còn vài access unit mới → đợt giả 23 ms → ngưỡng cắt tụt còn ~0,4 s → cắt nhầm.)
        let now = Instant::now();
        self.lows.push_back((now, ms(fill)));
        while self.lows.front().is_some_and(|(t, _)| now.duration_since(*t) > LOW_WINDOW) {
            self.lows.pop_front();
        }
        let low = self.lows.iter().map(|&(_, f)| f).fold(f64::INFINITY, f64::min);

        // 4) điều tốc kiểu tỉ lệ theo thời gian liên tục: đáy cao hơn mong muốn thì phát nhanh hơn một chút để
        //    đệm tự co lại, thấp hơn thì chậm lại. Hằng số thời gian ~10 s (độ lệch 400 ms → +3,6%) ≫ độ trễ đo
        //    (~1,5 s) nên không dao động. Nhanh tối đa +6%, chậm tối đa −3% (với tiếng nói gần như không nghe ra).
        let err = low - margin_ms;
        let dead = 40.0;
        let eps = if err.abs() <= dead { 0.0 } else { (err - dead * err.signum()) / 10_000.0 };
        self.speed = if self.burst_mode { 1.0 } else { 1.0 + eps.clamp(-0.03, 0.06) };

        // 5) resample (đồng thời đổi sample rate nguồn → thiết bị)
        let ratio = src_rate as f64 / dev * self.speed;
        self.resampled.clear();
        self.rs.process(&self.stereo, ratio, &mut self.resampled);
        let inc = self.resampled.len() / 2;
        if inc == 0 {
            return;
        }

        // 6) tụt quá xa live → bỏ phần cũ nhất. Đáy cao hơn mong muốn quá `slack` nghĩa là lúc nào cũng có chừng đó
        //    âm thanh nằm chờ vô ích. Ngoài ra chặn cứng để vòng đệm không tràn khi dữ liệu dồn một lúc.
        let mut start = 0usize;
        let drop = if ms(fill) + ms(inc) > BACKLOG_CAP_MS {
            ((ms(fill) + ms(inc) - margin_ms) * dev / 1000.0) as usize
        } else if low > margin_ms + slack_ms {
            ((low - margin_ms) * dev / 1000.0) as usize
        } else {
            0
        };
        if drop > 0 {
            let from_queue = drop.min(fill);
            if from_queue > 0 {
                self.skip.fetch_add(from_queue, Relaxed);
            }
            start = (drop - from_queue).min(inc) * 2;
            self.lows.clear();
            log::debug!("đuổi theo live: bỏ {:.0} ms (đáy {:.0} ms, mong muốn {:.0} ms)", ms(drop), low, margin_ms);
        }

        let slice = &self.resampled[start..];
        let n = self.prod.push_slice(slice);
        if n < slice.len() {
            log::warn!("vòng đệm đầy, bỏ {} mẫu", slice.len() - n);
        }
        self.meter.started.store(true, Relaxed);
    }

    /// Đẩy `self.resampled` vào vòng đệm, đợi khi đệm đã chứa quá 1,5 s (backpressure cho VOD).
    fn push_blocking(&mut self) {
        let cap = self.dev_rate as usize * 3; // 1,5 s × 2 kênh = mẫu
        let mut off = 0;
        while off < self.resampled.len() && !self.stop.load(Relaxed) {
            if self.prod.occupied_len() > cap {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let end = (off + 4096).min(self.resampled.len());
            let n = self.prod.push_slice(&self.resampled[off..end]);
            off += n;
            if n == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.meter.started.store(true, Relaxed);
    }

    fn push_silence(&mut self, frames: usize) {
        let zeros = vec![0f32; frames * 2];
        self.prod.push_slice(&zeros);
    }

    /// Chờ phát hết phần đã đệm (khi nội dung kết thúc tự nhiên).
    pub fn drain(&self, stop: &AtomicBool) {
        let t0 = Instant::now();
        while !stop.load(Relaxed)
            && self.prod.occupied_len() > 0
            && t0.elapsed() < Duration::from_secs(30)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !stop.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.closing.store(true, Relaxed);
        if let Some(h) = self.sim.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------- cpal

fn build_stream(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: SampleFormat,
    cb: Callback,
) -> Result<cpal::Stream> {
    match fmt {
        SampleFormat::F32 => build::<f32>(device, cfg, cb),
        SampleFormat::I16 => build::<i16>(device, cfg, cb),
        SampleFormat::U16 => build::<u16>(device, cfg, cb),
        other => bail!("Định dạng mẫu {other:?} của thiết bị chưa được hỗ trợ"),
    }
}

fn build<T>(device: &cpal::Device, cfg: &cpal::StreamConfig, mut cb: Callback) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ch = cfg.channels as usize;
    let mut tmp: Vec<f32> = Vec::new();
    let stream = device.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let frames = data.len() / ch;
            tmp.resize(frames * 2, 0.0);
            cb.fill(&mut tmp);
            for (i, f) in data.chunks_exact_mut(ch).enumerate() {
                let (l, r) = (tmp[2 * i], tmp[2 * i + 1]);
                if ch == 1 {
                    f[0] = T::from_sample((l + r) * 0.5);
                } else {
                    f[0] = T::from_sample(l);
                    f[1] = T::from_sample(r);
                    for x in &mut f[2..] {
                        *x = T::from_sample(0.0f32);
                    }
                }
            }
        },
        |e| log::warn!("lỗi luồng âm thanh: {e}"),
        None,
    )?;
    Ok(stream)
}

// ---------------------------------------------------------------- WAV (kiểm thử)

struct Wav {
    w: BufWriter<File>,
    bytes: u32,
}

impl Wav {
    fn create(path: &PathBuf, rate: u32) -> Result<Self> {
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(b"WAVEfmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&1u16.to_le_bytes())?; // PCM
        w.write_all(&2u16.to_le_bytes())?; // stereo
        w.write_all(&rate.to_le_bytes())?;
        w.write_all(&(rate * 4).to_le_bytes())?;
        w.write_all(&4u16.to_le_bytes())?;
        w.write_all(&16u16.to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?;
        Ok(Self { w, bytes: 0 })
    }

    fn write(&mut self, s: &[f32]) -> std::io::Result<()> {
        for &x in s {
            self.w.write_all(&((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
        self.bytes += (s.len() * 2) as u32;
        Ok(())
    }

    fn finish(self) -> std::io::Result<()> {
        let bytes = self.bytes;
        let mut f = self.w.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&bytes.to_le_bytes())
    }
}

/// Giả lập thiết bị âm thanh: gọi callback mỗi 10 ms theo thời gian thực, ghi kết quả ra WAV.
fn spawn_wav(mut wav: Wav, rate: u32, mut cb: Callback, closing: Arc<AtomicBool>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0f32; (rate as usize / 100) * 2];
        let mut next = Instant::now();
        while !closing.load(Relaxed) {
            next += Duration::from_millis(10);
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            cb.fill(&mut buf);
            let _ = wav.write(&buf);
        }
        let _ = wav.finish();
    })
}

// ---------------------------------------------------------------- resampler

/// Resample cubic Catmull-Rom dạng luồng, tỉ lệ thay đổi được theo từng lần gọi
/// (dùng cho cả đổi sample rate lẫn điều tốc thu hẹp độ trễ).
struct Resampler {
    buf: Vec<[f32; 2]>,
    pos: f64,
}

impl Resampler {
    fn new() -> Self {
        Self { buf: vec![[0.0; 2]], pos: 1.0 }
    }

    /// `ratio` = số khung nguồn tiêu thụ cho mỗi khung đầu ra.
    fn process(&mut self, input: &[f32], ratio: f64, out: &mut Vec<f32>) {
        for fr in input.chunks_exact(2) {
            self.buf.push([fr[0], fr[1]]);
        }
        while (self.pos as usize) + 2 < self.buf.len() {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..2 {
                out.push(cubic(self.buf[i - 1][c], self.buf[i][c], self.buf[i + 1][c], self.buf[i + 2][c], t));
            }
            self.pos += ratio;
        }
        let keep_from = (self.pos as usize).saturating_sub(1);
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    0.5 * (2.0 * p1
        + (p2 - p0) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t * t * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, rate: f32, hz: f32) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let v = (i as f32 / rate * hz * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn resampler_44k1_to_48k_keeps_length_and_shape() {
        let mut rs = Resampler::new();
        let input = sine(44_100, 44_100.0, 440.0);
        let mut out = Vec::new();
        for chunk in input.chunks(2048) {
            rs.process(chunk, 44_100.0 / 48_000.0, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() < 8, "frames = {frames}");
        // so khớp với sin lý tưởng ở tần số lấy mẫu mới (bỏ qua đoạn khởi động)
        let err = (200..frames - 200)
            .map(|i| (out[2 * i] - (i as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 0.01, "max err = {err}");
    }

    /// Hồi quy: nội dung VOD tải nhanh hơn thời gian thực KHÔNG được bị cắt bỏ như khi đuổi live.
    #[test]
    fn vod_is_played_in_full_with_backpressure() {
        let path = std::env::temp_dir().join(format!("fbaudio-vod-{}.wav", std::process::id()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut out = Output::new(
            Tuning { margin_ms: 120, slack_ms: 250 },
            SinkKind::Wav(path.clone()),
            stop.clone(),
        )
        .unwrap();
        out.set_live(false);
        let t0 = Instant::now();
        for _ in 0..4 {
            out.push(&sine(48_000, 48_000.0, 440.0), 48_000, 2); // 4 × 1 s dồn dập
        }
        out.drain(&stop);
        drop(out);
        let bytes = std::fs::metadata(&path).unwrap().len();
        let _ = std::fs::remove_file(&path);
        let secs = (bytes - 44) as f64 / (48_000.0 * 4.0);
        assert!(secs >= 4.0, "chỉ phát được {secs:.2} s trong 4 s nội dung");
        assert!(t0.elapsed().as_secs_f64() >= 3.9, "nguồn không bị chặn nhịp: {:?}", t0.elapsed());
    }

    #[test]
    fn speedup_consumes_faster() {
        let mut rs = Resampler::new();
        let input = sine(48_000, 48_000.0, 300.0);
        let mut out = Vec::new();
        rs.process(&input, 1.05, &mut out);
        let frames = out.len() / 2;
        assert!((frames as f64 - 48_000.0 / 1.05).abs() < 8.0);
    }

    #[test]
    fn identity_ratio_is_passthrough() {
        let mut rs = Resampler::new();
        let input = sine(1000, 48_000.0, 500.0);
        let mut out = Vec::new();
        rs.process(&input, 1.0, &mut out);
        assert!(out.len() / 2 >= 996);
        for i in 0..out.len() / 2 {
            assert!((out[2 * i] - input[2 * i]).abs() < 1e-5, "khung {i}");
        }
    }
}
