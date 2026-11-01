//! Mã hóa một file WAV stereo 16-bit thành hai luồng ADTS: he1.adts (HE-AAC v1, 48 kbps)
//! và he2.adts (HE-AAC v2, 32 kbps) trong thư mục hiện tại.   Dùng:  he-fixtures <file.wav>
use fdk_aac::enc::*;
use std::io::Write;

fn read_wav(path: &str) -> (u32, Vec<i16>) {
    let b = std::fs::read(path).expect("không đọc được WAV");
    let (mut p, mut rate) = (12, 0u32);
    while p + 8 <= b.len() {
        let len = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        match &b[p..p + 4] {
            b"fmt " => rate = u32::from_le_bytes(b[p + 12..p + 16].try_into().unwrap()),
            b"data" => {
                let end = (p + 8 + len).min(b.len());
                return (rate, b[p + 8..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect());
            }
            _ => {}
        }
        p += 8 + len + (len & 1);
    }
    panic!("WAV không có chunk data");
}

fn main() {
    let path = std::env::args().nth(1).expect("cách dùng: he-fixtures <file.wav>");
    let (rate, pcm) = read_wav(&path);
    for (name, aot, bps) in [
        ("he1", AudioObjectType::Mpeg4HeAac, 48_000),
        ("he2", AudioObjectType::Mpeg4HeAacV2, 32_000),
    ] {
        let enc = Encoder::new(EncoderParams {
            bit_rate: BitRate::Cbr(bps),
            sample_rate: rate,
            transport: Transport::Adts,
            channels: ChannelMode::Stereo,
            audio_object_type: aot,
        })
        .expect("khởi tạo bộ mã hóa");
        let info = enc.info().unwrap();
        let frame = info.frameLength as usize * 2; // mẫu xen kẽ 2 kênh trên mỗi access unit
        let mut out = std::fs::File::create(format!("{name}.adts")).unwrap();
        let mut buf = vec![0u8; info.maxOutBufBytes as usize];
        let mut pos = 0;
        while pos + frame <= pcm.len() {
            let r = enc.encode(&pcm[pos..pos + frame], &mut buf).unwrap();
            pos += r.input_consumed;
            out.write_all(&buf[..r.output_size]).unwrap();
        }
        println!("{name}.adts xong");
    }
}
