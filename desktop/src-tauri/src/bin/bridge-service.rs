//! Headless entry point: no Tauri, no window, only the Bluetooth service.
//!
//! Run it directly (`cargo run --bin bluetooth-input-bridge-service`) or let the
//! desktop UI start it on demand. It serves the control socket used by the UI.

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    if let Err(error) = bluetooth_input_desktop_lib::service::linux::run_headless().await {
        eprintln!("bridge-service: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("bluetooth-input-bridge-service 目前仅支持 Linux");
    std::process::exit(1);
}
