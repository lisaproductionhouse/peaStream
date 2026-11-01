#!/usr/bin/env bash
# Tạo lại fixture HE-AAC trong core/tests/data/ (he1-*.m4s, he2-*.m4s).
# Cần: ffmpeg, python3, cargo + trình biên dịch C++ (để dựng FDK-AAC).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; DATA="$HERE/../../core/tests/data"; W="$(mktemp -d)"; cd "$W"

# 1) tín hiệu 440 Hz stereo, 3,2 s
ffmpeg -loglevel error -f lavfi -i "aevalsrc='0.25*sin(2*PI*440*t)|0.25*sin(2*PI*440*t)':s=48000:d=3.2" tone440.wav
# 2) mã hóa HE-AAC v1 / v2 bằng FDK-AAC (ra ADTS)
cargo run -q --release --manifest-path "$HERE/Cargo.toml" -- tone440.wav
# 3) đóng gói thành fMP4 DASH, đoạn dài 1 s
for v in he1 he2; do
  mkdir -p $v
  ffmpeg -loglevel error -f aac -i $v.adts -c copy -f dash -seg_duration 1 -use_template 1 -use_timeline 1 \
         -init_seg_name init.m4s -media_seg_name 'seg-$Number$.m4s' $v/manifest.mpd
done
# 4) ffmpeg ghi ASC kiểu "ngầm" (AOT 2). Facebook dùng báo hiệu tường minh phân cấp (AOT 5 / 29) nên vá lại:
#    v1: AOT 5, lõi 24 kHz, stereo, SBR 48 kHz, AOT 2   → 2b118800
#    v2: AOT 29, lõi 24 kHz, 1 kênh, PS+SBR 48 kHz, AOT 2 → eb098800
python3 "$HERE/patch_asc.py" he1/init.m4s he1/init-x.m4s 2b118800
python3 "$HERE/patch_asc.py" he2/init.m4s he2/init-x.m4s eb098800
for v in he1 he2; do
  cp $v/init-x.m4s "$DATA/$v-init.m4s"; cp $v/seg-1.m4s "$DATA/$v-seg-1.m4s"; cp $v/seg-2.m4s "$DATA/$v-seg-2.m4s"
done
echo "Đã ghi fixture vào $DATA"
