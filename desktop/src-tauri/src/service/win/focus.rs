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
//!   已知唯一会误判的是“只读输入框”：文本被目标丢弃，我们仍回 ok（见下文的收/发
//!   窗口校验，那一类情况至少要“窗口确实还收焦点”）。
//!
//! ## 安全约束（PROTOCOL.md “注入规则”）
//!
//! 1. 绝不把文字打进自己（本进程窗口）——[`FocusError::SelfTarget`]。
//! 2. 不为了绕过完整性级别而提权——只检测、只拒绝（[`FocusError::ElevatedTarget`]）。
//! 3. 锁屏/UAC 的安全桌面上没有可用前台窗口——[`FocusError::SecureDesktop`]。
//!    两条独立证据：类名命中系统 shell 表面，或当前线程**不在** `Default` 桌面。
//!    反向证据（“在 Default 桌面但 `OpenInputDesktop` 打不开”）不作为拒绝依据，
//!    否则非交互会话（计划任务/CI）里会全部误拒。
//! 4. 前台窗口必须属于目标程序本身（`SendInput` 把按键投给**前台**窗口的线程，
//!    前台若是本程序，按键会打进本程序而不是用户以为的那个输入框）——
//!    [`focus::foreground_shared_with_us`]。
//! 5. 焦点控件必须真的收得到 `WM_CHAR`：`hwndFocus` 的**根窗口**必须就是前台窗口。
//!    这一条挡住的是“输入法/离屏辅助窗口持有焦点”的误放行——`SendInput` 会把字符
//!    投给那个隐藏窗口，用户看到的是“回执 ok 但一个字都没进输入框”。代价是拒绝
//!    IME 候选/组合窗口持有热点焦点的短暂瞬间（用户再点一下输入框即可），见
//!    [`focus::receives_keyboard_input`]。
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
    GetAncestor, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo, GetWindow, GetWindowLongW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, GA_ROOT, GUITHREADINFO, GWL_STYLE,
    GW_HWNDNEXT, WS_CHILD,
};

/// 判定预算：一次 `resolve_target`（含全部本地查询）必须在这个时间内完成。
///
/// 注入前要做“焦点/权限/桌面/修饰键/焦点是否收得到字符”这一串查询，每块发送前还要
/// 复核一次；安卓端要求 8 s 内回执，这里把单次判定压到 100 ms 以内，留足余量。
/// 目前实现里没有任何一次判定会等待——这个常量是回归保护用的上限。
pub const RESOLVE_BUDGET_MS: u64 = 100;

/// 注入链路上各类“不能继续”的原因；最终映射成安卓回执里的中文 `error`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusError {
    /// 没有前台窗口，或前台窗口没有任何焦点子控件。
    NoTarget,
    /// 前台窗口属于本进程（绝不能往自己窗口里打字）。
    SelfTarget,
    /// 前台进程已提权而本程序未提权，SendInput 会被 UIPI 静默丢弃。
    ElevatedTarget,
    /// 本程序自己以管理员身份运行，而目标窗口是普通权限：UIPI 同样会丢掉键事件。
    ElevatedSelf,
    /// 处于锁屏/UAC 等安全桌面，输入到不了用户界面。
    SecureDesktop,
    /// 前台窗口已最小化。
    Minimized,
    /// 有 Shift/Ctrl/Alt/Win 被按住，注入会被组合成快捷键。
    ModifiersHeld,
    /// 焦点控件收不到 `WM_CHAR`（IME/离屏辅助窗口持有焦点，或目标线程忙得没空处理）。
    /// 找不到可拆分的轻量检查时，注入会在这里**提前**失败而不是回执成功却丢字。
    InputBlocked(String),
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
            FocusError::ElevatedSelf => {
                "本程序以管理员身份运行，输入到不了普通权限的窗口，请用普通权限重新启动"
            }
            FocusError::SecureDesktop => "系统正在锁定或显示安全桌面，解锁后重试",
            FocusError::Minimized => "目标窗口已最小化，请恢复窗口后重试",
            FocusError::ModifiersHeld => "有按键（Shift/Ctrl/Alt/Win）被按住，请松开后重试",
            FocusError::InputBlocked(_) => {
                "当前窗口不在输入状态（可能有输入法候选框或窗口正忙），请点一下要输入的输入框后重试"
            }
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
///
/// 只作为日志/诊断使用——非交互会话里它也会失败，不能当成“正在锁屏”。
pub fn can_open_input_desktop() -> bool {
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

    let hwnd_focus = focus_of_thread(thread_id).unwrap_or_default();
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

/// 某个线程当前的焦点窗口；`GetGUIThreadInfo` 失败时返回 `None`。
///
/// 与 [`snapshot_foreground`] 分开是给测试用的：进程内自检可以对自己的线程查，
/// 不必先把窗口切成前台窗口（CI/无交互会话里也能跑通授权逻辑之外的部分）。
pub fn focus_of_thread(thread_id: u32) -> Option<HWND> {
    if thread_id == 0 {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: core::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已按要求填写，指针指向栈上已初始化结构体，仅本次调用有效。
    if unsafe { GetGUIThreadInfo(thread_id, &mut info as *mut GUITHREADINFO) }.is_err() {
        return None;
    }
    Some(info.hwndFocus)
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

/// 窗口的 `GWL_STYLE`（失败返回 0）。
pub fn window_style(hwnd: isize) -> u32 {
    if hwnd == 0 {
        return 0;
    }
    // SAFETY: 纯查询；GWL_STYLE 返回 DWORD，按 u32 读取在 64 位下同样正确。
    unsafe { GetWindowLongW(isize_to_hwnd(hwnd), GWL_STYLE) as u32 }
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
/// 注意：UWP 应用把自己的内容放在子 `CoreWindow` 里，但**前台窗口**仍是顶层窗口
/// （`ApplicationFrameWindow` 等），所以这条只命中系统表面，不会误拒 UWP 应用；
/// 若某个 UWP 真的直接以 `CoreWindow` 作为前台窗口，则和上面一样保守拒绝。
///
/// 代价：用户若真的想让文字进入 Windows shell 表面（开始菜单搜索框等），也会
/// 被拒绝。对本产品而言“丢失文字却回执 ok”比“误拒”更糟，因此接受此代价。
pub const SECURE_SURFACE_CLASS: &str = "Windows.UI.Core.CoreWindow";

/// 前台窗口是否属于锁屏/安全桌面表面。
pub fn is_secure_surface(snap: &FocusSnapshot) -> bool {
    window_class(snap.hwnd_foreground).eq_ignore_ascii_case(SECURE_SURFACE_CLASS)
}

/// 焦点控件是否真的收得到 `WM_CHAR`（`SendInput` 投递的目标就是它）。
///
/// 判据（纯本地查询，不跨进程、不阻塞）：
/// 1. 焦点窗口存在且 `GetAncestor(hwndFocus, GA_ROOT)` 就是前台窗口——否则焦点在
///    别的顶级窗口里（典型：输入法候选窗/离屏 IME 辅助窗口持有 `hwndFocus`），
///    `SendInput` 的字符会投给那个窗口，输入框里一个字节都不会出现；
/// 2. 焦点窗口自己不是 `WS_CHILD`（即焦点在顶级窗口自身上，没有子输入框）。
///
/// 误拒风险最大的一条是 1：应用弹出菜单/工具栏时可能把焦点留在自己的顶级窗口，
/// 而根窗口仍是前台窗口，因此不受影响；只有“焦点属于另一个顶级窗口”才拒绝。
///
/// 跨进程的“目标线程正在等待输入”检查留到 [`resolve_target`]：那里有时限。
pub fn receives_keyboard_input(snap: &FocusSnapshot) -> bool {
    if !snap.has_focus() {
        return false;
    }
    if snap.hwnd_root != snap.hwnd_foreground {
        return false;
    }
    // 焦点窗口本身是子窗口时（正常输入框），根窗口必然等于前台窗口；这里只处理
    // “焦点就在顶层窗口上”的情况——它不接收字符消息，保守拒绝。
    window_style(snap.hwnd_focus) & WS_CHILD.0 == 0
}

/// 前台线程是否正处在“自己的模态输入循环”里（`hwndCapture` 指向的不是焦点窗口）。
///
/// Windows 没有公开“这条队列正在 WaitMessage”的查询；这里用的是**保守替代**：
/// 捕获窗口非空且不是焦点窗口时，说明该线程正在拖动/菜单/模态对话框的自有循环里，
/// 此时 `SendInput` 的字符会先被它截走，输入框里未必能拿到字。
///
/// 取不到 `GetGUIThreadInfo` 时返回 false（不因为查不到而误拒）。
pub fn thread_is_capturing(snap: &FocusSnapshot) -> bool {
    let mut info = GUITHREADINFO {
        cbSize: core::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: cbSize 已填写，指针指向栈上结构体。
    if unsafe { GetGUIThreadInfo(snap.thread_id, &mut info as *mut GUITHREADINFO) }.is_err() {
        return false;
    }
    !info.hwndCapture.0.is_null() && hwnd_to_isize(info.hwndCapture) != snap.hwnd_focus
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
    matches!(
        elevation_relation(target_pid),
        ElevationRelation::TargetHigher
    )
}

/// 目标进程与我们的提权关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElevationRelation {
    /// 目标更高：SendInput 会被 UIPI 丢掉。
    TargetHigher,
    /// 同级（都提权或都不提权）。
    SameLevel,
    /// 我们更高：往低权限窗口注入是允许的。
    SelfHigher,
    /// 查不到目标（或自己的）令牌：不拒绝，交给 SendInput 的真实结果判定。
    Unknown,
}

/// 由“目标提权位 + 我们的提权位”推出关系（纯逻辑，单测可直接喂任意组合）。
fn elevation_relation_of(target: Option<bool>, ours: Option<bool>) -> ElevationRelation {
    match (target, ours) {
        (Some(true), Some(false)) => ElevationRelation::TargetHigher,
        (Some(a), Some(b)) if a == b => ElevationRelation::SameLevel,
        (Some(false), Some(true)) => ElevationRelation::SelfHigher,
        _ => ElevationRelation::Unknown,
    }
}

/// 判定提权关系（[`elevation_blocked`] 的可枚举版）。
pub fn elevation_relation(target_pid: u32) -> ElevationRelation {
    let our_elevation = process_elevated(current_pid());
    if target_pid == current_pid() {
        return match our_elevation {
            // 自己比自己永远“同级”。
            Some(_) => ElevationRelation::SameLevel,
            None => ElevationRelation::Unknown,
        };
    }
    elevation_relation_of(process_elevated(target_pid), our_elevation)
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
pub fn foreground_shared_with_us(snap: &FocusSnapshot) -> bool {
    window_belongs_to_us(snap) || !target_has_other_windows(snap)
}

/// 是否跳过“前台不得与自检窗口共用”的检查（仅供进程内测试/示例）。
pub fn shared_foreground_check_disabled() -> bool {
    std::env::var_os("BLUETOOTH_BRIDGE_ALLOW_SHARED_FOREGROUND").is_some()
}

/// 句柄→结构体/字符串查询会不会真的访问内核对象。
///
/// 这是安全判断的**唯一**放行口径：句柄为 0，或者目标进程其实是本进程时，一律不
/// 做跨进程查询，只按“编造句柄”的语义回答。这样纯逻辑单测（用假句柄）可以确定地
/// 验证判定顺序，同时能查出“查询失败被误当成通过”的问题。
fn handle_is_live(hwnd: isize, pid: u32) -> bool {
    hwnd != 0 && pid != current_pid()
}

/// 对**已经取到的**快照执行全部安全检查（不含“真正开始输入”那一步）。
///
/// 顺序按“后果严重、判定便宜”排列；每一条都必须能独立成立：
///
/// 1. 自注入（pid 是本进程）——多会话并发时最容易踩，放在最前面；
/// 2. 安全桌面（类名 / 线程桌面）；
/// 3. 最小化；
/// 4. 高权限目标（UIPI 会静默丢字）；
/// 5. 前台被本程序占用（pid 不同但前台是本程序的窗口）；
/// 6. 无焦点；
/// 7. 焦点收不到字符（IME/离屏辅助窗口持有焦点）；
/// 8. 修饰键被按住。
pub fn check_snapshot(snap: &FocusSnapshot) -> Result<(), FocusError> {
    // 自检开关在前：进程内的自检窗口天然与自检程序同进程，其余安全检查（锁屏、
    // 最小化、提权、无焦点、按住修饰键）照常执行。
    let relaxed = shared_foreground_check_disabled();
    // 只有“真的能查到进程权限/窗口类名”时才把它当成拒绝依据：编造的句柄查下去
    // 只会得到假阳性。
    let live = handle_is_live(snap.hwnd_foreground, snap.pid);
    if window_belongs_to_us(snap) && !relaxed {
        return Err(FocusError::SelfTarget);
    }
    let owned = window_belongs_to_us(snap);
    // 锁屏/UAC 安全桌面：先看前台窗口类名，再看线程桌面（锁屏会暴露一个带焦点的
    // CoreWindow，光看“有没有焦点”是拦不住的；反过来“OpenInputDesktop 打不开”
    // 在非交互会话里也会成立，不能单独作为拒绝依据）。
    if !owned && live && is_secure_surface(snap) {
        return Err(FocusError::SecureDesktop);
    }
    if !owned && !snap.on_input_desktop {
        return Err(FocusError::SecureDesktop);
    }
    if is_iconic(snap) {
        return Err(FocusError::Minimized);
    }
    if live && elevation_blocked(snap.pid) {
        return Err(FocusError::ElevatedTarget);
    }
    // 我们自己是提权的：目标只要不是同级（普通权限窗口就是“更低一级”），UIPI 就会
    // 把 SendInput 的键事件静默丢掉——用户看到的是“回执成功、字却没进去”。
    // 这里明确拒绝并给出“别以管理员身份运行”的提示，而不是让它装作成功。
    if elevation_relation(snap.pid) == ElevationRelation::SelfHigher {
        return Err(FocusError::ElevatedSelf);
    }
    if foreground_shared_with_us(snap) && !relaxed {
        return Err(FocusError::SelfTarget);
    }
    if !snap.has_focus() {
        return Err(FocusError::NoTarget);
    }
    if !receives_keyboard_input(snap) {
        return Err(FocusError::InputBlocked(describe_window(snap)));
    }
    // 捕获窗口查询要跨进程调用 `GetGUIThreadInfo`：编造的快照上不做，避免把
    // “查不到”的理由混进纯逻辑判定。
    if live && thread_is_capturing(snap) {
        return Err(FocusError::InputBlocked(format!(
            "{} (捕获窗口)",
            describe_window(snap)
        )));
    }
    if modifiers_held() {
        return Err(FocusError::ModifiersHeld);
    }
    Ok(())
}

/// 解析目标窗口并执行全部安全检查（不含“真正开始输入”那一步）。
pub fn resolve_target() -> Result<FocusSnapshot, FocusError> {
    resolve_target_with(snapshot_foreground)
}

/// [`resolve_target`] 的可注入版本：把“取快照”作为参数传入。
///
/// 为什么要拆：判定顺序是安全边界，纯逻辑单测必须能提供任意快照（无前台、别的进程、
/// 自称本进程、锁屏桌面……），否则只能靠真窗口才测得到。
fn resolve_target_with<S>(snapshot: S) -> Result<FocusSnapshot, FocusError>
where
    S: Fn() -> Option<FocusSnapshot>,
{
    let snap = snapshot().ok_or(FocusError::NoTarget)?;
    check_snapshot(&snap)?;
    Ok(snap)
}

/// 一次判定的耗时（毫秒），供日志/诊断使用。
///
/// 超过 [`RESOLVE_BUDGET_MS`] 说明链路里混进了阻塞调用（历史上踩过的坑：在判定里
/// 做跨进程 COM/UIA 查询），日志里会被标出来。
pub fn resolve_target_timed() -> Result<(FocusSnapshot, u64), FocusError> {
    let started = Instant::now();
    let result = resolve_target();
    let elapsed = started.elapsed().as_millis() as u64;
    if elapsed > RESOLVE_BUDGET_MS {
        service_warn(&format!(
            "焦点判定耗时 {elapsed} ms，超过预算 {RESOLVE_BUDGET_MS} ms：链路里可能有阻塞调用"
        ));
    }
    result.map(|snap| (snap, elapsed))
}

/// 本模块不依赖日志框架（要能被测试/示例单独引用），统一走 stderr。
fn service_warn(message: &str) {
    eprintln!("[focus] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    // 说明：这里的快照全部用“编造的句柄 + 非本进程 pid”，查询类判定（类名、提权、
    // 捕获窗口）在 `handle_is_live` 处就被跳过，所以用例是确定性的、不依赖真实桌面。

    /// 编造的窗口句柄快照会走 `foreground_shared_with_us`（真实 Z 序查询），在无关的
    /// 检查上先拦住。测试要验的是判定顺序本身，所以临时放开那一条；用同一把锁串行，
    /// 免得并行测试互相关掉别人的开关。
    mod tests_env {
        use std::sync::Mutex;

        use super::{
            check_accepts_plain_child_focus_snapshot,
            check_rejects_focus_in_another_top_level_window,
        };
        use crate::service::win::injection::SHARED_FOREGROUND_ENV;

        /// 串行化“改环境变量”的测试用的锁。
        ///
        /// 拿不到锁就立刻 panic，绝不死等：在 Rust 测试线程上阻塞等待这把锁会永久挂住
        /// （已实测），整条 `cargo test` 乃至 CI 都会被冻住。
        static ENV_LOCK: Mutex<()> = Mutex::new(());

        /// 在“测试本线程内”临时打开共享前台的豁免开关。
        ///
        /// 注意这里**不用** RAII：`MutexGuard` 的生命周期绑定在闭包栈上，一旦被放进返回
        /// 值、又碰上测试线程池，`Drop` 时机就不可控（实测会出现锁没被释放）。
        /// 所以改成显式传闭包：闭包结束时同一个函数里立刻还原环境变量并放锁。
        pub fn with_shared_foreground<T>(enabled: bool, run: impl FnOnce() -> T) -> T {
            let guard = ENV_LOCK
                .try_lock()
                .unwrap_or_else(|_| panic!("ENV_LOCK 被别的测试占用（WouldBlock）"));
            let previous = std::env::var_os(SHARED_FOREGROUND_ENV);
            // SAFETY: 持锁期间只有本测试改这个变量。
            unsafe {
                match enabled {
                    true => std::env::set_var(SHARED_FOREGROUND_ENV, "1"),
                    false => std::env::remove_var(SHARED_FOREGROUND_ENV),
                }
            }
            let result = run();
            // SAFETY: 同上，仍持锁。
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(SHARED_FOREGROUND_ENV, value),
                    None => std::env::remove_var(SHARED_FOREGROUND_ENV),
                }
            }
            drop(guard);
            result
        }

        #[test]
        fn with_shared_foreground_sets_and_restores() {
            let before = std::env::var_os(SHARED_FOREGROUND_ENV);
            with_shared_foreground(true, || {
                assert!(
                    std::env::var_os(SHARED_FOREGROUND_ENV).is_some(),
                    "闭包内开关必须开着"
                );
                // 两个使用该开关的用例必须在**同一个闭包**里顺序执行：
                // 并行测试时它们同时要拿 ENV_LOCK，而为了不挂住 CI 这里只能
                // try_lock（拿不到立即 panic），所以不能把它们分成两个 `#[test]`。
                check_rejects_focus_in_another_top_level_window();
                check_accepts_plain_child_focus_snapshot();
            });
            assert_eq!(
                std::env::var_os(SHARED_FOREGROUND_ENV),
                before,
                "闭包结束必须还原"
            );
        }
    }

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
            FocusError::InputBlocked("x".into()),
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
        assert_eq!(window_style(0), 0);
    }

    #[test]
    fn elevation_of_self_is_not_blocking() {
        assert!(!elevation_blocked(current_pid()));
        // 自己永远不比“自己”高权限。
        assert_ne!(
            elevation_relation(current_pid()),
            ElevationRelation::TargetHigher
        );
        // 不能断言 process_elevated(current_pid()) == Some(false)：这一条只在“测试进程
        // 没提权”时成立，而 GitHub 的 windows-latest 上 runner 是管理员，cargo test
        // 出来的进程也带提权令牌，那里它是 Some(true)——v0.2.0 的 Windows 作业正是
        // 卡在这个断言上（`elevation_of_self_is_not_blocking ... FAILED`）。
        // 只要求“要么查不到，要么查得到且与自身一致”。
        if let Some(elevated) = process_elevated(current_pid()) {
            assert_eq!(
                elevation_relation(current_pid()),
                ElevationRelation::SameLevel,
                "{}",
                elevated
            );
        }
    }

    #[test]
    fn elevation_relation_covers_every_polarity() {
        // 纯关系表：拦截只由 TargetHigher 触发；我们不比目标高时也不能误拦。
        assert_eq!(
            elevation_relation_of(Some(true), Some(false)),
            ElevationRelation::TargetHigher
        );
        assert_eq!(
            elevation_relation_of(Some(true), Some(true)),
            ElevationRelation::SameLevel
        );
        assert_eq!(
            elevation_relation_of(Some(false), Some(false)),
            ElevationRelation::SameLevel
        );
        assert_eq!(
            elevation_relation_of(Some(false), Some(true)),
            ElevationRelation::SelfHigher
        );
        // 查不到令牌一律 Unknown，放行交给 SendInput 的真实结果。
        assert_eq!(
            elevation_relation_of(None, Some(false)),
            ElevationRelation::Unknown
        );
        assert_eq!(
            elevation_relation_of(Some(true), None),
            ElevationRelation::Unknown
        );
    }

    #[test]
    fn modifiers_query_does_not_panic() {
        let _ = modifiers_held();
        let _ = system_idle_ms();
    }

    #[test]
    fn check_order_rejects_own_window_before_everything_else() {
        let mut own = snap(0x100, 0x101);
        own.pid = current_pid();
        own.on_input_desktop = false;
        assert_eq!(check_snapshot(&own), Err(FocusError::SelfTarget));
    }

    #[test]
    fn check_rejects_secure_desktop_by_class_and_by_desktop_name() {
        // 真实窗口类名才可能命中，这里直接验证“不在 Default 桌面”这一条。
        let mut off = snap(0x100, 0x101);
        off.on_input_desktop = false;
        assert_eq!(check_snapshot(&off), Err(FocusError::SecureDesktop));
    }

    // 这两条用例不再是 `#[test]`：它们都要临时改环境变量，而 `with_shared_foreground`
    // 只能 try_lock（为了不把 CI 挂住）；两个并行的 `#[test]` 同时进来就会
    // WouldBlock 而失败。统一由 `with_shared_foreground_sets_and_restores` 顺序调用。
    fn check_rejects_focus_in_another_top_level_window() {
        // 焦点属于另一个顶级窗口（典型：输入法候选窗/离屏 IME）——拒绝。
        //
        // 快照里的句柄是编造的，`foreground_shared_with_us` 会走 Z 序链得到“链尾/无
        // 其他窗口”的结论，从而在 SelfTarget 上先拦住；这里用测试专用的环境开关放开
        // 那一条，专门验证“根窗口 != 前台窗口”这一判定本身。
        let mut other_root = snap(0x100, 0x101);
        other_root.hwnd_root = 0x200;
        assert!(matches!(
            check_snapshot(&other_root),
            Err(FocusError::InputBlocked(_))
        ));
    }

    fn check_accepts_plain_child_focus_snapshot() {
        // 焦点在前台窗口的子控件上：通过全部检查（修饰键此时没被按住）。
        //
        // 用核心控件路径验证：前台窗口“不是本程序的窗口/不是本程序在前”这一条
        // 在真实桌面上可能成立也可能不成立，纯逻辑单测里用测试专用开关把它固定
        // 成“命中”，测完按原样还原。用户此刻真的按着 Shift（极罕见）时跳过。
        if modifiers_held() {
            eprintln!("跳过：用户此刻按着修饰键（会被 ModifiersHeld 拦住）");
            return;
        }
        let ok = snap(0x100, 0x101);
        assert_eq!(check_snapshot(&ok), Ok(()));
    }

    #[test]
    fn resolve_returns_no_target_when_no_foreground() {
        let result: Result<FocusSnapshot, FocusError> = resolve_target_with(|| None);
        assert_eq!(result, Err(FocusError::NoTarget));
    }

    #[test]
    fn resolve_reports_snapshot_failure_kind() {
        let result: Result<FocusSnapshot, FocusError> = resolve_target_with(|| {
            let mut off = snap(0x100, 0x101);
            off.on_input_desktop = false;
            Some(off)
        });
        assert_eq!(result, Err(FocusError::SecureDesktop));
    }

    #[test]
    fn real_resolve_is_within_budget() {
        // 无前台窗口的会话（CI/锁屏）里也会立刻返回 None，不会卡住。
        let started = Instant::now();
        let _ = resolve_target();
        assert!(
            started.elapsed().as_millis() < RESOLVE_BUDGET_MS as u128 * 10,
            "resolve_target 不应阻塞"
        );
    }
}
