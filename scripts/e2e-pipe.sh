#!/usr/bin/env bash
# Kiểm thử đầu-cuối kiến trúc pipe với yt-dlp + ffmpeg THẬT, không cần Facebook:
#   ffmpeg phát DASH live (có CẢ video lẫn audio, video đứng trước) → máy chủ tĩnh với bí danh /stream (link không
#   kết thúc bằng .mpd) → fbaudio chạy `yt-dlp -o -` và chỉ đọc byte thô từ stdout → WAV → đo độ trễ / tính liền mạch.
#
# Cần: yt-dlp (PATH hoặc FBAUDIO_YTDLP), ffmpeg, python3 + numpy, cargo.
# Dùng:  scripts/e2e-pipe.sh [giây_đoạn]      (mặc định 2)
# Dùng plugin mô phỏng Facebook:  URL_PATH=fb/live YTDLP_ARGS="--plugin-dirs $PWD/scripts/fblike-plugin"
#   (FBLIKE_DELAY / FBLIKE_JITTER / FBLIKE_SEED làm mỗi lượt trích xuất chậm và dao động như trang thật, xem fblike.py)
# Biến môi trường:  WINDOW=11  số đoạn trong cửa sổ manifest (mặc định 60; Facebook thật: ~11)
#                   FBAUDIO_NO_FFMPEG=1  đi thẳng bộ tải gốc (mô phỏng trang không có URL manifest dùng được)
#                   VIA_HELPER=1  chạy qua scripts/ytdlp_helper.py (đường dành cho Android) thay vì yt-dlp tại chỗ
#                   RUN_SECS=50  thời gian chạy app (mặc định 26);  CLI_FLAGS="--replay"  cờ thêm cho fbaudio
#                   URL_PATH=fb/live  đường dẫn link nguồn (mặc định "stream"; "fb/live" dùng với plugin mô phỏng Facebook)
#                   WARMUP=75   giây live chạy trước khi app vào (mô phỏng cửa sổ DVR dài; mặc định 9)
#                   TIMELINE=0  MPD kiểu $Number$ (mặc định 1 = SegmentTimeline)
#                   YTDLP_ARGS='--downloader-args "ffmpeg_i:-readrate 1"'  tham số thêm cho yt-dlp
set -euo pipefail
SEG="${1:-2}"; URL_PATH="${URL_PATH:-stream}"; WARMUP="${WARMUP:-9}"; DUR=$((WARMUP + ${RUN_SECS:-26} + 25)); PORT=8771; TIMELINE="${TIMELINE:-1}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; DIR="$(mktemp -d)"
YT="${FBAUDIO_YTDLP:-$(command -v yt-dlp || true)}"
[ -n "$YT" ] || { echo "Không thấy yt-dlp (đặt FBAUDIO_YTDLP=/đường/dẫn/yt-dlp)"; exit 1; }
cleanup() { kill ${HTTP_PID:-} ${FF_PID:-} ${HELPER_PID:-} 2>/dev/null || true; }
trap cleanup EXIT

rm -f "${FBLIKE_STATE:-/tmp/fblike.state}"
python3 "$ROOT/scripts/alias_server.py" "$DIR" "$PORT" "$DIR/requests.log" & HTTP_PID=$!
FF_START=$(date +%s.%N)
# video được map TRƯỚC (Representation 0 = video, 1 = audio): thứ tự mà ffmpeg/yt-dlp mong đợi
ffmpeg -loglevel error -re \
  -f lavfi -i "testsrc=size=160x120:rate=10:duration=$DUR" \
  -f lavfi -i "aevalsrc='0.3*sin(2*PI*440*t)+0.25*lt(mod(t,1),0.15)*(sin(2*PI*800*pow(1.15,mod(floor(t),8))*t)+sin(2*PI*2600*pow(1.15,mod(floor(t/8),8))*t))':s=44100:d=$DUR" \
  -map 0:v -map 1:a -c:v libx264 -preset ultrafast -g 20 -b:v 60k -pix_fmt yuv420p -c:a aac -b:a 96k -ac 2 \
  -f dash -seg_duration "$SEG" -window_size "${WINDOW:-60}" -extra_window_size 0 -remove_at_exit 0 -use_template 1 -use_timeline "$TIMELINE" \
  -init_seg_name 'init-$RepresentationID$.m4s' -media_seg_name 'chunk-$RepresentationID$-$Number%05d$.m4s' \
  "$DIR/manifest.mpd" & FF_PID=$!
for _ in $(seq 1 60); do [ -s "$DIR/manifest.mpd" ] && break; sleep 0.5; done
sleep "$WARMUP"   # để live đã chạy một lúc (có lịch sử = cửa sổ DVR) → app phải nhảy về live

( cd "$ROOT" && cargo build -q -p fbaudio-cli )
if [ -n "${VIA_HELPER:-}" ]; then
  python3 "$ROOT/scripts/ytdlp_helper.py" --port 8788 --allow-any --token sekret --no-update --yt-dlp "$YT" ${YTDLP_ARGS:+--ytdlp-args "$YTDLP_ARGS"} 2>"$DIR/helper.log" & HELPER_PID=$!
  sleep 1; SRC=(--server http://127.0.0.1:8788 --token sekret)
else
  SRC=(--ytdlp "$YT" ${YTDLP_ARGS:+--ytdlp-args "$YTDLP_ARGS"})
fi
"$ROOT/target/debug/fbaudio" "http://127.0.0.1:$PORT/$URL_PATH" "${SRC[@]}" --no-update --wav "$DIR/out.wav" --secs "${RUN_SECS:-26}" ${CLI_FLAGS:-} -v 2>&1 \
  | tee "$DIR/app.log" | grep -E "luồng|loại|đuổi|nới|thu hẹp|lượt xong|Error|Không|HE-AAC" | sed 's/^/  fbaudio: /' || true
V=$(grep -c "chunk-0-" "$DIR/requests.log" || true); A=$(grep -c "chunk-1-.* 200" "$DIR/requests.log" || true)
echo "  yêu cầu tới máy chủ nguồn: audio(200)=$A  video=$V (gồm init)   ← video phải rất ít / bằng 0"
python3 "$ROOT/scripts/analyze_wav.py" "$DIR" "$FF_START"
LAST=$(grep -E "(Playing|Connecting)" "$DIR/app.log" | tail -1 | sed 's/^ *//')
echo "  trạng thái cuối: $LAST"
