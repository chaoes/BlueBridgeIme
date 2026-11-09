//! Windows 蓝牙 RFCOMM 服务端（Win32 / WinSock AF_BTH 路径）。
//!
//! # 为什么不用 WinRT `RfcommServiceProvider`
//!
//! 在开发机上把 WinRT 的组合全试过一遍：
//!
//! - `RfcommServiceProvider.StartAdvertising` 一律返回 `0xD000000D`（E_INVALIDARG），
//!   连系统自带的串口 UUID（`RfcommServiceId::SerialPort()`，0x1101）也一样；
//! - `StreamSocketListener.BindServiceNameAsync` / `...WithProtectionLevelAsync(PlainSocket)`
//!   对任何蓝牙 UUID 都返回 `0x8007277D`（WSATYPE_NOT_FOUND）——WinRT 的监听器其实
//!   只会按 IP 服务名绑定；
//! - 带加密的 `SocketProtectionLevel::BluetoothEncryption*` 会被直接拒绝
//!   （`0x80070057`：“IP StreamSocketListeners 只能使用未加密的纯文本套接字”）。
//!
//! 也就是说 WinRT 那条路在桌面环境里不可用，因此这里走 Win32 蓝牙协议栈：
//!
//! 1. `WSASocketW(AF_BTH, SOCK_STREAM, BTHPROTO_RFCOMM)` 创建蓝牙套接字；
//! 2. `bind` 到 `SOCKADDR_BTH`（服务类 UUID，端口 0 交给协议栈分配）；
//! 3. `WSASetServiceW(RNRSERVICE_REGISTER, NS_BTH)` 把这条通道登记进 SDP——
//!    手机端 `createRfcommSocketToServiceRecord(uuid)` 就是靠这条记录找到我们的；
//! 4. `listen` + `accept` 收连接，再交给 [`session::serve_on`] 跑协议会话。
//!
//! 顺带的好处：`SOCKET` 是 `Send`，连接可以自由交给工作线程，不必像 WinRT 的
//! `StreamSocket` 那样把整个会话塞回同一个线程里。

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH};
use windows::Win32::Networking::WinSock::{
    accept, bind, closesocket, getpeername, getsockname, listen, setsockopt, WSASetServiceW,
    WSASocketW, WSAStartup, CSADDR_INFO, RNRSERVICE_DELETE, RNRSERVICE_REGISTER, SOCKADDR, SOCKET,
    SOCKET_ADDRESS, SOCK_STREAM, SOL_SOCKET, SO_RCVTIMEO, WSADATA, WSAQUERYSETW,
};

use super::session::{self, SessionStats, SocketWire};

/// SDP 记录中的服务名（手机端配对/服务列表里看到的文字）。
pub const SERVICE_NAME: &str = "Bluetooth Input Bridge";
/// 命名空间：`NS_BTH`。winsock2.h 里是 `#define NS_BTH 16`，windows crate 没导出。
const NS_BTH: u32 = 16;
/// RFCOMM 端口：`BT_PORT_ANY`（ws2bth.h 的 `0xFFFFFFFF`），交给协议栈挑一个空闲通道。
///
/// 服务端 `bind` 时端口必须是“有效通道号或 BT_PORT_ANY”——填 0 会被当成有效的
/// “通道 0”，`getsockname` 之后端口仍是 0，随后 SDP 登记就会报
/// `WSAEINVAL (10022) 提供了一个无效的参数`。windows crate 没有导出这个常量。
const BT_PORT_ANY: u32 = 0xFFFF_FFFF;
/// listen backlog。
const BACKLOG: i32 = 4;
/// 单连接读超时：手机端 8 秒不回执就重发，这里要短一些才能及时收尾。
const RECV_TIMEOUT_MS: u32 = 5_000;
/// SDP 重登记的最短/最长间隔。
const REGISTER_BACKOFF_MIN: Duration = Duration::from_secs(1);
const REGISTER_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// `0000e8b6-0000-1000-8000-00805f9b34fb` 的 u128 形式。
///
/// 服务 UUID 必须是 16/32 位基址形式：SDP 的 ServiceUUIDs 属性只装得下 16/32 位
/// UUID，而且安卓端与电脑端必须逐位一致。
const SERVICE_UUID_BITS: u128 = 0x0000_e8b6_0000_1000_8000_0080_5f9b_34fb;
/// 同一个 UUID 的文本形式（日志与界面显示用）。
pub const SERVICE_UUID_TEXT: &str = "0000e8b6-0000-1000-8000-00805f9b34fb";

/// 服务端状态（`ServiceStatus.lastError` 与界面文案都用它）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// 已监听且 SDP 已登记，新手机能搜到。
    Ready,
    /// 已监听但 SDP 未登记：已知/已配对的手机仍可能直连。
    NotPublished { error: String },
    /// 连监听都没建立（蓝牙关闭、没有适配器）。
    Unavailable { error: String },
}

impl State {
    pub fn summary(&self) -> String {
        match self {
            Self::Ready => format!("已发布 {SERVICE_NAME}（{SERVICE_UUID_TEXT}）"),
            Self::NotPublished { error } => {
                format!("已监听但未发布到蓝牙服务列表（已配对手机可能仍可连接）：{error}")
            }
            Self::Unavailable { error } => format!("蓝牙不可用：{error}"),
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

pub fn service_guid() -> GUID {
    GUID::from_u128(SERVICE_UUID_BITS)
}

/// 会话事件回调：连接/断开/每帧计数，交给上层更新状态面板。
///
/// 用 trait 而不是闭包参数，是为了让 `start()` 的签名保持简单，
/// 且上层（`service.rs`）能自己决定怎么合并状态。
pub trait SessionObserver: Send + Sync + 'static {
    fn connected(&self, device: Option<String>, address: Option<String>);
    fn disconnected(&self);
    fn progress(&self);
}

/// 什么都不做（测试与无界面场景）。
pub struct NoObserver;

impl SessionObserver for NoObserver {
    fn connected(&self, _device: Option<String>, _address: Option<String>) {}
    fn disconnected(&self) {}
    fn progress(&self) {}
}

/// 运行中的服务端。`drop` 时自动停止。
pub struct Server {
    listener: SOCKET,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: `SOCKET` 就是个整数句柄，Winsock 调用本身线程安全，且这里由 `Server`
// 独占它的所有权（关闭也只在 `stop()` 里做一次）。
unsafe impl Send for Server {}
unsafe impl Sync for Server {}

impl Server {
    pub fn state(&self) -> State {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => State::Unavailable {
                error: "服务端状态已损坏".into(),
            },
        }
    }

    /// 停止服务：结束接受线程、注销 SDP、关闭监听套接字。可重复调用。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        // 注销必须赶在 closesocket 之前：DELETE 要带上监听地址，套接字关了就拿不到通道号。
        if !self.listener.is_invalid() {
            deregister_service(service_guid(), self.listener);
            unsafe {
                closesocket(self.listener);
            }
        }
        self.listener = SOCKET::default();
        set_state(
            &self.state,
            State::Unavailable {
                error: "服务已停止".into(),
            },
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动服务端。返回时套接字已 bind+listen，SDP 登记已完成（失败则是降级状态）。
pub fn start(observer: Arc<dyn SessionObserver>) -> Result<Server, String> {
    ensure_winsock();
    let guid = service_guid();
    let listener = create_listener(guid)?;

    let initial = match republish_service(guid, listener) {
        Ok(channel) => {
            log(format!("SDP 已登记，RFCOMM 通道 {channel}"));
            State::Ready
        }
        Err(error) => State::NotPublished { error },
    };
    log(format!("蓝牙服务状态：{}", initial.summary()));

    let stop = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(initial));
    let accept_thread = spawn_accept_loop(listener, stop.clone(), state.clone(), guid, observer);
    Ok(Server {
        listener,
        stop,
        state,
        accept_thread: Some(accept_thread),
    })
}

fn spawn_accept_loop(
    listener: SOCKET,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    guid: GUID,
    observer: Arc<dyn SessionObserver>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("bridge-accept".into())
        .spawn(move || {
            let stats = Arc::new(SessionStats::default());
            let mut backoff = REGISTER_BACKOFF_MIN;
            let mut next_register = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                // 蓝牙被关掉再打开后 SDP 记录会丢，所以定期确认它还在。
                if Instant::now() >= next_register {
                    match republish_service(guid, listener) {
                        Ok(_channel) => {
                            let changed = !state.lock().map(|s| s.is_ready()).unwrap_or(false);
                            set_state(&state, State::Ready);
                            if changed {
                                log("蓝牙服务记录已恢复，手机现在可以搜到".into());
                            }
                            backoff = REGISTER_BACKOFF_MIN;
                        }
                        Err(error) => {
                            let first = !matches!(
                                state.lock().map(|s| s.clone()).unwrap_or(State::Ready),
                                State::NotPublished { .. }
                            );
                            if first {
                                log(format!("登记蓝牙服务记录失败：{error}（后台会自动重试）"));
                            }
                            set_state(&state, State::NotPublished { error });
                            backoff = (backoff * 2).min(REGISTER_BACKOFF_MAX);
                        }
                    }
                    next_register = Instant::now() + backoff;
                }

                match accept_peer(listener) {
                    Ok(Some(client)) => {
                        // 每连接一个线程：手机上一条消息最多等 8 秒，单线程排队会让
                        // 第二条消息直接超时；RFCOMM 会话之间本来也互不影响。
                        let session_stop = stop.clone();
                        let session_observer = observer.clone();
                        let stats = stats.clone();
                        if let Err(error) = std::thread::Builder::new()
                            .name("bridge-session".into())
                            .spawn(move || {
                                handle_client(
                                    client,
                                    &session_stop,
                                    &stats,
                                    session_observer.as_ref(),
                                )
                            })
                        {
                            log(format!("无法创建会话线程：{error}"));
                            observer.disconnected();
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        log(error);
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            log("接受连接的循环已退出".into());
        })
        .expect("创建 bridge-accept 线程")
}

/// 收一个连接：`Ok(None)` 表示这次 accept 没有结果（可继续循环）。
fn accept_peer(listener: SOCKET) -> Result<Option<SOCKET>, String> {
    let mut peer = SOCKADDR::default();
    let mut peer_len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
    match unsafe { accept(listener, Some(&mut peer), Some(&mut peer_len)) } {
        Ok(client) => Ok(Some(client)),
        Err(error) => Err(format!(
            "接受蓝牙连接失败：{}（{:#010X}）",
            describe_wsa(error.code().0),
            error.code().0 as u32
        )),
    }
}

/// 跑一条会话：读对端身份 → 通知上层 → 协议循环 → 关闭套接字 → 通知断开。
fn handle_client(
    client: SOCKET,
    stop: &Arc<AtomicBool>,
    stats: &Arc<SessionStats>,
    observer: &dyn SessionObserver,
) {
    set_recv_timeout(client);
    let (address, device_name) = peer_identity(client);
    let peer = match &device_name {
        Some(name) => format!("{name}（{address}）"),
        None => address.clone(),
    };
    log(format!("手机已连接：{peer}"));
    observer.connected(device_name, Some(address));
    let outcome = session::serve_on(SocketWire::new(client), &peer, stop, stats);
    unsafe {
        closesocket(client);
    }
    log(format!(
        "会话结束（{peer}）：{} 帧 / 成功 {} / 失败 {}（{}）",
        outcome.frames, outcome.injected, outcome.failed, outcome.reason
    ));
    observer.disconnected();
}

/// 对端 MAC 与设备名（设备名拿不到时为 `None`）。
fn peer_identity(client: SOCKET) -> (String, Option<String>) {
    let mut addr = SOCKADDR::default();
    let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
    if unsafe { getpeername(client, &mut addr, &mut len) } != 0 {
        return ("未知设备".into(), None);
    }
    // SAFETY: 蓝牙套接字的对端地址确实是 `SOCKADDR_BTH`。
    let bth = unsafe { *(&addr as *const SOCKADDR as *const SOCKADDR_BTH) };
    let address = format_bdaddr(bth.btAddr);
    let name = super::device_name::lookup(&address);
    (address, name)
}

fn set_recv_timeout(client: SOCKET) {
    let timeout = RECV_TIMEOUT_MS.to_ne_bytes();
    unsafe {
        setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, Some(&timeout));
    }
}

fn format_bdaddr(value: u64) -> String {
    let bytes = value.to_be_bytes();
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn ensure_winsock() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| unsafe {
        let mut data = WSADATA::default();
        let code = WSAStartup(0x0202, &mut data);
        if code != 0 {
            log(format!("WSAStartup 失败：{code}"));
        }
    });
}

fn create_listener(guid: GUID) -> Result<SOCKET, String> {
    let socket = unsafe {
        WSASocketW(
            AF_BTH as i32,
            SOCK_STREAM.0,
            BTHPROTO_RFCOMM as i32,
            None,
            0,
            0,
        )
    }
    .map_err(|error| {
        format!(
            "无法创建蓝牙套接字（蓝牙可能已关闭）：{}（{:#010X}）：请确认蓝牙已开启",
            describe_wsa(error.code().0),
            error.code().0 as u32
        )
    })?;

    let local = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: 0, // 全 0：绑定本机任意一个蓝牙适配器
        serviceClassId: guid,
        port: BT_PORT_ANY, // 交给协议栈分配 RFCOMM 通道号
    };
    let code = unsafe {
        bind(
            socket,
            &local as *const SOCKADDR_BTH as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_BTH>() as i32,
        )
    };
    if code != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        unsafe {
            closesocket(socket);
        }
        return Err(map_bind_error(error.0));
    }
    if unsafe { listen(socket, BACKLOG) } != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        unsafe {
            closesocket(socket);
        }
        return Err(format!(
            "监听蓝牙连接失败：{}（{:#010X}）",
            describe_wsa(error.0),
            error.0 as u32
        ));
    }
    Ok(socket)
}

fn map_bind_error(code: i32) -> String {
    const WSAEAFNOSUPPORT: i32 = 10047;
    const WSAEADDRINUSE: i32 = 10048;
    const WSAETIMEDOUT: i32 = 10060;
    const WSATYPE_NOT_FOUND: i32 = 10109;
    let hint = match code {
        WSAEAFNOSUPPORT => "本机没有可用的蓝牙适配器，或蓝牙驱动未加载",
        WSATYPE_NOT_FOUND => "蓝牙协议栈没有安装 RFCOMM 支持",
        WSAEADDRINUSE => "该蓝牙服务已被占用，请先退出其它同类程序",
        WSAETIMEDOUT => "蓝牙适配器无响应，请检查蓝牙是否开启",
        _ => "请确认蓝牙已开启且手机已与本机配对",
    };
    format!(
        "绑定 RFCOMM 通道失败：{}（{:#010X}）：{hint}",
        describe_wsa(code),
        code as u32
    )
}

/// 把 RFCOMM 通道登记到 SDP（蓝牙命名空间）。
///
/// 返回实际发布的 RFCOMM 通道号（手机连的就是它），方便写进日志。
fn publish_service(guid: GUID, socket: SOCKET) -> Result<u32, String> {
    // 重试路径会反复登记同一个 UUID，系统不会把重复登记合并成一条，所以先删掉旧记录
    // （删不到也无所谓，下面照着最新通道号重新登记）。第一个参数 true 表示“首次登记”，
    // 那时不需要这一步。
    // 先问协议栈实际分配了哪个通道号：SDP 记录要告诉手机连哪个端口，端口 0 会被
    // `WSASetService` 当成“无效参数”拒绝（`SOCKADDR_BTH` 是 packed 结构，读完立刻
    // 按值取出，避免拿未对齐字段的引用）。
    let local = local_endpoint(socket)?;
    let channel = local.port;
    if channel == 0 || channel == BT_PORT_ANY {
        return Err("协议栈没有分配 RFCOMM 通道号，无法登记蓝牙服务记录".into());
    }
    let mut sockaddr = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: local.btAddr,
        serviceClassId: guid,
        port: channel,
    };
    let address = SOCKET_ADDRESS {
        lpSockaddr: &mut sockaddr as *mut SOCKADDR_BTH as *mut SOCKADDR,
        iSockaddrLength: std::mem::size_of::<SOCKADDR_BTH>() as i32,
    };
    let mut csa = CSADDR_INFO {
        LocalAddr: address,
        RemoteAddr: SOCKET_ADDRESS::default(),
        iSocketType: SOCK_STREAM.0,
        iProtocol: BTHPROTO_RFCOMM as i32,
    };

    let mut name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut service_class = guid;
    let query = WSAQUERYSETW {
        dwSize: std::mem::size_of::<WSAQUERYSETW>() as u32,
        lpszServiceInstanceName: windows::core::PWSTR(name.as_mut_ptr()),
        lpServiceClassId: &mut service_class,
        lpVersion: ptr::null_mut(),
        lpszComment: windows::core::PWSTR::null(),
        dwNameSpace: NS_BTH,
        lpNSProviderId: ptr::null_mut(),
        lpszContext: windows::core::PWSTR::null(),
        dwNumberOfProtocols: 0,
        lpafpProtocols: ptr::null_mut(),
        lpszQueryString: windows::core::PWSTR::null(),
        dwNumberOfCsAddrs: 1,
        lpcsaBuffer: &mut csa,
        dwOutputFlags: 0,
        lpBlob: ptr::null_mut(),
    };
    if unsafe { WSASetServiceW(&query, RNRSERVICE_REGISTER, 0) } != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        return Err(format!("登记蓝牙服务记录失败：{}", format_wsa(error.0)));
    }
    Ok(channel)
}

/// 重试路径用的“先删后登记”：删掉这个 UUID 的所有旧记录，再登记当前通道号。
///
/// 系统不会把同一 UUID 的重复登记合并成一条（每次 REGISTER 都追加一条），所以后台
/// 每轮重试如果直接重新登记，SDP 数据库里的记录会越堆越多，手机端搜出多个同名服务。
fn republish_service(guid: GUID, socket: SOCKET) -> Result<u32, String> {
    // 先删旧记录：优先按完整记录删（名字 + 类 UUID + 监听地址），再按名字兜底。
    // 删不到（比如首次部署时压根没有记录）不算失败。
    let _ = delete_service_record(guid, socket);
    let _ = delete_service_record_by_name(guid);
    publish_service(guid, socket)
}

/// 注销 SDP 记录。
///
/// 文档明确写着：蓝牙命名空间下 `RNRSERVICE_DEREGISTER` 是**非法**的（会报错），
/// 停止对外广播只能用 `RNRSERVICE_DELETE`。
///
/// DELETE 还必须带上 `dwNumberOfCsAddrs: 1` + 指向监听套接字 `SOCKADDR_BTH` 的
/// `lp(c)s(a)Buffer`——少了它（或者把 `lpServiceClassId` 置空）系统就找不到要删哪条记录，
/// 会返回 `WSASERVICE_NOT_FOUND`，于是同一 UUID 的记录越登记越多。
fn deregister_service(guid: GUID, socket: SOCKET) {
    // 先按“名字 + 类 UUID + 监听地址”删除，命中完整记录。
    if let Err(error) = delete_service_record(guid, socket) {
        // 套接字已经关掉或拿不到地址时，退回“只给类 UUID 与名字”的删除方式。
        if let Err(second) = delete_service_record_by_name(guid) {
            log(format!(
                "注销蓝牙服务记录失败：{error}；{second}（记录会在蓝牙重启前继续存在）"
            ));
        }
    }
}

/// 读取监听套接字实际绑定的地址与 RFCOMM 通道号。
///
/// `SOCKADDR_BTH` 是 1 字节对齐的 packed 结构，所以这里始终按值返回，绝不把字段引用
/// 泄漏出去（否则 rustc 会报 `E0793`：unaligned reference）。
fn local_endpoint(socket: SOCKET) -> Result<SOCKADDR_BTH, String> {
    let mut local = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: 0,
        serviceClassId: GUID::zeroed(),
        port: BT_PORT_ANY,
    };
    let mut len = std::mem::size_of::<SOCKADDR_BTH>() as i32;
    let got = unsafe {
        getsockname(
            socket,
            &mut local as *mut SOCKADDR_BTH as *mut SOCKADDR,
            &mut len,
        )
    };
    if got != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        return Err(format!(
            "无法读取已绑定的通道号：{}（{:#010X}）",
            describe_wsa(error.0),
            error.0 as u32
        ));
    }
    Ok(local)
}

/// 按“名字 + 类 UUID + 监听地址”删除 SDP 记录，这是 NS_BTH 下 DELETE 的标准写法。
fn delete_service_record(guid: GUID, socket: SOCKET) -> Result<(), String> {
    let local = local_endpoint(socket)?;
    let mut name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut service_class = guid;
    let mut sockaddr = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: local.btAddr,
        serviceClassId: guid,
        port: local.port,
    };
    let address = SOCKET_ADDRESS {
        lpSockaddr: &mut sockaddr as *mut SOCKADDR_BTH as *mut SOCKADDR,
        iSockaddrLength: std::mem::size_of::<SOCKADDR_BTH>() as i32,
    };
    let mut csa = CSADDR_INFO {
        LocalAddr: address,
        RemoteAddr: SOCKET_ADDRESS::default(),
        iSocketType: SOCK_STREAM.0,
        iProtocol: BTHPROTO_RFCOMM as i32,
    };
    let query = WSAQUERYSETW {
        dwSize: std::mem::size_of::<WSAQUERYSETW>() as u32,
        lpszServiceInstanceName: windows::core::PWSTR(name.as_mut_ptr()),
        lpServiceClassId: &mut service_class,
        dwNameSpace: NS_BTH,
        dwNumberOfCsAddrs: 1,
        lpcsaBuffer: &mut csa,
        ..Default::default()
    };
    if unsafe { WSASetServiceW(&query, RNRSERVICE_DELETE, 0) } != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        return Err(format_wsa(error.0));
    }
    Ok(())
}

/// 兜底：只知道类 UUID 时按名字删除。
fn delete_service_record_by_name(guid: GUID) -> Result<(), String> {
    let mut name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut service_class = guid;
    let query = WSAQUERYSETW {
        dwSize: std::mem::size_of::<WSAQUERYSETW>() as u32,
        lpszServiceInstanceName: windows::core::PWSTR(name.as_mut_ptr()),
        lpServiceClassId: &mut service_class,
        dwNameSpace: NS_BTH,
        ..Default::default()
    };
    if unsafe { WSASetServiceW(&query, RNRSERVICE_DELETE, 0) } != 0 {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        return Err(format_wsa(error.0));
    }
    Ok(())
}

/// Winsock 错误码 → 系统错误文本（人话版留在 `map_bind_error` 里）。
///
/// Winsock 的 WSA 错误码直接放进 HRESULT 的低 16 位就能让系统给出对应描述，
/// 拿不到描述时退回十六进制码。`session.rs` 也用它，所以是 `pub(crate)`。
pub fn describe_wsa(code: i32) -> String {
    if code == 0 {
        return String::new();
    }
    let hresult = windows::core::HRESULT(code);
    let text = windows::core::Error::from_hresult(hresult).message();
    if text.trim().is_empty() {
        format!("Winsock 错误 {code}")
    } else {
        text
    }
}

/// 把一个 32 位状态码格式化成 `文本（0x…）`，供日志与界面显示。
pub fn format_wsa(code: i32) -> String {
    let text = describe_wsa(code);
    if text.is_empty() {
        format!("0x{:08X}", code as u32)
    } else {
        format!("{text}（0x{:08X}）", code as u32)
    }
}

fn set_state(shared: &Arc<Mutex<State>>, value: State) {
    if let Ok(mut guard) = shared.lock() {
        *guard = value;
    }
}

fn log(message: String) {
    super::service::log(&format!("[蓝牙] {message}"));
}

/// 解析各种写法的蓝牙地址（`11:22:33:44:55:66`、`112233445566`）。
pub fn normalize_address(raw: &str) -> Option<String> {
    let hex: String = raw.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    Some(
        (0..6)
            .map(|index| hex[index * 2..index * 2 + 2].to_ascii_uppercase())
            .collect::<Vec<_>>()
            .join(":"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_guid_matches_protocol() {
        assert_eq!(service_guid().to_u128(), SERVICE_UUID_BITS);
        assert_eq!(
            format!(
                "{:08X}-{:04X}-{:04X}-{:04X}-{:012X}",
                service_guid().data1,
                service_guid().data2,
                service_guid().data3,
                u16::from_be_bytes([service_guid().data4[0], service_guid().data4[1]]),
                {
                    let d = service_guid().data4;
                    u64::from_be_bytes([0, 0, d[2], d[3], d[4], d[5], d[6], d[7]])
                }
            ),
            crate::protocol::SERVICE_UUID.to_uppercase()
        );
    }

    #[test]
    fn service_uuid_is_a_16_bit_base_uuid() {
        // SDP 只认 16/32 位 UUID：低 96 位必须是蓝牙基址，高 32 位是实际编号。
        const BASE_SUFFIX: u128 = 0x0000_0000_0000_1000_8000_0080_5f9b_34fb;
        assert_eq!(SERVICE_UUID_BITS & BASE_SUFFIX, BASE_SUFFIX);
        assert_eq!(SERVICE_UUID_BITS >> 96, 0x0000_e8b6);
        assert_eq!(SERVICE_UUID_TEXT, crate::protocol::SERVICE_UUID);
    }

    #[test]
    fn normalize_address_accepts_common_forms() {
        assert_eq!(
            normalize_address("11:22:33:44:55:66").as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(
            normalize_address("112233445566").as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(normalize_address("112233"), None);
    }

    #[test]
    fn state_summaries_are_actionable() {
        assert!(State::Ready.summary().starts_with("已发布"));
        let degraded = State::NotPublished {
            error: "登记蓝牙服务记录失败".into(),
        };
        assert!(degraded.summary().contains("已配对手机可能仍可连接"));
        assert!(!degraded.is_ready());
        assert!(State::Unavailable {
            error: "蓝牙已关闭".into()
        }
        .summary()
        .contains("蓝牙不可用"));
    }

    #[test]
    fn bind_errors_carry_a_hint() {
        let text = map_bind_error(10047);
        assert!(text.contains("蓝牙适配器"));
        assert!(text.contains("0x"));
    }
}
