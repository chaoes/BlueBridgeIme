//! LÖVE 前端用的套接字适配器：AF_UNIX 监听 ⇄ 控制命名管道（Windows only）。
//!
//! 用法（LÖVE 侧 `love/src/service.lua` 调起）：
//!
//! ```text
//! bluetooth-input-bridge-socket.exe --parent <LÖVE 的 pid>
//! bluetooth-input-bridge-socket.exe --parent <LÖVE 的 pid> --stop
//! ```
//!
//! - 不带 `--parent`：套接字落在固定目录 `%TEMP%\bluetooth-input-bridge\control.sock`；
//! - 带 `--parent`：落在 `%TEMP%\bluetooth-input-bridge-<pid>\control.sock`；
//! - 套接字完整路径超过 107 字节时退化成 `%TEMP%\bluetooth-input-bridge[-<pid>].sock`；
//! - 父进程（LÖVE）退出后 1 秒内自己退出并删掉套接字文件。
//!
//! 收发都是纯字节转发，不解析协议：供“一行请求 → 一行应答”与 `subscribe` 长连接直接用。
//! 桥**不负责**拉起 `bluetooth-input-bridge-service.exe`（那是 LÖVE 的事）；管道打不开时
//! 只记日志、继续监听。
//!
//! 非 Windows 目标只编译出一个打印说明的 `main`，保证 `cargo check --all-targets`
//! 在 Linux CI 上也不炸（真正的实现在 `service/win/socket_bridge.rs`，只在 Windows 编译）。

/// 参数/用法说明。
const USAGE: &str =
    "用法：bluetooth-input-bridge-socket [--parent <pid>] [--stop] [--socket-dir <目录>]
  --parent <pid>  监视该进程；它退出时本桥也退出（套接字目录带这个 pid）
  --stop          通知同路径的已运行实例退出（LÖVE 退出时调用）
  --socket-dir    覆盖套接字目录（默认 %TEMP%\\bluetooth-input-bridge[-<pid>]）";

#[cfg(target_os = "windows")]
fn main() {
    use bluetooth_input_desktop_lib::service::win::socket_bridge;

    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return;
    }
    let args = match socket_bridge::parse_args(raw) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("socket-bridge: {error}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("socket-bridge: 无法启动异步运行时：{error}");
            std::process::exit(1);
        }
    };

    match runtime.block_on(socket_bridge::run(args)) {
        Ok(_action) => {}
        Err(error) => {
            eprintln!("socket-bridge: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("bluetooth-input-bridge-socket 目前仅支持 Windows");
    std::process::exit(1);
}
