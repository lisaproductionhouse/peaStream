//! Giải mã AAC-LC (Facebook dùng mp4a.40.2) bằng Symphonia, thuần Rust.
//! Nhận từng access unit thô lấy từ fMP4 (không cần ADTS).

use crate::fmp4::TrackInfo;
use anyhow::{anyhow, bail, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AacDecoder;

pub struct AacDec {
    dec: AacDecoder,
    track_id: u32,
    sbuf: Option<SampleBuffer<f32>>,
    ts: u64,
    pub asc: Vec<u8>,
    pub rate: u32,
    pub channels: usize,
}

impl AacDec {
    pub fn new(info: &TrackInfo) -> Result<Self> {
        if info.object_type != 2 {
            bail!(
                "Luồng dùng AAC profile {} (HE-AAC?). Hiện chỉ hỗ trợ AAC-LC.",
                info.object_type
            );
        }
        let mut p = CodecParameters::new();
        p.for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(info.sample_rate)
            .with_extra_data(info.asc.clone().into_boxed_slice());
        let dec = AacDecoder::try_new(&p, &DecoderOptions::default())
            .map_err(|e| anyhow!("không khởi tạo được bộ giải mã AAC: {e}"))?;
        Ok(Self {
            dec,
            track_id: info.track_id,
            sbuf: None,
            ts: 0,
            asc: info.asc.clone(),
            rate: info.sample_rate,
            channels: info.channels.max(1) as usize,
        })
    }

    /// Giải mã một access unit; nối PCM float xen kẽ kênh vào `out`.
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let pkt = Packet::new_from_slice(self.track_id, self.ts, 1024, frame);
        self.ts += 1024;
        match self.dec.decode(&pkt) {
            Ok(buf) => {
                let spec = *buf.spec();
                self.rate = spec.rate;
                self.channels = spec.channels.count().max(1);
                let need = buf.frames() * self.channels;
                if self.sbuf.as_ref().map_or(true, |s| s.capacity() < need) {
                    self.sbuf = Some(SampleBuffer::new(buf.capacity() as u64, spec));
                }
                let sb = self.sbuf.as_mut().expect("vừa tạo");
                sb.copy_interleaved_ref(buf);
                out.extend_from_slice(sb.samples());
                Ok(())
            }
            // frame hỏng đơn lẻ: bỏ qua, tiếp tục phát
            Err(SymError::DecodeError(e)) => {
                log::debug!("bỏ qua frame AAC lỗi: {e}");
                Ok(())
            }
            Err(e) => Err(anyhow!("giải mã AAC thất bại: {e}")),
        }
    }
}
