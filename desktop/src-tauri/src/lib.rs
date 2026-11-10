//! Tauri desktop shell for the Bluetooth Input Bridge.
//!
//! 蓝牙（RFCOMM/SDP）与文字注入都在后台服务里：
//!
//! - Linux：独立进程 `bluetooth-input-bridge-service`，界面通过 Unix socket 读写状态
//!   （`service::client`）；
//! - Windows：同一套 `ServiceStatus`/命令语义，但服务跑在界面进程内（命名管道也可单独
//!   连，见 `service::win`），注入用 `SendInput` 而不是剪贴板。
//!
//! 界面（`web/`）只依赖 `ServiceStatus` 的字段名和 `get_status` /
//! `set_registration` / `set_autostart` / `enable_pairing` / `send_test` 这些命令，
//! 换后端不需要改前端。

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};
use tauri::Emitter;

pub mod protocol;
pub mod service;

#[cfg(target_os = "linux")]
pub const INJECTION_METHOD: &str = "wl-copy + uinput Ctrl+V";
#[cfg(target_os = "windows")]
pub const INJECTION_METHOD: &str = service::win::injection::INJECTION_METHOD;

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
            injection_method: INJECTION_METHOD.into(),
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

/// Platform the UI is running on. The frontend has no reliable way to know this
/// (the webview User-Agent is the same engine on every OS), and several labels —
/// the platform badge, where the Bluetooth service is registered, which system
/// settings page holds the pairing switch — differ per OS. Snowmen here rather
/// than in JS so the frontend never has to guess.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformInfo {
    /// `"windows"`, `"linux"` or `"unsupported"`.
    os: &'static str,
    /// Human-readable label for the badge in the title bar, e.g. `Windows 10/11`.
    label: &'static str,
    /// i18n key of the note under the 服务注册 switch: where the RFCOMM service
    /// is published (BlueZ / the Windows Bluetooth SDP store).
    registration_note_key: &'static str,
    /// i18n key of the pairing hint: which system UI to open when pairing fails.
    pairing_hint_key: &'static str,
}

#[tauri::command]
fn get_platform() -> PlatformInfo {
    #[cfg(target_os = "windows")]
    {
        PlatformInfo {
            os: "windows",
            label: "Windows 10/11",
            registration_note_key: "registrationNoteOnWindows",
            pairing_hint_key: "pairingHintWindows",
        }
    }
    #[cfg(target_os = "linux")]
    {
        PlatformInfo {
            os: "linux",
            label: "GNOME · Wayland",
            registration_note_key: "registrationNoteOnLinux",
            pairing_hint_key: "pairingHintLinux",
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        PlatformInfo {
            os: "unsupported",
            label: "Unsupported",
            registration_note_key: "registrationNoteOnLinux",
            pairing_hint_key: "pairingHintLinux",
        }
    }
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
    #[cfg(target_os = "windows")]
    {
        match service::win::client::status().await {
            Ok(status) => status,
            Err(error) => offline_status(format!("后台服务未运行: {error}")),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
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
    #[cfg(target_os = "windows")]
    {
        service::win::client::set_registration(enabled).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = enabled;
        Err("当前平台尚未实现服务注册开关".into())
    }
}

#[tauri::command]
async fn set_autostart(enabled: bool) -> Result<ServiceStatus, String> {
    #[cfg(target_os = "linux")]
    {
        service::client::set_autostart(enabled).await
    }
    #[cfg(target_os = "windows")]
    {
        service::win::client::set_autostart(enabled).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = enabled;
        Err("当前平台尚未实现开机自启".into())
    }
}

#[tauri::command]
async fn enable_pairing() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        service::client::enable_pairing().await
    }
    #[cfg(target_os = "windows")]
    {
        service::win::client::enable_pairing().await
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err("当前平台尚未实现配对".into())
    }
}

#[tauri::command]
async fn send_test(text: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        service::client::send_test(&text).await
    }
    #[cfg(target_os = "windows")]
    {
        service::win::client::send_test(&text).await
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
        Err("当前平台尚未实现测试输入".into())
    }
}

/// 把状态推给 webview（Windows 后端的 `windows::run` 也会调用）。
#[cfg(target_os = "windows")]
pub(crate) fn emit_status(status: ServiceStatus) -> tauri::Result<()> {
    let Some(handle) = CURRENT_APP.get() else {
        return Ok(());
    };
    handle.emit("bridge-status", &status)
}

#[cfg(target_os = "windows")]
static CURRENT_APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

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
                                if let Some(status) = value.get("event").and_then(|event| {
                                    serde_json::from_value::<ServiceStatus>(event.clone()).ok()
                                }) {
                                    let _ = app.emit("bridge-status", &status);
                                }
                            }
                        }
                    }
                }
                let _ = app.emit(
                    "bridge-status",
                    offline_status("服务已断开，正在重连…".into()),
                );
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
    let binary = service_binary()
        .ok_or_else(|| "找不到 bluetooth-input-bridge-service，请先运行 cargo build".to_string())?;
    let log_path = service::linux::log_path();
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

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_locale,
            get_platform,
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
                let _ = CURRENT_APP.set(app.handle().clone());
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = service::win::run().await {
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

    #[cfg(target_os = "windows")]
    #[test]
    fn injection_method_is_not_clipboard_based_on_windows() {
        assert!(!super::INJECTION_METHOD.contains("剪贴板"));
        assert_eq!(
            super::ServiceStatus::default().injection_method,
            "SendInput + KEYEVENTF_UNICODE"
        );
    }
}
