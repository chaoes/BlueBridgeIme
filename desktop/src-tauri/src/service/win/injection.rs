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
//! - **分批**：`SendInput` 一次入队的事件里，键盘事件上限受系统限制，且部分应用在一次
//!   消息循环里消费不过来（实测：单次 ≤128 个事件最稳，单次 2 500 个事件会丢约 21%，
//!   4 000 个丢约 43%）。因此整段文本 ≤ 64 个码元（≤128 事件）时一次调用，更长则每批
//!   8 个码元（16 个事件）+ 2 ms 间隔，把“一大坨”拆成一串小队列积压，目标线程来得及
//!   逐个消化。安卓端单条消息最长 16 KiB 时也只会是一串 32 批以内的小消息，而不是一次
//!   巨型投递。
//! - **换行**：`\n` / `\r\n` 发送 `VK_RETURN`（`KEYEVENTF_UNICODE` 的 `\r` 在部分
//!   控件里表现为 CR 而不是“确定/换行”），孤立 `\r` 丢弃。注意 `\n` 只算**一个**
//!   码元（换行是“按键”不是“两个字符”），因此长度上限按“注入后的码元数”算。
//! - **代理对**：以 UTF-16 码元为单位发送，emoji（含 ZWJ 序列）与中文都不会被拆坏。
//! - **焦点复核**：每块发送前、发送后都重新取快照，`same_target` 不成立立即中止并
//!   回失败——半句话打进另一个输入框是比“没打进”严重得多的事故。
//! - **修饰键**：发送前等修饰键松开（最多 800 ms），避免把 `a` 变成 `Ctrl+A`。
//! - **并发**：只允许一个注入流程；第二个请求立刻拿到
//!   `上一次输入尚未完成，请稍后重试`（**不排队**——排到 8 s 后回执只会变成超时）。
//! - **可断言上限**：[`MAX_CODE_UNITS`] 保证“一次注入 ≈ 0.13 s”，远小于安卓端 8 s 回执
//!   超时。
//! - **部分成功**：`0 < sent < expected` 时已经有一半字符进了目标，回执里会明确说
//!   “已输入一部分”，让用户自己决定是重发全文还是只补剩下的，绝不假装全成或全败。
//!
//! ## 已知局限（写进文档的取舍）
//!
//! - 只读控件（如只读 `Edit`、只读视图）不会报错：字符进入 `WM_CHAR` 后被目标丢弃，
//!   我们仍回 `ok:true`。判断“是否可写”需要 UI Automation，代价见 `focus.rs` 顶部注释。
//! - 提权目标窗口（`UIPI`）：`SendInput` 返回 0 且 `GetLastError()` = 5，回执里给出
//!   “权限较高”的明确提示；本程序**不会**自动提权（PROTOCOL.md 明确禁止）。
//! - 远端桌面（RDP）会话中注入发往服务端会话，行为与本地一致，未做额外处理。
//! - `SendInput` 的返回值只说明“事件进了本进程的输入队列”；**锁定/安全桌面**下它可以
//!   返回“成功”而字符被丢弃（`injection.rs` 侧用发送前后的焦点快照兜底，仍有窗口
//!   在锁定瞬间切换的毫秒级竞态，故本模块保留返回值 + `GetLastError` 双重判定）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
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
///
/// 注意这是**注入后**的码元数：`\n`（含 `\r\n`）只占 1 个（换成一次回车键），
/// 只有 `\r` 的文本是空的（会以“没有可输入的文本”拒绝）。
pub const MAX_CODE_UNITS: usize = 256;
/// 环境变量：注入前最多等待用户停止输入多久（0 = 不等，默认 0）。
pub const IDLE_ENV: &str = "BLUETOOTH_BRIDGE_TYPING_IDLE_MS";
/// 环境开关：允许“前台就是本程序”的进程内自检（`focus.rs` 里的同名开关）。
pub const SHARED_FOREGROUND_ENV: &str = "BLUETOOTH_BRIDGE_ALLOW_SHARED_FOREGROUND";

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

/// 一次注入的最终结果：给安卓端的回执 + 给状态栏/日志的一句话。
///
/// `Ok` 表示“字符已经全部进入目标窗口的输入队列”；`Err` 里的中文会原样进回执的
/// `error` 字段。失败时也带上 [`InjectionReport`]，其中 `units_sent` 说明**已经进了
/// 多少**——半句话打进输入框后假装“什么都没发生”是最容易让用户重复输入、把文字搞乱
/// 的做法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectionOutcome {
    /// 已经交给系统的码元数（成功时等于文本的码元数）。
    pub units_sent: usize,
    /// 文本总码元数。
    pub units_total: usize,
    /// 分批数。
    pub chunks: usize,
    /// 是否被“前台就是本程序”的自检开关放开过（仅日志用）。
    pub relaxed_self_check: bool,
}

impl InjectionOutcome {
    /// 是否全部注入完成。
    pub fn is_complete(&self) -> bool {
        self.units_sent == self.units_total
    }

    /// 日志用的一句话，例如 `已输入 12/12 码元（1 批）`。
    pub fn summary(&self) -> String {
        format!(
            "已输入 {}/{} 码元（{} 批）",
            self.units_sent, self.units_total, self.chunks
        )
    }
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
    error_kind(sent, expected, last_error).message().to_string()
}

/// `SendInput` 的失败原因分类（全部有单测覆盖，文案集中在 [`SendError::message`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendError {
    /// `ERROR_ACCESS_DENIED`：UIPI 拦下了低完整性进程发往高完整性窗口的按键。
    Denied,
    /// 一个都没进去，但没有错误码（目标线程/窗口已经不存在，或队列拒绝）。
    Rejected,
    /// 只进去了一部分。
    Partial { sent: u32, expected: u32 },
    /// 有错误码但不是已知的几种。
    Failed(u32),
}

impl SendError {
    fn message(self) -> String {
        match self {
            SendError::Denied => FocusError::ElevatedTarget.message().to_string(),
            SendError::Rejected => "输入被系统拒绝（目标窗口可能已关闭或正被占用）".to_string(),
            SendError::Partial { sent, expected } => format!(
                "输入未完成：已输入 {} 个事件（共 {} 个），请核对输入框内容后再决定是否重发",
                sent / 2,
                expected / 2
            ),
            SendError::Failed(code) => format!("输入被系统拒绝（错误码 {code}）"),
        }
    }
}

/// `SendInput` 返回值 + `GetLastError` → 失败分类。`sent == expected` 时不应调用。
fn error_kind(sent: u32, expected: u32, last_error: u32) -> SendError {
    if last_error == ERROR_ACCESS_DENIED.0 {
        return SendError::Denied;
    }
    if sent > 0 {
        return SendError::Partial { sent, expected };
    }
    if last_error == 0 {
        SendError::Rejected
    } else {
        SendError::Failed(last_error)
    }
}

/// 一次注入流程里的失败（内部类型：既带“给用户看的中文”，也带已发送进度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InjectionError {
    /// 给用户看的中文原因（原样进回执）。
    pub message: String,
    /// 已经交给系统的码元数。
    pub units_sent: usize,
}

impl InjectionError {
    fn new(message: impl Into<String>, units_sent: usize) -> Self {
        Self {
            message: message.into(),
            units_sent,
        }
    }

    /// 从 [`focus`] 的判定结果构造（此时还没发任何字符）。
    fn from_focus(err: FocusError) -> Self {
        Self::new(err.message(), 0)
    }
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
///
/// 线程模型：同步阻塞、可在任意线程调用；同一时刻只允许一个调用（第二个立刻失败，
/// 不排队）。内部只用到本线程自己的 COM 初始化与无状态 Win32 查询，不需要消息循环。
pub fn inject_text(text: &str) -> Result<(), String> {
    inject_text_report(text)
        .map(|_| ())
        .map_err(|(message, _)| message)
}

/// 同 [`inject_text`]，但返回注入进度（供状态栏/日志/自检使用）。
///
/// 失败时 `Err` 的元组里也带进度：`units_sent > 0` 说明已经有字符进过目标窗口。
pub fn inject_text_report(text: &str) -> Result<InjectionOutcome, (String, InjectionOutcome)> {
    let units = plan_units(text);
    let total = units.len();
    let empty = || InjectionOutcome {
        units_sent: 0,
        units_total: total,
        chunks: chunks_of(total),
        relaxed_self_check: focus::shared_foreground_check_disabled(),
    };
    if let Err(message) = check_size(total) {
        return Err((message, empty()));
    }
    // 并发保护：两个会话同时注入只会互相打断。放在最前面，避免等待修饰键/空闲
    // 期间白占用户的时间。
    if INJECTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(("上一次输入尚未完成，请稍后重试".into(), empty()));
    }
    let _guard = InjectGuard;
    let mut outcome = empty();
    match inject_with(&units, idle_timeout(), &mut outcome) {
        Ok(()) => Ok(outcome),
        Err(err) => {
            #[cfg(test)]
            inject_diag::record_failure(&err, &outcome);
            Err((err.message, outcome))
        }
    }
}

/// 注入过程中的资源守卫（保证 `INJECTING` 一定复位）。
struct InjectGuard;

impl Drop for InjectGuard {
    fn drop(&mut self) {
        INJECTING.store(false, Ordering::SeqCst);
    }
}

/// 单条消息注入的主流程：安全检查 → 安静等待 → 分批发送 → 每块复核焦点。
///
/// `outcome.units_sent` 在每次 `SendInput` 成功后累加，异常路径也如实反映“已经打进去
/// 多少”。任何一块失败都不再继续：目标窗口/权限/焦点已经不可信。
fn inject_with(
    units: &[Option<u16>],
    idle: Duration,
    outcome: &mut InjectionOutcome,
) -> Result<(), InjectionError> {
    ensure_thread_ready();
    if !focus::wait_modifiers_released(Duration::from_millis(MODIFIER_WAIT_MS)) {
        return Err(InjectionError::from_focus(FocusError::ModifiersHeld));
    }
    let target = focus::resolve_target().map_err(InjectionError::from_focus)?;
    wait_for_idle(idle);
    stream_units(units, &target, outcome)
}

/// 与 [`inject_with`] 相同的发送主循环，但目标由调用方给定。
///
/// 生产路径只用 `resolve_target` 得到目标；这个入口留给测试构造“目标在下一次复核时
/// 必然不成立”的场景（真实“用户切窗口”没法在测试进程里可靠复现），因此不对外暴露。
#[cfg(test)]
fn inject_with_checking_abort(
    units: &[Option<u16>],
    target: &FocusSnapshot,
    outcome: &mut InjectionOutcome,
) -> Result<(), InjectionError> {
    ensure_thread_ready();
    stream_units(units, target, outcome)
}

/// 分批发送 + 每批前后复核目标是否还是同一个输入框。
fn stream_units(
    units: &[Option<u16>],
    target: &FocusSnapshot,
    outcome: &mut InjectionOutcome,
) -> Result<(), InjectionError> {
    let chunks = outcome.chunks.max(1);
    for index in 0..chunks {
        let start = outcome.units_sent;
        if start >= units.len() {
            break;
        }
        // 复核焦点：目标必须在发送前后都是同一个输入框。
        let now = focus::snapshot_foreground()
            .ok_or_else(|| InjectionError::from_focus(FocusError::NoTarget))?;
        if !target.same_target(&now) {
            return Err(InjectionError::new(
                "窗口失去焦点（请重新点击输入框后重试）",
                start,
            ));
        }
        if focus::modifiers_held() {
            return Err(InjectionError::from_focus(FocusError::ModifiersHeld));
        }
        let remaining = units.len() - start;
        let take = if chunks == 1 {
            remaining
        } else {
            CHUNK_UNITS.min(remaining)
        };
        let (sent, expected, last_error) = send_chunk(&units[start..start + take]);
        #[cfg(test)]
        inject_diag::record_send(sent, expected, last_error);
        if sent != expected {
            // 部分成功时按“实际入队的码元数”记进度（事件与码元 1:1 配对）。
            let landed = (sent / 2) as usize;
            outcome.units_sent = start + landed.min(take);
            return Err(InjectionError::new(
                error_from_send(sent, expected, last_error),
                outcome.units_sent,
            ));
        }
        outcome.units_sent = start + take;
        if index + 1 < chunks && outcome.units_sent < units.len() {
            std::thread::sleep(Duration::from_millis(CHUNK_DELAY_MS));
        }
    }
    // 发送完成后再复核一次：目标在发送过程中被切走说明有一部分码元可能落在别处，
    // 此时回失败让用户重发（安卓端会保留草稿）。
    let after = focus::snapshot_foreground()
        .ok_or_else(|| InjectionError::from_focus(FocusError::NoTarget))?;
    if !target.same_target(&after) {
        return Err(InjectionError::new(
            "窗口失去焦点（请重新点击输入框后重试）",
            outcome.units_sent,
        ));
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
mod inject_diag {
    //! 测试专用的诊断记录（生产构建里不存在）。
    //!
    //! - `LAST_SEND`：最近一次 `SendInput` 的 `(入队数, 期望数, GetLastError)`；
    //! - `LAST_FAILURE`：最近一次失败的文案与已发送进度；
    //! - `SEND_LOCK` / `WINDOW_LOCK`：串行化“真发按键”和“抢占前台/焦点”的测试。
    //!
    //! 为什么必须串行化：`SendInput` 写的是**系统**输入队列，测试并行跑时一个测试
    //! 打的字会进另一个测试的窗口（实测出现两段文本交错），因此两个自检都用同一把锁。

    use std::sync::Mutex;

    use super::InjectionOutcome;

    /// 最近一次 `SendInput(返回, 期望, GetLastError)`。
    pub static LAST_SEND: Mutex<Option<(u32, u32, u32)>> = Mutex::new(None);
    /// 最近一次失败：`(原因, 已发送码元, 总码元, 分批数)`。
    pub static LAST_FAILURE: Mutex<Option<(String, usize, usize, usize)>> = Mutex::new(None);
    static SEND_LOCK: Mutex<()> = Mutex::new(());
    static WINDOW_LOCK: Mutex<()> = Mutex::new(());

    /// 记录一次 `SendInput` 的结果。
    pub fn record_send(sent: u32, expected: u32, last_error: u32) {
        if let Ok(mut slot) = LAST_SEND.lock() {
            *slot = Some((sent, expected, last_error));
        }
    }

    /// 记录一次失败（含已发送进度）。
    pub fn record_failure(err: &super::InjectionError, outcome: &InjectionOutcome) {
        if let Ok(mut slot) = LAST_FAILURE.lock() {
            *slot = Some((
                err.message.clone(),
                err.units_sent,
                outcome.units_total,
                outcome.chunks,
            ));
        }
    }

    /// 取走最近一次 `SendInput` 记录。
    pub fn take_send() -> Option<(u32, u32, u32)> {
        LAST_SEND.lock().ok().and_then(|mut slot| slot.take())
    }

    /// 取走最近一次失败记录。
    pub fn take_failure() -> Option<(String, usize, usize, usize)> {
        LAST_FAILURE.lock().ok().and_then(|mut slot| slot.take())
    }

    /// 清空两条记录（自检开始前调用，避免读到上一次的残留）。
    pub fn clear() {
        if let Ok(mut slot) = LAST_SEND.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = LAST_FAILURE.lock() {
            *slot = None;
        }
    }

    /// 串行化“向全局输入队列发事件”的测试（`SendInput` 是系统级副作用）。
    pub fn send_lock() -> std::sync::MutexGuard<'static, ()> {
        SEND_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 串行化“前台窗口/焦点”相关的测试。
    pub fn window_lock() -> std::sync::MutexGuard<'static, ()> {
        WINDOW_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
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
        // 👨‍👩‍👧‍👦 = 4 个 emoji（各 2 码元）+ 3 个 ZWJ（各 1 码元）
        let text = "你好你好👨‍👩‍👧‍👦";
        let units = plan_units(text);
        assert_eq!(units.len(), 4 + 4 * 2 + 3);
        assert!(units.iter().all(|u| u.is_some()));
    }

    #[test]
    fn plan_units_keeps_combining_marks_as_separate_units() {
        // e + U+0301（组合附加符号）必须分两个码元发出，目标侧才能自己组合。
        let units = plan_units("e\u{0301}");
        assert_eq!(units, vec![Some('e' as u16), Some(0x0301)]);
        // 反过来，预组合形式只有 1 个码元。
        assert_eq!(plan_units("\u{00E9}").len(), 1);
    }

    #[test]
    fn plan_units_keeps_nul_and_tab() {
        // U+0000 / '\t' 都按原样作为字符码元发送，不丢弃、不改写成按键。
        assert_eq!(
            plan_units("a\u{0000}b"),
            vec![Some(0x61), Some(0x0000), Some(0x62)]
        );
        assert_eq!(plan_units("\t"), vec![Some(0x0009)]);
    }

    #[test]
    fn plan_units_newline_counts_as_one_unit() {
        // 长度上限按“注入后的码元数”算：\r\n 只占 1 个（一次回车键）。
        assert_eq!(plan_units("\r\n").len(), 1);
        assert_eq!(plan_units("\n\n\n").len(), 3);
        // 只有 \r 的文本是空的。
        assert!(plan_units("\r").is_empty());
    }

    #[test]
    fn plan_units_of_max_length_text_is_exactly_budget() {
        let text = "字".repeat(MAX_CODE_UNITS);
        assert_eq!(plan_units(&text).len(), MAX_CODE_UNITS);
        let with_newlines = "行\r\n".repeat(MAX_CODE_UNITS / 2);
        assert_eq!(plan_units(&with_newlines).len(), MAX_CODE_UNITS);
    }

    #[test]
    fn check_size_rejects_empty_and_oversized() {
        assert!(check_size(0).is_err());
        assert!(check_size(MAX_CODE_UNITS).is_ok());
        let err = check_size(MAX_CODE_UNITS + 1).unwrap_err();
        assert!(err.contains("文本过长"), "{err}");
    }

    #[test]
    fn check_size_messages_are_actionable() {
        assert_eq!(check_size(0).unwrap_err(), "没有可输入的文本");
        let err = check_size(MAX_CODE_UNITS + 1).unwrap_err();
        assert!(err.contains(&MAX_CODE_UNITS.to_string()), "{err}");
        assert!(err.contains("分次"), "{err}");
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
    fn full_max_text_needs_at_most_thirty_two_sends() {
        // 256 码元 / 每批 8 = 32 次 SendInput，加 31 次 2 ms 间隔 ⇒ 远低于 8 s。
        let chunks = chunks_of(MAX_CODE_UNITS);
        assert!(chunks <= 32, "{chunks}");
        let worst_ms = (chunks as u64 - 1) * CHUNK_DELAY_MS + chunks as u64 * 4;
        assert!(worst_ms < 1_000, "{worst_ms}");
    }

    #[test]
    fn plan_reports_units_and_chunks() {
        let p = plan("你好，世界").unwrap();
        assert_eq!(p.code_units, 5);
        assert_eq!(p.chunks, 1);
        let p = plan(&"字".repeat(MAX_CODE_UNITS)).unwrap();
        assert_eq!(p.code_units, MAX_CODE_UNITS);
        assert_eq!(p.chunks, MAX_CODE_UNITS / CHUNK_UNITS);
    }

    #[test]
    fn plan_rejects_empty_and_oversized_text() {
        assert_eq!(plan("").unwrap_err(), "没有可输入的文本");
        assert_eq!(plan("\r").unwrap_err(), "没有可输入的文本");
        assert!(plan(&"字".repeat(MAX_CODE_UNITS + 1)).is_err());
    }

    #[test]
    fn error_from_send_maps_access_denied_to_elevation_hint() {
        let msg = error_from_send(0, 4, ERROR_ACCESS_DENIED.0);
        assert_eq!(msg, FocusError::ElevatedTarget.message());
    }

    #[test]
    fn error_from_send_distinguishes_closed_target_from_partial_success() {
        // 一个都没进去、没有错误码 → 目标窗口没了/队列拒绝。
        assert_eq!(error_kind(0, 16, 0), SendError::Rejected);
        assert!(error_from_send(0, 16, 0).contains("目标窗口可能已关闭"));
        // 进去了一半 → 必须明说“已输入一部分”，不能报成“窗口已关闭”。
        assert_eq!(
            error_kind(6, 16, 0),
            SendError::Partial {
                sent: 6,
                expected: 16
            }
        );
        let msg = error_from_send(6, 16, 0);
        assert!(msg.contains("已输入 3"), "{msg}");
        assert!(msg.contains("请核对输入框"), "{msg}");
        // 未知错误码保留错误码，便于用户/我们定位。
        assert_eq!(error_kind(0, 16, 87), SendError::Failed(87));
        assert!(error_from_send(0, 16, 87).contains("87"));
    }

    #[test]
    fn injection_outcome_reports_progress() {
        let done = InjectionOutcome {
            units_sent: 12,
            units_total: 12,
            chunks: 1,
            relaxed_self_check: false,
        };
        assert!(done.is_complete());
        assert!(done.summary().contains("12/12"));
        let partial = InjectionOutcome {
            units_sent: 5,
            ..done.clone()
        };
        assert!(!partial.is_complete());
    }

    #[test]
    fn inject_text_refuses_empty_text_before_touching_input() {
        let err = inject_text("").unwrap_err();
        assert_eq!(err, "没有可输入的文本");
        // 只有 \r 的文本同样是空的。
        assert_eq!(inject_text("\r").unwrap_err(), "没有可输入的文本");
    }

    #[test]
    fn oversized_text_is_refused_before_touching_input() {
        let text = "字".repeat(MAX_CODE_UNITS + 1);
        let (err, outcome) = inject_text_report(&text).unwrap_err();
        assert!(err.contains("文本过长"), "{err}");
        assert_eq!(outcome.units_sent, 0);
        assert_eq!(outcome.units_total, MAX_CODE_UNITS + 1);
    }

    #[test]
    fn idle_timeout_reads_environment_bounded_by_cap() {
        // 默认 0（不等待）；环境变量里给超大值也要被上限截断。用同一把锁避免与
        // 其它测试互相看到对方的临时环境变量。
        let _lock = inject_diag::send_lock();
        let original = std::env::var_os(IDLE_ENV);
        // SAFETY: 单线程改环境变量，测试内持锁串行。
        unsafe { std::env::remove_var(IDLE_ENV) };
        assert!(idle_timeout().is_zero());
        // SAFETY: 同上。
        unsafe { std::env::set_var(IDLE_ENV, "600000") };
        assert_eq!(idle_timeout(), Duration::from_millis(IDLE_WAIT_CAP_MS));
        // SAFETY: 同上。
        unsafe { std::env::set_var(IDLE_ENV, "  300 ") };
        assert_eq!(idle_timeout(), Duration::from_millis(300));
        // SAFETY: 同上。
        unsafe { std::env::set_var(IDLE_ENV, "not-a-number") };
        assert!(idle_timeout().is_zero());
        match original {
            // SAFETY: 同上。
            Some(v) => unsafe { std::env::set_var(IDLE_ENV, v) },
            // SAFETY: 同上。
            None => unsafe { std::env::remove_var(IDLE_ENV) },
        }
    }

    #[test]
    fn injection_method_is_not_clipboard_based() {
        assert!(!INJECTION_METHOD.contains("clipboard"));
        assert!(!INJECTION_METHOD.contains("剪贴板"));
        assert!(INJECTION_METHOD.contains("SendInput"));
    }

    #[test]
    fn concurrent_second_injection_fails_fast() {
        // 模拟“上一次注入还没结束”：直接占用 INJECTING 标志，第二个请求必须立刻失败
        // 且带上可读原因（不能排队等到安卓 8 s 超时）。
        let _lock = inject_diag::send_lock();
        assert!(INJECTING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let (err, outcome) = inject_text_report("你好").unwrap_err();
        INJECTING.store(false, Ordering::SeqCst);
        assert!(err.contains("上一次输入尚未完成"), "{err}");
        assert_eq!(outcome.units_sent, 0);
        assert_eq!(outcome.units_total, 2);
    }

    #[test]
    fn empty_and_oversized_text_do_not_take_the_injection_slot() {
        // 被拒绝的文本不该占用并发标志，否则后续正常请求会被误判成“上一次还没完成”。
        assert!(inject_text("").is_err());
        assert!(inject_text(&"字".repeat(MAX_CODE_UNITS + 1)).is_err());
        assert!(
            INJECTING
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "并发标志应当已经复位"
        );
        INJECTING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn focus_is_not_stolen_from_another_program() {
        // 自检窗口在后台时，注入必须拒绝且**一个字符都不发**：这是“永不向错误窗口打字”
        // 的底线。用窗口锁保证没有别的自检在抢焦点。
        let _lock = inject_diag::window_lock();
        let Some(win) = self_test::prepare() else {
            return;
        };
        win.clear();
        if self_test::bring_to_front(win.hwnd()) {
            eprintln!("[inject-self-test] 跳过：自检窗口无法保持后台（前台切换权限过高）");
            return;
        }
        inject_diag::clear();
        let (err, outcome) = inject_text_report("不该出现").unwrap_err();
        let _ = inject_diag::take_failure();
        let landed = win.wait_text_stable(Duration::from_millis(300));
        self_test::dump(&outcome, &self_test::describe_now());
        assert!(landed.is_empty(), "后台时不该打进任何字符：{landed:?}");
        assert_eq!(outcome.units_sent, 0);
        assert!(!err.is_empty());
    }

    #[test]
    fn lost_focus_mid_stream_aborts_with_progress() {
        // 目标快照故意与“真实的前台/焦点”不一致（句柄用 1，磁盘上不可能存在）：
        // 这样第 1 批之前的复核必然失败，不需要（也做不到）在测试进程里真的把用户的
        // 窗口切走——那会把用户正当的窗口甩到后台。
        //
        // 本用例全程只走到“复核 -> 中止”，不会真的发按键，因此也不依赖自检窗口。
        let _lock = inject_diag::window_lock();
        // 27 个码元 ≤ SINGLE_CALL_CODE_UNITS(64)，按设计就是**一次** SendInput。
        let text = "蓝牙桥切换窗口测试".repeat(3);
        let units = plan_units(&text);
        let chunks = chunks_of(units.len());
        assert_eq!(chunks, 1, "码元数={} 分批={}", units.len(), chunks);
        let mut outcome = InjectionOutcome {
            units_sent: 0,
            units_total: units.len(),
            chunks,
            relaxed_self_check: true,
        };
        let fake = FocusSnapshot {
            hwnd_foreground: 1,
            thread_id: 1,
            pid: 1,
            hwnd_focus: 1,
            hwnd_root: 1,
            on_input_desktop: true,
        };
        let started = Instant::now();
        let err =
            inject_with_checking_abort(&units, &fake, &mut outcome).expect_err("目标变了必须中止");
        eprintln!("{}", flexi_dump(&outcome, &err));
        assert!(err.message.contains("窗口失去焦点"), "{}", err.message);
        assert_eq!(err.units_sent, 0, "第 1 批之前的复核就应当拦住");
        assert_eq!(outcome.units_sent, 0);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "发现目标不对必须立刻停手，不能继续发"
        );
    }

    #[test]
    fn multi_batch_aborts_before_sending_anything() {
        // 多批（>64 码元）时的中止语义：复核不成立就一批都不发，进度保持 0，
        // 中止原因必须是“焦点没了”，而不是 SendInput 自己的错误码。
        let _lock = inject_diag::window_lock();
        let text = "蓝牙桥切换窗口测试".repeat(9); // 81 个码元 → 11 批
        let units = plan_units(&text);
        let chunks = chunks_of(units.len());
        assert!(chunks > 1, "必须以多批发送：{chunks}");
        let mut outcome = InjectionOutcome {
            units_sent: 0,
            units_total: units.len(),
            chunks,
            relaxed_self_check: true,
        };
        let fake = FocusSnapshot {
            hwnd_foreground: 1,
            thread_id: 1,
            pid: 1,
            hwnd_focus: 1,
            hwnd_root: 1,
            on_input_desktop: true,
        };
        let started = Instant::now();
        let err =
            inject_with_checking_abort(&units, &fake, &mut outcome).expect_err("复核失败必须中止");
        eprintln!("{}", flexi_dump(&outcome, &err));
        assert!(err.message.contains("窗口失去焦点"), "{}", err.message);
        assert_eq!(outcome.units_sent, 0);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    /// 失败时打诊断（只在测试里用；正常回执里不带这些细节）。
    fn flexi_dump(outcome: &InjectionOutcome, err: &InjectionError) -> String {
        format!(
            "[inject-self-test] 中止：{}（已发 {} / 共 {} 码元，{} 批）",
            err.message, err.units_sent, outcome.units_total, outcome.chunks
        )
    }
}

/// 进程内自检脚手架：自建一个**隐藏的顶层窗口 + 多行 `EDIT`**，把焦点放到它上面，
/// 再走真实的 `SendInput` 路径，最后用 `WM_GETTEXT` 把内容读回来比对。
///
/// 为什么需要它：`SendInput` 的成功码只说明“事件进了系统输入队列”，真正“字符到了
/// 输入框”必须由控件自己证明。进程内的 `EDIT` 会像普通输入框一样把 `WM_CHAR` 转成
/// 文本，因此它能端到端验证：换行→回车、emoji 代理对、超长文本分批、焦点丢失中止。
///
/// 注意：`SendInput` 作用在**全局**输入队列上，所以本模块所有会真发按键的测试都
/// 用同一把锁（[`inject_diag::send_lock`]）串行化，避免互相打字。
#[cfg(test)]
mod self_test {
    use std::sync::Once;
    use std::time::{Duration, Instant};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetSysColorBrush, COLOR_WINDOW};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
    use windows::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow, PeekMessageW,
        PostMessageW, SendMessageW, SetForegroundWindow, TranslateMessage, ES_MULTILINE, MSG,
        PM_REMOVE, WINDOW_STYLE, WM_CLOSE, WM_GETTEXT, WM_GETTEXTLENGTH, WM_SETTEXT, WNDCLASSW,
    };

    use super::{inject_diag, InjectionOutcome};
    use crate::service::win::focus;

    /// 自检窗口用到的几个 Win32 入口。
    ///
    /// 为什么不用 `windows` crate 的封装：窗口类的注册/创建在 0.62 的封装里对
    /// `features` 的组合比较挑剔（`CreateWindowExW` 返回 `Result`、`WNDCLASSW.lpfnWndProc`
    /// 是 `Option`、`SetFocus` 收 `Option<HWND>`……），换个 feature 组合就编译不过；
    /// 这里的自检窗口属于测试脚手架，直接按 Windows 原始签名声明，规约最稳。
    mod win {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::WNDCLASSW;

        #[link(name = "user32")]
        unsafe extern "system" {
            pub fn RegisterClassW(class: *const WNDCLASSW) -> u16;
            pub fn CreateWindowExW(
                ex_style: u32,
                class_name: *const u16,
                window_name: *const u16,
                style: u32,
                x: i32,
                y: i32,
                width: i32,
                height: i32,
                parent: HWND,
                menu: *mut core::ffi::c_void,
                instance: isize,
                param: *mut core::ffi::c_void,
            ) -> HWND;
        }
    }

    /// 自检窗口类名（每进程只注册一次）。
    const CLASS_NAME: PCWSTR = w!("BluetoothBridgeInjectionSelfTest");
    /// 自检专用自定义消息：把自检窗口切成前台（在它自己的线程上处理）。
    const WM_SELF_TEST_FOREGROUND: u32 = 0x8000 + 1;
    static REGISTER: Once = Once::new();

    // 原始 Win32 样式常量（`windows` crate 里同名常量来自不同 feature，测试里只用这几个值）。
    const WS_POPUP: u32 = 0x8000_0000;
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;

    /// 取当前进程模块句柄（注册窗口类需要）。
    fn instance_handle() -> Result<isize, String> {
        // SAFETY: 无参查询；GetModuleHandleW(None) 返回本进程模块句柄。
        unsafe { GetModuleHandleW(None) }
            .map(|h| h.0 as isize)
            .map_err(|e| format!("取模块句柄失败：{e}"))
    }

    /// 自检窗口：隐藏的顶层窗口 + 它的多行 `EDIT` 子窗口。
    pub struct TestWindow {
        hwnd: HWND,
        edit: HWND,
    }

    impl TestWindow {
        /// 创建窗口并把焦点放到 `EDIT` 上；失败时返回原因（没有交互桌面、类注册失败等）。
        pub fn create() -> Result<Self, String> {
            register_class()?;
            let instance = instance_handle()?;
            // SAFETY: 类名已注册；句柄全为 0（无父/无菜单）；样式是“无边框顶层窗口”。
            let hwnd = unsafe {
                win::CreateWindowExW(
                    0,
                    CLASS_NAME.as_ptr(),
                    w!("injection self test").as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    320,
                    120,
                    HWND(core::ptr::null_mut()),
                    core::ptr::null_mut(),
                    instance,
                    core::ptr::null_mut(),
                )
            };
            if hwnd.0.is_null() {
                return Err("创建自检窗口失败（没有交互桌面？）".into());
            }
            // SAFETY: 父窗口刚创建成功；EDIT 样式 = 子窗口 + 可见 + 多行。
            let edit = unsafe {
                win::CreateWindowExW(
                    0,
                    w!("EDIT").as_ptr(),
                    core::ptr::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_MULTILINE as u32).0,
                    0,
                    0,
                    320,
                    120,
                    hwnd,
                    core::ptr::null_mut(),
                    instance,
                    core::ptr::null_mut(),
                )
            };
            if edit.0.is_null() {
                // SAFETY: hwnd 是本函数创建的窗口，销毁一次即可。
                let _ = unsafe { DestroyWindow(hwnd) };
                return Err("创建 EDIT 失败（没有交互桌面？）".into());
            }
            Ok(Self { hwnd, edit })
        }

        /// 焦点窗口。
        pub fn edit(&self) -> HWND {
            self.edit
        }

        /// 顶层窗口句柄（前台/焦点切换与“本程序把目标挡在后面”检查用）。
        pub fn hwnd(&self) -> HWND {
            self.hwnd
        }

        /// 把焦点放回 `EDIT`（并抽干队列），返回是否成功。
        pub fn focus_edit(&self) -> bool {
            // SAFETY: 两个句柄都是本进程窗口。
            unsafe {
                let focus = GetFocus();
                if focus != self.edit {
                    let _ = SetFocus(Some(self.edit));
                }
            }
            pump(30);
            // SAFETY: 纯查询。
            unsafe { GetFocus() == self.edit }
        }

        /// 当前 EDIT 内容。
        pub fn text(&self) -> String {
            // SAFETY: edit 是本进程的子窗口句柄，消息只在本进程内处理。
            let len = unsafe { SendMessageW(self.edit, WM_GETTEXTLENGTH, None, None) }.0 as usize;
            let mut buf = vec![0u16; len + 1];
            // SAFETY: 缓冲区按“长度 + 1”个 UTF-16 码元分配，WM_GETTEXT 会写入以 NUL 结尾的字符串。
            let got = unsafe {
                SendMessageW(
                    self.edit,
                    WM_GETTEXT,
                    Some(WPARAM(buf.len())),
                    Some(LPARAM(buf.as_mut_ptr() as isize)),
                )
            }
            .0 as usize;
            let got = got.min(buf.len());
            String::from_utf16_lossy(&buf[..got])
        }

        /// 读回内容（把 `\r\n` 归一成 `\n`，多行 EDIT 会自己插 CRLF）。
        pub fn text_lines(&self) -> String {
            self.text().replace("\r\n", "\n")
        }

        /// 清空内容。
        pub fn clear(&self) {
            let empty: Vec<u16> = vec![0];
            // SAFETY: WM_SETTEXT 只读取我们传入的以 NUL 结尾的栈外缓冲区。
            unsafe {
                SendMessageW(
                    self.edit,
                    WM_SETTEXT,
                    None,
                    Some(LPARAM(empty.as_ptr() as isize)),
                );
            }
            pump(10);
        }

        /// 等到内容长度连续若干轮不再变化（`SendInput` 的字符是系统输入队列里的后续
        /// 消息，读回前必须把队列抽干），返回最终内容。
        pub fn wait_text_stable(&self, timeout: Duration) -> String {
            let deadline = Instant::now() + timeout;
            let mut last_len = usize::MAX;
            let mut stable = 0u32;
            loop {
                pump(5);
                let text = self.text_lines();
                if text.len() == last_len {
                    stable += 1;
                    if stable >= 3 {
                        return text;
                    }
                } else {
                    stable = 0;
                    last_len = text.len();
                }
                if Instant::now() >= deadline {
                    return text;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: hwnd 是自己的顶层窗口，子窗口会一起销毁；进程内 wndproc 已处理完消息。
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }

    /// 把窗口类的消息处理注册进本进程（幂等）。
    fn register_class() -> Result<(), String> {
        let mut error = None;
        REGISTER.call_once(|| {
            // SAFETY: 无参查询。
            let instance = match unsafe { GetModuleHandleW(None) } {
                Ok(h) => h,
                Err(e) => {
                    error = Some(format!("取模块句柄失败：{e}"));
                    return;
                }
            };
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                // 背景刷不影响输入，只用来让窗口不是“黑块”。
                // SAFETY: GetSysColorBrush 返回系统共享画刷，不需要也不可以释放。
                hbrBackground: unsafe { GetSysColorBrush(COLOR_WINDOW) },
                hInstance: instance.into(),
                lpszClassName: CLASS_NAME,
                ..Default::default()
            };
            // SAFETY: class 在调用期间有效；同一进程重复注册时返回 0 也无所谓（类名唯一）。
            let atom = unsafe { win::RegisterClassW(&class) };
            if atom == 0 {
                // 已注册过时 RegisterClassW 会失败（ERROR_CLASS_ALREADY_EXISTS），不算错误。
                let _ = unsafe { windows::Win32::Foundation::GetLastError() };
            }
        });
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            // 测试窗口不接受关闭消息（避免被别的窗口管理逻辑提前销毁）。
            WM_CLOSE => LRESULT(0),
            WM_SELF_TEST_FOREGROUND => {
                // 自检窗口在自己线程上被要求“切成前台”。
                // SAFETY: hwnd 是自己的顶层窗口。
                let _ = unsafe { SetForegroundWindow(hwnd) };
                LRESULT(1)
            }
            // 默认处理：EDIT 自己会把 WM_CHAR 插进文本。
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// 抽干本线程消息队列：`ms == 0` 时抽到空为止，否则最多等 `ms` 毫秒。
    ///
    /// 关键点：`SendInput` 只是把事件塞进**系统**输入队列，真正的 `WM_CHAR` 要等
    /// 本线程的下一次 `PeekMessage` 才会被转换并投递给 `EDIT`。`ms == 0` 时每轮
    /// 让出 200 µs 再查，避免“查一次就走”导致长文本只到达前几个字符。
    pub fn pump(ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut msg = MSG::default();
        loop {
            // SAFETY: msg 是栈上已初始化的结构体；PM_REMOVE 会在取走消息后清空它。
            let has = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
            if has {
                // SAFETY: msg 来自 PeekMessageW，TranslateMessage/DispatchMessageW 只处理这一条。
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if ms > 0 && Instant::now() >= deadline {
                    return;
                }
                continue;
            }
            if ms == 0 {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// 把自检窗口切成前台窗口；返回是否成功。
    ///
    /// 只给“整条 `inject_text` 路径”的自检用：它会真的调用 `resolve_target`，
    /// 那里有“前台不得是本程序”的保护；而 `SendInput` 把按键投给**前台**窗口的线程，
    /// 所以目标窗口必须是前台。CI/远程会话里切不过去时调用方应跳过而不是失败。
    ///
    /// 先看“是不是已经在前台”，只有不在前台时才请求切换：`SetForegroundWindow` 一旦
    /// 真的成功就会把用户正在用的窗口甩到后面去，所以在后台时宁可直接失败让调用方跳过。
    pub fn bring_to_front(hwnd: HWND) -> bool {
        // SAFETY: 纯查询 + 向本进程窗口投递消息。
        unsafe {
            if GetForegroundWindow() != hwnd {
                let _ = PostMessageW(Some(hwnd), WM_SELF_TEST_FOREGROUND, WPARAM(0), LPARAM(0));
                pump(20);
            }
            for _ in 0..100 {
                if GetForegroundWindow() == hwnd {
                    return true;
                }
                let _ = SetForegroundWindow(hwnd);
                pump(10);
            }
            GetForegroundWindow() == hwnd
        }
    }

    /// 打一行诊断（失败时用，不进回执）。
    pub fn dump(outcome: &InjectionOutcome, extra: &str) {
        let send = inject_diag::take_send();
        eprintln!(
            "[inject-self-test] {}；SendInput(返回/期望/GetLastError)={send:?}{extra}",
            outcome.summary()
        );
    }

    /// 一行“当前焦点/前台”描述，失败诊断用。
    pub fn describe_now() -> String {
        match focus::snapshot_foreground() {
            Some(snap) => format!(
                "前台={:#x} 焦点={:#x} pid={} 类名={:?}",
                snap.hwnd_foreground,
                snap.hwnd_focus,
                snap.pid,
                focus::window_class(snap.hwnd_foreground)
            ),
            None => "前台=无".to_string(),
        }
    }

    /// 建一个自检窗口并把焦点放到 `EDIT` 上；环境不支持时返回跳过原因。
    ///
    /// 调用方拿到 `None` 后打印原因并**返回成功**：无交互桌面不是代码错误。
    pub fn prepare() -> Option<TestWindow> {
        let win = match TestWindow::create() {
            Ok(w) => w,
            Err(message) => {
                eprintln!("[inject-self-test] 跳过：{message}");
                return None;
            }
        };
        if !win.focus_edit() {
            eprintln!(
                "[inject-self-test] 跳过：焦点无法回到自检 EDIT（{}）",
                describe_now()
            );
            return None;
        }
        Some(win)
    }

    #[test]
    fn self_test_window_can_be_created_and_focused() {
        // 环境不支持（无桌面会话）时跳过，而不是失败。
        let _lock = inject_diag::window_lock();
        let Some(win) = prepare() else {
            return;
        };
        win.clear();
        assert_eq!(win.text_lines(), "");
        // SAFETY: 本进程窗口句柄。
        assert!(!win_hwnd_is_null(win.edit()));
    }

    fn win_hwnd_is_null(hwnd: HWND) -> bool {
        hwnd.0.is_null()
    }
}
