use crate::protocol::{Session, TextInjector, MAX_FRAME_BYTES, SERVICE_UUID};
use crate::update_status;
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::AppHandle;
use zbus::{interface, Connection, Proxy};
use zvariant::{OwnedObjectPath, OwnedValue, Value};

const PROFILE_PATH: &str = "/com/example/BluetoothInputBridge/Profile";

struct GnomeInjector {
    keyboard: Arc<Mutex<VirtualDevice>>,
}

impl TextInjector for GnomeInjector {
    fn inject(&mut self, text: &str) -> Result<(), String> {
        // A Wayland client cannot write directly into another client's text field.
        // wl-copy owns the UTF-8 clipboard; the virtual keyboard triggers paste
        // in whichever desktop field currently has focus.
        let previous = Command::new("wl-paste")
            .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
            .output()
            .ok()
            .filter(|output| output.status.success() && output.stdout.len() <= 1024 * 1024)
            .map(|output| output.stdout);
        let mut clipboard = Command::new("wl-copy")
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
        let mut keyboard = self.keyboard.lock().map_err(|_| "虚拟键盘锁异常")?;
        keyboard
            .emit(&[
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTCTRL.0, 1),
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_V.0, 1),
            ])
            .map_err(|error| format!("粘贴按键失败: {error}"))?;
        std::thread::sleep(Duration::from_millis(25));
        keyboard
            .emit(&[
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_V.0, 0),
                InputEvent::new(EventType::KEY.0, KeyCode::KEY_LEFTCTRL.0, 0),
            ])
            .map_err(|error| format!("释放按键失败: {error}"))?;
        drop(keyboard);
        // Restore a prior text clipboard only if another app has not replaced it.
        std::thread::sleep(Duration::from_millis(500));
        let still_ours = Command::new("wl-paste")
            .args(["--no-newline", "--type", "text/plain;charset=utf-8"])
            .output()
            .ok()
            .is_some_and(|output| output.status.success() && output.stdout == text.as_bytes());
        if still_ours {
            if let Some(old) = previous {
                if let Ok(mut restore) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
                    if let Some(mut stdin) = restore.stdin.take() {
                        let _ = stdin.write_all(&old);
                    }
                    let _ = restore.wait();
                }
            } else {
                let _ = Command::new("wl-copy").arg("--clear").status();
            }
        }
        Ok(())
    }
}

struct Profile {
    app: AppHandle,
    keyboard: Arc<Mutex<VirtualDevice>>,
    active: Arc<Mutex<Option<(String, OwnedFd)>>>,
}

#[interface(name = "org.bluez.Profile1")]
impl Profile {
    async fn release(&self) {
        update_status(&self.app, |status| {
            status.listening = false;
            status.last_error = Some("BlueZ 已释放服务，请重启应用".into());
        });
    }

    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        eprintln!("bridge: BlueZ NewConnection received");
        let device_name = device.to_string();
        let owned: OwnedFd = fd.into();
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| zbus::fdo::Error::Failed("状态锁异常".into()))?;
            if active.is_some() {
                return Err(zbus::fdo::Error::Failed("已有设备连接".into()));
            }
            let control = owned
                .try_clone()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            *active = Some((device_name.clone(), control));
        }
        update_status(&self.app, |status| {
            status.connected_device = Some(device_name.clone());
            status.last_error = None;
        });
        let app = self.app.clone();
        let keyboard = self.keyboard.clone();
        let active = self.active.clone();
        std::thread::spawn(move || {
            eprintln!("bridge: RFCOMM worker started");
            let outcome = serve_client(owned, GnomeInjector { keyboard }, &app);
            if let Err(error) = outcome {
                eprintln!("bridge: RFCOMM worker error: {error}");
                update_status(&app, |status| status.last_error = Some(error));
            }
            if let Ok(mut current) = active.lock() {
                *current = None;
            }
            update_status(&app, |status| status.connected_device = None);
        });
        Ok(())
    }

    async fn request_disconnection(&self, _device: OwnedObjectPath) {
        if let Ok(active) = self.active.lock() {
            if let Some((_, fd)) = active.as_ref() {
                // SAFETY: the active entry owns this socket until the session exits.
                unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_RDWR) };
            }
        }
    }
}

fn serve_client(fd: OwnedFd, injector: GnomeInjector, app: &AppHandle) -> Result<(), String> {
    // BlueZ transfers the accepted RFCOMM socket with O_NONBLOCK enabled.
    // This worker uses blocking BufRead, so clear the flag before reading.
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
        eprintln!("bridge: received RFCOMM frame ({} bytes)", bytes.len());
        if bytes.last() != Some(&b'\n') || bytes.len() > MAX_FRAME_BYTES {
            return Err("蓝牙消息过长或未以换行结束".into());
        }
        match session.accept(&bytes) {
            Ok((ack, accepted_text)) => {
                eprintln!("bridge: processed frame, ok={}", ack.is_ok());
                let mut line = serde_json::to_vec(&ack).map_err(|error| error.to_string())?;
                line.push(b'\n');
                writer.write_all(&line).map_err(|error| error.to_string())?;
                writer.flush().map_err(|error| error.to_string())?;
                if accepted_text.is_some() {
                    update_status(app, |status| {
                        status.received_count += 1;
                        status.last_error = None;
                    });
                } else if !ack.is_ok() {
                    update_status(app, |status| {
                        status.last_error = Some("输入失败，已回传错误".into())
                    });
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub async fn run(app: AppHandle) -> Result<(), String> {
    if std::env::var("XDG_SESSION_TYPE").unwrap_or_default() != "wayland" {
        return Err("当前不是 Wayland 会话".into());
    }
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::KEY_LEFTCTRL);
    keys.insert(KeyCode::KEY_V);
    let keyboard = VirtualDevice::builder()
        .map_err(|error| format!("无法打开 /dev/uinput: {error}"))?
        .name("Bluetooth Input Bridge Virtual Keyboard")
        .with_keys(&keys)
        .map_err(|error| format!("配置虚拟键盘失败: {error}"))?
        .build()
        .map_err(|error| format!("创建虚拟键盘失败: {error}"))?;
    let connection = Connection::system()
        .await
        .map_err(|error| format!("无法连接系统 D-Bus: {error}"))?;
    connection
        .object_server()
        .at(
            PROFILE_PATH,
            Profile {
                app: app.clone(),
                keyboard: Arc::new(Mutex::new(keyboard)),
                active: Arc::new(Mutex::new(None)),
            },
        )
        .await
        .map_err(|error| format!("注册 Profile 对象失败: {error}"))?;
    let proxy = Proxy::new(
        &connection,
        "org.bluez",
        "/org/bluez",
        "org.bluez.ProfileManager1",
    )
    .await
    .map_err(|error| format!("BlueZ 不可用: {error}"))?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("Name", Value::from("Bluetooth Input Bridge"));
    options.insert("Role", Value::from("server"));
    // For a custom UUID BlueZ otherwise publishes an L2CAP-only SDP record.
    // Channel 0 requests an auto-assigned RFCOMM channel and advertises it.
    options.insert("Channel", Value::from(0u16));
    options.insert("RequireAuthentication", Value::from(true));
    options.insert("RequireAuthorization", Value::from(false));
    let path = OwnedObjectPath::try_from(PROFILE_PATH).map_err(|error| error.to_string())?;
    proxy
        .call::<_, _, ()>("RegisterProfile", &(path, SERVICE_UUID, options))
        .await
        .map_err(|error| format!("BlueZ 注册失败: {error}"))?;
    update_status(&app, |status| {
        status.listening = true;
        status.last_error = None;
    });
    std::future::pending::<()>().await;
    Ok(())
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
