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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::rfcomm;
use crate::protocol::{Session, TextInjector};

/// 单帧最大字节数（与 `protocol::MAX_FRAME_BYTES` 一致，这里单独常量便于测试）。
pub const MAX_LINE_BYTES: usize = crate::protocol::MAX_FRAME_BYTES;
/// 一次 `recv` 请求的字节数。
const READ_CHUNK_BYTES: u32 = 8 * 1024;

/// 会话的传输层：读一块、写一行。
///
/// 抽成 trait 是为了让分帧、去重、回执顺序这些有状态的逻辑只写一遍：生产用
/// [`SocketWire`]（WinSock `recv`/`send`），测试里可以换成内存管道。
pub trait Transport {
    /// 读一块字节；`Ok(None)` 表示对端正常关闭。
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, String>;
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
    pub fn push(&mut self, mut chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        while !chunk.is_empty() {
            match chunk.iter().position(|b| *b == b'\n') {
                Some(index) => {
                    let (head, tail) = chunk.split_at(index);
                    if !self.skipping {
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
                    chunk = &tail[1..];
                }
                None => {
                    if self.skipping {
                        self.discarded_bytes += chunk.len() as u64;
                    } else if self.buf.len() + chunk.len() > MAX_LINE_BYTES {
                        self.discard();
                    } else {
                        self.buf.extend_from_slice(chunk);
                    }
                    chunk = &[];
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

/// 在任意传输上跑一条会话。所有传输共用同一套分帧/去重/回执逻辑。
pub fn serve_on<T: Transport>(
    mut transport: T,
    peer: &str,
    stop: &AtomicBool,
    stats: &SessionStats,
) -> SessionOutcome {
    let started = Instant::now();
    let mut frames = 0u64;
    let mut injected = 0u64;
    let mut failed = 0u64;
    let mut reason = "对端断开".to_string();

    let mut framer = LineFramer::new();
    let mut session = Session::new(WinInjector);

    loop {
        if stop.load(Ordering::SeqCst) {
            reason = "服务停止".into();
            break;
        }
        let chunk = match transport.read_chunk() {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(error) => {
                reason = error;
                break;
            }
        };
        for line in framer.push(&chunk) {
            frames += 1;
            stats.received.fetch_add(1, Ordering::SeqCst);
            match session.accept(&line) {
                Ok((ack, _sent_text)) => {
                    if ack.is_ok() {
                        injected += 1;
                        stats.injected.fetch_add(1, Ordering::SeqCst);
                    } else {
                        failed += 1;
                        stats.failed.fetch_add(1, Ordering::SeqCst);
                    }
                    let payload = match serde_json::to_vec(&ack) {
                        Ok(payload) => payload,
                        Err(error) => {
                            reason = format!("回执序列化失败：{error}");
                            return finish(started, frames, injected, failed, reason, framer, peer);
                        }
                    };
                    if let Err(error) = transport.write_line(payload) {
                        reason = error;
                        return finish(started, frames, injected, failed, reason, framer, peer);
                    }
                }
                Err(error) => {
                    // 坏帧：记录后跳过，不回执（安卓端会因超时保留草稿）。
                    eprintln!("bridge: 丢弃无效帧：{error}");
                }
            }
        }
        if framer.oversized > 0 {
            eprintln!(
                "bridge: 已丢弃 {} 条超长帧（合计 {} 字节，上限 {MAX_LINE_BYTES}）",
                framer.oversized, framer.discarded_bytes
            );
        }
    }
    finish(started, frames, injected, failed, reason, framer, peer)
}

fn finish(
    started: Instant,
    frames: u64,
    injected: u64,
    failed: u64,
    reason: String,
    framer: LineFramer,
    peer: &str,
) -> SessionOutcome {
    let elapsed = started.elapsed();
    eprintln!(
        "bridge: 会话结束（{peer}）：{frames} 帧 / 成功 {injected} / 失败 {failed}，\
         用时 {:.1}s，原因：{reason}{}",
        elapsed.as_secs_f64(),
        if framer.has_partial() {
            "（末尾有半条未完成的帧）"
        } else {
            ""
        }
    );
    SessionOutcome {
        frames,
        injected,
        failed,
        reason,
    }
}

/// 给测试与日志用的“会话超时”：长时间无数据但连接仍在，交由上层决定是否断开。
pub const IDLE_SESSION_LIMIT: Duration = Duration::from_secs(60);

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
    fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
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
            return Ok(Some(buf));
        }
        if got == 0 {
            return Ok(None);
        }
        let error = unsafe { windows::Win32::Networking::WinSock::WSAGetLastError() };
        // 10054 WSAECONNRESET / 10053 WSAECONNABORTED / 10060 WSAETIMEDOUT：
        // 手机侧断开、锁屏或蓝牙关闭都会走到这里，按正常结束处理。
        match error.0 {
            10054 | 10053 | 10060 => Ok(None),
            code => Err(format!("读取蓝牙数据失败：{}", rfcomm::format_wsa(code))),
        }
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
}
