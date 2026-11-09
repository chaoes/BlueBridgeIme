//! 把对端蓝牙地址换成设备名（手机型号）。
//!
//! WinSock 的 `accept()` 只给出 `SOCKADDR_BTH`（里面是 BT_ADDR），设备名得另查；
//! 这里用 Win32 的 Bluetooth API 查设备缓存，查不到就退回地址。
//!
//! 查询分两条路：
//!
//! 1. `BluetoothGetDeviceInfo(hRadio = NULL, …)`：最直接，但 **NULL 句柄在多适配器或
//!    适配器状态异常时会整体失败**（ERROR_INVALID_HANDLE 等），并不等于“设备不存在”；
//! 2. 失败时用 `BluetoothFindFirstDevice` / `BluetoothFindNextDevice` 遍历缓存，按地址匹配
//!    （`fIssueInquiry = FALSE`，不触发新一轮搜索，不会让界面卡住几秒）。
//!
//! 名称只用于界面显示，功能不依赖它，所以整条链路都以 `Option` 表达失败。

use windows::Win32::Devices::Bluetooth::{
    BluetoothFindDeviceClose, BluetoothFindFirstDevice, BluetoothFindNextDevice,
    BluetoothGetDeviceInfo, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS,
};

/// 设备信息的结构体大小必须填（ABI 的一部分）。
fn blank_device_info(address: u64) -> BLUETOOTH_DEVICE_INFO {
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    info.Address.Anonymous.ullLong = address;
    info
}

/// 把 `AA:BB:CC:DD:EE:FF` 形式的地址转成 u64（蓝牙地址是 48 位）。
pub fn address_to_u64(address: &str) -> Option<u64> {
    let hex: String = address.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    u64::from_str_radix(&hex, 16).ok()
}

/// 查询已配对设备缓存里的名称；查不到返回 `None`。
///
/// 先用 `BluetoothGetDeviceInfo(None, …)`（最快），失败再遍历设备缓存。
pub fn lookup(address: &str) -> Option<String> {
    let value = address_to_u64(address)?;
    if let Some(name) = lookup_direct(value) {
        return Some(name);
    }
    lookup_by_scan(value)
}

/// `BluetoothGetDeviceInfo`，`hRadio = NULL` 表示“任意本地适配器”。
fn lookup_direct(value: u64) -> Option<String> {
    let mut info = blank_device_info(value);
    let code = unsafe { BluetoothGetDeviceInfo(None, &mut info) };
    // ERROR_SUCCESS = 0；查不到设备返回 1168 (ERROR_NOT_FOUND)。
    if code != 0 {
        return None;
    }
    name_of(&info)
}

/// 遍历设备缓存按地址匹配（不发起查询，只读本地缓存）。
fn lookup_by_scan(value: u64) -> Option<String> {
    let mut params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: true.into(),
        fReturnRemembered: true.into(),
        fReturnConnected: true.into(),
        // 不发起 inquiry：只读缓存，避免多等 10 秒。
        fIssueInquiry: false.into(),
        ..Default::default()
    };
    params.dwSize = std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32;
    let mut info = blank_device_info(0);
    // SAFETY: 两个结构体都已按 ABI 填好 dwSize，句柄在末尾关闭。
    let find = unsafe { BluetoothFindFirstDevice(&params, &mut info) };
    let find = match find {
        Ok(find) if !find.is_invalid() => find,
        _ => return None,
    };
    let mut found = None;
    loop {
        // SAFETY: info 由 BluetoothFindFirstDevice/BluetoothFindNextDevice 填满整个结构体。
        if unsafe { info.Address.Anonymous.ullLong } == value {
            found = name_of(&info);
            break;
        }
        // SAFETY: find 来自 BluetoothFindFirstDevice，info 是有效的结构体缓冲区。
        if unsafe { BluetoothFindNextDevice(find, &mut info) }.is_err() {
            break;
        }
    }
    // SAFETY: find 只在这里关闭一次。
    let _ = unsafe { BluetoothFindDeviceClose(find) };
    found
}

/// 非空名称才算查到了。
fn name_of(info: &BLUETOOTH_DEVICE_INFO) -> Option<String> {
    let name = utf16_z_string(&info.szName);
    if name.trim().is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 固定长度 UTF-16 数组 → String（截断到第一个 NUL）。
fn utf16_z_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// 给 UI 用的名称：查得到用名称，查不到用地址。空地址返回 `None`。
pub fn display_name(address: Option<&str>) -> Option<String> {
    let address = address?;
    if address.trim().is_empty() {
        return None;
    }
    Some(lookup(address).unwrap_or_else(|| address.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parsing_accepts_common_forms() {
        assert_eq!(address_to_u64("AA:BB:CC:DD:EE:FF"), Some(0xAABBCCDDEEFF));
        assert_eq!(address_to_u64("aa-bb-cc-dd-ee-ff"), Some(0xAABBCCDDEEFF));
        assert_eq!(address_to_u64("AABBCCDDEEFF"), Some(0xAABBCCDDEEFF));
        assert_eq!(address_to_u64("ZZ:BB:CC:DD:EE:FF"), None);
        assert_eq!(address_to_u64("AA:BB"), None);
    }

    #[test]
    fn utf16_helpers_stop_at_nul() {
        assert_eq!(utf16_z_string(&[0x41, 0x42, 0]), "AB");
        assert_eq!(utf16_z_string(&[0x4F60, 0x597D]), "你好");
        assert_eq!(utf16_z_string(&[]), "");
    }

    #[test]
    fn display_name_falls_back_to_address() {
        // 没有对应设备时 lookup 失败，必须退回地址本身；本机若恰好有同地址的
        // 已配对设备则会返回真实名称，所以断言“要么是设备名、要么是地址”。
        let address = "11:22:33:44:55:66";
        let name = display_name(Some(address)).expect("非空地址必须有显示名");
        assert!(!name.trim().is_empty(), "{name}");
        if name != address {
            assert_eq!(lookup(address).as_deref(), Some(name.as_str()));
        }
        assert_eq!(display_name(None), None);
        assert_eq!(display_name(Some("   ")), None);
    }
}
