# Dữ liệu test

- `init.m4s`, `seg-1.m4s`, `seg-2.m4s`: AAC-LC 44,1 kHz stereo 64 kbps, 440 Hz (fMP4). Tạo bằng
  `ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=3.2" -c:a aac -b:a 64k -ac 2 -f dash -seg_duration 1 ...`
- `he1-*.m4s`, `he2-*.m4s`: HE-AAC v1 (AOT 5) và v2 (AOT 29) với ASC báo hiệu **tường minh** (fMP4).
  Tạo bằng `scripts/he-fixtures/make.sh` (bộ mã hóa FDK-AAC + ffmpeg).
- `lc.adts`, `he1.adts`: cùng âm thanh ở dạng ADTS (AAC thô, đúng thứ `yt-dlp -o -` + `-f adts` ghi ra stdout cho
  live). HE-AAC trong ADTS luôn báo "ngầm": tiêu đề ghi LC ở tần số lõi 24 kHz, SBR nằm trong access unit.
  Tạo lại bằng `scripts/make-adts-fixtures.sh`.
- `noise.adts`: 6 s nhiễu hồng AAC-LC, **mọi access unit đều khác nhau** (≈258 AU). Dùng cho test nối tiếp giữa các lượt
  chạy của công cụ; tín hiệu sin thuần không dùng được vì cứ vài khung lại lặp cùng một khung.
