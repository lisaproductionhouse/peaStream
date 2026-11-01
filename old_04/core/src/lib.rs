//! fbaudio-core — nghe Facebook Live chỉ bằng âm thanh, kiến trúc "pipe luồng thô".
//!
//! ```text
//!  yt-dlp ... -o -   ──stdout──►  đọc byte thô  ─►  demux (ADTS | fMP4)  ─►  lọc AU lặp
//!  (tự lo link → manifest                                                        │
//!   → tải → xuất AAC)                                              giải mã AAC (Symphonia | FDK-AAC)
//!                                                                                │
//!         bám live (bỏ phần dồn) · đệm chống giật · điều tốc  ◄──  output (vòng đệm → loa)
//! ```
//!
//! Ứng dụng không tự tải và không phân tích manifest DASH: việc đó thuộc về công cụ ngoài. Nguồn byte thể
//! là tiến trình yt-dlp tại chỗ, máy chủ yt-dlp qua HTTP (Android), một lệnh tuỳ ý, hoặc stdin.

mod decode;
mod demux;
mod fmp4;
mod output;
mod player;
mod source;

pub use player::{LatencyMode, Player, PlayerConfig, State, Status};
pub use source::{split_args, update_ytdlp, Remote, Source, YtDlp};
