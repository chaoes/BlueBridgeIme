//! AF_UNIX ⇄ 命名管道适配器（`bluetooth-input-bridge-socket.exe`）。
//!
//! ## 为什么需要它
//!
//! LÖVE 前端的控制面客户端（`love/src/service.lua`）用 LuaJIT FFI 连
//! `struct sockaddr_un`，也就是本机套接字（AF_UNIX）。Windows 上的 Rust 标准库提供了
//! `std::os::windows::net::UnixListener`/`UnixStream`（AF_UNIX），但 LuaJIT 拿不到这些
//! 句柄；而我们的控制通道本身是 [`control::PIPE_NAME`] 命名管道。
//!
//! 这个小二进制把两者接起来：对外监听一个**普通文件路径**的 AF_UNIX 套接字，
//! 每条连接打开一条控制管道，然后**纯字节双向转发**（不解析 JSON、不分帧）。
//!
//! ```text
//! LÖVE  connect C:\…\bluetooth-input-bridge-<pid>\control.sock
//!   →  [本桥] AF_UNIX 监听 + 字节双向转发
//!   →  \\.\pipe\bluetooth-input-bridge-control
//! ```
//!
//! ## 为什么是“无状态字节转发”
//!
//! 控制通道是“一行请求 → 一行应答”，`subscribe` 是一条长连接——两者都是字节流，
//! 语义全在 JSON 里。桥不解析、不缓存、不改写，所以：
//!
//! - 两头的 EOF/错误原样透传，客户端行为与直连管道完全一致；
//! - 协议以后加命令/加事件都不用改桥；
//! - 服务不在时桥**继续监听**，下一条连接再试（它不负责拉起服务）。
//!
//! ## 目录 / 路径（与 LÖVE 侧约定）
//!
//! | 情况 | 目录 | 套接字路径 |
//! |---|---|---|
//! | `--parent <pid>` | `%TEMP%\bluetooth-input-bridge-<pid>` | `<目录>\control.sock` |
//! | 无 `--parent` | `%TEMP%\bluetooth-input-bridge` | `<目录>\control.sock` |
//! | 上面两行的路径 > 107 字节 | — | `%TEMP%\bluetooth-input-bridge[-<pid>].sock` |
//! | `BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR` / `--socket-dir` | 覆盖目录 | 同上两条长度规则 |
//!
//! 107 字节是 `sockaddr_un.sun_path` 放得下的最大值（108 - 结尾 NUL）。**不截断路径**：
//! 超过就用扁平短名（`sockaddr_un` 只传前 107 字节，截断出来的路径根本连不上）。
//!
//! ## 单实例与 `--stop`
//!
//! 单实例锁**按套接字路径派生**：`TcpListener::bind("127.0.0.1:<派生端口>")`，
//! 端口 = FNV-1a(小写路径) 映射到 `[37000, 41000)`（见 [`lock_port`]）。好处是：
//!
//! - 同一路径只允许一个桥（第二个实例退出 0），不同 LÖVE 进程各有各的目录，互不干扰；
//! - `--stop [--parent <pid>]` 往同一个派生端口连一下就是“目标在不在跑”的探测，
//!   连上再写一行 `stop` 让对方退出——不用另想办法找进程。
//!
//! ## 生命周期
//!
//! - `--parent <pid>`：每秒 `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject(0)` 查一次；
//!   父进程（LÖVE）消失 → 删套接字 → 退出。
//! - `--stop`：同上，退出前先删套接字。
//! - 启动时发现残留的 `control.sock`：先连一次，连不上才删掉重建。

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;

use super::control::PIPE_NAME;
use super::service;

/// 套接字文件名（目录模式）。
pub const SOCKET_FILE_NAME: &str = "control.sock";
/// 套接字目录名前缀（后面接父进程 pid，或为空 = 固定目录）。
pub const DIR_PREFIX: &str = "bluetooth-input-bridge";
/// 目录覆盖用的环境变量。
pub const SOCKET_DIR_ENV: &str = "BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR";
/// 路径超限时退化到的扁平文件名。
pub const FLAT_FILE_PREFIX: &str = "bluetooth-input-bridge";
/// 套接字路径的字节上限（`sockaddr_un.sun_path` 108 字节，含结尾 NUL）。
pub const MAX_SOCKET_PATH_BYTES: usize = 107;
/// 单实例锁的端口区间起点。
pub const LOCK_PORT_BASE: u16 = 37_000;
/// 单实例锁的端口区间长度。
pub const LOCK_PORT_SPAN: u16 = 4_000;
/// 父进程检查周期。
pub const PARENT_POLL: Duration = Duration::from_secs(1);
/// accept 的轮询间隔（毫秒）：`WSAPoll` 的超时，同时也是停止请求的检查节拍。
const ACCEPT_TICK_MS: i32 = 50;
/// 连控制管道时的重试次数（服务可能晚 1 秒才起来）。
const PIPE_OPEN_RETRIES: usize = 10;
/// 连控制管道的重试间隔。
const PIPE_OPEN_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// AF_UNIX over WinSock（std 的 `std::os::windows::net::UnixStream` 在 rustc 1.99.1
// 里还是 unstable（`windows_unix_domain_sockets`，#150487），而且那些类型没有实现
// tokio 的 AsyncRead/AsyncWrite。所以这里直接用 WinSock 建 `AF_UNIX` 套接字，
// 再把原始 `SOCKET` 句柄提升成 tokio 的 `TcpStream`——Windows 上那个 wrapper 的底层
// 就是一个已注册到 IOCP 的 SOCKET，读写走的都是标准 socket 语义，与地址族无关。
// ---------------------------------------------------------------------------

/// AF_UNIX 套接字的原始句柄包装：只提供 `AsRawSocket`/`FromRawSocket`。
///
/// 存 `usize`（= `std::os::windows::io::RawSocket`），每次用时转成 `WinSock::SOCKET`。
#[derive(Clone, Copy)]
struct RawSocket(std::os::windows::io::RawSocket);

/// `SOCKET` ↔ `RawSocket`：`SOCKET` 内部是 `usize`，`RawSocket` 是 `u64`（64 位机上同宽）。
fn socket_of(raw: &RawSocket) -> windows::Win32::Networking::WinSock::SOCKET {
    windows::Win32::Networking::WinSock::SOCKET(usize::try_from(raw.0).unwrap_or(usize::MAX))
}

fn raw_of(socket: windows::Win32::Networking::WinSock::SOCKET) -> RawSocket {
    RawSocket(socket.0 as std::os::windows::io::RawSocket)
}

impl RawSocket {
    /// 转成 WinSock 句柄类型。
    fn win(&self) -> windows::Win32::Networking::WinSock::SOCKET {
        socket_of(self)
    }
}

impl AsRawSocket for RawSocket {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        self.0
    }
}

impl FromRawSocket for RawSocket {
    /// # Safety
    /// `socket` 必须是一个由 `socket()` 建出、且尚未被其它 `FromRawSocket` 接管的句柄。
    unsafe fn from_raw_socket(socket: std::os::windows::io::RawSocket) -> Self {
        RawSocket(socket)
    }
}

/// 把任意族（AF_UNIX）的原始 SOCKET 提升成 tokio 的 `TcpStream`。
///
/// Windows 上 tokio 的 `TcpStream` 只是一个 IOCP 注册过的 socket：`from_std` 只做
/// 注册与 `set_nonblocking`，不检查地址族；读写/关闭都与 AF_INET 完全同路。
/// 句柄所有权转移给返回的流（由它负责 `closesocket`）。
fn adopt_socket(socket: &RawSocket) -> io::Result<tokio::net::TcpStream> {
    // SAFETY: 所有权从这里交给 std::net::TcpStream；之后不会再有人用 `socket` 关它。
    let std_stream = unsafe { std::net::TcpStream::from_raw_socket(socket.as_raw_socket()) };
    tokio::net::TcpStream::from_std(std_stream)
}

/// 把路径装进 `SOCKADDR_UN`（`sun_path` 定长 108 字节，末尾必须是 NUL）。
///
/// 返回 `None` 表示路径太长（调用方应当早就退化到短路径了；真到这里就是编程错误）。
fn sockaddr_un(path: &Path) -> Option<(windows::Win32::Networking::WinSock::SOCKADDR_UN, i32)> {
    use windows::Win32::Networking::WinSock::{AF_UNIX, SOCKADDR_UN};

    let bytes = path.to_string_lossy();
    let bytes = bytes.as_bytes();
    if bytes.len() >= 108 {
        return None;
    }
    let mut addr = SOCKADDR_UN {
        sun_family: windows::Win32::Networking::WinSock::ADDRESS_FAMILY(AF_UNIX),
        sun_path: [0i8; 108],
    };
    for (index, byte) in bytes.iter().enumerate() {
        addr.sun_path[index] = i8::try_from(*byte).ok()?;
    }
    // 长度 = 头（family + 对齐）+ 路径 + 结尾 NUL，与 LÖVE 侧 FFI 的计算一致。
    let header = core::mem::size_of_val(&addr.sun_family);
    let len = header + bytes.len() + 1;
    Some((addr, i32::try_from(len).ok()?))
}

/// 新建一个**已设为非阻塞**的 AF_UNIX SOCK_STREAM 套接字。
///
/// 用 `WSASocketW(..., WSA_FLAG_OVERLAPPED)`：tokio 在 Windows 上靠 IOCP 拿就绪事件，
/// 没有 OVERLAPPED 标志的 socket 注册不上。
fn new_unix_socket() -> io::Result<RawSocket> {
    use windows::Win32::Networking::WinSock::{
        ioctlsocket, WSAGetLastError, WSASocketW, AF_UNIX, FIONBIO, SOCK_STREAM,
        WSA_FLAG_OVERLAPPED,
    };

    // SAFETY: 纯 FFI；SOCKET 句柄由本函数接收并在失败时立刻关闭。
    unsafe {
        // 协议参数必须传 0（由传输层决定）。实测：传 IPPROTO_TCP(6) 会被拒绝，
        // `WSASocketW` 返回 WSAEAFNOSUPPORT（10043 / 0x273B，
        // “请求的协议还没有在系统中配置”），而传 0 能直接拿到套接字。
        let raw = match WSASocketW(
            i32::from(AF_UNIX),
            SOCK_STREAM.0,
            0,
            None,
            0,
            WSA_FLAG_OVERLAPPED,
        ) {
            Ok(socket) => socket,
            Err(error) => {
                let code = WSAGetLastError();
                return Err(io::Error::other(format!(
                    "WSASocketW(AF_UNIX) 失败：{error}（{}）",
                    code.0
                )));
            }
        };
        // 非阻塞：tokio 只会在“已就绪”时调 read/write，阻塞句柄会让工作线程卡死。
        let mut nonblocking: u32 = 1;
        if ioctlsocket(raw, FIONBIO, &mut nonblocking) != 0 {
            let code = WSAGetLastError();
            let _ = windows::Win32::Networking::WinSock::closesocket(raw);
            return Err(io::Error::other(format!(
                "ioctlsocket(FIONBIO) 失败：{}",
                code.0
            )));
        }
        Ok(raw_of(raw))
    }
}

/// 连一个 AF_UNIX 监听者（阻塞、带超时由调用方控制）；返回已提升成 tokio 流的连接。
fn connect_unix(path: &Path) -> io::Result<tokio::net::TcpStream> {
    use windows::Win32::Networking::WinSock::{closesocket, connect, WSAGetLastError};

    let Some((addr, len)) = sockaddr_un(path) else {
        return Err(io::Error::other(format!(
            "套接字路径太长（{} 字节）：{}",
            path_bytes(path),
            path.display()
        )));
    };
    let socket = new_unix_socket()?;
    // SAFETY: addr/len 与 socket 同一次调用内有效；失败时句柄立即关闭。
    let code = unsafe {
        connect(
            socket.win(),
            &addr as *const _ as *const windows::Win32::Networking::WinSock::SOCKADDR,
            len,
        )
    };
    if code != 0 {
        let last = unsafe { WSAGetLastError() };
        unsafe { closesocket(socket.win()) };
        return Err(io::Error::other(format!("connect 失败：{}", last.0)));
    }
    adopt_socket(&socket)
}

/// 在同路径上探测“有没有活着的监听者”。
fn probe_socket_impl(path: &Path) -> bool {
    // 用 std 的阻塞 connect 探测会卡住（没有监听者时 Windows 立刻返回错误，不会卡）；
    // 这里不进入 tokio 运行时，所以直接同步判断。
    connect_unix(path).is_ok()
}

/// AF_UNIX 监听者：持有原始句柄，accept 出来的连接被提升成 tokio 流。
struct UnixListener {
    socket: RawSocket,
}

impl UnixListener {
    /// 建 socket → bind → listen。
    fn bind(path: &Path) -> io::Result<Self> {
        use windows::Win32::Networking::WinSock::{
            bind, closesocket, listen, WSAGetLastError, SOCKADDR,
        };

        let Some((addr, len)) = sockaddr_un(path) else {
            return Err(io::Error::other(format!(
                "套接字路径太长（{} 字节）：{}",
                path_bytes(path),
                path.display()
            )));
        };
        let socket = new_unix_socket()?;
        // SAFETY: addr/len 在调用内有效；失败时立即关闭句柄。
        let code = unsafe { bind(socket.win(), &addr as *const _ as *const SOCKADDR, len) };
        if code != 0 {
            let last = unsafe { WSAGetLastError() };
            unsafe { closesocket(socket.win()) };
            return Err(io::Error::other(format!("bind 失败：{}", last.0)));
        }
        // 32 条并发足够（LÖVE 每条命令开一条新连接，服务上限 16）。
        if unsafe { listen(socket.win(), 32) } != 0 {
            let last = unsafe { WSAGetLastError() };
            unsafe { closesocket(socket.win()) };
            return Err(io::Error::other(format!("listen 失败：{}", last.0)));
        }
        Ok(UnixListener { socket })
    }

    /// accept 一条连接；`None` 表示“暂时没有”（`WSAEWOULDBLOCK`）。
    ///
    /// 调用方先 [`poll_readable`] 等可读；这里仍是非阻塞的，失败即返回。
    fn try_accept(&self) -> io::Result<Option<tokio::net::TcpStream>> {
        use windows::Win32::Networking::WinSock::{accept, WSAGetLastError, WSAEWOULDBLOCK};

        // SAFETY: accept 只写调用方给的地址缓冲区；这里不要对端地址所以传 None。
        let result = unsafe { accept(self.socket.win(), None, None) };
        match result {
            Ok(socket) => Ok(Some(adopt_socket(&raw_of(socket))?)),
            Err(error) if error.code().0 == WSAEWOULDBLOCK.0 => Ok(None),
            Err(_) => {
                let last = unsafe { WSAGetLastError() };
                Err(io::Error::other(format!("accept 失败：{}", last.0)))
            }
        }
    }
}

impl Drop for UnixListener {
    fn drop(&mut self) {
        // SAFETY: 句柄由自己唯一持有。
        unsafe {
            let _ = windows::Win32::Networking::WinSock::closesocket(self.socket.win());
        }
    }
}

/// 最多等 `timeout_ms` 毫秒，看这个监听句柄上有没有新连接可 accept。
///
/// 只读裸句柄（`RawSocket` 是 `Copy`），所以可以在 `spawn_blocking` 里跑而不搬监听器。
fn poll_readable(socket: RawSocket, timeout_ms: i32) -> bool {
    use windows::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, WSAPOLLFD};

    let mut fd = WSAPOLLFD {
        fd: socket.win(),
        events: POLLRDNORM,
        revents: Default::default(),
    };
    // SAFETY: 单个元素数组，SDK 语义即“轮询这一个 fd”。
    unsafe { WSAPoll(&mut fd, 1, timeout_ms) > 0 }
}

/// 桥的命令行参数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeArgs {
    /// 父进程 pid（LÖVE）：决定套接字目录与生命周期。`None` = 不监视、用固定目录。
    pub parent: Option<u32>,
    /// `--stop`：通知同路径的已有实例退出后自己退出。
    pub stop: bool,
    /// `--socket-dir <目录>`：覆盖套接字目录（调试/多实例用）。
    pub socket_dir: Option<PathBuf>,
}

/// 参数错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgsError {
    /// 参数后面缺少值。
    MissingValue(&'static str),
    /// pid 不是正整数。
    BadPid(String),
    /// 不认识的参数。
    Unknown(String),
}

impl std::fmt::Display for ArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgsError::MissingValue(flag) => write!(f, "参数 {flag} 后面缺少值"),
            ArgsError::BadPid(value) => write!(f, "参数 --parent 的 pid 不认识：{value}"),
            ArgsError::Unknown(flag) => write!(f, "未知参数：{flag}"),
        }
    }
}

/// 解析命令行（不含可执行文件名）。
///
/// 同时接受 `--flag value` 与 `--flag=value`。`--parent 0` 视为非法（0 不是进程）。
pub fn parse_args<I, S>(args: I) -> Result<BridgeArgs, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut parsed = BridgeArgs::default();
    let mut iter = args.into_iter().map(Into::into);
    while let Some(arg) = iter.next() {
        if arg == "--help" || arg == "-h" {
            return Err(ArgsError::Unknown(arg));
        }
        // 支持 `--parent=123` 与 `--parent 123` 两种写法：拆出 flag 与行内值。
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_string(), Some(value.to_string()))
            }
            _ => (arg, None),
        };
        match flag.as_str() {
            "--parent" => {
                let text = match inline {
                    Some(text) => text,
                    None => iter.next().ok_or(ArgsError::MissingValue("--parent"))?,
                };
                let pid: u32 = text
                    .parse()
                    .ok()
                    .filter(|pid| *pid > 0)
                    .ok_or_else(|| ArgsError::BadPid(text.clone()))?;
                parsed.parent = Some(pid);
            }
            "--stop" => parsed.stop = true,
            "--socket-dir" => {
                let text = match inline {
                    Some(text) => text,
                    None => iter.next().ok_or(ArgsError::MissingValue("--socket-dir"))?,
                };
                parsed.socket_dir = Some(PathBuf::from(text));
            }
            other => return Err(ArgsError::Unknown(other.to_string())),
        }
    }
    Ok(parsed)
}

/// `%TEMP%`（PowerShell 里的 `$env:TEMP`）。
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// 默认套接字**目录**：`<TEMP>\bluetooth-input-bridge-<parent>`；无父进程时去掉 pid 段。
pub fn socket_dir_from(temp: &Path, parent: Option<u32>) -> PathBuf {
    match parent {
        Some(pid) => temp.join(format!("{DIR_PREFIX}-{pid}")),
        None => temp.join(DIR_PREFIX),
    }
}

/// 超过长度上限时的扁平路径：`<TEMP>\bluetooth-input-bridge[-<pid>].sock`。
pub fn flat_socket_path_from(temp: &Path, parent: Option<u32>) -> PathBuf {
    match parent {
        Some(pid) => temp.join(format!("{FLAT_FILE_PREFIX}-{pid}.sock")),
        None => temp.join(format!("{FLAT_FILE_PREFIX}.sock")),
    }
}

/// 从 `%TEMP%` + 目录 + 父进程 pid 推出套接字路径（必要时退化到扁平短名）。
///
/// 规则见模块头部的表格。扁平路径也超限时**原样返回**（日志里会警告），不截断。
pub fn socket_path_from(temp: &Path, dir: &Path, parent: Option<u32>) -> PathBuf {
    let nested = dir.join(SOCKET_FILE_NAME);
    if path_bytes(&nested) <= MAX_SOCKET_PATH_BYTES {
        return nested;
    }
    flat_socket_path_from(temp, parent)
}

/// 判断路径走的是哪种形态（日志用）。
pub fn path_shape(path: &Path) -> &'static str {
    if path.file_name().and_then(|name| name.to_str()) == Some(SOCKET_FILE_NAME) {
        "目录"
    } else {
        "扁平"
    }
}

/// 路径的字节数（`sun_path` 按 UTF-8 字节算，LuaJIT `ffi.copy` 也是按字节）。
pub fn path_bytes(path: &Path) -> usize {
    path.to_string_lossy().len()
}

/// 由套接字路径派生单实例锁端口：FNV-1a 32 位哈希，映射到
/// [`LOCK_PORT_BASE`, `LOCK_PORT_BASE + LOCK_PORT_SPAN`)。
///
/// 用**路径**而不是进程名派生，是为了让不同 LÖVE 进程（不同 `<pid>` 目录）各有各的锁。
/// Windows 路径大小写不敏感，所以先统一小写再哈希。
pub fn lock_port(socket_path: &Path) -> u16 {
    let normalized = socket_path.to_string_lossy().to_lowercase();
    let mut hash: u32 = 0x811c_9dc5;
    for byte in normalized.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    LOCK_PORT_BASE + (hash % u32::from(LOCK_PORT_SPAN)) as u16
}

/// 实际采用的套接字路径。
///
/// 优先级：`--socket-dir` > `BLUETOOTH_INPUT_BRIDGE_SOCKET_DIR` > `%TEMP%\bluetooth-input-bridge[-<pid>]`。
pub fn resolve_socket_path(args: &BridgeArgs) -> PathBuf {
    let temp = temp_dir();
    let dir = match &args.socket_dir {
        Some(dir) => dir.clone(),
        None => match std::env::var_os(SOCKET_DIR_ENV) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => socket_dir_from(&temp, args.parent),
        },
    };
    socket_path_from(&temp, &dir, args.parent)
}

/// 本桥的动作结果（`--stop` 与“已有实例”都靠它决定退出码与日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 已经开始监听，并因父进程消失而退出。
    ParentGone,
    /// 已有同路径实例，本次直接退出 0。
    AlreadyRunning,
    /// `--stop`：已通知对方退出，或自己因收到 `--stop` 而退出。
    StopRequested,
    /// `--stop`：目标没在跑。
    NotRunning,
}

/// 单实例锁：占住一条本机回环端口，`release`/`drop` 即释放。
pub struct InstanceLock {
    listener: Option<TcpListener>,
    port: u16,
}

impl InstanceLock {
    /// 锁端口（`--stop` 往这里连）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 立即释放（退出前调用；`drop` 也会释放）。
    pub fn release(&mut self) {
        self.listener = None;
    }
}

/// 按路径派生的端口抢单实例锁：成功返回句柄，已被占返回 `None`。
///
/// Windows 默认不允许端口复用，所以第二个 `bind` 同一端口必然失败——这就是锁语义。
pub fn acquire_lock(port: u16) -> Option<InstanceLock> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = TcpListener::bind(addr).ok()?;
    // 非阻塞：只用来 select/轮询，不能卡住任何线程。
    let _ = listener.set_nonblocking(true);
    Some(InstanceLock {
        listener: Some(listener),
        port,
    })
}

/// `--stop`：连一下派生端口；连得上就写一行 `stop` 让对端退出。
pub fn stop_running(port: u16) -> Action {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return Action::NotRunning;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    {
        use std::io::Write;
        // 对端只是“读到东西就当停止”，内容不参与判定。
        let _ = stream.write_all(b"stop\n");
        let _ = stream.flush();
    }
    Action::StopRequested
}

/// 父进程是否还活着（`OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject(0)`）。
///
/// 打不开句柄一律当“已退出”：最常见原因是 `ERROR_INVALID_PARAMETER`（进程已经没了）。
/// 宁可早退出，也不要留下一个没人用的桥。
pub fn parent_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    // SAFETY: 句柄在函数内必然关闭；WaitForSingleObject(0) 只查询，不阻塞。
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        if handle.is_invalid() {
            return false;
        }
        let state = WaitForSingleObject(handle, 0);
        let _ = CloseHandle(handle);
        // WAIT_TIMEOUT = 还在跑；WAIT_OBJECT_0 = 已退出；WAIT_FAILED 等按“已退出”处理。
        state == WAIT_TIMEOUT
    }
}

/// 当前进程 pid。
pub fn own_pid() -> u32 {
    // SAFETY: 无参数，永远成功。
    unsafe { windows::Win32::System::Threading::GetCurrentProcessId() }
}

/// 把两个字节流双向接起来；**任一方向结束就整条连接结束并关闭两边**。
///
/// 返回 `(上行字节数, 下行字节数)`。只有先完成的那一方向有计数，另一方向为 0
/// （它被取消掉了；取消不会调用 `shutdown`，两个半流随 future 一起 drop，等价于关连接）。
///
/// 这样处理的原因：控制通道是一问一答。客户端写完就 `close` 时，管道侧读到 EOF 会
/// 回最后一帧再关；若等“两个方向都自然结束”才收尾，客户端已关闭的连接会一直挂着。
pub async fn bridge_streams<A, B>(client: A, pipe: B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut client_rx, mut client_tx) = tokio::io::split(client);
    let (mut pipe_rx, mut pipe_tx) = tokio::io::split(pipe);

    let upward = async {
        let copied = tokio::io::copy(&mut client_rx, &mut pipe_tx).await?;
        pipe_tx.shutdown().await?;
        Ok::<u64, io::Error>(copied)
    };
    let downward = async {
        let copied = tokio::io::copy(&mut pipe_rx, &mut client_tx).await?;
        client_tx.shutdown().await?;
        Ok::<u64, io::Error>(copied)
    };
    tokio::pin!(upward);
    tokio::pin!(downward);

    match tokio::select! {
        up = &mut upward => up.map(|bytes| (bytes, 0)),
        down = &mut downward => down.map(|bytes| (0, bytes)),
    } {
        Ok(counts) => Ok(counts),
        Err(error) => Err(error),
    }
}

/// 连控制管道；服务可能还没起来，短暂重试（最多 `PIPE_OPEN_RETRIES` 次，约 1 秒）。
///
/// 重试之内能连上就返回；连不上把最后一次的错误返回给调用方（调用方只记日志）。
pub async fn open_pipe() -> io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let mut last: Option<io::Error> = None;
    for attempt in 0..=PIPE_OPEN_RETRIES {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(client) => return Ok(client),
            Err(error) => {
                last = Some(error);
                if attempt < PIPE_OPEN_RETRIES {
                    tokio::time::sleep(PIPE_OPEN_INTERVAL).await;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("打开控制管道失败")))
}

/// 一条 AF_UNIX 连接：连管道 → 双向转发。失败只记日志（桥继续活着）。
///
/// 参数是已经提升成 tokio 流的 AF_UNIX 连接（见 [`adopt_socket`]）。
async fn handle_client(client: tokio::net::TcpStream) {
    let pipe = match open_pipe().await {
        Ok(pipe) => pipe,
        Err(error) => {
            service::log(&format!("[桥] 控制管道暂不可用（服务还没起来？）：{error}"));
            return;
        }
    };
    match bridge_streams(client, pipe).await {
        Ok((up, down)) => {
            if up + down > 0 {
                service::log(&format!("[桥] 连接结束：上行 {up} 字节，下行 {down} 字节"));
            }
        }
        Err(error) => service::log(&format!("[桥] 连接异常结束：{error}")),
    }
}

/// 探测：能不能连上这个套接字（用于判断有没有活着的监听者）。
///
/// 连上后立刻关掉（返回的流 drop 时 closesocket）——回环道上的空连接对服务端
/// 只是一次“读到 EOF”，`control::serve` 会安静地关掉它。
pub fn probe_socket(path: &Path) -> bool {
    probe_socket_impl(path)
}

/// 启动前处理残留套接字文件：能连上=还有活着的实例（返回 `false`），连不上=删掉重建（`true`）。
pub fn prepare_socket_file(path: &Path) -> bool {
    if std::fs::symlink_metadata(path).is_err() {
        return true; // 本来就没有
    }
    if probe_socket(path) {
        return false;
    }
    service::log(&format!("[桥] 清理残留套接字 {}", path.display()));
    // 忽略错误：真删不掉时 bind 会给出更准确的原因。
    let _ = std::fs::remove_file(path);
    true
}

/// 退出清理：删套接字文件，忽略错误。
pub fn cleanup(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => service::log(&format!("[桥] 已删除 {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => service::log(&format!("[桥] 删除 {} 失败：{error}", path.display())),
    }
}

/// 退出清理：套接字所在目录若是本程序建的（`%TEMP%\bluetooth-input-bridge[-<pid>]`），
/// 一并删掉，免得 `%TEMP%` 里越攒越多。只认自己这两种目录名，别的一概不动。
pub fn cleanup_dir(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let Some(name) = parent.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let own_dir = name == DIR_PREFIX
        || name
            .strip_prefix(DIR_PREFIX)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()));
    if !own_dir {
        return;
    }
    match std::fs::remove_dir(parent) {
        Ok(()) => service::log(&format!("[桥] 已删除目录 {}", parent.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        // 目录不空（用户往里面放了别的东西）就别动，这不是错误。
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
        Err(error) => service::log(&format!("[桥] 删除目录 {} 失败：{error}", parent.display())),
    }
}

/// 完整入口：参数 → （`--stop` | 抢锁 → 清理残留 → 监听 → 转发）→ 清理 → 退出。
pub async fn run(args: BridgeArgs) -> Result<Action, String> {
    let path = resolve_socket_path(&args);
    let port = lock_port(&path);
    let bytes = path_bytes(&path);
    service::log(&format!(
        "[桥] pid {}，套接字 {}（{bytes} 字节，{}形态），锁端口 127.0.0.1:{port}",
        own_pid(),
        path.display(),
        path_shape(&path)
    ));
    if bytes > MAX_SOCKET_PATH_BYTES {
        service::log(&format!(
            "[桥] 警告：套接字路径 {bytes} 字节，超过上限 {MAX_SOCKET_PATH_BYTES}，客户端可能连不上"
        ));
    }

    if args.stop {
        return Ok(match stop_running(port) {
            Action::StopRequested => {
                service::log(&format!("[桥] 已通知 {} 退出", path.display()));
                Action::StopRequested
            }
            _ => {
                service::log(&format!("[桥] {} 没有在监听（无需停止）", path.display()));
                Action::NotRunning
            }
        });
    }

    let Some(mut lock) = acquire_lock(port) else {
        service::log(&format!(
            "[桥] 已有同路径实例在运行（锁端口 127.0.0.1:{port}），本次直接退出"
        ));
        return Ok(Action::AlreadyRunning);
    };

    if let Some(dir) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(dir) {
            lock.release();
            return Err(format!("无法创建套接字目录 {}：{error}", dir.display()));
        }
    }
    if !prepare_socket_file(&path) {
        lock.release();
        return Err(format!("{} 上已有活着的监听者，本实例退出", path.display()));
    }

    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("无法监听 {}：{error}", path.display()))?;
    service::log(&format!("[桥] 已监听 {}", path.display()));

    // 不管怎么退出（正常/父进程消失/--stop），都要删套接字再放锁：先删文件，再放锁，
    // 免得下一个实例抢到锁之后看到的是“旧文件 + 新锁”的时序。
    let outcome = serve(listener, &lock, args.parent).await;
    cleanup(&path);
    cleanup_dir(&path);
    lock.release();
    match outcome {
        Exit::ParentGone(pid) => {
            service::log(&format!("[桥] 父进程 {pid} 已退出，桥随之退出"));
            Ok(Action::ParentGone)
        }
        Exit::BasicStopped => {
            service::log("[桥] 收到 --stop，正在退出");
            Ok(Action::StopRequested)
        }
    }
}

/// 监听循环的结束原因。
enum Exit {
    /// 父进程没了（`--parent` 那一行：先删套接字再退出）。
    ParentGone(u32),
    /// 收到了 `--stop`（同样先删套接字再退出）。
    BasicStopped,
}

/// 监听循环：accept + 父进程检查 + `--stop` 端口检查。
///
/// - 每 [`ACCEPT_TICK`] 醒一次，处理排队的连接与停止请求；
/// - 每 [`PARENT_POLL`]（1 秒）用 `OpenProcess(SYNCHRONIZE)` + `WaitForSingleObject(0)`
///   查一次父进程。
async fn serve(listener: UnixListener, lock: &InstanceLock, parent: Option<u32>) -> Exit {
    let mut parent_poll = tokio::time::interval(PARENT_POLL);
    parent_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // 第一次 tick 立即就绪：启动时马上查一次父进程，避免父进程在桥起来前就死了。
    parent_poll.tick().await;
    if let Some(pid) = parent {
        if !parent_alive(pid) {
            return Exit::ParentGone(pid);
        }
    }

    loop {
        // 最多等 50 ms 看有没有新连接。`WSAPoll` 是同步调用，但只等很短，
        // 且直接作用于裸句柄：`spawn_blocking` 里跑，别卡住异步工作线程。
        let raw = listener.socket;
        let ready = tokio::task::spawn_blocking(move || poll_readable(raw, ACCEPT_TICK_MS))
            .await
            .unwrap_or(false);

        if ready {
            // 可能一次就绪了多条连接：循环 accept 到空为止。
            loop {
                match listener.try_accept() {
                    Ok(Some(client)) => {
                        tokio::spawn(handle_client(client));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        // 单条连接失败不影响整体（对端提前关闭很常见）。
                        service::log(&format!("[桥] accept 失败：{error}"));
                        break;
                    }
                }
            }
        }

        if stop_requested(lock) {
            return Exit::BasicStopped;
        }

        // 每秒一次父进程检查（interval 的 tick 在两次 accept 之间累积，最多晚一个节拍）。
        if tokio::time::timeout(Duration::ZERO, parent_poll.tick())
            .await
            .is_ok()
        {
            if let Some(pid) = parent {
                if !parent_alive(pid) {
                    return Exit::ParentGone(pid);
                }
            }
        }
    }
}

/// 是否有客户端往单实例锁端口写了停止请求。
fn stop_requested(lock: &InstanceLock) -> bool {
    use std::io::Read;

    let Some(listener) = lock.listener.as_ref() else {
        return true;
    };
    match listener.accept() {
        Ok((mut stream, _)) => {
            // 谁主动连这个端口、写了点东西，就视为停止请求：端口号由路径派生，
            // 外部（不知道路径）既找不到也没必要连。
            let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
            let mut buffer = [0u8; 64];
            let _ = stream.read(&mut buffer);
            true
        }
        // WouldBlock: 没人连；其它错误也只是“这一拍没有请求”。
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn temp() -> PathBuf {
        PathBuf::from(r"C:\Users\chao\AppData\Local\Temp")
    }

    #[test]
    fn socket_dir_carries_the_parent_pid() {
        let dir = socket_dir_from(&temp(), Some(4242));
        assert_eq!(
            dir,
            PathBuf::from(r"C:\Users\chao\AppData\Local\Temp\bluetooth-input-bridge-4242")
        );
        // 没有父进程时是固定目录，不带 pid 段。
        assert_eq!(
            socket_dir_from(&temp(), None),
            temp().join("bluetooth-input-bridge")
        );
    }

    #[test]
    fn socket_path_is_nested_when_short_enough() {
        let dir = socket_dir_from(&temp(), Some(4242));
        let path = socket_path_from(&temp(), &dir, Some(4242));
        assert_eq!(path, dir.join("control.sock"));
        // 本机 TEMP 长度 + 固定部分 + pid 位数：实测约 79 字节，必须在上限内。
        assert!(
            path_bytes(&path) <= MAX_SOCKET_PATH_BYTES,
            "{}",
            path_bytes(&path)
        );
        assert_eq!(path_shape(&path), "目录");
    }

    #[test]
    fn long_path_falls_back_to_flat_name_without_truncation() {
        let long_temp = PathBuf::from(format!("C:\\{}", "x".repeat(120)));
        let dir = socket_dir_from(&long_temp, Some(777));
        let path = socket_path_from(&long_temp, &dir, Some(777));
        assert_eq!(path, long_temp.join("bluetooth-input-bridge-777.sock"));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("bluetooth-input-bridge-777.sock")
        );
        // 不带 pid 的扁平名。
        assert_eq!(
            socket_path_from(&long_temp, &dir, None),
            long_temp.join("bluetooth-input-bridge.sock")
        );
        assert_eq!(path_shape(&path), "扁平");
    }

    #[test]
    fn boundary_is_exactly_107_bytes() {
        // 造一个长度正好 107 的嵌套路径：必须还走嵌套形态（107 能放进 sun_path）。
        let base = PathBuf::from(r"C:\T");
        let fixed = base.join("bluetooth-input-bridge-1").join("control.sock");
        let pad = MAX_SOCKET_PATH_BYTES - path_bytes(&fixed);
        let dir_name = format!("{}{}", "b".repeat(pad), 1);
        let dir = base.join(format!("bluetooth-input-bridge-{dir_name}"));
        let path = socket_path_from(&base, &dir, Some(1));
        assert_eq!(path, dir.join("control.sock"));
        assert_eq!(path_bytes(&path), MAX_SOCKET_PATH_BYTES);
        // 再多 1 字节就必须退化。
        let dir2 = base.join(format!("bluetooth-input-bridge-{dir_name}b"));
        let path2 = socket_path_from(&base, &dir2, Some(1));
        assert_eq!(path2, base.join("bluetooth-input-bridge-1.sock"));
    }

    #[test]
    fn lock_port_is_path_derived_and_stable() {
        let a = PathBuf::from(r"C:\Temp\bluetooth-input-bridge-1\control.sock");
        let same = PathBuf::from(r"c:\temp\BLUETOOTH-INPUT-BRIDGE-1\control.sock");
        let other = PathBuf::from(r"C:\Temp\bluetooth-input-bridge-2\control.sock");
        assert_eq!(lock_port(&a), lock_port(&a));
        // Windows 路径大小写不敏感：同一个日志目录必须映射到同一个锁。
        assert_eq!(lock_port(&a), lock_port(&same));
        // 不同 LÖVE 进程（不同 pid 目录）互不干扰。
        assert_ne!(lock_port(&a), lock_port(&other));
        let port = lock_port(&a);
        assert!((LOCK_PORT_BASE..LOCK_PORT_BASE + LOCK_PORT_SPAN).contains(&port));
    }

    #[test]
    fn parses_parent_and_stop() {
        let parsed = parse_args(["--parent", "4242", "--stop"]).expect("应可解析");
        assert_eq!(parsed.parent, Some(4242));
        assert!(parsed.stop);
        assert_eq!(parsed.socket_dir, None);

        // `--flag=value` 形式。
        let parsed = parse_args(["--parent=99"]).expect("应可解析");
        assert_eq!(parsed.parent, Some(99));

        let parsed = parse_args(["--socket-dir", r"D:\tmp\abc"]).expect("应可解析");
        assert_eq!(parsed.socket_dir, Some(PathBuf::from(r"D:\tmp\abc")));

        // 无参数：固定目录、不监视、不停止。
        let parsed = parse_args(Vec::<String>::new()).expect("应可解析");
        assert_eq!(parsed, BridgeArgs::default());
    }

    #[test]
    fn rejects_bad_arguments() {
        assert_eq!(
            parse_args(["--parent"]),
            Err(ArgsError::MissingValue("--parent"))
        );
        assert_eq!(
            parse_args(["--parent", "abc"]),
            Err(ArgsError::BadPid("abc".into()))
        );
        assert_eq!(
            parse_args(["--parent", "0"]),
            Err(ArgsError::BadPid("0".into()))
        );
        assert_eq!(
            parse_args(["--wat"]),
            Err(ArgsError::Unknown("--wat".into()))
        );
        assert_eq!(
            parse_args(["--socket-dir"]),
            Err(ArgsError::MissingValue("--socket-dir"))
        );
    }

    #[tokio::test]
    async fn bridge_streams_returns_when_the_client_eofs() {
        // 客户端像“一问一答”一样：写一行，然后关闭连接；管道侧不应该被挂住。
        let (client_a, client_b) = tokio::io::duplex(1024);
        let (pipe_a, mut pipe_b) = tokio::io::duplex(1024);
        let task = tokio::spawn(bridge_streams(client_a, pipe_a));

        // client_b 扮演 LÖVE，pipe_b 扮演服务。
        let mut lua = client_b;
        lua.write_all(b"{\"cmd\":\"status\"}\n").await.expect("写");
        let mut request = vec![0u8; 64];
        let read = pipe_b.read(&mut request).await.expect("读请求");
        assert_eq!(&request[..read], b"{\"cmd\":\"status\"}\n");
        pipe_b.write_all(b"{\"ok\":true}\n").await.expect("写应答");

        // 读回完整应答（DUPLEX 可能分多次送达，所以要循环读到 12 字节），然后关掉客户端
        // → 桥必须结束（不能一直等管道侧）。缓冲开大一点：原来只有 11 字节，末尾的 \n 被截掉了。
        let mut reply = [0u8; 32];
        let mut got = 0usize;
        while got < b"{\"ok\":true}\n".len() {
            let read = lua.read(&mut reply[got..]).await.expect("读应答");
            assert_ne!(read, 0, "客户端没收到完整应答");
            got += read;
        }
        assert_eq!(&reply[..got], b"{\"ok\":true}\n");
        drop(lua);

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("客户端 EOF 后桥必须结束")
            .expect("task 不应 panic")
            .expect("转发不该报错");
        assert_eq!(result, (17, 0), "{result:?}");
        drop(pipe_b);
    }

    #[tokio::test]
    async fn bridge_streams_returns_when_the_service_eofs() {
        // 反方向：管道（服务）先关闭，客户端还在等 → 桥也必须结束。
        let (client_a, mut client_b) = tokio::io::duplex(1024);
        let (pipe_a, pipe_b) = tokio::io::duplex(1024);
        let task = tokio::spawn(bridge_streams(client_a, pipe_a));
        drop(pipe_b);

        let mut buffer = vec![0u8; 16];
        let read = tokio::time::timeout(Duration::from_secs(2), client_b.read(&mut buffer))
            .await
            .expect("管道 EOF 后客户端应立刻看到 EOF")
            .expect("读");
        assert_eq!(read, 0, "应该是干净的 EOF，不是错误");

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("管道 EOF 后桥必须结束")
            .expect("task 不应 panic")
            .expect("转发不该报错");
        // 下行只有 EOF，没有有效字节。
        assert_eq!(result, (0, 0), "{result:?}");
    }

    #[tokio::test]
    async fn bridge_streams_forwards_bytes_in_both_directions() {
        let (client_a, mut client_b) = tokio::io::duplex(4096);
        let (pipe_a, mut pipe_b) = tokio::io::duplex(4096);
        let task = tokio::spawn(bridge_streams(client_a, pipe_a));

        client_b.write_all(b"up\n").await.expect("上行写");
        let mut buffer = vec![0u8; 8];
        let read = pipe_b.read(&mut buffer).await.expect("上行读");
        assert_eq!(&buffer[..read], b"up\n");

        pipe_b.write_all(b"down\n").await.expect("下行写");
        let read = client_b.read(&mut buffer).await.expect("下行读");
        assert_eq!(&buffer[..read], b"down\n");

        drop(client_b);
        let (up, _down) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("应结束")
            .expect("task 不应 panic")
            .expect("转发不该报错");
        assert_eq!(up, 3, "{up}");
    }

    #[test]
    fn resolve_prefers_explicit_dir() {
        // 不碰进程环境变量：只验证 `--socket-dir` 分支（其它分支依赖 %TEMP%）。
        let args = BridgeArgs {
            parent: Some(42),
            stop: false,
            socket_dir: Some(temp().join("bluetooth-input-bridge-custom")),
        };
        let path = resolve_socket_path(&args);
        assert_eq!(
            path,
            temp()
                .join("bluetooth-input-bridge-custom")
                .join("control.sock")
        );
    }
}
