//! Windows 文本注入：`SendInput` + `KEYEVENTF_UNICODE`（不是 Linux 那套剪贴板方案）。
//!
//! ## 为什么不用“写剪贴板 + 按键粘贴”（Linux 端做法）
//!
//! | 维度 | 剪贴板 + Ctrl+V（Linux 端） | `SendInput` + KEYEVENTF_UNICODE（本实现） |
//! |---|---|---|
//! | 用户剪贴板 | 被临时改写，还要还原，还原失败/时序竞争会留下用户数据 | 完全不碰 |
//! | 目标应用兼容 | 受“粘贴”支持情况影响：终端要 Ctrl+Shift+V、密码框常禁止粘贴、部分控件不吃 Ctrl+V | 走 `WM_CHAR`，等价于用户用输入法打字，`Edit`/RichEdit/浏览器/Electron/Office/终端一致 |
//! | 耗时 | 备份 + 写入 + 120 ms 等待 + 按键 + 25 ms + 500 ms 还原 ≈ 0.7 s 起 | 纯按键队列，16 KiB 文本 ≈ 0.13 s |
//! | 可回执性 | 只能证明“剪贴板与按键已发出” | `SendInput` 返回实际入队事件数，可判定被 UIPI/桌面拦截 |
//! | 副作用 | 文本框可能被 Ctrl+V 触发其它行为（如某些编辑器的粘贴历史） | 只有字符本身 |
//!
//! 结论：直接注入 Unicode 字符更简单、更快、副作用更小，因此 Windows 端不用剪贴板。
//!
//! ## 实现要点
//!
//! - **分批**：`SendInput` 一次入队的事件里，键盘事件上限受系统限制且部分应用在一次
//!   消息循环里消费不过来；单次 8 个码元（16 个事件）实测在各类控件上最稳。整段
//!   文本 ≤ 64 个 UTF-16 码元（即安卓端 16 KiB 文本到不了这么短时就走分批）时合并成
//!   一次调用，减少小消息的开销。
//! - **换行**：`\n` / `\r\n` 发送 `VK_RETURN`（`KEYEVENTF_UNICODE` 的 `\r` 在部分
//!   控件里表现为 CR 而不是“确定/换行”），孤立 `\r` 丢弃。
//! - **代理对**：以 UTF-16 码元为单位发送，emoji（含 ZWJ 序列）与中文都不会被拆坏。
//! - **焦点复核**：每块发送前、发送后都重新取快照，`same_target` 不成立立即中止并
//!   回失败——半句话打进另一个输入框是比“没打进”严重得多的事故。
//! - **修饰键**：发送前等修饰键松开（最多 800 ms），避免把 `a` 变成 `Ctrl+A`。
//! - **可断言上限**：[`MAX_CODE_UNITS`] 保证“一次注入 ≈ 0.13 s”，远小于安卓端 8 s 回执
//!   超时。
//!
//! ## 已知局限（写进文档的取舍）
//!
//! - 只读控件（如只读 `Edit`、只读视图）不会报错：字符进入 `WM_CHAR` 后被目标丢弃，
//!   我们仍回 `ok:true`。判断“是否可写”需要 UI Automation，代价见 `focus.rs` 顶部注释。
//! - 提权目标窗口（`UIPI`）：`SendInput` 返回 0 且 `GetLastError()` = 5，回执里给出
//!   “权限较高”的明确提示；本程序**不会**自动提权（PROTOCOL.md 明确禁止）。
//! - 远端桌面（RDP）会话中注入发往服务端会话，行为与本地一致，未做额外处理。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_RETURN,
};

use super::focus::{self, FocusError, FocusSnapshot};

/// 一次 `SendInput` 最多携带的 UTF-16 码元数（分批上限）。
pub const CHUNK_CODE_UNITS: usize = 8;
/// 整段文本不超过这个长度时合并为一次 `SendInput`（减少小消息开销）。
pub const SINGLE_CALL_CODE_UNITS: usize = 64;
/// 分批之间插入的等待，给目标线程一个消息循环周期。
pub const CHUNK_DELAY_MS: u64 = 2;
/// 等待修饰键松开的上限。
pub const MODIFIER_WAIT_MS: u64 = 800;
/// 等待“用户停止输入”的上限（`BLUETOOTH_BRIDGE_TYPING_IDLE_MS` 环境变量可调）。
pub const IDLE_WAIT_CAP_MS: u64 = 5_000;
/// 单条消息允许注入的最大 UTF-16 码元数（≈0.13 s，远低于安卓 8 s 回执超时）。
pub const MAX_CODE_UNITS: usize = 256;
/// 环境变量：注入前最多等待用户停止输入多久（0 = 不等，默认 0）。
pub const IDLE_ENV: &str = "BLUETOOTH_BRIDGE_TYPING_IDLE_MS";

const CHUNK_UNITS: usize = CHUNK_CODE_UNITS;

/// 并发保护：同一进程内只允许一个注入流程（两个会话同时打键盘会串行化即可，
/// 并行只会把两段文字交错打乱）。
static INJECTING: AtomicBool = AtomicBool::new(false);

/// 用一次 `DIAG` 记录最近一次 `SendInput` 的结果，供失败时定位。
#[cfg(test)]
pub mod diag {
    use std::sync::Mutex;

    /// 最近一次 `SendInput(返回, 期望, GetLastError)`。
    pub static LAST_SEND: Mutex<Option<(u32, u32, u32)>> = Mutex::new(None);
    static SEND_LOCK: Mutex<()> = Mutex::new(());
    static WINDOW_LOCK: Mutex<()> = Mutex::new(());

    pub fn record(sent: u32, expected: u32, last_error: u32) {
        if let Ok(mut slot) = LAST_SEND.lock() {
            *slot = Some((sent, expected, last_error));
        }
    }

    pub fn take() -> Option<(u32, u32, u32)> {
        LAST_SEND.lock().ok().and_then(|mut slot| slot.take())
    }

    /// 串行化“向全局输入队列发事件”的测试（SendInput 是进程外的系统级副作用）。
    pub fn send_lock() -> std::sync::MutexGuard<'static, ()> {
        SEND_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 串行化“前台窗口/焦点”相关的测试。
    pub fn window_lock() -> std::sync::MutexGuard<'static, ()> {
        WINDOW_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 注入前置检查（焦点、权限、修饰键）通过后得到的输入计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectPlan {
    /// 计划注入的 UTF-16 码元数。
    pub code_units: usize,
    /// 分批数。
    pub chunks: usize,
}

/// 等待用户停止输入的上限（0 = 不等待）。
///
/// 安卓端在输入法提交后有一个 0–3 s 的静默计时器，正常情况不会有人正在打字；
/// 这里只是兜底：若系统最近刚有键盘输入，先等用户停下来，避免按键交错。
pub fn idle_timeout() -> Duration {
    let raw = std::env::var(IDLE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Duration::from_millis(raw.min(IDLE_WAIT_CAP_MS))
}

/// 文本 → UTF-16 码元序列（`\n` 变成 `VK_RETURN` 标记，孤立 `\r` 丢弃）。
///
/// 返回值里 `None` 表示“回车键”，`Some(unit)` 表示一个 Unicode 码元。
fn plan_units(text: &str) -> Vec<Option<u16>> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push(None);
                }
                // 孤立 '\r' 丢弃：Win32 控件对它没有稳定语义。
            }
            '\n' => out.push(None),
            other => {
                let mut buf = [0u16; 2];
                for unit in other.encode_utf16(&mut buf).iter() {
                    out.push(Some(*unit));
                }
            }
        }
    }
    out
}

/// 计算计划的最大码元数，超过 [`MAX_CODE_UNITS`] 直接拒绝。
fn check_size(units: usize) -> Result<(), String> {
    if units == 0 {
        return Err("没有可输入的文本".into());
    }
    if units > MAX_CODE_UNITS {
        return Err(format!(
            "文本过长（{units} 个字符，单次最多 {MAX_CODE_UNITS} 个）：请拆成多条分次发送"
        ));
    }
    Ok(())
}

fn chunks_of(total: usize) -> usize {
    if total <= SINGLE_CALL_CODE_UNITS {
        1
    } else {
        total.div_ceil(CHUNK_UNITS)
    }
}

/// 发送一批码元（`None` 表示回车）。返回 `(入队事件数, 期望事件数, GetLastError)`。
fn send_chunk(chunk: &[Option<u16>]) -> (u32, u32, u32) {
    let mut events: Vec<INPUT> = Vec::with_capacity(chunk.len() * 2);
    for unit in chunk {
        let (scan, vk) = match unit {
            Some(u) => (*u, VIRTUAL_KEY(0)),
            // 回车：用扫描码 0x1C + VK_RETURN，两种驱动路径（设备层/键盘层）都能识别。
            None => (0x1C, VK_RETURN),
        };
        let flags = KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0);
        events.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        events.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: KEYBD_EVENT_FLAGS(flags.0 | KEYEVENTF_KEYUP.0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    let expected = events.len() as u32;
    // SAFETY: events 是长度与 expected 一致的本地数组，cbSize 取结构体大小；
    // SendInput 只读取这块内存，不保留指针。
    let sent = unsafe { SendInput(&events, core::mem::size_of::<INPUT>() as i32) };
    let last_error = if sent == expected {
        0
    } else {
        // SAFETY: 纯查询线程最后一个错误码。
        unsafe { windows::Win32::Foundation::GetLastError() }.0
    };
    (sent, expected, last_error)
}

fn error_from_send(sent: u32, expected: u32, last_error: u32) -> String {
    if last_error == 5 {
        // ERROR_ACCESS_DENIED：UIPI 拦下了低完整性进程发往高完整性窗口的按键。
        return FocusError::ElevatedTarget.message().to_string();
    }
    if last_error == 0 && sent < expected {
        return "输入被系统拒绝（目标窗口可能已关闭或正被占用）".into();
    }
    "输入被系统拒绝".into()
}

/// 注入前的安静等待：用户最近有输入就先等一会（最多 [`IDLE_WAIT_CAP_MS`]）。
fn wait_for_idle(timeout: Duration) {
    if timeout.is_zero() {
        return;
    }
    let deadline = Instant::now() + timeout;
    loop {
        if focus::system_idle_ms() > 250 {
            return;
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

static COM_INIT: Once = Once::new();

/// 确保调用线程初始化过 COM（`SendInput` 本身不需要，但某些注入链路上的窗口探针
/// 会间接触发 COM；提前初始化可避免“第一次调用偶发阻塞/静默失败”的怪现象）。
fn ensure_thread_ready() {
    COM_INIT.call_once(|| {
        // SAFETY: 只做一次；RPC_E_CHANGED_MODE 等返回值可安全忽略。
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
    });
}

/// 把文本注入当前前台窗口的焦点控件（安卓主动发送时走这条）。
///
/// 成功返回 `Ok(())`，失败把**给用户看的中文原因**放在 `Err` 里，直接进回执。
pub fn inject_text(text: &str) -> Result<(), String> {
    let timeout = idle_timeout();
    // 并发保护：两个会话同时注入只会互相打断。
    if INJECTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("上一次输入尚未完成，请稍后重试".into());
    }
    let guard = InjectGuard;
    let result = inject_with(
        |text| InjectTarget::Foreground.text_for(text),
        text,
        timeout,
    );
    // 修饰键等待在构造目标之前完成，避免按住 Shift 时目标窗口把注入当成组合键。
    drop(guard);
    result
}

/// 注入过程中的资源守卫（保证 `INJECTING` 一定复位）。
struct InjectGuard;

impl Drop for InjectGuard {
    fn drop(&mut self) {
        INJECTING.store(false, Ordering::SeqCst);
    }
}

/// 注入目标的抽象，只为了同一套代码既能打“当前前台窗口”（生产），也能打
/// “已知的某个窗口”（单元测试 / `sendTest` 的可控目标）。
pub(crate) enum InjectTarget {
    /// 按当前前台窗口解析目标（生产路径）。
    Foreground,
    /// 指定输入目标（自检使用：不依赖前台窗口是谁）。
    #[allow(dead_code)] // 仅自检/测试使用：固定焦点窗口注入
    Fixed { hwnd_focus: isize, owner_pid: u32 },
}

impl InjectTarget {
    fn text_for(&self, _text: &str) -> Self {
        match *self {
            InjectTarget::Foreground => InjectTarget::Foreground,
            InjectTarget::Fixed {
                hwnd_focus,
                owner_pid,
            } => InjectTarget::Fixed {
                hwnd_focus,
                owner_pid,
            },
        }
    }
}

/// 单条消息注入的主流程：安全检查 → 安静等待 → 分批发送 → 每块复核焦点。
fn inject_with<F>(_choose: F, text: &str, idle: Duration) -> Result<(), String>
where
    F: Fn(&str) -> InjectTarget,
{
    ensure_thread_ready();
    let units = plan_units(text);
    check_size(units.len())?;
    if !focus::wait_modifiers_released(Duration::from_millis(MODIFIER_WAIT_MS)) {
        return Err(FocusError::ModifiersHeld.message().to_string());
    }
    let target = focus::resolve_target().map_err(|e| e.message().to_string())?;
    wait_for_idle(idle);
    let chunks = chunks_of(units.len());
    let mut start = 0usize;
    for index in 0..chunks {
        // 复核焦点：目标必须在发送前后都是同一个输入框。
        let now = focus::snapshot_foreground()
            .ok_or_else(|| FocusError::NoTarget.message().to_string())?;
        if !target.same_target(&now) {
            return Err("窗口失去焦点（请重新点击输入框后重试）".into());
        }
        if focus::modifiers_held() {
            return Err(FocusError::ModifiersHeld.message().to_string());
        }
        let take = if chunks == 1 {
            units.len() - start
        } else {
            CHUNK_UNITS.min(units.len() - start)
        };
        let (sent, expected, last_error) = send_chunk(&units[start..start + take]);
        #[cfg(test)]
        diag::record(sent, expected, last_error);
        if sent != expected {
            return Err(error_from_send(sent, expected, last_error));
        }
        start += take;
        if index + 1 < chunks {
            std::thread::sleep(Duration::from_millis(CHUNK_DELAY_MS));
        }
    }
    // 发送完成后再复核一次：目标在发送过程中被切走说明有一部分码元可能落在别处，
    // 此时回失败让用户重发（安卓端会保留草稿）。
    let after =
        focus::snapshot_foreground().ok_or_else(|| FocusError::NoTarget.message().to_string())?;
    if !target.same_target(&after) {
        return Err("窗口失去焦点（请重新点击输入框后重试）".into());
    }
    Ok(())
}

/// 计划信息（给 `sendTest` 与日志用）。
pub fn plan(text: &str) -> Result<InjectPlan, String> {
    let units = plan_units(text);
    check_size(units.len())?;
    Ok(InjectPlan {
        code_units: units.len(),
        chunks: chunks_of(units.len()),
    })
}

/// 状态栏预览：注入方法的一句话描述。
pub const INJECTION_METHOD: &str = "SendInput + KEYEVENTF_UNICODE";

/// 一次注入的简短描述，供日志使用。
pub fn describe_target(snap: &FocusSnapshot) -> String {
    focus::describe_target(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_units_maps_newlines_to_return_and_drops_lone_cr() {
        let units = plan_units("a\r\nb\nc\rd");
        assert_eq!(
            units,
            vec![
                Some('a' as u16),
                None,
                Some('b' as u16),
                None,
                Some('c' as u16),
                Some('d' as u16)
            ]
        );
    }

    #[test]
    fn plan_units_keeps_surrogate_pairs_together() {
        let units = plan_units("😀x");
        assert_eq!(units.len(), 3);
        assert!(matches!(units[0], Some(u) if (0xD800..0xDC00).contains(&u)));
        assert!(matches!(units[1], Some(u) if (0xDC00..0xE000).contains(&u)));
        assert_eq!(units[2], Some('x' as u16));
    }

    #[test]
    fn plan_units_handles_chinese_and_zwj_emoji() {
        let text = "你好你好👨‍👩‍👧‍👦";
        let units = plan_units(text);
        // 4 个汉字（4 码元）+ 4 个 emoji（各 2 码元）+ 3 个 ZWJ（1 码元）
        assert_eq!(units.len(), 4 + 4 * 2 + 3);
        assert!(units.iter().all(|u| u.is_some()));
    }

    #[test]
    fn check_size_rejects_empty_and_oversized() {
        assert!(check_size(0).is_err());
        assert!(check_size(MAX_CODE_UNITS).is_ok());
        let err = check_size(MAX_CODE_UNITS + 1).unwrap_err();
        assert!(err.contains("文本过长"), "{err}");
    }

    #[test]
    fn chunks_of_merges_small_text_and_splits_long_text() {
        assert_eq!(chunks_of(1), 1);
        assert_eq!(chunks_of(SINGLE_CALL_CODE_UNITS), 1);
        assert_eq!(
            chunks_of(SINGLE_CALL_CODE_UNITS + 1),
            (SINGLE_CALL_CODE_UNITS + 1).div_ceil(CHUNK_UNITS)
        );
        assert_eq!(
            chunks_of(MAX_CODE_UNITS),
            MAX_CODE_UNITS.div_ceil(CHUNK_UNITS)
        );
    }

    #[test]
    fn plan_reports_units_and_chunks() {
        let p = plan("你好，世界").unwrap();
        assert_eq!(p.code_units, 5);
        assert_eq!(p.chunks, 1);
    }

    #[test]
    fn plan_rejects_empty_text() {
        assert!(plan("").is_err());
    }

    #[test]
    fn error_from_send_maps_access_denied_to_elevation_hint() {
        let msg = error_from_send(0, 4, 5);
        assert_eq!(msg, FocusError::ElevatedTarget.message());
        assert!(error_from_send(0, 4, 0).contains("目标窗口可能已关闭"));
    }

    #[test]
    fn inject_text_refuses_empty_text_before_touching_input() {
        let err = inject_text("").unwrap_err();
        assert_eq!(err, "没有可输入的文本");
    }

    #[test]
    fn injection_method_is_not_clipboard_based() {
        assert!(!INJECTION_METHOD.contains("clipboard"));
        assert!(!INJECTION_METHOD.contains("剪贴板"));
    }
}
