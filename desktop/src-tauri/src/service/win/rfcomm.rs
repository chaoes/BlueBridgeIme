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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH};
use windows::Win32::Networking::WinSock::{
    accept, bind, closesocket, getpeername, getsockname, listen, select, setsockopt, shutdown,
    WSASetServiceW, WSASocketW, WSAStartup, CSADDR_INFO, FD_SET, RNRSERVICE_DELETE,
    RNRSERVICE_REGISTER, SD_BOTH, SOCKADDR, SOCKET, SOCKET_ADDRESS, SOCKET_ERROR, SOCK_STREAM,
    SOL_SOCKET, SO_RCVTIMEO, TIMEVAL, WSADATA, WSAQUERYSETW,
};

use super::session::{self, SessionStats, SocketWire};
use crate::protocol::Ack;

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
/// 换会话时等旧线程写完最后一条回执的宽限（安卓端等 ack 超时是 8 秒，这里只给
/// 已经读完的那一帧留一点时间，不拖住新手机）。
const HANDOVER_GRACE: Duration = Duration::from_millis(200);
/// `stop()` 等接受线程/会话线程收尾的上限：超过就放弃 join（线程自己会在下一次
/// 读超时里看到停止标志退出），绝不让界面卡死。
pub const STOP_JOIN_DEADLINE: Duration = Duration::from_secs(3);
/// 换会话时继承给新会话的缓存上限：[`session::ACK_CACHE_CAP`] 的一小部分即可——
/// 正常情况下是 0~1 条，这里兜住“旧会话写回执全失败”的极端情况。
const PENDING_HANDOVER_CAP: usize = 8;
/// 接受线程单次等待连接的上限：醒来就复查停止标志，保证 `stop()` 不被卡住。
const ACCEPT_SLICE: Duration = Duration::from_millis(200);
/// 起新会话前等旧会话通知完“已连接/已断开”的上限（旧会话通常已在清理尾段）。
const SESSION_NOTIFY_WAIT: Duration = Duration::from_millis(500);

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
    /// 当前活动会话的句柄与它的结束信号（同一时间只允许一条会话）。
    session: Arc<SessionSlot>,
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

    /// 停止服务：结束接受线程、结束当前会话、注销 SDP、关闭监听套接字。可重复调用。
    ///
    /// 流程刻意不依赖 `join` 的阻塞语义：
    ///
    /// 1. 置停止标志，让接受线程和会话线程在下一个循环边界自己退出；
    /// 2. `shutdown(SD_BOTH)` 打开卡在 `accept()` / `recv()` 里的系统调用——不做这一步，
    ///    手机一直不重连时 `accept()` 永远不返回，`stop()` 就会永久挂死（接受线程另有
    ///    [`ACCEPT_SLICE`] 的等待上限兜底）；
    /// 3. `closesocket`，然后带 deadline 地 `join`——**join 必须在注销前**：注销完立刻
    ///    重新 `start()`（界面连点开关）时，旧接受线程如果还活着会和新线程一起
    ///    `accept()` 同一个 UUID，导致两次 `accapt` 抢连接、会话与 SDP 记录错乱；
    /// 4. 注销 SDP（必须在 closesocket 之前，DELETE 还要用监听地址取通道号）。
    pub fn stop(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return; // 已经停过：句柄只允许关一次，避免误关新分配的套接字
        }
        // 让阻塞中的 accept()/recv() 立刻返回。shutdown 不释放句柄，所以这里不会
        // 出现“同一句柄关两次”。
        unsafe {
            let _ = shutdown(self.listener, SD_BOTH);
        }
        self.session.interrupt_all();

        // 先等接受线程退出（它最多卡 ACCEPT_SLICE，通常几毫秒）。
        let exited = if let Some(handle) = self.accept_thread.take() {
            join_before(handle, STOP_JOIN_DEADLINE)
        } else {
            true
        };
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
        if !exited {
            log(format!(
                "接受线程未在 {}ms 内退出（已在后台退出，不会影响下次启动）",
                STOP_JOIN_DEADLINE.as_millis()
            ));
        }
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
    let session = Arc::new(SessionSlot::default());
    let accept_thread = spawn_accept_loop(
        listener,
        stop.clone(),
        state.clone(),
        guid,
        observer,
        session.clone(),
    );
    Ok(Server {
        listener,
        stop,
        state,
        accept_thread: Some(accept_thread),
        session,
    })
}

/// 同一时间只允许一条会话。
///
/// 手机侧一次只保留一条未回执消息、而且经常“断开-重连”重发同一条，若每个连接都
/// 常驻一个线程，反复重连就会把线程数堆上去（旧线程还卡在 `recv` 里）。这里让新
/// 连接直接接管：先让旧会话停在帧边界退出（最多等 [`HANDOVER_GRACE`]），再开新会话。
///
/// 线程数的上界因此是“1 条会话 + 若干还没退干净的旧会话”，而旧会话最晚在
/// [`RECV_TIMEOUT_MS`] 之后看到停止标志退出。
#[derive(Default)]
struct SessionSlot {
    active: Mutex<Option<SessionHandle>>,
    /// 正在运行/收尾的会话线程数（诊断与日志用）。
    live: AtomicU64,
    /// 跨会话继承的“已注入但回执未写出”的缓存（见 [`PENDING_HANDOVER_CAP`]）。
    handover: Mutex<Vec<Ack>>,
}

struct SessionHandle {
    socket: SOCKET,
    /// 本轮会话的取消标志：置位后会话在下一个循环边界退出（不等下一次读超时）。
    cancelled: Arc<AtomicBool>,
    /// 会话线程已经通知过上层“已连接”（或最终没能通知）。
    notified: Arc<AtomicBool>,
    /// 旧会话线程写完最后一条回执后置位。
    done: Arc<(Mutex<bool>, Condvar)>,
    _thread: std::thread::JoinHandle<()>,
}

impl SessionHandle {
    /// 等会话线程把最后一条回执写完（带宽限，不无限等）。
    fn wait_done(&self, limit: Duration) {
        if let Ok(guard) = self.done.0.lock() {
            let outcome = self.done.1.wait_timeout_while(guard, limit, |done| !*done);
            drop(outcome);
        }
    }

    /// 等会话线程通知完上层“已连接”（或确定不会通知了，带宽限）。
    ///
    /// 新会话必须在旧会话的 `observer.disconnected()` 之后才 `connected()`，否则界面
    /// 会先显示“已连接”，随后被旧会话的“断开”覆盖，`active_sessions` 也会错位。
    fn wait_notified(&self, limit: Duration) {
        wait_flag(&self.notified, limit);
    }
}

/// 自旋等一个标志位（带宽限）。用于跨会话的“通知已送达”握手，线程很快会置位。
fn wait_flag(flag: &AtomicBool, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !flag.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl SessionSlot {
    /// 让当前会话停在帧边界退出，并等它收尾（带宽限），返回它的句柄。
    ///
    /// 只等这么一会儿：旧会话也许还没走到 `observer.connected()`（这段时间里它在查
    /// 对端设备名，可能耗时几十毫秒），超时就不再等——调用方在起新会话前会等
    /// [`notified`](Self::wait_notified)，保证界面上“断开 → 连接”的顺序不乱。
    fn take_active(&self, grace: Duration) -> Option<SessionHandle> {
        let handle = match self.active.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        }?;
        handle.cancelled.store(true, Ordering::SeqCst);
        unsafe {
            // 打开阻塞中的 recv()：旧会话立刻看到停止，把已经读完的那一帧的回执写完。
            let _ = shutdown(handle.socket, SD_BOTH);
        }
        handle.wait_done(grace);
        Some(handle)
    }

    /// 服务停止时打断所有会话（含已经在收尾的）。
    fn interrupt_all(&self) {
        let handle = match self.active.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        if let Some(handle) = handle {
            handle.cancelled.store(true, Ordering::SeqCst);
            unsafe {
                let _ = shutdown(handle.socket, SD_BOTH);
            }
        }
    }

    /// 取出继承给下一条会话的回执缓存。
    fn take_handover(&self) -> Vec<Ack> {
        match self.handover.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(_) => Vec::new(),
        }
    }

    /// 旧会话结束前登记“已注入但回执没写出去”的条目，交给下一条会话。
    fn push_handover(&self, pending: Vec<Ack>) {
        if pending.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.handover.lock() {
            for ack in pending {
                if guard.len() >= PENDING_HANDOVER_CAP {
                    guard.remove(0); // 最旧的先淘汰：比它更早的消息不会再被重发
                }
                guard.push(ack);
            }
        }
    }
}

/// 带 deadline 的 join：返回 true 表示线程确实退出了，false 表示超时放手。
fn join_before(handle: std::thread::JoinHandle<()>, deadline: Duration) -> bool {
    let limit = Instant::now() + deadline;
    while !handle.is_finished() {
        if Instant::now() >= limit {
            // 线程会在下一次 ACCEPT_SLICE / 读超时看到停止标志退出（套接字已
            // shutdown），这里不再等它。
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
    true
}

fn spawn_accept_loop(
    listener: SOCKET,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    guid: GUID,
    observer: Arc<dyn SessionObserver>,
    slot: Arc<SessionSlot>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("bridge-accept".into())
        .spawn(move || {
            let stats = Arc::new(SessionStats::default());
            let mut backoff = REGISTER_BACKOFF_MIN;
            let mut next_register = Instant::now();
            let mut last_accept_error: Option<String> = None;
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
                        // 同一时间只允许一条会话：新连接先接管（旧会话停在帧边界）。
                        let previous = slot.take_active(HANDOVER_GRACE);
                        if let Some(previous) = previous {
                            log(format!(
                                "新的手机连接已建立：结束上一条会话（等它写完最后一条回执，最多 {}ms）",
                                HANDOVER_GRACE.as_millis()
                            ));
                            previous.wait_done(HANDOVER_GRACE);
                            // 旧会话可能还没通知上层“已连接/已断开”：等它通知完再起
                            // 新会话，界面上不会出现“已连接”被随后覆盖的乱序。
                            previous.wait_notified(SESSION_NOTIFY_WAIT);
                        }
                        // 旧会话线程在结束前把“已注入但回执未写出”的条目留在槽里，
                        // 新会话必须继承它们，否则手机会重发同 id 被二次注入。
                        let inherited = slot.take_handover();
                        spawn_session(client, &stop, &stats, &observer, &slot, inherited);
                    }
                    Ok(None) => {} // 一次等待超时：回到循环顶部复查停止标志
                    Err(error) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        log_once(&mut last_accept_error, error);
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            log("接受连接的循环已退出".into());
        })
        .expect("创建 bridge-accept 线程")
}

/// 收一个连接；等待超过 [`ACCEPT_SLICE`] 就返回 `Ok(None)`（接受线程复查停止标志）。
///
/// 两个层面的唤醒同时存在：select 超时（可靠，不依赖驱动）和 `stop()` 里的
/// `shutdown`（立刻返回）。不管哪个先到，这里最终都会在 [`ACCEPT_SLICE`] 内交回控制权
/// ——`stop()` 用 `shutdown(SD_BOTH)` 唤醒 `accept()` 在蓝牙套接字上并不可靠（实测
/// `shutdown` 之后 `accept` 仍可能继续阻塞），所以这里不能只靠它。select 是只读操作，
/// 不改变套接字状态，也不会误关句柄，与“句柄只能关一次”的约定不冲突。
fn accept_peer(listener: SOCKET) -> Result<Option<SOCKET>, String> {
    let mut set = FD_SET {
        fd_count: 1,
        ..Default::default()
    };
    set.fd_array[0] = listener;
    let timeout = TIMEVAL {
        tv_sec: 0,
        tv_usec: ACCEPT_SLICE.as_micros() as i32,
    };
    let ready = unsafe { select(0, Some(&mut set), None, None, Some(&timeout)) };
    if ready == SOCKET_ERROR {
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        return Err(format!(
            "等待蓝牙连接出错：{}（{:#010X}）",
            describe_wsa(error.0),
            error.0 as u32
        ));
    }
    if ready == 0 || set.fd_count == 0 {
        return Ok(None); // 这段时间没有新连接：回到循环顶部复查停止标志
    }
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

/// 起一条会话线程（新连接已经接管了 `slot`）。
fn spawn_session(
    client: SOCKET,
    stop: &Arc<AtomicBool>,
    stats: &Arc<SessionStats>,
    observer: &Arc<dyn SessionObserver>,
    slot: &Arc<SessionSlot>,
    inherited: Vec<Ack>,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let notified = Arc::new(AtomicBool::new(false));
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let session_stop = stop.clone();
    let session_cancel = cancelled.clone();
    let session_notified = notified.clone();
    let session_done = done.clone();
    let session_observer = Arc::clone(observer);
    let session_stats = Arc::clone(stats);
    let session_slot = Arc::clone(slot);
    let handle_cancelled = cancelled;
    let handle_notified = notified;
    let spawn_slot = Arc::clone(slot);
    slot.live.fetch_add(1, Ordering::SeqCst);

    let builder = std::thread::Builder::new().name("bridge-session".into());
    let spawned = builder.spawn(move || {
        handle_client(
            client,
            &session_stop,
            &session_cancel,
            &session_stats,
            session_observer.as_ref(),
            &session_slot,
            inherited,
            &session_notified,
            &session_done,
        );
        session_slot.live.fetch_sub(1, Ordering::SeqCst);
        // 线程自己的收尾已经做完（含 `observer.disconnected()`），这里只是让还在
        // 等“通知已送达”的接管方立刻放行。
        session_notified.store(true, Ordering::SeqCst);
    });
    match spawned {
        Ok(thread) => {
            let slot_handle = SessionHandle {
                socket: client,
                cancelled: handle_cancelled,
                notified: handle_notified,
                done,
                _thread: thread,
            };
            if let Ok(mut guard) = slot.active.lock() {
                *guard = Some(slot_handle);
            }
        }
        Err(error) => {
            spawn_slot.live.fetch_sub(1, Ordering::SeqCst);
            log(format!("无法创建会话线程：{error}"));
            unsafe {
                closesocket(client);
            }
            // 这里没有配对调用过 `observer.connected()`：线程都没起来，界面从未显示
            // “已连接”。再报一次 disconnected 会把上一条（正常的）会话的
            // `active_sessions` 计数和面板状态一起改掉，所以只记录日志。
        }
    }
}

/// 跑一条会话：读对端身份 → 通知上层 → 协议循环 → 关闭套接字 → 通知断开。
#[allow(clippy::too_many_arguments)]
fn handle_client(
    client: SOCKET,
    stop: &Arc<AtomicBool>,
    cancelled: &Arc<AtomicBool>,
    stats: &Arc<SessionStats>,
    observer: &dyn SessionObserver,
    slot: &Arc<SessionSlot>,
    inherited: Vec<Ack>,
    notified: &Arc<AtomicBool>,
    done: &Arc<(Mutex<bool>, Condvar)>,
) {
    set_recv_timeout(client);
    let (address, device_name) = peer_identity(client);
    let peer = match &device_name {
        Some(name) => format!("{name}（{address}）"),
        None => address.clone(),
    };
    if !inherited.is_empty() {
        log(format!(
            "继承上一条会话的 {} 条未回执记录（同 id 重发不会再注入）",
            inherited.len()
        ));
    }
    log(format!("手机已连接：{peer}"));
    observer.connected(device_name, Some(address));
    notified.store(true, Ordering::SeqCst);

    // 不管会话怎么结束（含注入代码 panic），都保证：关套接字、通知上层断开、放出
    // 结束信号——否则接管的新会话会一直等到 deadline，状态面板也会停在“已连接”。
    let guard = SessionTail {
        socket: client,
        observer,
        done,
    };
    let wire = SocketWire::new(client);
    // 继承的“已注入但未回执”条目先装进协议层，再开始读帧：否则手机重发同 id
    // 会被再注入一次（同一段文字打两遍）。
    let outcome = session::serve_with(
        wire,
        &peer,
        stop,
        cancelled,
        stats,
        inherited,
        Box::new(session::WinInjector),
    );
    drop(guard);

    log(format!(
        "会话结束（{peer}）：{} 帧 / 成功 {} / 失败 {}（{}）",
        outcome.frames, outcome.injected, outcome.failed, outcome.reason
    ));
    // “已注入但回执没写出去”的条目留给下一条会话：手机重发同 id 时直接回缓存，
    // 不再注入一次（换会话时唯一会重复注入的窗口就在这里）。
    let pending = session::pending_acks(&outcome);
    slot.push_handover(pending);
}

/// 会话收尾的兜底：`handle_client` 无论正常返回还是 panic，都要关套接字并通知断开。
struct SessionTail<'a> {
    socket: SOCKET,
    observer: &'a dyn SessionObserver,
    done: &'a Arc<(Mutex<bool>, Condvar)>,
}

impl Drop for SessionTail<'_> {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.socket);
        }
        self.observer.disconnected();
        if let Ok(mut guard) = self.done.0.lock() {
            *guard = true;
            self.done.1.notify_all();
        }
    }
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

/// 同一个错误只在“文本变了”时写一次日志。
///
/// 蓝牙被关掉、适配器被拔掉之后 `accept` 会每 200ms 报一次同样的错；只记首次能让
/// 日志保持可读（状态面板另有 `last_error`，不会丢信息）。
fn log_once(last: &mut Option<String>, message: String) {
    if last.as_deref() == Some(message.as_str()) {
        return;
    }
    log(format!("{message}（同类错误只记录一次）"));
    *last = Some(message);
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
