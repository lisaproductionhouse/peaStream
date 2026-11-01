//! Bản dòng lệnh để thử nhanh trên Windows/Linux/macOS (không cần dựng app Tauri).

use fbaudio_core::{
    split_args, update_ytdlp, LatencyMode, Player, PlayerConfig, ResolverConfig, ResolverMode, State,
    YtDlpRemote,
};
use std::time::{Duration, Instant};

struct Logger;
static LOGGER: Logger = Logger;
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.target().starts_with("fbaudio_core") && m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{}] {}", r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

const USAGE: &str = "Cách dùng: fbaudio <link Facebook Live | link .mpd> [tuỳ chọn]

  --mode ultra|balanced|stable   mức độ trễ (mặc định ultra)
  --wav out.wav                  ghi ra file thay vì phát ra loa
  --secs N                       tự dừng sau N giây
  -v                             log chi tiết

Cách lấy link từ Facebook (mặc định auto: yt-dlp trước, bộ cào tích hợp sau):
  --resolver auto|ytdlp|builtin  chọn cách lấy link
  --ytdlp PATH                   đường dẫn yt-dlp (mặc định tự tìm: FBAUDIO_YTDLP, cạnh file chạy, PATH, python -m yt_dlp)
  --ytdlp-args \"...\"             tham số thêm cho yt-dlp, ví dụ \"--cookies-from-browser firefox\"
                                 (cũng đọc từ biến môi trường FBAUDIO_YTDLP_ARGS)
  --ytdlp-server URL             dùng máy chủ yt-dlp (scripts/ytdlp_helper.py), ví dụ http://192.168.1.5:8787
  --ytdlp-token TOKEN            mã truy cập của máy chủ (nếu có)
  --no-update                    không tự chạy `yt-dlp -U` khi trích xuất hỏng
  --update-ytdlp                 chạy `yt-dlp -U` rồi thoát";

fn main() -> anyhow::Result<()> {
    let (mut url, mut mode, mut wav, mut secs, mut verbose) =
        (None, LatencyMode::Ultra, None, None::<u64>, false);
    let mut resolver = ResolverConfig::default();
    let (mut server, mut token) = (None::<String>, None::<String>);
    let mut update_only = false;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" | "-m" => mode = LatencyMode::parse(&it.next().unwrap_or_default()),
            "--wav" => wav = it.next().map(Into::into),
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()),
            "-v" => verbose = true,
            "--resolver" => resolver.mode = ResolverMode::parse(&it.next().unwrap_or_default()),
            "--ytdlp" => resolver.ytdlp.path = it.next().map(Into::into),
            "--ytdlp-args" => resolver.ytdlp.args.extend(split_args(&it.next().unwrap_or_default())),
            "--ytdlp-server" => server = it.next(),
            "--ytdlp-token" => token = it.next(),
            "--no-update" => resolver.ytdlp.auto_update = false,
            "--update-ytdlp" => update_only = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => url = Some(a),
        }
    }
    resolver.remote = server.as_deref().and_then(|s| YtDlpRemote::new(s, token.as_deref()));
    if verbose {
        log::set_logger(&LOGGER).ok();
        log::set_max_level(log::LevelFilter::Debug);
    }

    if update_only {
        match update_ytdlp(&resolver.ytdlp) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("Không cập nhật được yt-dlp: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    let Some(url) = url else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let player = Player::start(PlayerConfig { url, latency: mode, wav, resolver });
    let t0 = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let s = player.status();
        eprintln!(
            "{:>12?} | đệm {:>4} ms | ngắt {} | đoạn {} | {} kbps {} | {}",
            s.state, s.buffer_ms, s.underruns, s.segments, s.bitrate_kbps, s.codec, s.message
        );
        if matches!(s.state, State::Error | State::Ended | State::Idle) && t0.elapsed().as_secs() > 1 {
            if s.state == State::Error {
                eprintln!("\n{}", s.message);
            }
            break;
        }
        if secs.is_some_and(|n| t0.elapsed().as_secs() >= n) {
            player.stop();
            break;
        }
    }
    player.wait_finished(Duration::from_secs(5));
    Ok(())
}
