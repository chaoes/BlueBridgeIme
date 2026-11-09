//! Tauri desktop shell for the Bluetooth Input Bridge.
//!
//! The RFCOMM/Bluetooth work lives in the headless `bluetooth-input-bridge-service`
//! binary (see `src/bin/bridge-service.rs`). This shell only talks to it over the
//! local control socket described in `service/client.rs`; the frontend can be
//! swapped for another stack without touching the service.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tauri::Emitter;

mod protocol;
#[cfg(target_os = "linux")]
pub mod service;
#[cfg(target_os = "windows")]
mod windows;

pub const INJECTION_METHOD: &str = "wl-copy + uinput Ctrl+V";

/// Status shared by the service and the UI. Serialized as camelCase for the
/// frontend, which also means the control socket uses the same shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    pub service_running: bool,
    pub listening: bool,
    pub registration_enabled: bool,
    pub autostart_enabled: bool,
    pub connected_device: Option<String>,
    pub connected_address: Option<String>,
    pub received_count: u64,
    pub last_error: Option<String>,
    pub injection_method: String,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self {
            service_running: false,
            listening: false,
            registration_enabled: false,
            autostart_enabled: false,
            connected_device: None,
            connected_address: None,
            received_count: 0,
            last_error: None,
            injection_method: if cfg!(target_os = "linux") {
                INJECTION_METHOD.into()
            } else {
                "尚未实现".into()
            },
        }
    }
}

fn offline_status(error: String) -> ServiceStatus {
    ServiceStatus {
        last_error: Some(error),
        ..Default::default()
    }
}

/// UI languages the frontend ships; any other OS locale falls back to English.
const UI_LOCALES: [&str; 5] = ["zh", "en", "ru", "ja", "ko"];

fn ui_locale(raw: Option<&str>) -> &'static str {
    let Some(raw) = raw else { return "en" };
    let lowered = raw.to_ascii_lowercase();
    let primary = lowered.split(['-', '_']).next().unwrap_or_default();
    UI_LOCALES
        .iter()
        .copied()
        .find(|code| *code == primary)
        .unwrap_or("en")
}

/// Language for the frontend, derived from the operating system locale.
#[tauri::command]
fn get_locale() -> &'static str {
    ui_locale(sys_locale::get_locale().as_deref())
}

#[tauri::command]
async fn get_status() -> ServiceStatus {
    #[cfg(target_os = "linux")]
    {
        match service::client::status().await {
            Ok(status) => status,
            Err(error) => offline_status(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        offline_status("当前平台尚未实现蓝牙服务".into())
    }
}

#[tauri::command]
async fn set_registration(enabled: bool) -> Result<ServiceStatus, String> {
    #[cfg(target_os = "linux")]
    {
        service::client::set_registration(enabled).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = enabled;
        Err("仅 Linux 后端支持服务注册开关".into())
    }
}

#[tauri::command]
async fn set_autostart(enabled: bool) -> Result<ServiceStatus, String> {
    #[cfg(target_os = "linux")]
    {
        service::client::set_autostart(enabled).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = enabled;
        Err("仅 Linux 后端支持开机自启".into())
    }
}

#[tauri::command]
async fn enable_pairing() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        service::client::enable_pairing().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("仅 Linux 后端支持配对".into())
    }
}

#[tauri::command]
async fn send_test(text: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        service::client::send_test(&text).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = text;
        Err("仅 Linux 后端支持测试输入".into())
    }
}

/// Keeps the webview fed with status events. Reconnects to the control socket
/// and starts the headless service when it is not running yet.
#[cfg(target_os = "linux")]
async fn forward_status(app: tauri::AppHandle) {
    let mut last_spawn: Option<Instant> = None;
    loop {
        match service::client::subscribe().await {
            Ok((status, mut reader)) => {
                let _ = app.emit("bridge-status", &status);
                let mut line = String::new();
                loop {
                    line.clear();
                    match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                                if let Some(status) = value
                                    .get("event")
                                    .and_then(|event| serde_json::from_value::<ServiceStatus>(event.clone()).ok())
                                {
                                    let _ = app.emit("bridge-status", &status);
                                }
                            }
                        }
                    }
                }
                let _ = app.emit("bridge-status", offline_status("服务已断开，正在重连…".into()));
            }
            Err(error) => {
                if last_spawn.map_or(true, |moment| moment.elapsed() > Duration::from_secs(5)) {
                    last_spawn = Some(Instant::now());
                    if let Err(spawn_error) = spawn_service() {
                        eprintln!("bridge: {spawn_error}");
                    }
                }
                let _ = app.emit("bridge-status", offline_status(error));
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn service_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("BLUETOOTH_BRIDGE_SERVICE") {
        return Some(PathBuf::from(explicit));
    }
    let executable = std::env::current_exe().ok()?;
    let sibling = executable.parent()?.join("bluetooth-input-bridge-service");
    sibling.exists().then_some(sibling)
}

#[cfg(target_os = "linux")]
fn spawn_service() -> Result<(), String> {
    let binary = service_binary().ok_or_else(|| {
        "找不到 bluetooth-input-bridge-service，请先运行 cargo build".to_string()
    })?;
    let log_path = log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("无法打开服务日志: {error}"))?;
    let stderr = log
        .try_clone()
        .map_err(|error| format!("复制日志句柄失败: {error}"))?;
    std::process::Command::new(&binary)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .spawn()
        .map_err(|error| format!("启动后台服务失败: {error}"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn log_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("bluetooth-input-bridge").join("service.log")
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_locale,
            get_status,
            set_registration,
            set_autostart,
            enable_pairing,
            send_test
        ])
        .setup(|app| {
            #[cfg(target_os = "linux")]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    forward_status(handle).await;
                });
            }
            #[cfg(target_os = "windows")]
            {
                let _ = app.handle();
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = windows::run().await {
                        eprintln!("bridge: {error}");
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to start Tauri app");
}

#[cfg(test)]
mod locale_tests {
    use super::ui_locale;

    #[test]
    fn maps_supported_os_locales() {
        assert_eq!(ui_locale(Some("zh_CN.UTF-8")), "zh");
        assert_eq!(ui_locale(Some("zh-Hans-CN")), "zh");
        assert_eq!(ui_locale(Some("ru_RU.UTF-8")), "ru");
        assert_eq!(ui_locale(Some("ja_JP.UTF-8")), "ja");
        assert_eq!(ui_locale(Some("ko_KR.UTF-8")), "ko");
        assert_eq!(ui_locale(Some("en_US.UTF-8")), "en");
        assert_eq!(ui_locale(Some("de_DE.UTF-8")), "en");
        assert_eq!(ui_locale(Some("C")), "en");
        assert_eq!(ui_locale(None), "en");
    }
}
