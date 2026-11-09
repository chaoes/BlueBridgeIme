//! 把对端蓝牙地址换成设备名（手机型号）。
//!
//! WinSock 的 `accept()` 只给出 `SOCKADDR_BTH`（里面是 BT_ADDR），设备名得另查；
//! 这里用 Win32 的 `BluetoothGetDeviceInfo` 查已配对设备缓存，查不到就退回地址。
//!
//! 名称只用于界面显示，功能不依赖它，所以整条链路都以 `Option` 表达失败。

use windows::Win32::Devices::Bluetooth::{BluetoothGetDeviceInfo, BLUETOOTH_DEVICE_INFO};

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
/// `hRadio` 传 `None` 表示“任意本地蓝牙适配器”——我们只关心对端地址。
pub fn lookup(address: &str) -> Option<String> {
    let value = address_to_u64(address)?;
    // BLUETOOTH_DEVICE_INFO 的 sizeOfStruct 是 ABI 的一部分，必须填。
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    info.Address.Anonymous.ullLong = value;
    let code = unsafe { BluetoothGetDeviceInfo(None, &mut info) };
    // ERROR_SUCCESS = 0；查不到设备返回 1168 (ERROR_NOT_FOUND)。
    if code != 0 {
        return None;
    }
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
