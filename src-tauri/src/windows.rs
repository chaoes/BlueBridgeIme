//! Windows integration boundary.
//!
//! Implement an RFCOMM listener with the shared UUID, then feed each UTF-8
//! JSON line into `protocol::Session`. Implement `TextInjector` using the
//! foreground editable control and Unicode input APIs. The Tauri UI and wire
//! protocol do not need changes.

pub async fn run() -> Result<(), String> {
    Err("Windows RFCOMM 和文字输入后端尚未实现".into())
}
