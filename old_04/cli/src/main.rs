//! Bản dòng lệnh để thử nhanh trên Windows/Linux/macOS (không cần dựng app Tauri).

use fbaudio_core::{
    split_args, update_ytdlp, LatencyMode, Player, PlayerConfig, Remote, Source, State, YtDlp,
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

const USAGE: &str = "Cách dùng: fbaudio <link Facebook Live | -> [tuỳ chọn]

Ứng dụng chỉ đọc luồng byte thô do công cụ ngoài ghi ra; mặc định công cụ đó là yt-dlp (cần ffmpeg khi phát live).

  --mode ultra|balanced|stable   mức độ trễ (mặc định ultra)
  --wav out.wav                  ghi ra file thay vì phát ra loa
  --secs N                       tự dừng sau N giây
  -v                             log chi tiết

Nguồn byte thô:
  (mặc định)                     chạy yt-dlp tại chỗ
  --ytdlp PATH                   đường dẫn yt-dlp (mặc định tự tìm: FBAUDIO_YTDLP, cạnh file chạy, PATH, python -m yt_dlp)
  --ytdlp-args \"...\"             tham số thêm cho yt-dlp, ví dụ \"--cookies-from-browser firefox\"
                                 (cũng đọc từ biến môi trường FBAUDIO_YTDLP_ARGS)
  --server URL [--token T]       đọc luồng từ máy chủ yt-dlp (scripts/ytdlp_helper.py), cho Android
  --cmd \"lệnh {url}\"             chạy lệnh tuỳ ý in AAC (ADTS hoặc fMP4) ra stdout; {url} được thay bằng link
  -  (thay cho link)             đọc luồng thô từ stdin:  yt-dlp ... -o - LINK | fbaudio - --replay
  --replay                       video đã kết thúc: phát đủ từ đầu (mặc định là LIVE: bám live, bỏ phần lịch sử).
                                 Phải chọn tay: yt-dlp không báo live/không live cho Facebook.
  --no-update                    không tự chạy `yt-dlp -U` khi trích xuất hỏng
  --update-ytdlp                 chạy `yt-dlp -U` rồi thoát";

fn main() -> anyhow::Result<()> {
    let (mut url, mut mode, mut wav, mut secs, mut verbose) =
        (None, LatencyMode::Ultra, None, None::<u64>, false);
    let mut ytdlp = YtDlp::default();
    let (mut server, mut token, mut cmd) = (None::<String>, None::<String>, None::<String>);
    let (mut live, mut update_only) = (true, false);

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" | "-m" => mode = LatencyMode::parse(&it.next().unwrap_or_default()),
            "--wav" => wav = it.next().map(Into::into),
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()),
            "-v" => verbose = true,
            "--ytdlp" => ytdlp.path = it.next().map(Into::into),
            "--ytdlp-args" => ytdlp.args.extend(split_args(&it.next().unwrap_or_default())),
            "--server" => server = it.next(),
            "--token" => token = it.next(),
            "--cmd" => cmd = it.next(),
            "--replay" => live = false,
            "--no-update" => ytdlp.auto_update = false,
            "--update-ytdlp" => update_only = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => url = Some(a),
        }
    }
    if verbose {
        log::set_logger(&LOGGER).ok();
        log::set_max_level(log::LevelFilter::Debug);
    }

    if update_only {
        match update_ytdlp(&ytdlp) {
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

    let source = if url == "-" {
        Source::Stdin
    } else if let Some(template) = cmd {
        Source::Command { template }
    } else if let Some(r) = server.as_deref().and_then(|s| Remote::new(s, token.as_deref())) {
        Source::Remote(r)
    } else {
        Source::YtDlp(ytdlp)
    };

    let player = Player::start(PlayerConfig { url, latency: mode, wav, source, live });
    let t0 = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let s = player.status();
        eprintln!(
            "{:>12?} | đệm {:>4} ms | ngắt {} | {} KB | {} kbps {} | {}",
            s.state, s.buffer_ms, s.underruns, s.received_kb, s.bitrate_kbps, s.codec, s.message
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
