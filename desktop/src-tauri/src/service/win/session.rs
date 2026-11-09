//! 单条 RFCOMM 会话：读 `\n` 分隔的 JSON 行、去重、注入、回执。
//!
//! 与安卓端 `BridgeViewModel` 的行为对齐：
//!
//! - 安卓一次只保留**一条未回执消息**，8 秒超时后重发同一条（同 `id`）——所以
//!   “同一会话里重复 id 不再注入、只重发缓存回执”是必须的，否则一次连接抖动就会
//!   把同一段文字打两遍。
//! - 回执必须**在注入完成之后**写回，且回执写完再读下一帧：安卓端收到回执才把草稿
//!   里的这段前缀删掉，顺序颠倒会让用户看到“文字已发出去但草稿又被塞回来”。
//! - 帧边界是 LF（`0x0A`）；单帧上限 64 KiB、单条文本 16 KiB，超限的帧**整帧丢弃**
//!   并继续读下一帧（不能让一条坏消息把整条连接搞死，也不能把半条消息注入进去）。
//!
//! # 读超时（`Ok(Idle)`）与空闲上限
//!
//! 套接字上设了 5 秒 `SO_RCVTIMEO`，这个超时**不等于**对端断开：手机只是闲着的时候
//! 每秒都会触发一次超时。所以传输层把“超时无数据”单独报成 [`ReadOutcome::Idle`]，
//! 由这里的 [`IDLE_SESSION_LIMIT`] 决定什么时候才算“手机挂起/离开了”。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::rfcomm;
use crate::protocol::{Ack, Session, TextInjector};

/// 单帧最大字节数（与 `protocol::MAX_FRAME_BYTES` 一致，这里单独常量便于测试）。
pub const MAX_LINE_BYTES: usize = crate::protocol::MAX_FRAME_BYTES;
/// 一次 `recv` 请求的字节数。
const READ_CHUNK_BYTES: u32 = 8 * 1024;

/// 一条会话允许“连续无数据”的最长时间。
///
/// 手机端一条消息最多等 8 秒回执，空闲超过这个时长说明手机侧已经走了（RFCOMM 在
/// 蓝牙链路断掉时经常收不到 FIN，只能靠这里兜底），直接结束会话，让界面回到“未连接”。
pub const IDLE_SESSION_LIMIT: Duration = Duration::from_secs(60);
/// 同类日志的最小间隔（坏帧/超长帧/写回执失败，防止被刷屏）。
const LOG_THROTTLE: Duration = Duration::from_secs(5);
/// 日志里截断的错误文本长度。
const LOG_PREVIEW_CHARS: usize = 160;

/// 一次读操作的结果。
///
/// 把“读超时”和“对端关闭”分开，是因为两者语义完全不同：前者只是眼下没数据，
/// 后者才是会话结束。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// 读到了一块数据（长度 > 0）。
    Data(Vec<u8>),
    /// 读超时：连接还在，只是这段时间没有数据。
    Idle,
    /// 对端关闭（`recv` 返回 0）或连接被复位/中止。
    Closed,
}

/// 会话的传输层：读一块、写一行。
///
/// 抽成 trait 是为了让分帧、去重、回执顺序这些有状态的逻辑只写一遍：生产用
/// [`SocketWire`]（WinSock `recv`/`send`），测试里可以换成内存管道。
pub trait Transport {
    /// 读一块字节。
    fn read_chunk(&mut self) -> Result<ReadOutcome, String>;
    /// 写一行（不含 LF，由实现补上）并确认已交给对端。
    fn write_line(&mut self, payload: Vec<u8>) -> Result<(), String>;
}

/// 会话结束原因（日志用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOutcome {
    pub frames: u64,
    pub injected: u64,
    pub failed: u64,
    pub reason: String,
    /// 已经注入成功、但回执没能写出去的条目（数量通常是 0 或 1）。
    ///
    /// 换会话时把这批条目交给下一条会话：手机重发同 `id` 时直接回缓存，
    /// 不再注入第二遍。
    pub pending: Vec<Ack>,
}

impl SessionOutcome {
    /// “一条回执都没写出去”的会话（比如新连接把还没跑起来的会话挤掉）。
    pub fn empty(reason: impl Into<String>) -> Self {
        Self {
            frames: 0,
            injected: 0,
            failed: 0,
            reason: reason.into(),
            pending: Vec::new(),
        }
    }
}

/// 取出“已注入但回执未写出”的条目（跨会话继承用）。
///
/// 这些 id 的文本已经注入过了，必须继承给下一条会话：不继承的话，手机会重发同 `id`，
/// 而新会话的缓存里没有它 → 同一段文字被二次注入。
pub fn pending_acks(outcome: &SessionOutcome) -> Vec<Ack> {
    outcome.pending.clone()
}

/// 会话语义：注入实现（生产是 [`super::injection::inject_text`]）。
pub struct WinInjector;

impl TextInjector for WinInjector {
    fn inject(&mut self, text: &str) -> Result<(), String> {
        super::injection::inject_text(text)
    }
}

/// 按行切分的帧缓冲区。
///
/// 单独抽出来是因为这部分才是有状态的逻辑（分包、超长帧丢弃），可以纯函数测试，
/// 不依赖 WinRT 流对象。
#[derive(Default)]
pub struct LineFramer {
    buf: Vec<u8>,
    /// 当前是否正处于“超长帧丢弃”模式（一直丢弃到下一个 LF）。
    skipping: bool,
    /// 因超长被丢弃的帧数（日志/诊断用）。
    pub oversized: u64,
    /// 因超长被丢弃的字节数。
    pub discarded_bytes: u64,
}

impl LineFramer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，返回其中所有完整的行（不含 LF）。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut cursor = 0;
        while cursor < chunk.len() {
            match chunk[cursor..].iter().position(|b| *b == b'\n') {
                Some(offset) => {
                    let end = cursor + offset;
                    if !self.skipping {
                        let head = &chunk[cursor..end];
                        if self.buf.len() + head.len() > MAX_LINE_BYTES {
                            self.discard();
                        } else {
                            self.buf.extend_from_slice(head);
                            let mut line = std::mem::take(&mut self.buf);
                            // 兼容手机端可能发送的 CRLF。
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            if !line.is_empty() {
                                lines.push(line);
                            }
                        }
                    }
                    self.skipping = false;
                    cursor = end + 1;
                }
                None => {
                    let tail = &chunk[cursor..];
                    if self.skipping {
                        self.discarded_bytes += tail.len() as u64;
                    } else if self.buf.len() + tail.len() > MAX_LINE_BYTES {
                        self.discard();
                    } else {
                        self.buf.extend_from_slice(tail);
                    }
                    cursor = chunk.len();
                }
            }
        }
        lines
    }

    fn discard(&mut self) {
        self.oversized += 1;
        self.discarded_bytes += self.buf.len() as u64;
        self.buf.clear();
        self.skipping = true;
    }

    /// 连接结束时是否还有半条未完成的帧（日志用）。
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty() || self.skipping
    }
}

/// 会话统计（状态接口 `receivedCount` 用它，线程安全）。
#[derive(Default)]
pub struct SessionStats {
    pub received: AtomicU64,
    pub injected: AtomicU64,
    pub failed: AtomicU64,
}

impl SessionStats {
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::SeqCst)
    }
    pub fn injected(&self) -> u64 {
        self.injected.load(Ordering::SeqCst)
    }
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::SeqCst)
    }
}

/// 节流后的日志：同一类消息在 `LOG_THROTTLE` 内只记第一次，并汇总被压掉的条数。
///
/// 手机发来一串坏帧时，一台机器一秒钟能刷出成百上千行日志；这里保证“第一次一定
/// 看得见，后面不淹没其它日志”。
pub struct ThrottledLog {
    last_at: Option<Instant>,
    suppressed: u64,
    window: Duration,
}

impl ThrottledLog {
    pub fn new(window: Duration) -> Self {
        Self {
            last_at: None,
            suppressed: 0,
            window,
        }
    }

    /// 到点就输出（`message()` 只在确实要输出时才调用）。
    pub fn log_if_due(&mut self, message: impl FnOnce() -> String) {
        let now = Instant::now();
        let due = match self.last_at {
            None => true,
            Some(last) => now.duration_since(last) >= self.window,
        };
        if !due {
            self.suppressed += 1;
            return;
        }
        let mut text = message();
        if self.suppressed > 0 {
            text.push_str(&format!("（另有 {} 条同类日志被压缩）", self.suppressed));
            self.suppressed = 0;
        }
        log(text);
        self.last_at = Some(now);
    }
}

/// 一次会话里用到的全部可替换状态，`serve_on` 与 `serve_with` 共用。
struct SessionRun<'a, T> {
    transport: T,
    peer: &'a str,
    stop: &'a AtomicBool,
    /// 本轮会话的取消标志（有新连接接管/服务停止时置位）。
    cancelled: &'a AtomicBool,
    stats: &'a SessionStats,
    framer: LineFramer,
    session: Session<Box<dyn TextInjector + 'a>>,
    frames_log: ThrottledLog,
    oversized_log: ThrottledLog,
    write_log: ThrottledLog,
    started: Instant,
    frames: u64,
    injected: u64,
    failed: u64,
    /// 已经注入成功、回执还没写成功的 `id`（写成功就清空）。
    unacked: Vec<String>,
}

impl<'a, T: Transport> SessionRun<'a, T> {
    fn new(
        transport: T,
        peer: &'a str,
        stop: &'a AtomicBool,
        cancelled: &'a AtomicBool,
        stats: &'a SessionStats,
        injector: Box<dyn TextInjector + 'a>,
    ) -> Self {
        Self {
            transport,
            peer,
            stop,
            cancelled,
            stats,
            framer: LineFramer::new(),
            session: Session::new(injector),
            frames_log: ThrottledLog::new(LOG_THROTTLE),
            oversized_log: ThrottledLog::new(LOG_THROTTLE),
            write_log: ThrottledLog::new(LOG_THROTTLE),
            started: Instant::now(),
            frames: 0,
            injected: 0,
            failed: 0,
            unacked: Vec::new(),
        }
    }

    /// 本轮是否该收工（服务停止，或新连接已经接管）。
    fn halted(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.cancelled.load(Ordering::SeqCst)
    }

    /// 跑协议循环，直到断开、空闲超限或停止；返回结束原因。
    fn run(&mut self) -> String {
        let mut last_data = Instant::now();
        loop {
            if self.halted() {
                return self.halt_reason();
            }
            let chunk = match self.transport.read_chunk() {
                Ok(ReadOutcome::Data(chunk)) => chunk,
                Ok(ReadOutcome::Closed) => {
                    // 断开时可能正好被接管（`shutdown` 会让我们读到 0 字节）。
                    if self.halted() {
                        return self.halt_reason();
                    }
                    return "对端断开".to_string();
                }
                Ok(ReadOutcome::Idle) => {
                    // 只是这一次读超时：连接还在，只要没超过空闲上限就继续等。
                    let idle = last_data.elapsed();
                    if idle >= IDLE_SESSION_LIMIT {
                        return format!("空闲 {} 秒无数据，判定手机挂起或已离开", idle.as_secs());
                    }
                    continue;
                }
                Err(error) => {
                    if self.halted() {
                        return self.halt_reason();
                    }
                    return error;
                }
            };
            // 收到数据（哪怕只是半条帧）就重置空闲计时，否则一条分段到达的长消息
            // 会被误判成空闲。
            last_data = Instant::now();
            for line in self.framer.push(&chunk) {
                if let Some(reason) = self.handle_line(&line) {
                    return reason;
                }
            }
            self.report_oversized();
        }
    }

    fn halt_reason(&self) -> String {
        if self.cancelled.load(Ordering::SeqCst) {
            format!("新的手机连接接管了会话（已运行 {}）", self.elapsed_text())
        } else {
            format!("服务停止（已运行 {}）", self.elapsed_text())
        }
    }

    /// 处理一帧；返回 `Some(原因)` 表示会话必须就此结束（写回执失败）。
    fn handle_line(&mut self, line: &[u8]) -> Option<String> {
        self.frames += 1;
        self.stats.received.fetch_add(1, Ordering::SeqCst);
        // `Session::accept` 在返回前已经完成注入：回执永远在注入之后生成。
        match self.session.accept(line) {
            Ok((ack, new_message)) => {
                if ack.is_ok() {
                    // 只有真的注入过才记“成功”；同 id 重发命中缓存不计入注入次数。
                    if let Some(Some(_)) = new_message {
                        self.injected += 1;
                        self.stats.injected.fetch_add(1, Ordering::SeqCst);
                        // 新注入的一条：回执写成功之前它都是“未回执”。
                        self.unacked.push(ack.id().to_string());
                    }
                } else if new_message.is_some() {
                    // 只有“新消息且注入失败”才算失败；命中缓存的回执（包括缓存里的
                    // ok:false 回执，例如空文本）重发时不再重复计数。
                    self.failed += 1;
                    self.stats.failed.fetch_add(1, Ordering::SeqCst);
                }
                let payload = match serde_json::to_vec(&ack) {
                    Ok(payload) => payload,
                    Err(error) => return Some(format!("回执序列化失败：{error}")),
                };
                if let Err(error) = self.transport.write_line(payload) {
                    // 连接已经不可用；未回执条目留在 `unacked` 里交给下一条会话。
                    self.write_log
                        .log_if_due(|| format!("[蓝牙] 写回执失败：{}", preview(&error)));
                    return Some(error);
                }
                if ack.is_ok() && !self.unacked.is_empty() {
                    // 这一帧的回执写出了，排在它前面的注入也都确认过了。
                    self.unacked.clear();
                }
                None
            }
            Err(error) => {
                // 坏帧：记录后跳过，不回执（安卓端会因超时保留草稿），也不断开会话。
                self.frames_log
                    .log_if_due(|| format!("[蓝牙] 丢弃无效帧：{}", preview(&error)));
                self.report_oversized();
                None
            }
        }
    }

    /// 超长帧只在数字变化时记一次（原来每个循环都重复打印同一句）。
    fn report_oversized(&mut self) {
        if self.framer.oversized == 0 {
            return;
        }
        let oversized = self.framer.oversized;
        let bytes = self.framer.discarded_bytes;
        self.oversized_log.log_if_due(|| {
            format!(
                "[蓝牙] 已丢弃 {oversized} 条超长帧（合计 {bytes} 字节，上限 {MAX_LINE_BYTES} 字节）"
            )
        });
    }

    fn elapsed_text(&self) -> String {
        format!("{:.1}s", self.started.elapsed().as_secs_f64())
    }

    fn finish(mut self, reason: String) -> SessionOutcome {
        self.report_oversized();
        let elapsed = self.elapsed_text();
        let partial = if self.framer.has_partial() {
            "；末尾有半条未完成的帧"
        } else {
            ""
        };
        let pending = self.pending_acks();
        let pending_note = if pending.is_empty() {
            String::new()
        } else {
            format!(
                "；{} 条回执未写出（已注入，保留给下一条连接）",
                pending.len()
            )
        };
        log(format!(
            "[蓝牙] 会话结束（{}）：{} 帧 / 成功 {} / 失败 {}，用时 {elapsed}{partial}{pending_note}；原因：{reason}",
            self.peer, self.frames, self.injected, self.failed
        ));
        SessionOutcome {
            frames: self.frames,
            injected: self.injected,
            failed: self.failed,
            reason,
            pending,
        }
    }

    /// 把“已注入但回执没写出去”的 `id` 还原成缓存回执。
    fn pending_acks(&self) -> Vec<Ack> {
        self.unacked
            .iter()
            .map(|id| Ack::success(id.clone()))
            .collect()
    }
}

/// 在任意传输上跑一条会话。所有传输共用同一套分帧/去重/回执逻辑。
pub fn serve_on<T: Transport>(
    transport: T,
    peer: &str,
    stop: &AtomicBool,
    stats: &SessionStats,
) -> SessionOutcome {
    serve_with(
        transport,
        peer,
        stop,
        &NEVER_HALTED,
        stats,
        Vec::new(),
        Box::new(WinInjector),
    )
}

/// 带预置回执缓存会话（换会话时继承上一条“已注入但未回执”的条目）。
///
/// `cancelled` 在有新连接接管或服务停止时被置位，让本轮在**帧边界**退出，
/// 不把一条消息的注入和回执拆到两个会话里。
///
/// `injector` 为 `Some` 时替换真正的键盘注入——单元测试用它在不碰真实键盘的前提下
/// 验证“回执在注入之后写出”“同 id 不重复注入”。
#[allow(clippy::too_many_arguments)]
pub fn serve_with<T: Transport>(
    transport: T,
    peer: &str,
    stop: &AtomicBool,
    cancelled: &AtomicBool,
    stats: &SessionStats,
    inherited: Vec<Ack>,
    injector: Box<dyn TextInjector + '_>,
) -> SessionOutcome {
    let mut run = SessionRun::new(transport, peer, stop, cancelled, stats, injector);
    for ack in inherited {
        run.session.preload(ack);
    }
    let reason = run.run();
    run.finish(reason)
}

/// `serve_on` 用不到取消标志时的占位（永远不会被置位）。
static NEVER_HALTED: AtomicBool = AtomicBool::new(false);

fn preview(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(LOG_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn log(message: String) {
    super::service::log(&message);
}

/// WinSock 传输：生产路径。`SOCKET` 是 `Send`，所以整个会话可以放进任意线程。
pub struct SocketWire {
    socket: windows::Win32::Networking::WinSock::SOCKET,
}

impl SocketWire {
    pub fn new(socket: windows::Win32::Networking::WinSock::SOCKET) -> Self {
        Self { socket }
    }
}

impl Transport for SocketWire {
    fn read_chunk(&mut self) -> Result<ReadOutcome, String> {
        let mut buf = vec![0_u8; READ_CHUNK_BYTES as usize];
        let got = unsafe {
            windows::Win32::Networking::WinSock::recv(
                self.socket,
                &mut buf,
                windows::Win32::Networking::WinSock::SEND_RECV_FLAGS(0),
            )
        };
        if got > 0 {
            buf.truncate(got as usize);
            return Ok(ReadOutcome::Data(buf));
        }
        if got == 0 {
            return Ok(ReadOutcome::Closed);
        }
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        Ok(match error.0 {
            // 10060 WSAETIMEDOUT：`SO_RCVTIMEO` 到点，连接还在，只是这段时间没数据。
            10060 => ReadOutcome::Idle,
            // 10054 WSAECONNRESET / 10053 WSAECONNABORTED：手机侧断开或被系统中止。
            10054 | 10053 => ReadOutcome::Closed,
            _ => return Err(format!("读取蓝牙数据失败：{}", rfcomm::format_wsa(error.0))),
        })
    }

    fn write_line(&mut self, mut payload: Vec<u8>) -> Result<(), String> {
        payload.push(b'\n');
        let mut remaining = &payload[..];
        while !remaining.is_empty() {
            let sent = unsafe {
                windows::Win32::Networking::WinSock::send(
                    self.socket,
                    remaining,
                    windows::Win32::Networking::WinSock::SEND_RECV_FLAGS(0),
                )
            };
            if sent <= 0 {
                let code = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() }.0;
                if code == 10054 || code == 10053 {
                    return Err("手机已断开连接".into());
                }
                return Err(format!("发送回执失败：{}", rfcomm::format_wsa(code)));
            }
            remaining = &remaining[sent as usize..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn framer_splits_on_lf_and_keeps_partial() {
        let mut framer = LineFramer::new();
        assert!(framer.push(b"{\"a\":1}").is_empty());
        let lines = framer.push(b"\n{\"b\"");
        assert_eq!(lines, vec![b"{\"a\":1}".to_vec()]);
        let lines = framer.push(b":2}\r\n");
        assert_eq!(lines, vec![b"{\"b\":2}".to_vec()]);
        assert!(!framer.has_partial());
    }

    #[test]
    fn framer_handles_multiple_lines_in_one_chunk() {
        let mut framer = LineFramer::new();
        let lines = framer.push(b"one\ntwo\nthree");
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec()]);
        let lines = framer.push(b"\n");
        assert_eq!(lines, vec![b"three".to_vec()]);
    }

    #[test]
    fn framer_discards_oversized_frame_and_resyncs() {
        let mut framer = LineFramer::new();
        let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(framer.push(&oversized).is_empty());
        assert_eq!(framer.oversized, 1);
        // 下一个 LF 结束这个坏帧，之后恢复正常。
        let lines = framer.push(b"\nok\n");
        assert_eq!(lines, vec![b"ok".to_vec()]);
        assert!(!framer.skipping);
    }

    #[test]
    fn framer_skips_bytes_after_oversized_without_trailing_lf_yet() {
        let mut framer = LineFramer::new();
        let oversized = vec![b'y'; MAX_LINE_BYTES + 1];
        assert!(framer.push(&oversized).is_empty());
        assert!(framer.push(b"junk-without-lf").is_empty());
        assert!(framer.has_partial());
        let lines = framer.push(b"\nnext\n");
        assert_eq!(lines, vec![b"next".to_vec()]);
    }

    #[test]
    fn framer_ignores_blank_lines() {
        let mut framer = LineFramer::new();
        assert!(framer.push(b"\n\n\r\n").is_empty());
    }

    #[test]
    fn framer_keeps_frame_of_exactly_the_limit() {
        let mut framer = LineFramer::new();
        let exact = vec![b'x'; MAX_LINE_BYTES];
        let mut chunk = exact.clone();
        chunk.push(b'\n');
        assert_eq!(framer.push(&chunk), vec![exact]);
        assert_eq!(framer.oversized, 0);
    }

    #[test]
    fn empty_frame_is_not_a_message() {
        let mut framer = LineFramer::new();
        assert!(framer.push(b"\n").is_empty());
    }

    #[test]
    fn stats_counters_start_at_zero() {
        let stats = SessionStats::default();
        assert_eq!(stats.received(), 0);
        assert_eq!(stats.injected(), 0);
        assert_eq!(stats.failed(), 0);
    }

    // ---- 以下用假 Transport 跑完整的协议循环（不碰蓝牙/不碰注入） ----

    /// 假传输：喂进来的东西依次当作读结果，写出去的每一行都记下来。
    struct FakeWire {
        reads: VecDeque<ReadOutcome>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        fail_write_with: Option<String>,
        reads_left_after_empty: ReadOutcome,
    }

    impl FakeWire {
        fn new(reads: Vec<ReadOutcome>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                writes: Arc::new(Mutex::new(Vec::new())),
                fail_write_with: None,
                reads_left_after_empty: ReadOutcome::Closed,
            }
        }
    }

    impl Transport for FakeWire {
        fn read_chunk(&mut self) -> Result<ReadOutcome, String> {
            Ok(self
                .reads
                .pop_front()
                .unwrap_or_else(|| self.reads_left_after_empty.clone()))
        }

        fn write_line(&mut self, payload: Vec<u8>) -> Result<(), String> {
            if let Some(error) = &self.fail_write_with {
                return Err(error.clone());
            }
            self.writes.lock().expect("写日志锁").push(payload);
            Ok(())
        }
    }

    fn text_frame(id: &str, text: &str) -> String {
        format!("{{\"type\":\"text\",\"id\":\"{id}\",\"text\":\"{text}\"}}")
    }

    fn halted_flag() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn stop_flag() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// 假注入器：记录注入顺序，可让指定文本注入失败。
    struct RecordingInjector {
        injected: Arc<Mutex<Vec<String>>>,
        fail_on: Option<String>,
    }

    impl RecordingInjector {
        fn new() -> Self {
            Self {
                injected: Arc::new(Mutex::new(Vec::new())),
                fail_on: None,
            }
        }
    }

    impl TextInjector for RecordingInjector {
        fn inject(&mut self, text: &str) -> Result<(), String> {
            if self.fail_on.as_deref() == Some(text) {
                return Err("没有可输入的窗口".into());
            }
            self.injected
                .lock()
                .expect("注入记录锁")
                .push(text.to_string());
            Ok(())
        }
    }

    #[test]
    fn oversized_frame_is_dropped_and_session_resyncs() {
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&vec![b'x'; MAX_LINE_BYTES + 10]);
        oversized.push(b'\n');
        oversized.extend_from_slice(b"{\"type\":\"nope\",\"id\":\"after\",\"text\":\"x\"}\n");
        let wire = FakeWire::new(vec![ReadOutcome::Data(oversized), ReadOutcome::Closed]);
        let writes = wire.writes.clone();
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        // 超长帧不进协议层，只处理了后面那条（坏帧类型 → 不回执）。
        assert_eq!(outcome.frames, 1);
        assert!(writes.lock().expect("写日志锁").is_empty());
        assert_eq!(stats.received(), 1);
        assert_eq!(outcome.reason, "对端断开");
    }

    #[test]
    fn invalid_json_gets_no_ack_and_keeps_session_alive() {
        let payload = format!("not-json-at-all\n{}\n", text_frame("after-bad", ""));
        let wire = FakeWire::new(vec![
            ReadOutcome::Data(payload.into_bytes()),
            ReadOutcome::Closed,
        ]);
        let writes = wire.writes.clone();
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        // 两帧都被协议层看到：第一帧 JSON 错误，第二帧文本为空 → ok:false。
        assert_eq!(outcome.frames, 2);
        let writes = writes.lock().expect("写日志锁");
        assert_eq!(writes.len(), 1, "只有空文本那一帧有回执");
        let ack: serde_json::Value = serde_json::from_slice(&writes[0]).expect("回执是 JSON");
        assert_eq!(ack["id"], "after-bad");
        assert_eq!(ack["ok"], false);
        assert!(ack["error"].as_str().expect("错误文本").contains("16 KiB"));
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let payload = format!("\r\n\r\n{}\r\n\n", text_frame("crlf", ""));
        let wire = FakeWire::new(vec![
            ReadOutcome::Data(payload.into_bytes()),
            ReadOutcome::Closed,
        ]);
        let writes = wire.writes.clone();
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        assert_eq!(outcome.frames, 1, "空行不算帧");
        let writes = writes.lock().expect("写日志锁");
        assert_eq!(writes.len(), 1);
        let ack: serde_json::Value = serde_json::from_slice(&writes[0]).expect("回执是 JSON");
        assert_eq!(ack["id"], "crlf");
    }

    #[test]
    fn duplicate_id_is_answered_from_cache_only() {
        let frame = text_frame("dup", "");
        let payload = format!("{frame}\n{frame}\n{frame}\n");
        let wire = FakeWire::new(vec![
            ReadOutcome::Data(payload.into_bytes()),
            ReadOutcome::Closed,
        ]);
        let writes = wire.writes.clone();
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        assert_eq!(outcome.frames, 3);
        // 三次请求都回了回执；后两次命中缓存，不再计入失败。
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.injected, 0);
        let writes = writes.lock().expect("写日志锁");
        assert_eq!(writes.len(), 3);
        for line in writes.iter() {
            let ack: serde_json::Value = serde_json::from_slice(line).expect("回执是 JSON");
            assert_eq!(ack["id"], "dup");
            assert_eq!(ack["ok"], false);
        }
    }

    /// 把“注入顺序”和“回执写出顺序”记在同一本账上，用来证明回执不会早于注入。
    #[derive(Default, Clone)]
    struct EventLog(Arc<Mutex<Vec<String>>>);

    impl EventLog {
        fn push(&self, event: impl Into<String>) {
            self.0.lock().expect("事件账本锁").push(event.into());
        }

        fn events(&self) -> Vec<String> {
            self.0.lock().expect("事件账本锁").clone()
        }
    }

    fn logging_injector(
        log: &EventLog,
        cancel_on_inject: Option<Arc<AtomicBool>>,
    ) -> Box<dyn TextInjector> {
        struct LogInjector {
            log: EventLog,
            cancel: Option<Arc<AtomicBool>>,
        }
        impl TextInjector for LogInjector {
            fn inject(&mut self, text: &str) -> Result<(), String> {
                self.log.push(format!("inject:{text}"));
                if let Some(cancel) = &self.cancel {
                    // 注入完成的同时收到新连接：取消必须等这一帧的回执写完才生效。
                    cancel.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
        }
        Box::new(LogInjector {
            log: log.clone(),
            cancel: cancel_on_inject,
        })
    }

    fn logging_wire(events: &[ReadOutcome], log: &EventLog) -> ScriptedWire {
        ScriptedWire {
            events: events.to_vec().into(),
            writes: Vec::new(),
            fail_write_with: None,
            log: log.clone(),
        }
    }

    /// 既能“写一行”，又会在写回执时记下顺序的假传输。
    struct ScriptedWire {
        events: VecDeque<ReadOutcome>,
        writes: Vec<Vec<u8>>,
        fail_write_with: Option<String>,
        log: EventLog,
    }

    impl Transport for ScriptedWire {
        fn read_chunk(&mut self) -> Result<ReadOutcome, String> {
            Ok(self.events.pop_front().unwrap_or(ReadOutcome::Closed))
        }

        fn write_line(&mut self, payload: Vec<u8>) -> Result<(), String> {
            if let Some(error) = &self.fail_write_with {
                return Err(error.clone());
            }
            let ack: serde_json::Value = serde_json::from_slice(&payload).expect("回执是 JSON");
            self.log
                .push(format!("ack:{}", ack["id"].as_str().unwrap_or("?")));
            self.writes.push(payload);
            Ok(())
        }
    }

    /// 注入成功、文本非空、但回执写失败：必须在 `pending` 里留下可继承的回执。
    #[test]
    fn injected_but_unacked_ids_are_handed_over() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let log = EventLog::default();
        let wire = ScriptedWire {
            events: VecDeque::from(vec![ReadOutcome::Data(
                format!("{}\n", text_frame("m1", "hello")).into_bytes(),
            )]),
            writes: Vec::new(),
            fail_write_with: Some("手机已断开连接".into()),
            log: log.clone(),
        };
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            logging_injector(&log, None),
        );
        assert_eq!(outcome.reason, "手机已断开连接");
        assert_eq!(outcome.injected, 1);
        let pending = pending_acks(&outcome);
        assert_eq!(pending.len(), 1, "已注入未回执的条目必须留下来");
        assert_eq!(pending[0].id(), "m1");
        assert!(pending[0].is_ok());
        assert!(log.events().iter().any(|event| event == "inject:hello"));
    }

    /// 回执写成功之后 `pending` 清空（不要无限继承旧记录）。
    #[test]
    fn pending_is_cleared_after_the_ack_is_written() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let log = EventLog::default();
        let wire = logging_wire(
            &[ReadOutcome::Data(
                format!("{}\n", text_frame("m2", "world")).into_bytes(),
            )],
            &log,
        );
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            logging_injector(&log, None),
        );
        assert_eq!(outcome.injected, 1);
        assert!(pending_acks(&outcome).is_empty());
        assert_eq!(log.events(), vec!["inject:world", "ack:m2"]);
    }

    /// 继承来的回执：同 id 重发只回缓存，不再注入一次。
    #[test]
    fn inherited_ack_prevents_second_injection() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let log = EventLog::default();
        let wire = logging_wire(
            &[ReadOutcome::Data(
                format!("{}\n", text_frame("m3", "again")).into_bytes(),
            )],
            &log,
        );
        let inherited = vec![Ack::success("m3".to_string())];
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            inherited,
            logging_injector(&log, None),
        );
        assert_eq!(outcome.frames, 1);
        assert_eq!(outcome.injected, 0, "继承的回执不能再注入一次");
        assert_eq!(log.events(), vec!["ack:m3"]);
    }

    /// 注入进行中被新连接接管：这一帧的回执仍然先写完，然后才退出。
    #[test]
    fn handover_waits_for_the_current_ack() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = halted_flag();
        let stats = SessionStats::default();
        let log = EventLog::default();
        let wire = logging_wire(
            &[ReadOutcome::Data(
                format!("{}\n", text_frame("m4", "queued")).into_bytes(),
            )],
            &log,
        );
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            logging_injector(&log, Some(cancel.clone())),
        );
        assert_eq!(
            log.events(),
            vec!["inject:queued", "ack:m4"],
            "取消在帧边界生效：回执必须写在注入之后、退出之前"
        );
        assert!(outcome.reason.starts_with("新的手机连接接管了会话"));
        assert!(pending_acks(&outcome).is_empty());
    }

    /// 注入失败要如实写 `ok:false` + 中文原因，并且不算“已注入”。
    #[test]
    fn injection_failure_returns_honest_negative_ack() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let mut injector = RecordingInjector::new();
        injector.fail_on = Some("boom".into());
        let wire = FakeWire::new(vec![ReadOutcome::Data(
            format!("{}\n", text_frame("m5", "boom")).into_bytes(),
        )]);
        let writes = wire.writes.clone();
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            Box::new(injector),
        );
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.injected, 0);
        assert_eq!(outcome.reason, "对端断开");
        let writes = writes.lock().expect("写日志锁");
        let ack: serde_json::Value = serde_json::from_slice(&writes[0]).expect("回执是 JSON");
        assert_eq!(ack["ok"], false);
        assert_eq!(ack["error"], "没有可输入的窗口");
    }

    /// 注入器真的被调用（`serve_on` 之外的生产路径）——用录制注入器锁定“一定注入了”。
    #[test]
    fn successful_frame_injects_text_once() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let injector = RecordingInjector::new();
        let recorded = injector.injected.clone();
        let wire = FakeWire::new(vec![ReadOutcome::Data(
            format!("{}\n", text_frame("m6", "你好")).into_bytes(),
        )]);
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            Box::new(injector),
        );
        assert_eq!(outcome.injected, 1);
        assert_eq!(
            recorded.lock().expect("注入记录锁").as_slice(),
            ["你好".to_string()]
        );
    }

    /// 已经成功处理的帧在断开后重连（同一会话内）不重复注入。
    #[test]
    fn duplicate_after_ack_reuses_cache() {
        let frame = text_frame("m7", "once");
        let payload = format!("{frame}\n{frame}\n");
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let injector = RecordingInjector::new();
        let recorded = injector.injected.clone();
        let wire = FakeWire::new(vec![ReadOutcome::Data(payload.into_bytes())]);
        let writes = wire.writes.clone();
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            Box::new(injector),
        );
        assert_eq!(outcome.frames, 2);
        assert_eq!(outcome.injected, 1);
        assert_eq!(recorded.lock().expect("注入记录锁").len(), 1);
        assert_eq!(writes.lock().expect("写日志锁").len(), 2);
    }

    /// 连接空闲超时（`ReadOutcome::Idle` 连续出现）最终以“挂起”收尾，
    /// 但少量空闲不影响后续帧。
    #[test]
    fn idle_limit_is_not_reached_by_short_gaps() {
        let cancel = halted_flag();
        let stop = halted_flag();
        let stats = SessionStats::default();
        let injector = RecordingInjector::new();
        let recorded = injector.injected.clone();
        let wire = FakeWire::new(vec![
            ReadOutcome::Idle,
            ReadOutcome::Idle,
            ReadOutcome::Data(format!("{}\n", text_frame("m8", "after-idle")).into_bytes()),
        ]);
        let outcome = serve_with(
            wire,
            "测试手机",
            &stop,
            &cancel,
            &stats,
            Vec::new(),
            Box::new(injector),
        );
        assert_eq!(outcome.frames, 1);
        assert_eq!(
            recorded.lock().expect("注入记录锁").as_slice(),
            ["after-idle".to_string()]
        );
    }

    #[test]
    fn write_failure_ends_the_session_with_reason() {
        let mut wire = FakeWire::new(vec![
            ReadOutcome::Data(format!("{}\n", text_frame("x", "")).into_bytes()),
            // 如果写失败后还继续读，这里会被消费掉——用它来证明“写完才读下一帧”。
            ReadOutcome::Data(format!("{}\n", text_frame("y", "")).into_bytes()),
        ]);
        wire.fail_write_with = Some("手机已断开连接".into());
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        assert_eq!(outcome.reason, "手机已断开连接");
        assert_eq!(outcome.frames, 1, "写失败后不再读下一帧");
    }

    #[test]
    fn idle_reads_keep_the_session_until_limit() {
        // 连续两次“读超时”不该结束会话，之后收到数据仍然正常处理。
        let wire = FakeWire::new(vec![
            ReadOutcome::Idle,
            ReadOutcome::Idle,
            ReadOutcome::Data(format!("{}\n", text_frame("after-idle", "")).into_bytes()),
            ReadOutcome::Closed,
        ]);
        let writes = wire.writes.clone();
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop_flag(), &stats);
        assert_eq!(outcome.reason, "对端断开");
        assert_eq!(outcome.frames, 1);
        assert_eq!(writes.lock().expect("写日志锁").len(), 1);
    }

    #[test]
    fn idle_reason_text_mentions_hang_up_detection() {
        let text = format!("空闲 {} 秒无数据", IDLE_SESSION_LIMIT.as_secs());
        assert!(text.contains("空闲 60 秒无数据"));
    }

    #[test]
    fn stop_flag_ends_the_session() {
        let stop = AtomicBool::new(true);
        let wire = FakeWire::new(Vec::new());
        let stats = SessionStats::default();
        let outcome = serve_on(wire, "测试手机", &stop, &stats);
        assert!(outcome.reason.starts_with("服务停止"));
        assert_eq!(outcome.frames, 0);
    }
}
