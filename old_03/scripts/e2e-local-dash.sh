#!/usr/bin/env bash
# Kiểm thử đầu-cuối KHÔNG cần Facebook: ffmpeg phát một luồng DASH "live" giả lập (AAC-LC, fMP4,
# SegmentTimeline), HTTP server cục bộ phục vụ, fbaudio nghe và ghi ra WAV, rồi phân tích.
# Cần: ffmpeg, python3, numpy, cargo.
# Dùng:  scripts/e2e-local-dash.sh [ultra|balanced|stable] [giây_đoạn]
# Biến môi trường:  JITTER_MS=250  (giả lập mạng giật)   STALL=1  (nguồn khựng 5 s rồi chạy bù)
# Lưu ý: với STALL=1, ffmpeg tự đổi nhịp nên cột "bước nhảy" quanh chỗ khựng có thể sai;
# hãy nhìn "6 mốc cuối" (phải về mức ổn định) và log "đuổi theo live".
set -euo pipefail
MODE="${1:-ultra}"; SEG="${2:-2}"; DUR=36
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; DIR="$(mktemp -d)"; PORT=8765
cleanup() { kill "${HTTP_PID:-}" "${FF_PID:-}" 2>/dev/null || true; }
trap cleanup EXIT

if [ -n "${JITTER_MS:-}" ]; then
  python3 "$ROOT/scripts/jitter_server.py" "$DIR" "$PORT" "$JITTER_MS" & HTTP_PID=$!
else
  python3 -m http.server "$PORT" --directory "$DIR" >/dev/null 2>&1 & HTTP_PID=$!
fi
FF_START=$(date +%s.%N)
ffmpeg -loglevel error -re \
  -f lavfi -i "aevalsrc='0.3*sin(2*PI*t*440)+0.4*lt(mod(t,1),0.15)*sin(2*PI*t*800*pow(1.15,mod(floor(t),8)))':s=44100:d=$DUR" \
  -c:a aac -b:a 96k -ac 2 \
  -f dash -seg_duration "$SEG" -window_size 40 -extra_window_size 0 -remove_at_exit 0 \
  -use_template 1 -use_timeline 1 \
  -init_seg_name 'init-$RepresentationID$.m4s' \
  -media_seg_name 'chunk-$RepresentationID$-$Number%05d$.m4s' \
  "$DIR/manifest.mpd" & FF_PID=$!

# chờ có manifest (người xem thật cũng vào giữa chừng)
for _ in $(seq 1 60); do [ -s "$DIR/manifest.mpd" ] && break; sleep 0.5; done
sleep 9   # để live đã chạy một lúc → fbaudio phải nhảy vào đoạn mới nhất

( cd "$ROOT" && cargo build -q -p fbaudio-cli )   # build trước để mốc thời gian không dính thời gian biên dịch
BIN="$ROOT/target/debug/fbaudio"
APP_START=$(date +%s.%N)
if [ -n "${STALL:-}" ]; then ( sleep 6; kill -STOP "$FF_PID"; sleep 5; kill -CONT "$FF_PID" ) & fi
"$BIN" "http://127.0.0.1:$PORT/manifest.mpd" --mode "$MODE" --wav "$DIR/out.wav" --secs 22 -v 2>&1 \
  | tee "$DIR/app.log" | sed 's/^/  fbaudio: /'
echo "  số lần ngắt (underrun) cuối phiên: $(grep -oE 'ngắt [0-9]+' "$DIR/app.log" | tail -1)"
python3 "$ROOT/scripts/analyze_wav.py" "$DIR" "$FF_START"
