#!/usr/bin/env bash
# Kiểm thử đầu-cuối đường lấy link bằng yt-dlp THẬT, không cần Facebook:
#   ffmpeg phát DASH live có CẢ video lẫn audio → máy chủ tĩnh với bí danh /stream (link không kết thúc
#   bằng .mpd) → fbaudio hỏi yt-dlp → chỉ tải audio (đếm số request video, phải bằng 0) → WAV → phân tích.
#
# Cần: yt-dlp (trong PATH hoặc biến FBAUDIO_YTDLP), ffmpeg, python3 + numpy, cargo.
# Dùng:  scripts/e2e-ytdlp.sh                 yt-dlp chạy tại chỗ
#        VIA=server scripts/e2e-ytdlp.sh      qua máy chủ yt-dlp (scripts/ytdlp_helper.py)
set -euo pipefail
VIA="${VIA:-local}"; DUR=40; PORT=8771; HPORT=8787
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; DIR="$(mktemp -d)"
YT="${FBAUDIO_YTDLP:-$(command -v yt-dlp || true)}"
[ -n "$YT" ] || { echo "Không thấy yt-dlp (đặt FBAUDIO_YTDLP=/đường/dẫn/yt-dlp)"; exit 1; }
cleanup() { kill ${HTTP_PID:-} ${FF_PID:-} ${HELP_PID:-} 2>/dev/null || true; }
trap cleanup EXIT

python3 "$ROOT/scripts/alias_server.py" "$DIR" "$PORT" "$DIR/requests.log" & HTTP_PID=$!
FF_START=$(date +%s.%N)
# audio được map TRƯỚC nên là Representation 0 (chunk-0-*), video là 1 (chunk-1-*)
ffmpeg -loglevel error -re \
  -f lavfi -i "aevalsrc='0.3*sin(2*PI*440*t)+0.4*lt(mod(t,1),0.15)*sin(2*PI*800*pow(1.15,mod(floor(t),8))*t)':s=44100:d=$DUR" \
  -f lavfi -i "testsrc=size=160x120:rate=10:duration=$DUR" \
  -map 0:a -map 1:v -c:a aac -b:a 96k -ac 2 -c:v libx264 -preset ultrafast -g 20 -b:v 60k -pix_fmt yuv420p \
  -f dash -seg_duration 2 -window_size 40 -extra_window_size 0 -remove_at_exit 0 -use_template 1 -use_timeline 1 \
  -init_seg_name 'init-$RepresentationID$.m4s' -media_seg_name 'chunk-$RepresentationID$-$Number%05d$.m4s' \
  "$DIR/manifest.mpd" & FF_PID=$!
for _ in $(seq 1 60); do [ -s "$DIR/manifest.mpd" ] && break; sleep 0.5; done
sleep 9

EXTRA=(--resolver ytdlp --ytdlp "$YT" --no-update)
if [ "$VIA" = server ]; then
  python3 "$ROOT/scripts/ytdlp_helper.py" --port "$HPORT" --allow-any --token testtoken --yt-dlp "$YT" --no-update 2>"$DIR/helper.log" & HELP_PID=$!
  for _ in $(seq 1 40); do curl -sf "http://127.0.0.1:$HPORT/health" >/dev/null && break; sleep 0.25; done
  EXTRA+=(--ytdlp-server "http://127.0.0.1:$HPORT" --ytdlp-token testtoken)
fi

( cd "$ROOT" && cargo build -q -p fbaudio-cli )
APP_T0=$(date +%s.%N)
"$ROOT/target/debug/fbaudio" "http://127.0.0.1:$PORT/stream" "${EXTRA[@]}" --wav "$DIR/out.wav" --secs 22 -v 2>&1 \
  | tee "$DIR/app.log" | grep -E "lấy link bằng|HE-AAC|audio:|đuổi|Error|Không lấy" | sed 's/^/  fbaudio: /' || true
echo "  thời gian từ lúc chạy tới lúc có âm thanh đầu tiên: xem dòng đầu của phân tích (âm thanh bắt đầu lúc …)"
V=$(grep -c "chunk-1-" "$DIR/requests.log" || true); A=$(grep -c "chunk-0-" "$DIR/requests.log" || true)
echo "  yêu cầu tới máy chủ nguồn: audio=$A  video=$V   ← video phải bằng 0"
python3 "$ROOT/scripts/analyze_wav.py" "$DIR" "$FF_START"
