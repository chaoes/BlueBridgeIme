//! Windows 后端的进程内编排：蓝牙服务 + 命名管道控制通道 + 状态与设置。
//!
//! 结构对齐 Linux 端 `service::linux`（`Inner` + `dispatch` + 状态广播），但传输层是
//! Windows 命名管道、蓝牙是 Win32 WinSock（AF_BTH + WSASetService）、注入是 SendInput——
//! 没有复制 Linux 的剪贴板方案。
//!
//! ## 为什么进程内而不是“UI 进程 spawn 服务进程”（Linux 的做法）
//!
//! Linux 端靠 Unix socket 把界面和服务拆成两个进程，是因为 Wayland 的 `wl-copy`
//! 与 `uinput` 都要求“会话环境”。Windows 这边：
//!
//! - 命名管道本来就是内核对象：手机输入由**同一进程**的 RFCOMM 线程直接处理，
//!   少了“服务进程被系统在休眠后回收”和“两套日志”的麻烦；
//! - 但管道仍然存在，`bluetooth-input-bridge-service.exe` 可以独立无界面运行，
//!   界面通过管道读写状态——这样“服务与界面分离”的设计目标保持原样。
//!
//! ## 状态一致性
//!
//! 所有读写都过 `Mutex<ServiceStatus>`；每次修改都 `broadcast` 一份给订阅者
//! （`subscribe` 命令），界面据此刷新。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::autostart;
use super::injection;
use super::rfcomm::{self, SessionObserver};
use super::session::{self, SessionStats};
use crate::protocol::MAX_TEXT_BYTES;
use crate::ServiceStatus;

/// 制品名（配置、注册表键、日志目录都用它）。
pub const PRODUCT: &str = "bluetooth-input-bridge";
/// 拾进制注册时用的显示名。
pub const PRODUCT_DISPLAY: &str = "蓝牙输入桥";

/// 初始化状态：服务在进程内运行，读到配置就默认“已注册、未监听”。
///
/// 结构本身在 `crate::ServiceStatus`（界面契约，字段名不能改）。
fn initial_status(config: &Config) -> crate::ServiceStatus {
    crate::ServiceStatus {
        service_running: true,
        listening: false,
        registration_enabled: config.registration_enabled,
        autostart_enabled: autostart::is_enabled(),
        connected_device: None,
        connected_address: None,
        received_count: 0,
        last_error: None,
        injection_method: injection::INJECTION_METHOD.to_string(),
    }
}

/// 持久化设置（`%APPDATA%\bluebridge-ime\service.json`）。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Config {
    #[serde(rename = "registrationEnabled", default = "default_true")]
    registration_enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            registration_enabled: true,
        }
    }
}

/// `%APPDATA%` 下的数据目录（配置、日志、单实例锁都在这里）。
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("bluebridge-ime")
}

/// 配置文件路径。
pub fn config_path() -> PathBuf {
    data_dir().join("service.json")
}

/// 日志文件路径（服务进程与界面进程共用，便于“复制日志”）。
pub fn log_path() -> PathBuf {
    data_dir().join("service.log")
}

fn load_config() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(config: &Config) -> Result<(), String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("无法创建数据目录 {}：{e}", dir.display()))?;
    let text = serde_json::to_string_pretty(config).map_err(|e| format!("设置序列化失败：{e}"))?;
    let path = config_path();
    std::fs::write(&path, text).map_err(|e| format!("无法写入 {}：{e}", path.display()))
}

/// 追加一行日志（同时打到 stderr，便于从控制台看）。
pub fn log(line: &str) {
    eprintln!("bridge: {line}");
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let stamp = chrono_stamp();
        let text = format!("{stamp} {line}\n");
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("service.log"))
        {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

/// `hh:mm:ss` 时间戳（日志用；不引入额外依赖）。
fn chrono_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// 服务内部共享状态。
pub struct Inner {
    pub status: Mutex<ServiceStatus>,
    updates: broadcast::Sender<ServiceStatus>,
    pub stats: SessionStats,
    pub stop: AtomicBool,
    registration: AtomicBool,
    config: Mutex<Config>,
    /// 蓝牙服务端；`None` 表示尚未启动或已停止（`setRegistration(false)`）。
    pub server: Mutex<Option<rfcomm::Server>>,
    /// 当前进行中的会话数（注册/停止时会话不被打断）。
    pub active_sessions: AtomicU64,
    /// 服务启动时间（日志/诊断用）。
    pub started_at: Mutex<Instant>,
}

impl Inner {
    /// 最新状态快照。
    pub fn snapshot(&self) -> ServiceStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| ServiceStatus::default())
    }

    /// 修改状态并推送给订阅者。
    pub fn update<F: FnOnce(&mut ServiceStatus)>(&self, apply: F) {
        let snapshot = {
            let mut guard = match self.status.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            apply(&mut guard);
            guard.clone()
        };
        // 没有订阅者时 send 会 Err，属正常情况。
        let _ = self.updates.send(snapshot);
    }

    /// 记录错误并推送给界面。
    pub fn fail(&self, error: impl Into<String>) {
        let error = error.into();
        log(&format!("错误：{error}"));
        self.update(|s| s.last_error = Some(error));
    }

    /// 记录状态描述（成功路径也清掉旧错误）。
    pub fn describe(&self, message: impl Into<String>) {
        let message = message.into();
        log(&message);
        self.update(|s| {
            s.last_error = None;
            s.listening = true;
        });
    }

    /// 蓝牙注册开关的当前期望值。
    pub fn registration_enabled(&self) -> bool {
        self.registration.load(Ordering::SeqCst)
    }
}

/// 蓝牙线程 → 状态面板的桥：连接/断开/计数变化都写回 `ServiceStatus`。
///
/// 观察者在 RFCOMM 的会话线程里被调用，所以这里只做加锁 + 广播，不做阻塞 I/O。
struct StatusObserver {
    inner: Arc<Inner>,
}

impl SessionObserver for StatusObserver {
    fn connected(&self, device: Option<String>, address: Option<String>) {
        note_connection(&self.inner, device, address);
    }

    fn disconnected(&self) {
        note_disconnection(&self.inner);
    }

    fn progress(&self) {
        let inner = &self.inner;
        inner.update(|s| s.received_count = inner.stats.received());
    }
}

/// 启动蓝牙服务端（幂等：已在运行则直接返回）。
///
/// 监听失败才算启动失败；SDP 登记失败只是降级（状态里显示，后台自动重试）。
pub fn start_server(inner: &Arc<Inner>) -> Result<(), String> {
    {
        let slot = inner
            .server
            .lock()
            .map_err(|_| "蓝牙服务状态损坏".to_string())?;
        if slot.is_some() {
            return Ok(());
        }
    }
    let observer: Arc<dyn SessionObserver> = Arc::new(StatusObserver {
        inner: inner.clone(),
    });
    let server = rfcomm::start(observer)?;
    let state = server.state();
    let ready = state.is_ready();
    if !ready {
        // 降级是“能连但搜不到”，不是致命错误：先把原因展示出来，后台继续重试。
        log(&format!("蓝牙服务降级运行：{}", state.summary()));
    }
    *inner
        .server
        .lock()
        .map_err(|_| "蓝牙服务状态损坏".to_string())? = Some(server);
    inner.update(|s| {
        s.registration_enabled = true;
        s.listening = true;
        s.service_running = true;
        s.last_error = if ready {
            None
        } else {
            Some(inner_status_hint(&state))
        };
    });
    Ok(())
}

fn inner_status_hint(state: &rfcomm::State) -> String {
    state.summary()
}

/// 停止蓝牙服务端（`setRegistration(false)` 与进程退出都走这里）。
pub fn stop_server(inner: &Arc<Inner>) {
    let server = inner.server.lock().ok().and_then(|mut slot| slot.take());
    if let Some(mut server) = server {
        // `stop()` 会 join 接受线程并注销 SDP 记录。
        server.stop();
        log("蓝牙服务已停止");
    }
    inner.update(|s| {
        s.listening = false;
        s.connected_device = None;
        s.connected_address = None;
    });
}

/// 让状态接口知道当前有哪些会话在跑（状态/RPC 与串口线程共用的桥）。
pub fn note_connection(inner: &Arc<Inner>, device: Option<String>, address: Option<String>) {
    inner.active_sessions.fetch_add(1, Ordering::SeqCst);
    inner.update(|s| {
        s.connected_device = device;
        s.connected_address = address;
        s.listening = true;
        s.service_running = true;
        s.last_error = None;
        s.received_count = inner.stats.received();
    });
}

/// 会话结束时更新计数。
pub fn note_disconnection(inner: &Arc<Inner>) {
    let remaining = inner
        .active_sessions
        .fetch_sub(1, Ordering::SeqCst)
        .saturating_sub(1);
    inner.update(|s| {
        s.received_count = inner.stats.received();
        if remaining == 0 {
            s.connected_device = None;
            s.connected_address = None;
        }
    });
}

/// 一行请求的应答（`docs/service-api.md`）。
fn reply_ok(inner: &Arc<Inner>) -> Value {
    json!({ "ok": true, "status": inner.snapshot() })
}

fn reply_err(inner: &Arc<Inner>, error: impl Into<String>) -> Value {
    json!({ "ok": false, "error": error.into(), "status": inner.snapshot() })
}

/// 处理一条控制命令。
pub async fn dispatch(inner: Arc<Inner>, request: Value) -> Value {
    let command = request
        .get("cmd")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match command.as_str() {
        "status" => reply_ok(&inner),
        "setRegistration" => {
            let enabled = request
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            match toggle_registration(inner.clone(), enabled).await {
                Ok(()) => reply_ok(&inner),
                Err(error) => reply_err(&inner, error),
            }
        }
        "setAutostart" => {
            let enabled = request
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            match super::autostart::apply(enabled) {
                Ok(actual) => {
                    if let Ok(config) = inner.config.lock() {
                        let _ = save_config(&config);
                    }
                    inner.update(|s| s.autostart_enabled = actual);
                    reply_ok(&inner)
                }
                Err(error) => reply_err(&inner, error),
            }
        }
        "enablePairing" => match enable_pairing().await {
            Ok(message) => {
                log(&message);
                reply_ok(&inner)
            }
            Err(error) => reply_err(&inner, error),
        },
        "sendTest" => {
            let text = request
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if text.is_empty() {
                return reply_err(&inner, "测试文字为空");
            }
            if text.len() > MAX_TEXT_BYTES {
                return reply_err(&inner, "测试文字超过 16 KiB");
            }
            match tokio::task::spawn_blocking(move || injection::inject_text(&text)).await {
                Ok(Ok(())) => reply_ok(&inner),
                Ok(Err(error)) => reply_err(&inner, error),
                Err(error) => reply_err(&inner, format!("测试输入失败：{error}")),
            }
        }
        other => reply_err(&inner, format!("未知命令: {other}")),
    }
}

/// 打开/关闭蓝牙服务注册。
async fn toggle_registration(inner: Arc<Inner>, enabled: bool) -> Result<(), String> {
    if enabled {
        let task_inner = inner.clone();
        tokio::task::spawn_blocking(move || start_server(&task_inner))
            .await
            .map_err(|e| format!("启动蓝牙服务失败：{e}"))??;
        inner.registration.store(true, Ordering::SeqCst);
        if let Ok(mut config) = inner.config.lock() {
            config.registration_enabled = true;
            let _ = save_config(&config);
        }
    } else {
        stop_server(&inner);
        inner.registration.store(false, Ordering::SeqCst);
        if let Ok(mut config) = inner.config.lock() {
            config.registration_enabled = false;
            let _ = save_config(&config);
        }
        inner.update(|s| s.registration_enabled = false);
    }
    Ok(())
}

/// 让本机在 3 分钟内可被手机发现/配对（对应 Linux 端的 `DiscoverableTimeout`）。
async fn enable_pairing() -> Result<String, String> {
    tokio::task::spawn_blocking(|| -> Result<String, String> {
        // 没有“设为可发现”的 WinRT API；退而求其次：打开“设置 → 蓝牙和设备”，
        // 用户在那里点“添加设备”即可（Windows 会自动进入可发现状态）。
        open_bluetooth_settings();
        Ok("已打开 Windows 蓝牙设置：请在那里点“添加设备”，手机即可搜索到本机".into())
    })
    .await
    .map_err(|e| format!("打开蓝牙设置失败：{e}"))?
}

fn open_bluetooth_settings() {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let target = HSTRING::from("ms-settings:bluetooth");
    let verb = HSTRING::from("open");
    let result = unsafe {
        ShellExecuteW(
            None,
            &verb,
            &target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if (result.0 as isize) <= 32 {
        // 回退到旧的“设备”页面。
        let legacy = HSTRING::from("ms-settings:devices");
        unsafe {
            ShellExecuteW(
                None,
                &verb,
                &legacy,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

/// 创建共享状态（并读入持久化设置）。
pub fn new_inner() -> Arc<Inner> {
    let config = load_config();
    let (updates, _) = broadcast::channel(64);
    let status = initial_status(&config);
    Arc::new(Inner {
        status: Mutex::new(status),
        updates,
        stats: SessionStats::default(),
        stop: AtomicBool::new(false),
        registration: AtomicBool::new(config.registration_enabled),
        config: Mutex::new(config),
        server: Mutex::new(None),
        active_sessions: AtomicU64::new(0),
        started_at: Mutex::new(Instant::now()),
    })
}

/// 订阅状态变化（控制通道的 `subscribe` 命令用）。
pub fn subscribe(inner: &Arc<Inner>) -> broadcast::Receiver<ServiceStatus> {
    inner.updates.subscribe()
}

/// 如果设置里允许注册，则启动蓝牙服务；失败不致命（状态里报错，界面可见）。
pub fn start_if_enabled(inner: &Arc<Inner>) {
    if !inner.registration_enabled() {
        log("服务注册开关已关闭，暂不启动蓝牙服务");
        inner.update(|s| s.listening = false);
        return;
    }
    if let Err(error) = start_server(inner) {
        inner.fail(error);
        inner.update(|s| s.listening = false);
    }
}

/// 会话空闲超时（给上层决定是否提醒用户“手机还连着”）。
pub const IDLE_HINT: Duration = session::IDLE_SESSION_LIMIT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_to_the_ui_contract() {
        let value = serde_json::to_value(ServiceStatus::default()).expect("状态应可序列化");
        for key in [
            "serviceRunning",
            "listening",
            "registrationEnabled",
            "autostartEnabled",
            "connectedDevice",
            "connectedAddress",
            "receivedCount",
            "lastError",
            "injectionMethod",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}");
        }
    }

    #[test]
    fn status_roundtrips_through_json() {
        let status = ServiceStatus {
            connected_device: Some("OPPO Find X8".into()),
            connected_address: Some("AA:BB:CC:DD:EE:FF".into()),
            received_count: 7,
            ..ServiceStatus::default()
        };
        let text = serde_json::to_string(&status).expect("可序列化");
        let back: ServiceStatus = serde_json::from_str(&text).expect("可反序列化");
        assert_eq!(back, status);
    }

    #[test]
    fn config_defaults_to_registration_on() {
        assert!(Config::default().registration_enabled);
        let parsed: Config = serde_json::from_str("{}").expect("空对象应可用默认值");
        assert!(parsed.registration_enabled);
    }

    #[test]
    fn injection_method_is_not_clipboard_based() {
        assert!(!injection::INJECTION_METHOD.contains("剪贴板"));
        assert_eq!(
            ServiceStatus::default().injection_method,
            injection::INJECTION_METHOD
        );
    }

    #[test]
    fn service_uuid_is_exposed_for_status_messages() {
        assert_eq!(crate::protocol::SERVICE_UUID, crate::protocol::SERVICE_UUID);
        assert_eq!(PRODUCT, "bluetooth-input-bridge");
    }

    #[test]
    fn initial_status_mirrors_config() {
        let status = initial_status(&Config {
            registration_enabled: false,
        });
        assert!(status.service_running);
        assert!(!status.registration_enabled);
        assert!(!status.listening);
    }
}
