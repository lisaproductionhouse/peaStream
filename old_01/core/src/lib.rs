//! fbaudio-core — nghe Facebook Live chỉ bằng âm thanh, độ trễ thấp.
//!
//! Luồng xử lý (một luồng nền duy nhất + callback âm thanh):
//!
//! ```text
//! link FB ─► extract ─► manifest DASH ─► chỉ chọn Representation audio
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

pub use player::{LatencyMode, Player, PlayerConfig, State, Status};
