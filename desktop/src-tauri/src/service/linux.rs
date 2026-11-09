//! Linux backend: BlueZ RFCOMM profile, JSON session handling and GNOME
//! Wayland text injection.
//!
//! This module is deliberately free of any Tauri/webview types. It runs inside
//! `bluetooth-input-bridge-service`, which can be started by the UI, by the
//! user, or by a systemd user unit without any window.

use crate::protocol::{Session, TextInjector, MAX_FRAME_BYTES, MAX_TEXT_BYTES, SERVICE_UUID};
use crate::service::client;
use crate::ServiceStatus;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use zbus::{interface, Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

const PROFILE_PATH: &str = "/com/example/BluetoothInputBridge/Profile";
const PROFILE_NAME: &str = "Bluetooth Input Bridge";
const UNIT_NAME: &str = "bluetooth-input-bridge.service";
const INJECTION_METHOD: &str = "wl-copy + uinput Ctrl+V";

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Runtime environment for `wl-copy` / `wl-paste`. systemd user units do not
/// always inherit WAYLAND_DISPLAY, so detect the socket from XDG_RUNTIME_DIR.
#[derive(Clone, Debug)]
struct WaylandEnv {
    display: Option<String>,
    runtime_dir: Option<String>,
}

impl WaylandEnv {
    fn detect() -> Self {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                let uid = unsafe { libc::getuid() };
                let candidate = format!("/run/user/{uid}");
                Path::new(&candidate).is_dir().then_some(candidate)
            });
        let mut display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|value| !value.is_empty());
        if display.is_none() {
            if let Some(dir) = runtime_dir.as_deref() {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    let mut names: Vec<String> = entries
                        .filter_map(|entry| entry.ok())
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .filter(|name| name.starts_with("wayland-"))
                        .filter(|name| Path::new(dir).join(name).exists())
                        .collect();
                    names.sort();
                    display = names.into_iter().next();
                }
            }
        }
        Self { display, runtime_dir }
    }

    fn available(&self) -> bool {
        self.display.is_some()
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        if let Some(display) = &self.display {
            command.env("WAYLAND_DISPLAY", display);
        }
        if let Some(dir) = &self.runtime_dir {
            command.env("XDG_RUNTIME_DIR", dir);
        }
        command
    }
}

fn create_keyboard() -> Result<VirtualDevice, String> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::KEY_LEFTCTRL);
    keys.insert(KeyCode::KEY_V);
    VirtualDevice::builder()
        .map_err(|error| format!("无法打开 /dev/uinput: {error}"))?
        .name("Bluetooth Input Bridge Virtual Keyboard")
        .with_keys(&keys)
        .map_err(|error| format!("配置虚拟键盘失败: {error}"))?
        .build()
        .map_err(|error| format!("创建虚拟键盘失败: {error}"))
}

struct GnomeInjector {
    keyboard: Arc<Mutex<Option<VirtualDevice>>>,
    env: WaylandEnv,
}

impl TextInjector for GnomeInjector {
    fn inject(&mut self, text: &str) -> Result<(), String> {
        // A Wayland client cannot write into another client's text field. Put
        // the UTF-8 text on the clipboard and let a virtual keyboard press
        // Ctrl+V, which the focused application handles as a normal paste.
        let mut keyboard = self.keyboard.lock().map_err(|_| "虚拟键盘锁异常")?;
        if keyboard.is_none() {
            *keyboard = Some(create_keyboard()?);
        }
        let previous = self
            .env
            .command("wl-paste")
            .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
            .output()
            .ok()
            .filter(|output| output.status.success() && output.stdout.len() <= 1024 * 1024)
            .map(|output| output.stdout);
        let mut clipboard = self
            .env
            .command("wl-copy")
            .args(["--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("无法启动 wl-copy: {error}"))?;
        clipboard
            .stdin
            .take()
            .ok_or("无法写入剪贴板")?
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        let status = clipboard.wait().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err("剪贴板写入失败".into());
        }
        // Give the compositor time to pick up the new clipboard owner.
        std::thread::sleep(Duration::from_millis(120));
        let device = keyboard.as_mut().ok_or("虚拟键盘不可用")?;
        device
            .emit(&[
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTCTRL.0, 1),
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_V.0, 1),
            ])
            .map_err(|error| format!("粘贴按键失败: {error}"))?;
        std::thread::sleep(Duration::from_millis(25));
        device
            .emit(&[
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_V.0, 0),
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTCTRL.0, 0),
            ])
            .map_err(|error| format!("释放按键失败: {error}"))?;
        drop(keyboard);
        // Restore the previous text clipboard only if nobody replaced ours.
        std::thread::sleep(Duration::from_millis(500));
        let still_ours = self
            .env
            .command("wl-paste")
            .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
            .output()
            .ok()
            .is_some_and(|output| output.status.success() && output.stdout == text.as_bytes());
        if still_ours {
            if let Some(old) = previous {
                if let Ok(mut restore) = self.env.command("wl-copy").stdin(Stdio::piped()).spawn() {
                    if let Some(mut stdin) = restore.stdin.take() {
                        let _ = stdin.write_all(&old);
                    }
                    let _ = restore.wait();
                }
            } else {
                let _ = self.env.command("wl-copy").arg("--clear").status();
            }
        }
        Ok(())
    }
}

struct ActiveSession {
    id: u64,
    fd: OwnedFd,
}

struct Inner {
    connection: Connection,
    status: Mutex<ServiceStatus>,
    updates: broadcast::Sender<ServiceStatus>,
    keyboard: Arc<Mutex<Option<VirtualDevice>>>,
    active: Mutex<Option<ActiveSession>>,
    registered: AtomicBool,
    config_path: PathBuf,
    env: WaylandEnv,
}

impl Inner {
    fn snapshot(&self) -> ServiceStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    fn update(&self, change: impl FnOnce(&mut ServiceStatus)) {
        let mut status = match self.status.lock() {
            Ok(status) => status,
            Err(_) => return,
        };
        change(&mut status);
        status.service_running = true;
        let _ = self.updates.send(status.clone());
    }
}

#[derive(Clone)]
struct Profile {
    inner: Arc<Inner>,
}

#[interface(name = "org.bluez.Profile1")]
impl Profile {
    async fn release(&self) {
        eprintln!("bridge-service: BlueZ released the profile");
        self.inner.registered.store(false, Ordering::SeqCst);
        self.inner.update(|status| {
            status.listening = false;
            status.registration_enabled = false;
            status.last_error = Some("BlueZ 已释放蓝牙服务，可在界面重新开启服务注册".into());
        });
    }

    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let (name, address) = describe_device(&self.inner, &device).await;
        eprintln!("bridge-service: incoming connection from {name}");
        let owned: OwnedFd = fd.into();
        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::SeqCst);
        {
            let mut active = self
                .inner
                .active
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("状态锁异常".into()))?;
            if active.is_some() {
                return Err(zbus::fdo::Error::Failed("已有设备连接".into()));
            }
            let control = owned
                .try_clone()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            *active = Some(ActiveSession { id, fd: control });
        }
        self.inner.update(|status| {
            status.connected_device = Some(name.clone());
            status.connected_address = address.clone();
            status.last_error = None;
        });
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let injector = GnomeInjector {
                keyboard: inner.keyboard.clone(),
                env: inner.env.clone(),
            };
            let outcome = serve_client(owned, injector, inner.clone(), id);
            match &outcome {
                Ok(()) => eprintln!("bridge-service: session {id} ended"),
                Err(error) => eprintln!("bridge-service: session {id} error: {error}"),
            }
            finish_session(&inner, id, outcome.err());
        });
        Ok(())
    }

    async fn request_disconnection(&self, _device: OwnedObjectPath) {
        if let Ok(active) = self.inner.active.lock() {
            if let Some(session) = active.as_ref() {
                // SAFETY: the active entry owns this socket until the session exits.
                unsafe { libc::shutdown(session.fd.as_raw_fd(), libc::SHUT_RDWR) };
            }
        }
    }
}

fn finish_session(inner: &Arc<Inner>, id: u64, error: Option<String>) {
    let current = {
        let mut active = match inner.active.lock() {
            Ok(active) => active,
            Err(_) => return,
        };
        if active.as_ref().map(|session| session.id) == Some(id) {
            *active = None;
            true
        } else {
            false
        }
    };
    if current {
        inner.update(|status| {
            status.connected_device = None;
            status.connected_address = None;
            if let Some(error) = error {
                status.last_error = Some(error);
            }
        });
    }
}

fn address_from_path(path: &OwnedObjectPath) -> Option<String> {
    let text = path.as_str();
    let tail = text.rsplit('/').next()?;
    let raw = tail.strip_prefix("dev_")?;
    let address = raw.replace('_', ":");
    (address.len() == 17).then_some(address)
}

async fn device_property(inner: &Inner, path: &OwnedObjectPath, property: &str) -> Option<String> {
    let proxy = Proxy::new_owned(
        inner.connection.clone(),
        "org.bluez",
        path.clone(),
        "org.freedesktop.DBus.Properties",
    )
    .await
    .ok()?;
    let value: OwnedValue = proxy
        .call("Get", &("org.bluez.Device1", property))
        .await
        .ok()?;
    String::try_from(value)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

async fn describe_device(inner: &Inner, device: &OwnedObjectPath) -> (String, Option<String>) {
    let address = address_from_path(device);
    let alias = device_property(inner, device, "Alias").await;
    let name = match alias.or(device_property(inner, device, "Name").await) {
        Some(name) => name,
        None => address
            .clone()
            .map(|address| format!("蓝牙设备 {address}"))
            .unwrap_or_else(|| device.to_string()),
    };
    (name, address)
}

fn serve_client(fd: OwnedFd, injector: GnomeInjector, inner: Arc<Inner>, id: u64) -> Result<(), String> {
    // BlueZ hands the accepted RFCOMM socket over with O_NONBLOCK set; the
    // worker reads blocking lines, so clear the flag first.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "读取 RFCOMM 标志失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "设置 RFCOMM 阻塞模式失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    let stream: std::fs::File = fd.into();
    let read_stream = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(read_stream);
    let mut writer = stream;
    let mut session = Session::new(injector);
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        if bytes.last() != Some(&b'\n') || bytes.len() > MAX_FRAME_BYTES {
            return Err("蓝牙消息过长或未以换行结束".into());
        }
        match session.accept(&bytes) {
            Ok((ack, accepted_text)) => {
                let mut line = serde_json::to_vec(&ack).map_err(|error| error.to_string())?;
                line.push(b'\n');
                writer.write_all(&line).map_err(|error| error.to_string())?;
                writer.flush().map_err(|error| error.to_string())?;
                if accepted_text.is_some() {
                    inner.update(|status| {
                        status.received_count += 1;
                        status.last_error = None;
                    });
                } else if let Some(error) = ack.error_message() {
                    inner.update(|status| status.last_error = Some(format!("注入失败: {error}")));
                }
            }
            Err(error) => return Err(error),
        }
    }
    let _ = id;
    Ok(())
}

fn profile_options<'a>() -> HashMap<&'a str, Value<'static>> {
    let mut options: HashMap<&str, Value<'static>> = HashMap::new();
    options.insert("Name", Value::from(PROFILE_NAME));
    options.insert("Role", Value::from("server"));
    // For a custom UUID BlueZ otherwise publishes an L2CAP-only SDP record.
    // Channel 0 requests an auto-assigned RFCOMM channel and advertises it.
    options.insert("Channel", Value::from(0u16));
    options.insert("RequireAuthentication", Value::from(true));
    options.insert("RequireAuthorization", Value::from(false));
    options
}

impl Inner {
    async fn register_profile(self: &Arc<Self>) -> Result<(), String> {
        if self.registered.load(Ordering::SeqCst) {
            return Ok(());
        }
        let proxy = Proxy::new_owned(
            self.connection.clone(),
            "org.bluez",
            "/org/bluez",
            "org.bluez.ProfileManager1",
        )
        .await
        .map_err(|error| format!("BlueZ 不可用: {error}"))?;
        self.connection
            .object_server()
            .at(PROFILE_PATH, Profile { inner: self.clone() })
            .await
            .map_err(|error| format!("注册 Profile 对象失败: {error}"))?;
        let path = OwnedObjectPath::try_from(PROFILE_PATH).map_err(|error| error.to_string())?;
        let options = profile_options();
        if let Err(error) = proxy
            .call::<_, _, ()>("RegisterProfile", &(path, SERVICE_UUID, options))
            .await
        {
            let _ = self
                .connection
                .object_server()
                .remove::<Profile, _>(PROFILE_PATH)
                .await;
            return Err(format!("BlueZ 注册失败: {error}"));
        }
        self.registered.store(true, Ordering::SeqCst);
        self.update(|status| {
            status.listening = true;
            status.registration_enabled = true;
            status.last_error = None;
        });
        eprintln!("bridge-service: profile registered");
        Ok(())
    }

    async fn unregister_profile(self: &Arc<Self>) -> Result<(), String> {
        if self.registered.load(Ordering::SeqCst) {
            let proxy = Proxy::new_owned(
                self.connection.clone(),
                "org.bluez",
                "/org/bluez",
                "org.bluez.ProfileManager1",
            )
            .await
            .map_err(|error| format!("BlueZ 不可用: {error}"))?;
            let path = OwnedObjectPath::try_from(PROFILE_PATH).map_err(|error| error.to_string())?;
            proxy
                .call::<_, _, ()>("UnregisterProfile", &(path,))
                .await
                .map_err(|error| format!("BlueZ 注销失败: {error}"))?;
            let _ = self
                .connection
                .object_server()
                .remove::<Profile, _>(PROFILE_PATH)
                .await;
            self.registered.store(false, Ordering::SeqCst);
        }
        if let Ok(mut active) = self.active.lock() {
            if let Some(session) = active.take() {
                // SAFETY: this entry owns the socket until the worker exits.
                unsafe { libc::shutdown(session.fd.as_raw_fd(), libc::SHUT_RDWR) };
            }
        }
        self.update(|status| {
            status.listening = false;
            status.registration_enabled = false;
            status.connected_device = None;
            status.connected_address = None;
            status.last_error = None;
        });
        eprintln!("bridge-service: profile unregistered");
        Ok(())
    }

    async fn set_registration(self: &Arc<Self>, enabled: bool) -> Result<(), String> {
        if enabled {
            self.register_profile().await?;
        } else {
            self.unregister_profile().await?;
        }
        let config = Config {
            registration_enabled: self.registered.load(Ordering::SeqCst),
        };
        save_config(&self.config_path, &config);
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default = "default_true")]
    registration_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
}

fn config_path() -> PathBuf {
    config_base().join("bluetooth-input-bridge").join("service.json")
}

fn load_config(path: &Path) -> Config {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Config {
            registration_enabled: true,
        })
}

fn save_config(path: &Path, config: &Config) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(config) {
        let _ = std::fs::write(path, bytes);
    }
}

fn autostart_unit_path() -> PathBuf {
    config_base()
        .join("systemd")
        .join("user")
        .join(UNIT_NAME)
}

fn autostart_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT_NAME])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "enabled")
        .unwrap_or(false)
}

fn apply_autostart(enabled: bool) -> Result<bool, String> {
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|error| format!("无法定位服务程序: {error}"))?;
        let unit = format!(
            "[Unit]\nDescription=Bluetooth Input Bridge headless service\nAfter=graphical-session.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=graphical-session.target\n",
            executable.display()
        );
        let path = autostart_unit_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("创建 systemd 目录失败: {error}"))?;
        }
        std::fs::write(&path, unit).map_err(|error| format!("写入自启单元失败: {error}"))?;
        run_systemctl(&["daemon-reload"])?;
        run_systemctl(&["enable", UNIT_NAME])?;
    } else {
        run_systemctl(&["disable", UNIT_NAME])?;
    }
    let current = autostart_enabled();
    if current != enabled {
        return Err(format!(
            "systemctl 未切换到预期状态（当前为 {}）",
            if current { "enabled" } else { "disabled" }
        ));
    }
    Ok(current)
}

fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| format!("无法执行 systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "systemctl {} 失败: {}",
            args.join(" "),
            if message.is_empty() { "未知错误" } else { &message }
        ))
    }
}

fn reply_status(inner: &Inner) -> Json {
    json!({ "ok": true, "status": inner.snapshot() })
}

fn reply_error(inner: &Inner, error: impl Into<String>) -> Json {
    json!({ "ok": false, "error": error.into(), "status": inner.snapshot() })
}

async fn dispatch(inner: Arc<Inner>, command: &str, request: &Json) -> Json {
    match command {
        "status" => reply_status(&inner),
        "setRegistration" => {
            let enabled = request.get("enabled").and_then(Json::as_bool).unwrap_or(false);
            match inner.set_registration(enabled).await {
                Ok(()) => reply_status(&inner),
                Err(error) => reply_error(&inner, error),
            }
        }
        "setAutostart" => {
            let enabled = request.get("enabled").and_then(Json::as_bool).unwrap_or(false);
            match tokio::task::spawn_blocking(move || apply_autostart(enabled)).await {
                Ok(Ok(actual)) => {
                    inner.update(|status| status.autostart_enabled = actual);
                    reply_status(&inner)
                }
                Ok(Err(error)) => reply_error(&inner, error),
                Err(error) => reply_error(&inner, format!("自启设置任务失败: {error}")),
            }
        }
        "enablePairing" => match enable_pairing().await {
            Ok(()) => json!({ "ok": true, "status": inner.snapshot() }),
            Err(error) => reply_error(&inner, error),
        },
        "sendTest" => {
            let text = request
                .get("text")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string();
            if text.trim().is_empty() {
                return json!({ "ok": false, "error": "测试文字为空" });
            }
            if text.len() > MAX_TEXT_BYTES {
                return reply_error(&inner, "测试文字超过 16 KiB");
            }
            let injector = GnomeInjector {
                keyboard: inner.keyboard.clone(),
                env: inner.env.clone(),
            };
            let result = tokio::task::spawn_blocking(move || {
                let mut injector = injector;
                injector.inject(&text)
            })
            .await;
            match result {
                Ok(Ok(())) => reply_status(&inner),
                Ok(Err(error)) => reply_error(&inner, error),
                Err(error) => reply_error(&inner, format!("注入任务失败: {error}")),
            }
        }
        other => json!({ "ok": false, "error": format!("未知命令: {other}") }),
    }
}

async fn write_json(writer: &mut OwnedWriteHalf, value: &Json) -> bool {
    let mut line = match serde_json::to_string(value) {
        Ok(line) => line,
        Err(_) => return false,
    };
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok() && writer.flush().await.is_ok()
}

async fn handle_connection(stream: UnixStream, inner: Arc<Inner>) {
    let (read_half, mut writer) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let request: Json = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(_) => {
            let _ = write_json(&mut writer, &json!({ "ok": false, "error": "请求格式错误" })).await;
            return;
        }
    };
    let command = request
        .get("cmd")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    if command == "subscribe" {
        if !write_json(&mut writer, &json!({ "ok": true, "status": inner.snapshot() })).await {
            return;
        }
        let mut updates = inner.updates.subscribe();
        loop {
            tokio::select! {
                update = updates.recv() => {
                    match update {
                        Ok(status) => {
                            if !write_json(&mut writer, &json!({ "event": status })).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                _ = reader.read_line(&mut line) => break,
            }
        }
        return;
    }
    let reply = dispatch(inner, &command, &request).await;
    let _ = write_json(&mut writer, &reply).await;
}

async fn bind_control_socket() -> Result<UnixListener, String> {
    let path = client::socket_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建控制目录失败: {error}"))?;
    }
    UnixListener::bind(&path).map_err(|error| format!("控制接口监听失败: {error}"))
}

async fn serve_control(listener: UnixListener, inner: Arc<Inner>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let inner = inner.clone();
                tokio::spawn(handle_connection(stream, inner));
            }
            Err(error) => {
                eprintln!("bridge-service: control accept error: {error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

pub async fn start() -> Result<(), String> {
    let env = WaylandEnv::detect();
    if !env.available() {
        return Err("当前不是 Wayland 会话（找不到 WAYLAND_DISPLAY）".into());
    }
    let config_path = config_path();
    let config = load_config(&config_path);
    let keyboard = Arc::new(Mutex::new(None));
    let mut startup_error = None;
    match create_keyboard() {
        Ok(device) => {
            if let Ok(mut slot) = keyboard.lock() {
                *slot = Some(device);
            }
        }
        Err(error) => startup_error = Some(error),
    }
    let connection = Connection::system()
        .await
        .map_err(|error| format!("无法连接系统 D-Bus: {error}"))?;
    let (updates, _) = broadcast::channel(64);
    let inner = Arc::new(Inner {
        connection,
        status: Mutex::new(ServiceStatus {
            service_running: true,
            listening: false,
            registration_enabled: config.registration_enabled,
            autostart_enabled: autostart_enabled(),
            connected_device: None,
            connected_address: None,
            received_count: 0,
            last_error: startup_error.clone(),
            injection_method: INJECTION_METHOD.into(),
        }),
        updates,
        keyboard,
        active: Mutex::new(None),
        registered: AtomicBool::new(false),
        config_path,
        env,
    });
    let listener = bind_control_socket().await?;
    eprintln!("bridge-service: control socket {}", client::socket_path().display());
    tokio::spawn(serve_control(listener, inner.clone()));
    if config.registration_enabled {
        if let Err(error) = inner.register_profile().await {
            inner.update(|status| status.last_error = Some(error));
        }
    }
    std::future::pending::<()>().await;
    Ok(())
}

/// Entry point shared by the headless binary. Refuses to start twice.
pub async fn run_headless() -> Result<(), String> {
    let path = client::socket_path();
    if UnixStream::connect(&path).await.is_ok() {
        eprintln!("bridge-service: 已有实例在运行（{}）", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建控制目录失败: {error}"))?;
    }
    let _ = std::fs::remove_file(&path);
    start().await
}

pub async fn enable_pairing() -> Result<(), String> {
    let connection = Connection::system()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = Proxy::new(
        &connection,
        "org.bluez",
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties",
    )
    .await
    .map_err(|error| error.to_string())?;
    for (name, value) in [
        ("DiscoverableTimeout", Value::from(180u32)),
        ("PairableTimeout", Value::from(180u32)),
        ("Pairable", Value::from(true)),
        ("Discoverable", Value::from(true)),
    ] {
        proxy
            .call::<_, _, ()>("Set", &("org.bluez.Adapter1", name, value))
            .await
            .map_err(|error| format!("设置 {name} 失败: {error}"))?;
    }
    Ok(())
}
