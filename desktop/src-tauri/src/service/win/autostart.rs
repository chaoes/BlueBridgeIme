//! 开机自启（对应 Linux 端的 systemd user unit）。
//!
//! 用 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`：
//!
//! - 只影响当前用户、不需要管理员权限（`docs/windows.md` 给的两种方案之一）；
//! - 直接启动**无界面服务** `bluetooth-input-bridge-service.exe` 而不是界面进程，
//!   这样开机后不需要点开界面手机就能直接打字；界面需要时自己会连管道；
//! - 卸载/关闭开关就是删这个值，不留系统服务、不留计划任务。
//!
//! 计划任务更适合“服务要在其它用户登录后仍运行”的场景，本程序是单用户桌面工具，
//! 因此选注册表。

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// 自启项名称（注册表值名）。
pub const RUN_VALUE: &str = "BluetoothInputBridge";

fn run_key_path() -> HSTRING {
    HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Run")
}

/// 服务可执行文件路径：优先环境变量（调试用），否则同目录下的
/// `bluetooth-input-bridge-service.exe`。
pub fn service_binary() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("BLUETOOTH_BRIDGE_SERVICE") {
        return Some(std::path::PathBuf::from(path));
    }
    let current = std::env::current_exe().ok()?;
    let dir = current.parent()?;
    let candidate = dir.join(if cfg!(target_os = "windows") {
        "bluetooth-input-bridge-service.exe"
    } else {
        "bluetooth-input-bridge-service"
    });
    Some(candidate)
}

/// 期望写入注册表的命令行（带引号，路径含空格时也能被正确解析）。
pub fn command_line() -> String {
    let path = service_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| product_binary_name().to_string());
    format!("\"{path}\"")
}

/// 服务程序文件名（找不到自身路径时的兜底）。
fn product_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "bluetooth-input-bridge-service.exe"
    } else {
        "bluetooth-input-bridge-service"
    }
}

/// 当前是否已开启自启。
///
/// 只判断“值存在”会谎报：程序换过安装目录后，注册表里还留着旧路径的自启项，
/// 开机时系统只会静默失败。这里同时比对命令行，路径不一致视为需要修复（返回 `false`，
/// 让界面上开关显示为关闭，用户再打开一次就会写入新路径）。
pub fn is_enabled() -> bool {
    match read_run_value() {
        Some(current) => same_command(&current, &command_line()),
        None => false,
    }
}

/// 注册表里记录的自启命令行（空格规范化后比较）。
fn same_command(current: &str, expected: &str) -> bool {
    normalize(current) == normalize(expected)
}

fn normalize(command: &str) -> String {
    let trimmed = command.trim();
    // 去掉首尾引号的分层差异：带引号和不带引号指向同一个程序也算一致。
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.split('"').next())
        .unwrap_or(trimmed)
        .trim();
    unquoted.to_lowercase()
}

/// 读出自启值（REG_SZ），不存在或读取失败返回 `None`。
fn read_run_value() -> Option<String> {
    let key_path = run_key_path();
    let mut key = HKEY::default();
    // SAFETY: 键路径是只读字符串，句柄只在本函数内使用并在末尾关闭。
    unsafe {
        if RegOpenKeyExW(HKEY_CURRENT_USER, &key_path, Some(0), KEY_READ, &mut key) != ERROR_SUCCESS
        {
            return None;
        }
        let name = HSTRING::from(RUN_VALUE);
        let mut data_type = REG_SZ;
        let mut len = 0u32;
        let status = RegQueryValueExW(key, &name, None, Some(&mut data_type), None, Some(&mut len));
        if status != ERROR_SUCCESS || len == 0 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buffer = vec![0u8; len as usize];
        let status = RegQueryValueExW(
            key,
            &name,
            None,
            Some(&mut data_type),
            Some(buffer.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = RegCloseKey(key);
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate(len as usize);
        Some(decode_reg_sz(&buffer))
    }
}

/// REG_SZ 是 UTF-16LE、可带结尾 NUL 的字节串。
fn decode_reg_sz(bytes: &[u8]) -> String {
    // 末尾不足两个字节的残余直接忽略（合法值总是偶数长）。
    let (pairs, _tail) = bytes.as_chunks::<2>();
    let mut units: Vec<u16> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let unit = u16::from_le_bytes(*pair);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    String::from_utf16_lossy(&units)
}

/// 开启/关闭自启，返回注册表里**实际**的状态（写入失败时不会谎报成功）。
pub fn apply(enabled: bool) -> Result<bool, String> {
    if enabled {
        write_run_value()?;
    } else {
        delete_run_value()?;
    }
    Ok(is_enabled())
}

fn open_run_key(write: bool) -> Result<HKEY, WIN32_ERROR> {
    let key_path = run_key_path();
    let mut key = HKEY::default();
    // SAFETY: 输出句柄由本模块负责关闭。
    unsafe {
        if write {
            let status = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &key_path,
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            );
            if status != ERROR_SUCCESS {
                return Err(status);
            }
        } else {
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &key_path,
                Some(0),
                KEY_SET_VALUE,
                &mut key,
            );
            if status != ERROR_SUCCESS {
                return Err(status);
            }
        }
    }
    Ok(key)
}

fn write_run_value() -> Result<(), String> {
    let command = command_line();
    if let Some(path) = service_binary() {
        // 自启项指向一个不存在的文件时，开机只会静默失败；宁可让用户现在就知道。
        if !path.is_file() {
            return Err(format!(
                "找不到无界面服务程序：{}（请先重新安装或编译）",
                path.display()
            ));
        }
    }
    let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
    let key = open_run_key(true)
        .map_err(|status| format!("无法打开开机自启注册表项（Win32 {:#06X}）", status.0))?;
    let name = HSTRING::from(RUN_VALUE);
    // SAFETY: wide 是 NUL 结尾的 UTF-16 缓冲区，长度按字节传入（含结尾 NUL）。
    let status = unsafe {
        RegSetValueExW(
            key,
            &name,
            None,
            REG_SZ,
            Some(std::slice::from_raw_parts(
                wide.as_ptr().cast::<u8>(),
                wide.len() * 2,
            )),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        return Err(format!("写入开机自启失败（Win32 {:#06X}）", status.0));
    }
    super::service::log(&format!("已开启开机自启：{command}"));
    Ok(())
}

fn delete_run_value() -> Result<(), String> {
    let key = match open_run_key(false) {
        Ok(key) => key,
        // Run 项不存在 = 本来就没开启，视为成功，也不再写一条“已关闭”的日志。
        Err(_) => return Ok(()),
    };
    let name = HSTRING::from(RUN_VALUE);
    // SAFETY: 句柄来自 open_run_key，参数是只读字符串。
    let status = unsafe { RegDeleteValueW(key, &name) };
    let _ = unsafe { RegCloseKey(key) };
    if status == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        // 本来就没设置：不是一次实际变更，保持日志干净。
        return Ok(());
    }
    if status != ERROR_SUCCESS {
        return Err(format!("删除开机自启失败（Win32 {:#06X}）", status.0));
    }
    super::service::log("已关闭开机自启");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_value_name_is_stable() {
        // 改名会让老用户的自启项残留，必须固定。
        assert_eq!(RUN_VALUE, "BluetoothInputBridge");
    }

    #[test]
    fn command_line_is_quoted() {
        let line = command_line();
        assert!(line.starts_with('"'), "{line}");
        assert!(line.ends_with('"'), "{line}");
        assert!(line.contains("bluetooth-input-bridge-service"), "{line}");
    }

    #[test]
    fn service_binary_prefers_env_override() {
        // SAFETY: 本测试独占该环境变量名，并立即恢复。
        let previous = std::env::var_os("BLUETOOTH_BRIDGE_SERVICE");
        unsafe { std::env::set_var("BLUETOOTH_BRIDGE_SERVICE", r"C:\tmp\svc.exe") };
        let path = service_binary();
        match previous {
            Some(value) => unsafe { std::env::set_var("BLUETOOTH_BRIDGE_SERVICE", value) },
            None => unsafe { std::env::remove_var("BLUETOOTH_BRIDGE_SERVICE") },
        }
        assert_eq!(
            path.as_deref(),
            Some(std::path::Path::new(r"C:\tmp\svc.exe"))
        );
    }

    #[test]
    fn product_binary_name_matches_the_cargo_bin() {
        assert_eq!(product_binary_name(), "bluetooth-input-bridge-service.exe");
    }

    #[test]
    fn reg_sz_value_is_decoded_as_utf16() {
        // “C:\a b\svc.exe” + 结尾 NUL，按 UTF-16LE 存放。
        let text = "C:\\a b\\svc.exe";
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        assert_eq!(decode_reg_sz(&bytes), text);
        // 没有结尾 NUL 也要能读。
        let bytes: Vec<u8> = "svc.exe"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_reg_sz(&bytes), "svc.exe");
        assert_eq!(decode_reg_sz(&[]), "");
    }

    #[test]
    fn stale_autostart_path_is_reported_as_off() {
        // 换过安装目录后注册表里残留旧路径：必须判定为“没开启”，否则用户看到开关是开的，
        // 开机却启动不了（系统只会静默失败）。
        let current = r#""D:\old\bluetooth-input-bridge-service.exe""#;
        let expected = r#""D:\new\bluetooth-input-bridge-service.exe""#;
        assert!(!same_command(current, expected), "{current} / {expected}");
        assert!(same_command(
            current,
            r#"D:\old\bluetooth-input-bridge-service.exe"#
        ));
        // 大小写差异不算变化（Windows 路径不区分大小写）。
        assert!(same_command(r#""C:\SVC.EXE""#, r#""c:\svc.exe""#));
        // 带不带引号指向同一路径也算一致。
        assert!(same_command(r#""C:\svc.exe""#, r#"C:\svc.exe"#));
    }

    #[test]
    fn command_line_normalization_is_stable() {
        assert_eq!(normalize(r#""a b c.exe""#), normalize("a b c.exe"));
        assert_eq!(normalize("  x  "), "x");
    }
}
