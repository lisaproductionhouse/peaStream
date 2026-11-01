//! fbaudio-core — nghe Facebook Live chỉ bằng âm thanh, độ trễ thấp.
//!
//! Luồng xử lý (một luồng nền duy nhất + callback âm thanh):
//!
//! ```text
//! link FB ─► extract (yt-dlp / máy chủ yt-dlp / bộ cào tích hợp) ─► manifest DASH
//!                                            ─► chỉ chọn Representation audio
//!                                            │  (không bao giờ tải video)
//!      HTTP keep-alive, đọc theo từng chunk ◄┘
//!            │
//!          fmp4 (tách AAC frame ngay khi có byte) ─► decode (AAC-LC) ─► output
//!                                                          vòng đệm SPSC ─► loa
//! ```

mod dash;
mod decode;
mod extract;
mod fmp4;
mod http;
mod output;
mod player;
mod ytdlp;

pub use extract::{ResolverConfig, ResolverMode};
pub use player::{LatencyMode, Player, PlayerConfig, State, Status};
pub use ytdlp::{split_args, update as update_ytdlp, Local as YtDlpLocal, Remote as YtDlpRemote};
