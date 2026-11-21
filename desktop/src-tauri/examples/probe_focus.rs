//! 实机焦点判定探针：对“当前前台窗口”跑一遍真实的 resolve_target()，
//! 打印快照细节与判定结果。用于真机验证 focus.rs 的行为（仅 Windows）。
//!
//! 用法：先把要测的应用调到前台，再运行：
//!   cargo run --example probe_focus
//! 每次输出一行 JSON，方便脚本采集。

#[cfg(windows)]
mod probe {
    use bluetooth_input_desktop_lib::service::win::focus::{
        check_snapshot, resolve_target, snapshot_foreground, window_class, window_style,
    };

    const WS_CHILD: u32 = 0x4000_0000;

    fn cls_or_dash(hwnd: isize) -> String {
        if hwnd == 0 {
            "-".to_string()
        } else {
            let c = window_class(hwnd);
            if c.is_empty() { "-".to_string() } else { c }
        }
    }

    pub fn run() {
        match snapshot_foreground() {
            None => println!("{{\"verdict\":\"no_foreground\"}}"),
            Some(snap) => {
                let focus_class = cls_or_dash(snap.hwnd_focus);
                let fg_class = cls_or_dash(snap.hwnd_foreground);
                let root_class = cls_or_dash(snap.hwnd_root);
                let style = window_style(snap.hwnd_focus);
                let is_child = style & WS_CHILD != 0;
                let root_is_fg = snap.hwnd_root == snap.hwnd_foreground;
                let check = check_snapshot(&snap).map_err(|e| e.message());
                let resolve = resolve_target().map_err(|e| e.message());
                println!(
                    "{{\"fg\":{},\"fgClass\":{:?},\"focus\":{},\"focusClass\":{:?},\"rootClass\":{:?},\"style\":\"0x{style:08X}\",\"child\":{is_child},\"rootIsFg\":{root_is_fg},\"check\":{:?},\"resolve\":{:?}}}",
                    snap.hwnd_foreground,
                    fg_class,
                    snap.hwnd_focus,
                    focus_class,
                    root_class,
                    check,
                    resolve,
                );
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    probe::run();
}

/// 非 Windows 平台没有前台窗口概念；保持示例可编译。
#[cfg(not(windows))]
fn main() {
    println!("probe_focus is Windows-only");
}
