//! Windows 后端入口（`cfg(target_os = "windows")`）。
//!
//! 与 Linux 端的对应关系：
//!
//! | Linux | Windows |
//! |---|---|
//! | `service/linux.rs`（BlueZ + Unix socket） | [`service`] + [`control`] + [`rfcomm`] |
//! | `service/client.rs`（`UnixStream`） | [`client`]（命名管道） |
//! | `wl-copy` + `uinput` Ctrl+V | [`injection`]（SendInput + KEYEVENTF_UNICODE） |
//! | systemd user unit | [`autostart`]（`HKCU\…\Run`） |
//!
//! 界面进程（Tauri）与服务进程（`bluetooth-input-bridge-service.exe`）都有各自入口：
//! - 界面：`lib.rs` 的 setup 里 `windows::run()`——起后台服务线程、转发状态给界面；
//! - 服务：`bin/bridge-service.rs` 的 `win::run_headless()`——只跑服务，无窗口。
//!
//! ## 两个入口都不带 Tauri 事件循环
//!
//! `run()` 在 Tauri 的 async 运行时里调用，通过 `crate::emit_status` 推事件；
//! `run_headless()` 在没有 Tauri 的独立进程里跑，只写日志与命名管道。
//! 两者共享同一个 `service::Inner` 类型，所以“界面直连进程内状态”与
//! “界面连管道”两种模式的状态字段完全一致。

pub mod autostart;
pub mod client;
pub mod control;
pub mod device_name;
pub mod focus;
pub mod injection;
pub mod rfcomm;
pub mod service;
pub mod session;

use std::sync::Arc;

use crate::ServiceStatus;

/// 界面进程入口：启动（或连接已有）后台服务，并把状态推给界面。
///
/// 返回 `Ok(())` 时服务线程仍在运行；出错只记录到 stderr，不阻塞界面显示——
/// 界面用 `offline_status` 展示原因（与 Linux 端行为一致）。
pub async fn run() -> Result<(), String> {
    let inner = service::new_inner();
    let task_inner = inner.clone();

    // 控制通道：如果已有别的服务实例在监听，本进程只作为界面转发者。
    let already_running = control::probe().await;
    if already_running {
        service::log("检测到已有后台服务实例，界面将连接它");
    } else {
        // 蓝牙服务端（WinRT）在阻塞线程里启动，避免拖住 async 运行时。
        let startup_inner = inner.clone();
        let started = tokio::task::spawn_blocking(move || {
            service::start_if_enabled(&startup_inner);
        })
        .await;
        if let Err(error) = started {
            service::log(&format!("启动蓝牙服务失败：{error}"));
        }
        let serve_inner = inner.clone();
        tokio::spawn(async move {
            if let Err(error) = control::serve(serve_inner).await {
                service::log(&format!("控制通道退出：{error}"));
            }
        });
    }

    let app_handle_inner: Arc<service::Inner> = task_inner;
    // 状态转发：界面靠 `bridge-status` 事件刷新；这里用轻量轮询（1 s）而不是
    // 管道订阅，原因是界面进程里本来就持有 `Arc<Inner>`，无需再绕一圈 IPC。
    tokio::spawn(async move {
        let mut last: Option<ServiceStatus> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            let current = app_handle_inner.snapshot();
            if last.as_ref() != Some(&current) {
                let _ = crate::emit_status(current.clone());
                last = Some(current);
            }
            if app_handle_inner
                .stop
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }
        }
    });
    Ok(())
}

/// 无界面服务入口（`bluetooth-input-bridge-service.exe`）。
///
/// 行为对齐 Linux 端 `run_headless()`：已有实例 → 直接退出；否则启动蓝牙服务与控制通道，
/// 直到收到 Ctrl+C / 停止信号。
pub async fn run_headless() -> Result<(), String> {
    if control::probe().await {
        println!("已有实例在运行（{})", control::PIPE_NAME);
        return Ok(());
    }
    let inner = service::new_inner();
    service::log("后台服务启动");
    let startup_inner = inner.clone();
    tokio::task::spawn_blocking(move || service::start_if_enabled(&startup_inner))
        .await
        .map_err(|e| format!("启动蓝牙服务失败：{e}"))?;

    let serve_inner = inner.clone();
    let control_task = tokio::spawn(async move {
        if let Err(error) = control::serve(serve_inner).await {
            service::log(&format!("控制通道退出：{error}"));
        }
    });

    // 等待退出信号：服务通常由界面进程拉起/关闭，所以监听"停止标志 + 控制通道退出"。
    // 不依赖 `tokio::signal`（Cargo.toml 里没开 `signal` feature，Windows 上也没必要）。
    loop {
        if inner.stop.load(std::sync::atomic::Ordering::SeqCst) || control_task.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    inner.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    service::stop_server(&inner);
    service::log("后台服务已退出");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_are_exported() {
        // 编译期检查：这些符号必须存在，入口代码依赖它们。
        let _ = focus::SECURE_SURFACE_CLASS;
        let _ = injection::INJECTION_METHOD;
        let _ = rfcomm::SERVICE_NAME;
        let _ = control::PIPE_NAME;
        let _ = autostart::RUN_VALUE;
        let _ = service::PRODUCT;
    }
}
