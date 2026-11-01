//! Bản dòng lệnh để thử nhanh trên Windows/Linux/macOS (không cần dựng app Tauri).
//!
//!   fbaudio <link Facebook Live | link .mpd> [--mode ultra|balanced|stable] [--wav out.wav] [--secs N] [-v]

use fbaudio_core::{LatencyMode, Player, PlayerConfig, State};
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

fn usage() {
    eprintln!("Cách dùng: fbaudio <link Facebook Live | link .mpd> [--mode ultra|balanced|stable] [--wav out.wav] [--secs N] [-v]");
}

fn main() -> anyhow::Result<()> {
    let (mut url, mut mode, mut wav, mut secs, mut verbose) =
        (None, LatencyMode::Ultra, None, None::<u64>, false);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" | "-m" => mode = LatencyMode::parse(&it.next().unwrap_or_default()),
            "--wav" => wav = it.next().map(Into::into),
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()),
            "-v" => verbose = true,
            "-h" | "--help" => return Ok(usage()),
            _ => url = Some(a),
        }
    }
    let Some(url) = url else {
        usage();
        std::process::exit(2);
    };
    if verbose {
        log::set_logger(&LOGGER).ok();
        log::set_max_level(log::LevelFilter::Debug);
    }

    let player = Player::start(PlayerConfig { url, latency: mode, wav });
    let t0 = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let s = player.status();
        eprintln!(
            "{:>12?} | đệm {:>4} ms | ngắt {} | đoạn {} | {} kbps {} | {}",
            s.state, s.buffer_ms, s.underruns, s.segments, s.bitrate_kbps, s.codec, s.message
        );
        if matches!(s.state, State::Error | State::Ended | State::Idle) && t0.elapsed().as_secs() > 1 {
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
