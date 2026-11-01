//! Giải mã AAC từng access unit thô lấy từ fMP4 (không cần ADTS).
//!
//! * AAC-LC (Facebook thường dùng `mp4a.40.2`): Symphonia, thuần Rust.
//! * HE-AAC (`mp4a.40.5` / `.29`, ASC có AOT 5 / 29 — luồng Facebook thực tế hay gặp):
//!   - bật feature `sbr`: giải mã đầy đủ SBR + PS bằng FDK-AAC, đầu ra ở tần số sau SBR;
//!   - mặc định: Symphonia giải mã phần lõi AAC-LC (bỏ qua dữ liệu SBR trong phần tử FIL).
//!     Đúng cao độ và tốc độ, chỉ thiếu dải cao do SBR tái tạo (xem README).

use crate::fmp4::TrackInfo;
use anyhow::{anyhow, bail, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AacDecoder;

enum Backend {
    Symphonia { dec: AacDecoder, track_id: u32, sbuf: Option<SampleBuffer<f32>>, ts: u64 },
    #[cfg(feature = "sbr")]
    Fdk { dec: fdk_aac::dec::Decoder, pcm: Vec<i16> },
}

pub struct AacDec {
    backend: Backend,
    pub asc: Vec<u8>,
    pub rate: u32,
    pub channels: usize,
    /// HE-AAC đang được giải mã không có SBR (chỉ phần lõi).
    pub core_only: bool,
}

impl AacDec {
    /// `he_hint`: manifest khai báo HE-AAC (codecs `mp4a.40.5`/`.29`) dù ASC không báo hiệu tường minh.
    pub fn new(info: &TrackInfo, he_hint: bool) -> Result<Self> {
        // 2 = AAC-LC; 5 / 29 = HE-AAC v1 / v2.
        if !matches!(info.object_type, 2 | 5 | 29) {
            bail!("AAC profile {} chưa được hỗ trợ (hiện chỉ AAC-LC và HE-AAC).", info.object_type);
        }
        let he = info.is_he() || he_hint;

        #[cfg(feature = "sbr")]
        if he {
            let mut dec = fdk_aac::dec::Decoder::new(fdk_aac::dec::Transport::Raw);
            dec.config_raw(&info.asc)
                .map_err(|e| anyhow!("FDK-AAC từ chối cấu hình luồng: {e}"))?;
            log::info!("HE-AAC (AOT {}): giải mã đầy đủ SBR/PS bằng FDK-AAC", info.object_type);
            return Ok(Self {
                backend: Backend::Fdk { dec, pcm: vec![0i16; 8192] },
                asc: info.asc.clone(),
                rate: info.ext_rate.unwrap_or(info.sample_rate * 2),
                channels: if info.object_type == 29 { 2 } else { info.channels.max(1) as usize },
                core_only: false,
            });
        }

        if he {
            log::info!(
                "HE-AAC (AOT {}): giải mã phần lõi {} Hz{}",
                info.object_type,
                info.sample_rate,
                info.ext_rate.map_or(String::new(), |r| format!(", SBR sẽ nâng lên {r} Hz"))
            );
        }
        let mut p = CodecParameters::new();
        p.for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(info.sample_rate)
            .with_extra_data(info.asc.clone().into_boxed_slice());
        let dec = AacDecoder::try_new(&p, &DecoderOptions::default())
            .map_err(|e| anyhow!("không khởi tạo được bộ giải mã AAC: {e}"))?;
        Ok(Self {
            backend: Backend::Symphonia { dec, track_id: info.track_id, sbuf: None, ts: 0 },
            asc: info.asc.clone(),
            rate: info.sample_rate,
            channels: info.channels.max(1) as usize,
            core_only: he,
        })
    }

    /// Giải mã một access unit; nối PCM float xen kẽ kênh vào `out`.
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<f32>) -> Result<()> {
        match &mut self.backend {
            Backend::Symphonia { dec, track_id, sbuf, ts } => {
                let pkt = Packet::new_from_slice(*track_id, *ts, 1024, frame);
                *ts += 1024;
                match dec.decode(&pkt) {
                    Ok(buf) => {
                        let spec = *buf.spec();
                        self.rate = spec.rate;
                        self.channels = spec.channels.count().max(1);
                        let need = buf.frames() * self.channels;
                        if sbuf.as_ref().map_or(true, |s| s.capacity() < need) {
                            *sbuf = Some(SampleBuffer::new(buf.capacity() as u64, spec));
                        }
                        let sb = sbuf.as_mut().expect("vừa tạo");
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
            #[cfg(feature = "sbr")]
            Backend::Fdk { dec, pcm } => {
                if let Err(e) = dec.fill(frame) {
                    log::debug!("FDK từ chối access unit: {e}");
                    return Ok(());
                }
                match dec.decode_frame(pcm) {
                    Ok(()) => {
                        let si = dec.stream_info();
                        self.rate = si.sampleRate.max(1) as u32;
                        self.channels = si.numChannels.max(1) as usize;
                        let n = dec.decoded_frame_size().min(pcm.len());
                        out.extend(pcm[..n].iter().map(|&s| s as f32 / 32768.0));
                        Ok(())
                    }
                    Err(e) => {
                        log::debug!("FDK bỏ qua frame: {e}");
                        Ok(())
                    }
                }
            }
        }
    }
}
