//! 前台窗口 / 焦点控件 / 权限 / 桌面 状态查询（Windows 注入链路的“能不能打字”判定）。
//!
//! 设计目标：所有查询都必须**便宜且不阻塞**（只在 user32/kernel32 打转，不做任何
//! 跨进程 COM/UIA 调用），因为 (a) 注入过程中每发一块文本都要复核一次焦点，
//! (b) 状态查询会被控制通道频繁调用。
//!
//! ## 为什么不用 UI Automation 判断“是否可编辑”
//!
//! PROTOCOL.md 建议“可结合 UI Automation，无法确认时返回失败回执”。但 UIA 跨进程
//! 调用在“焦点刚切换”“目标进程繁忙”时经常阻塞数十到数百毫秒，而安卓端要求 8 秒内
//! 回执、注入过程中还要每块复核焦点；把 UIA 放进这条链路只会让“回执超时”变成新的
//! 失败模式。因此取舍如下：
//!
//! - **焦点是否存在**：`GetForegroundWindow` + `GetGUIThreadInfo(hwndFocus)`。
//!   普通输入框（EDIT/RichEdit/浏览器/Electron/Office/终端）被点击后一定有
//!   hwndFocus；hwndFocus 为空说明“顶级窗口自己持有焦点”，保守拒绝——宁可让用户
//!   点一下输入框，也不要往不可预知的地点打字。
//! - **是否可写**：不做 UIA 探测，由 `SendInput` 的返回值 + 目标窗口的实际反应兜底。
//!   已知唯一会误判的是“只读输入框”：文本被目标丢弃，我们仍回 ok。这类情况不会
//!   破坏安卓端草稿（用户可见内容没变，可手动重发），属于可接受的误报。
//!
//! ## 安全约束（PROTOCOL.md “注入规则”）
//!
//! 1. 绝不把文字打进自己（本进程窗口）——[`FocusError::SelfTarget`]。
//! 2. 不为了绕过完整性级别而提权——只检测、只拒绝（[`FocusError::ElevatedTarget`]）。
//! 3. 锁屏/UAC 的安全桌面上没有可用前台窗口——[`FocusError::SecureDesktop`]。
//! 4. 前台窗口必须属于目标程序本身（`SendInput` 把按键投给**前台**窗口的线程，
//!    前台若是本程序，按键会打进本程序而不是用户以为的那个输入框）——
//!    [`foreground_is_shared_with_us`]，同样映射成 [`FocusError::SelfTarget`]。
//!
//! ## 线程模型
//!
//! 本模块所有函数都是**无状态**的纯查询，可在任意线程调用；`GetAsyncKeyState`
//! 要求调用线程有消息队列（任何线程 Windows 都会自动补建，实测 RFCOMM 会话线程
//! 行为一致）。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::StationsAndDesktops::{
    GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, UOI_NAME,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, LASTINPUTINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindow, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, GA_ROOT, GUITHREADINFO, GW_HWNDNEXT,
};

/// 注入链路上各类“不能继续”的原因；最终映射成安卓回执里的中文 `error`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusError {
    /// 没有前台窗口，或前台窗口没有任何焦点子控件。
    NoTarget,
    /// 前台窗口属于本进程（绝不能往自己窗口里打字）。
    SelfTarget,
    /// 前台进程已提权而本程序未提权，SendInput 会被 UIPI 静默丢弃。
    ElevatedTarget,
    /// 处于锁屏/UAC 等安全桌面，输入到不了用户界面。
    SecureDesktop,
    /// 前台窗口已最小化。
    Minimized,
    /// 有 Shift/Ctrl/Alt/Win 被按住，注入会被组合成快捷键。
    ModifiersHeld,
}

impl FocusError {
    /// 回执 `error` 字段的最终文案（中文，安卓端原样展示给用户）。
    ///
    /// 文案要能直接指导用户下一步动作：点输入框 / 切换窗口 / 松开按键 /
    /// 不要提权 / 先解锁。
    pub fn message(&self) -> &'static str {
        match self {
            FocusError::NoTarget => "没有可输入的窗口（请先点击要输入的输入框）",
            FocusError::SelfTarget => {
                "当前窗口属于本程序（或本程序正挡在它前面），请把要输入的窗口切到最前面"
            }
            FocusError::ElevatedTarget => {
                "目标窗口权限较高，请以普通权限运行本程序（不要提权）或切换到普通窗口"
            }
            FocusError::SecureDesktop => "系统正在锁定或显示安全桌面，解锁后重试",
            FocusError::Minimized => "目标窗口已最小化，请恢复窗口后重试",
            FocusError::ModifiersHeld => "有按键（Shift/Ctrl/Alt/Win）被按住，请松开后重试",
        }
    }
}

/// 一次焦点快照。`PartialEq` 用于判断“前后是否还是同一个输入目标”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusSnapshot {
    /// 前台窗口（`GetForegroundWindow`）。
    pub hwnd_foreground: isize,
    /// 前台窗口所属线程。
    pub thread_id: u32,
    /// 前台窗口所属进程。
    pub pid: u32,
    /// 该线程当前的焦点窗口（`GetGUIThreadInfo.hwndFocus`），0 表示无。
    pub hwnd_focus: isize,
    /// 焦点控件的顶级窗口（`GetAncestor(hwndFocus, GA_ROOT)`）。
    pub hwnd_root: isize,
    /// 当前线程桌面是 “Default”（可交互）而非锁屏安全桌面。
    pub on_input_desktop: bool,
}

impl FocusSnapshot {
    /// 是否与另一份快照指向同一个输入目标。
    ///
    /// 只比较“前台窗口 + 焦点控件”：同一窗口内用户点了别的输入框（hwndFocus
    /// 变化）也算不同目标并中止——半句话打进另一个输入框同样是事故。
    pub fn same_target(&self, other: &FocusSnapshot) -> bool {
        self.hwnd_foreground == other.hwnd_foreground && self.hwnd_focus == other.hwnd_focus
    }

    /// 焦点控件是否非空（Windows 空 HWND 用 0 表示）。
    pub fn has_focus(&self) -> bool {
        self.hwnd_focus != 0
    }
}

fn hwnd_to_isize(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn isize_to_hwnd(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

/// 当前线程所在桌面是否是可交互的 “Default”（非锁屏、非 UAC 安全桌面）。
///
/// 锁屏时整个用户界面被切到 `Winlogon` 桌面，用户程序线程也随之切过去，
/// 所以“能不能 OpenInputDesktop”区分不出来；必须读**线程桌面名**。
fn thread_desktop_is_default() -> bool {
    // SAFETY: GetThreadDesktop 返回当前线程桌面句柄（不可 CloseDesktop）。
    let hdesk = match unsafe { GetThreadDesktop(GetCurrentThreadId()) } {
        Ok(h) if !h.is_invalid() && !h.0.is_null() => h,
        // 查不到时按“可交互”处理，让后续 GetForegroundWindow 的判断兜底。
        _ => return true,
    };
    let mut buf = [0u8; 512];
    let mut needed = 0u32;
    // SAFETY: 栈上定长缓冲区 + 指向栈上 u32 的长度指针，仅本次调用有效。
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            buf.len() as u32,
            Some(&mut needed as *mut u32),
        )
    }
    .is_ok();
    if !ok || needed < 2 {
        return true;
    }
    // UOI_NAME 返回以 NUL 结尾的 UTF-16 名字（不是 UNICODE_STRING）。
    let mut units = Vec::with_capacity(needed as usize / 2);
    let mut index = 0;
    while index + 1 < buf.len() {
        let unit = u16::from_le_bytes([buf[index], buf[index + 1]]);
        index += 2;
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let name = String::from_utf16_lossy(&units);
    name.is_empty() || name.eq_ignore_ascii_case("Default")
}

/// 额外探测：能否打开“输入桌面”（锁屏安全桌面通常打不开）。
fn can_open_input_desktop() -> bool {
    // SAFETY: OpenInputDesktop 无指针参数；成功后句柄需要 CloseDesktop。
    let hdesk = unsafe {
        OpenInputDesktop(
            windows::Win32::System::StationsAndDesktops::DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0),
        )
    };
    match hdesk {
        Ok(h) => {
            // SAFETY: h 是刚返回的有效句柄，关闭一次即可。
            let _ = unsafe { windows::Win32::System::StationsAndDesktops::CloseDesktop(h) };
            true
        }
        Err(_) => false,
    }
}

/// 读取当前前台窗口与焦点控件，不做策略判断（纯查询，不阻塞）。
pub fn snapshot_foreground() -> Option<FocusSnapshot> {
    // SAFETY: GetForegroundWindow 无参数无指针；无前台窗口时返回 NULL。
    let hwnd_fg = unsafe { GetForegroundWindow() };
    if hwnd_fg.0.is_null() {
        return None;
    }

    // SAFETY: 第二个参数是指向栈上 u32 的可写指针，生命周期覆盖调用。
    let (thread_id, pid) = unsafe {
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(hwnd_fg, Some(&mut pid as *mut u32));
        (tid, pid)
    };
    if thread_id == 0 {
        return None;
    }

    let mut info = GUITHREADINFO {
        cbSize: core::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已按要求填写，指针指向栈上已初始化结构体，仅本次调用有效。
    let got = unsafe { GetGUIThreadInfo(thread_id, &mut info as *mut GUITHREADINFO) }.is_ok();

    let hwnd_focus = if got { info.hwndFocus } else { HWND::default() };
    let hwnd_root = if hwnd_focus.0.is_null() {
        hwnd_fg
    } else {
        // SAFETY: 纯查询，hwndFocus 已排除 NULL 且由系统返回。
        unsafe { GetAncestor(hwnd_focus, GA_ROOT) }
    };

    Some(FocusSnapshot {
        hwnd_foreground: hwnd_to_isize(hwnd_fg),
        thread_id,
        pid,
        hwnd_focus: hwnd_to_isize(hwnd_focus),
        hwnd_root: hwnd_to_isize(hwnd_root),
        on_input_desktop: thread_desktop_is_default(),
    })
}

/// 某个窗口的类名（失败时返回空串）。
pub fn window_class(hwnd: isize) -> String {
    if hwnd == 0 {
        return String::new();
    }
    let mut class = [0u16; 128];
    // SAFETY: 只把栈上定长缓冲区交给系统写入，长度以切片长度为准。
    let n = unsafe { GetClassNameW(isize_to_hwnd(hwnd), &mut class) } as usize;
    String::from_utf16_lossy(&class[..n.min(class.len())])
}

/// 某个窗口的标题（失败时返回空串）。
pub fn window_title(hwnd: isize) -> String {
    if hwnd == 0 {
        return String::new();
    }
    let mut title = [0u16; 512];
    // SAFETY: 同上，栈上定长缓冲区。
    let n = unsafe { GetWindowTextW(isize_to_hwnd(hwnd), &mut title) } as usize;
    String::from_utf16_lossy(&title[..n.min(title.len())])
}

/// 一次快照的可读描述，供日志使用。
pub fn describe_window(snap: &FocusSnapshot) -> String {
    let class = window_class(snap.hwnd_focus);
    let title = window_title(snap.hwnd_root);
    format!(
        "hwnd={:#x} class={class:?} top={title:?} pid={} root={:#x}",
        snap.hwnd_focus, snap.pid, snap.hwnd_root
    )
}

/// 锁屏 / UAC / 其它安全桌面的前台窗口类名。
///
/// `Windows.UI.Core.CoreWindow` 是系统 shell 表面的窗口类：锁屏界面、UAC 之外的
/// 安全界面都用它。这类窗口**会**暴露一个看起来正常的 `hwndFocus`，所以“有焦点”
/// 不足以说明可以输入。
///
/// 代价：用户若真的想让文字进入 Windows shell 表面（开始菜单搜索框等），也会
/// 被拒绝。对本产品而言“丢失文字却回执 ok”比“误拒”更糟，因此接受此代价。
pub const SECURE_SURFACE_CLASS: &str = "Windows.UI.Core.CoreWindow";

/// 前台窗口是否属于锁屏/安全桌面表面。
pub fn is_secure_surface(snap: &FocusSnapshot) -> bool {
    window_class(snap.hwnd_foreground).eq_ignore_ascii_case(SECURE_SURFACE_CLASS)
}

/// 按字符数截断并加省略号。
fn compact(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 状态预览用的一句话：`记事本 / Edit`。
pub fn describe_target(snap: &FocusSnapshot) -> String {
    if is_secure_surface(snap) && snap.pid != current_pid() {
        return "锁屏/安全桌面（无法输入）".into();
    }
    let title = {
        let raw = window_title(snap.hwnd_root);
        if raw.is_empty() {
            "(无标题)".to_string()
        } else {
            compact(&raw, 40)
        }
    };
    let class = window_class(snap.hwnd_focus);
    if class.is_empty() {
        title
    } else {
        format!("{title} / {class}")
    }
}

/// 目标进程是否已提权（`TokenElevation`）。查不到返回 None。
pub fn process_elevated(pid: u32) -> Option<bool> {
    // SAFETY: 打开的进程句柄在函数内必然关闭一次；OpenProcess 失败安全返回 None。
    unsafe {
        let is_self = pid == GetCurrentProcessId();
        let process = if is_self {
            GetCurrentProcess()
        } else {
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) if !h.is_invalid() => h,
                _ => return None,
            }
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token).is_ok();
        if !is_self {
            let _ = CloseHandle(process);
        }
        if !opened {
            return None;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            core::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len as *mut u32,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !ok || ret_len as usize != core::mem::size_of::<TOKEN_ELEVATION>() {
            return None;
        }
        Some(elevation.TokenIsElevated != 0)
    }
}

/// 目标进程是否比我们“权限更高”，此时 SendInput 会被 UIPI 静默丢弃。
///
/// 保守判据：目标已提权而我们未提权 → 阻止。查不到目标权限时放行，交给
/// SendInput 的真实返回值判定，避免因查询失败误拒正常窗口。
pub fn elevation_blocked(target_pid: u32) -> bool {
    if target_pid == current_pid() {
        return false;
    }
    matches!(
        (
            process_elevated(target_pid),
            process_elevated(current_pid())
        ),
        (Some(true), Some(false))
    )
}

/// 当前进程 ID。
pub fn current_pid() -> u32 {
    // SAFETY: 无参数无指针，永远成功。
    unsafe { GetCurrentProcessId() }
}

/// 目标窗口是否已最小化（图标化）——仍是前台，但注入会失败。
pub fn is_iconic(snap: &FocusSnapshot) -> bool {
    if snap.hwnd_root == 0 {
        return false;
    }
    // SAFETY: 纯查询，句柄来自同一次快照。
    unsafe { IsIconic(isize_to_hwnd(snap.hwnd_root)).as_bool() }
}

/// 参与“防止误触快捷键”判断的修饰键（VK 码）：
/// VK_SHIFT / VK_CONTROL / VK_MENU(Alt) / VK_LWIN / VK_RWIN。
pub const MODIFIER_KEYS: [i32; 5] = [0x10, 0x11, 0x12, 0x5B, 0x5C];

/// 任一修饰键当前被物理按下则返回 true。
///
/// CapsLock/NumLock 是锁定状态而非“按住”，不参与判断。
pub fn modifiers_held() -> bool {
    MODIFIER_KEYS.iter().copied().any(key_down)
}

/// 某个虚拟键是否处于按下状态（`GetAsyncKeyState` 最高位）。
pub fn key_down(vk: i32) -> bool {
    // SAFETY: 纯查询，参数是任意 i32 VK 码。
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// 等待修饰键全部松开，最多等 `limit`。返回是否已全部松开。
pub fn wait_modifiers_released(limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !modifiers_held() {
            return true;
        }
        if Instant::now() >= deadline {
            return !modifiers_held();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 系统空闲时长（自最后一次键盘/鼠标输入起，毫秒）。
///
/// 查询失败返回 `u64::MAX`（视为“已空闲很久”），不会因为查不到而阻塞注入。
pub fn system_idle_ms() -> u64 {
    let mut lii = LASTINPUTINFO {
        cbSize: core::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: lii 是栈上已填 cbSize 的结构体指针，仅本次调用有效。
    if !unsafe { GetLastInputInfo(&mut lii) }.as_bool() {
        return u64::MAX;
    }
    // SAFETY: 纯查询。
    let now = unsafe { GetTickCount() };
    // GetTickCount 约 49.7 天回绕一次，用 u32 回绕减法天然正确。
    now.wrapping_sub(lii.dwTime) as u64
}

/// 目标是“本进程自己的窗口”吗（绝不能往自己窗口里打字）。
#[inline(never)]
pub fn window_belongs_to_us(snap: &FocusSnapshot) -> bool {
    snap.pid == current_pid()
}

/// 目标进程里是否还有别的（非本程序的）顶级窗口。
///
/// 用途只有一个：防止“本程序的前台窗口同时又恰好是前台窗口”把注入转进自己。
/// 枚举方式：`GetWindow(hwnd, GW_HWNDNEXT)` 走 Z 序顶层窗口链（返回 `Err` 表示链尾）。
fn target_has_other_windows(snap: &FocusSnapshot) -> bool {
    // SAFETY: 纯查询；`hwnd` 来自前台窗口快照，`GetWindow` 对已失效句柄返回 Err。
    let mut cur = unsafe { GetWindow(HWND(snap.hwnd_foreground as *mut _), GW_HWNDNEXT) }.ok();
    let mut seen = 0usize;
    while let Some(h) = cur {
        seen += 1;
        if seen > 512 {
            // 链异常（理论上不会）时保守认为“有别窗口”，不要因此拒掉正常输入。
            return true;
        }
        let mut owner = 0u32;
        // SAFETY: `h` 是 `GetWindow` 返回的有效顶层窗口句柄。
        unsafe { GetWindowThreadProcessId(h, Some(&mut owner)) };
        if owner != snap.pid {
            return true;
        }
        // SAFETY: 沿 Z 序取下一个窗口；到链尾时返回 Err。
        cur = unsafe { GetWindow(h, GW_HWNDNEXT) }.ok();
    }
    false
}

/// 目标是否就是“本程序自己的前台窗口”（注意：与 [`window_belongs_to_us`] 不同，
/// 这一条即使 pid 不同也要拦）。
///
/// 为什么需要：`SendInput` 把按键塞进**前台**窗口的输入队列，再投给该线程的焦点
/// 窗口。所以只要前台是本程序的窗口，焦点再在别的进程里也白搭——文字只会落在本
/// 程序自己身上。
pub fn foreground_is_shared_with_us(snap: &FocusSnapshot) -> bool {
    window_belongs_to_us(snap) || !target_has_other_windows(snap)
}

/// 是否跳过“前台不得与自检窗口共用”的检查（仅供进程内测试/示例）。
pub fn shared_foreground_check_disabled() -> bool {
    std::env::var_os("BLUETOOTH_BRIDGE_ALLOW_SHARED_FOREGROUND").is_some()
}

/// 解析目标窗口并执行全部安全检查（不含“真正开始输入”那一步）。
pub fn resolve_target() -> Result<FocusSnapshot, FocusError> {
    let snap = snapshot_foreground().ok_or(FocusError::NoTarget)?;

    // 自检开关在前：进程内的自检窗口天然与自检程序同进程，其余安全检查（锁屏、
    // 最小化、提权、无焦点、按住修饰键）照常执行。
    // 注意 `owned` 必须比窗口身份判断更严：目标 pid 是本进程 **且** 焦点/前台
    // 确实在本进程的窗口上才算“自己的窗口”。只看 pid 会把“本程序只是个前台
    // 垫层、焦点在别的进程”的常规场景误判成 SelfTarget。
    if window_belongs_to_us(&snap) && !shared_foreground_check_disabled() {
        return Err(FocusError::SelfTarget);
    }
    let owned = window_belongs_to_us(&snap);
    // 锁屏/UAC 安全桌面：先看线程桌面，再看前台窗口类名（锁屏会暴露一个带焦点
    // 的 CoreWindow，光看“有没有焦点”是拦不住的）。
    if is_secure_surface(&snap) && !owned {
        return Err(FocusError::SecureDesktop);
    }
    if !owned && !snap.on_input_desktop && !can_open_input_desktop() {
        return Err(FocusError::SecureDesktop);
    }
    if is_iconic(&snap) {
        return Err(FocusError::Minimized);
    }
    if elevation_blocked(snap.pid) {
        return Err(FocusError::ElevatedTarget);
    }
    if foreground_is_shared_with_us(&snap) && !shared_foreground_check_disabled() {
        return Err(FocusError::SelfTarget);
    }
    if !snap.has_focus() {
        return Err(FocusError::NoTarget);
    }
    if modifiers_held() {
        return Err(FocusError::ModifiersHeld);
    }
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(hwnd_fg: isize, hwnd_focus: isize) -> FocusSnapshot {
        FocusSnapshot {
            hwnd_foreground: hwnd_fg,
            thread_id: 1,
            pid: 4242,
            hwnd_focus,
            hwnd_root: hwnd_fg,
            on_input_desktop: true,
        }
    }

    #[test]
    fn same_target_compares_foreground_and_focus() {
        assert!(snap(0x100, 0x101).same_target(&snap(0x100, 0x101)));
        assert!(!snap(0x100, 0x101).same_target(&snap(0x100, 0x102)));
        assert!(!snap(0x100, 0x101).same_target(&snap(0x200, 0x101)));
    }

    #[test]
    fn has_focus_treats_zero_as_none() {
        assert!(!snap(0x100, 0).has_focus());
        assert!(snap(0x100, 0x101).has_focus());
    }

    #[test]
    fn modifier_keys_match_expected_vks() {
        assert_eq!(MODIFIER_KEYS, [0x10, 0x11, 0x12, 0x5B, 0x5C]);
    }

    #[test]
    fn every_error_has_actionable_chinese_message() {
        for error in [
            FocusError::NoTarget,
            FocusError::SelfTarget,
            FocusError::ElevatedTarget,
            FocusError::SecureDesktop,
            FocusError::Minimized,
            FocusError::ModifiersHeld,
        ] {
            let text = error.message();
            assert!(!text.is_empty());
            assert!(
                text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "{text} 应当是中文"
            );
        }
    }

    #[test]
    fn no_target_message_is_the_protocol_example() {
        assert_eq!(
            FocusError::NoTarget.message(),
            "没有可输入的窗口（请先点击要输入的输入框）"
        );
    }

    #[test]
    fn secure_surface_class_is_case_insensitive_constant() {
        assert_eq!(SECURE_SURFACE_CLASS, "Windows.UI.Core.CoreWindow");
    }

    #[test]
    fn describe_target_tolerates_null_hwnd() {
        let s = snap(0, 0);
        assert!(!describe_target(&s).is_empty());
        assert_eq!(window_class(0), "");
        assert_eq!(window_title(0), "");
    }

    #[test]
    fn elevation_of_self_is_not_blocking() {
        assert!(!elevation_blocked(current_pid()));
        assert_eq!(process_elevated(current_pid()), Some(false));
    }

    #[test]
    fn modifiers_query_does_not_panic() {
        let _ = modifiers_held();
        let _ = system_idle_ms();
    }
}
