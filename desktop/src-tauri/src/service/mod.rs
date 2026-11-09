//! Backend: one Linux implementation and one Windows implementation behind the
//! same control protocol and status structure.
//!
//! - `client`: the shell-side client (Unix socket on Linux, named pipe on Windows);
//! - `linux`: BlueZ profile + `wl-copy`/`uinput` injection;
//! - `win`: WinRT RFCOMM + `SendInput` injection (see the table at the top of
//!   `win/mod.rs`).
//!
//! The Tauri shell only ever sees the `client` API, so swapping the webview stack
//! never requires touching a backend.

#[cfg(unix)]
pub mod client;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod win;
