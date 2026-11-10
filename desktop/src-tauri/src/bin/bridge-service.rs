//! Headless entry point: no Tauri, no window, only the Bluetooth service.
//!
//! Run it directly (`cargo run --bin bluetooth-input-bridge-service`) or let the
//! desktop UI start it on demand. It serves the control channel used by the UI
//! (Unix socket on Linux, named pipe on Windows).
//!
//! 额外参数（本机自测用，不影响正常启动）：
//!
//! - `--selftest-ctl`：进程内模拟界面的开关操作（`setRegistration` 开 → 查状态 →
//!   关 → 查状态），逐条打印耗时。用来验证控制面不会卡住（见 `win::selftest_ctl`）。
//! - `--no-radio`：跳过蓝牙服务启动，只跑控制通道（没有蓝牙适配器的机器上自测用）。

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--selftest-ctl") {
        match bluetooth_input_desktop_lib::service::win::selftest_ctl(
            !args.iter().any(|arg| arg == "--no-radio"),
        )
        .await
        {
            Ok(()) => return,
            Err(error) => {
                eprintln!("自测失败：{error}");
                std::process::exit(1);
            }
        }
    }
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
