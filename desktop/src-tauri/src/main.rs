// Windows 上必须是 GUI 子系统，否则双击 exe 会附带一个黑色的控制台窗口。
// Tauri v2 不再替我们加这个属性（v1 时是模板自带的），所以要显式写。
// Linux/macOS 上这两种子系统属性不存在，只在 Windows 目标下生效。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    bluetooth_input_desktop_lib::run()
}
