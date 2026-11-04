# Plugin yt-dlp dùng để KIỂM THỬ, bắt chước đúng đường mã của extractor Facebook:
#   - lấy định dạng bằng _extract_mpd_formats (một ảnh chụp manifest tại lúc trích xuất),
#   - trả về info KHÔNG có is_live / live_status (facebook.py không hề đặt hai trường này).
# Nhờ vậy bộ tải gốc của yt-dlp chỉ tải các đoạn có trong ảnh chụp rồi thoát, y như quan sát trên Facebook thật.
#
# Giả lập độ chậm/dao động của trang thật (lượt chạy thật gồm tải trang + trích xuất + tải cả cửa sổ):
#   FBLIKE_DELAY=3    luôn chờ thêm 3 giây trước khi trả kết quả
#   FBLIKE_JITTER=4   cộng thêm một khoảng ngẫu nhiên 0..4 giây
#   FBLIKE_SEED=7     làm chuỗi dao động XÁC ĐỊNH: lần trích xuất thứ i luôn chờ cùng một khoảng (bộ đếm lưu ở
#                     FBLIKE_STATE, mặc định /tmp/fblike.state; scripts/e2e-pipe.sh xoá tệp đó trước mỗi phép đo)
#                     → so sánh A/B công bằng
import os
import random
import time

from yt_dlp.extractor.common import InfoExtractor


class FbLikeIE(InfoExtractor):
    IE_NAME = 'fblike'
    _VALID_URL = r'https?://(?P<host>127\.0\.0\.1:\d+)/fb/(?P<id>\w+)'

    def _real_extract(self, url):
        host, video_id = self._match_valid_url(url).group('host', 'id')
        jitter = float(os.environ.get('FBLIKE_JITTER') or 0)
        seed = os.environ.get('FBLIKE_SEED')
        if seed is not None:
            state = os.environ.get('FBLIKE_STATE') or '/tmp/fblike.state'
            try:
                n = int(open(state).read() or 0)
            except (OSError, ValueError):
                n = 0
            open(state, 'w').write(str(n + 1))
            frac = random.Random(f'{seed}:{n}').random()
        else:
            frac = random.random()
        delay = float(os.environ.get('FBLIKE_DELAY') or 0) + jitter * frac
        if delay > 0:
            time.sleep(delay)
        formats = self._extract_mpd_formats(f'http://{host}/manifest.mpd', video_id)
        return {'id': video_id, 'title': 'Phiên live mô phỏng Facebook', 'formats': formats}
