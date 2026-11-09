//! Headless entry point: no Tauri, no window, only the Bluetooth service.
//!
//! Run it directly (`cargo run --bin bluetooth-input-bridge-service`) or let the
//! desktop UI start it on demand. It serves the control channel used by the UI
//! (Unix socket on Linux, named pipe on Windows).

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    if let Err(error) = bluetooth_input_desktop_lib::service::linux::run_headless().await {
        eprintln!("bridge-service: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    if let Err(error) = bluetooth_input_desktop_lib::service::win::run_headless().await {
        eprintln!("bridge-service: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("bluetooth-input-bridge-service 目前仅支持 Linux 与 Windows");
    std::process::exit(1);
}
