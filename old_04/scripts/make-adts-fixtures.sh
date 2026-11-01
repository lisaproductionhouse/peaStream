#!/usr/bin/env bash
# Tạo lại core/tests/data/{lc,he1}.adts từ các fixture fMP4 có sẵn (cần ffmpeg).
# ADTS là dạng AAC thô mà yt-dlp+ffmpeg ghi ra stdout cho live; HE-AAC trong ADTS luôn "ngầm" (tiêu đề ghi LC ở
# tần số lõi, SBR nằm trong access unit), đúng như luồng thật.
set -euo pipefail
D="$(cd "$(dirname "$0")/../core/tests/data" && pwd)"
cat "$D/init.m4s" "$D/seg-1.m4s" "$D/seg-2.m4s" > /tmp/lc_cat.mp4
ffmpeg -loglevel error -y -i /tmp/lc_cat.mp4 -c copy -f adts "$D/lc.adts"
cat "$D/he1-init.m4s" "$D/he1-seg-1.m4s" "$D/he1-seg-2.m4s" > /tmp/he_cat.mp4
ffmpeg -loglevel error -y -i /tmp/he_cat.mp4 -c copy -f adts "$D/he1.adts"
ls -l "$D"/*.adts
