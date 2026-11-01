# Dữ liệu test

- `init.m4s`, `seg-1.m4s`, `seg-2.m4s`: AAC-LC 44,1 kHz stereo 64 kbps, 440 Hz. Tạo bằng
  `ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=3.2" -c:a aac -b:a 64k -ac 2 -f dash -seg_duration 1 ...`
- `he1-*.m4s`, `he2-*.m4s`: HE-AAC v1 (AOT 5) và v2 (AOT 29) với ASC báo hiệu **tường minh**,
  đúng kiểu luồng Facebook. Tạo bằng `scripts/he-fixtures/make.sh` (bộ mã hóa FDK-AAC + ffmpeg).
