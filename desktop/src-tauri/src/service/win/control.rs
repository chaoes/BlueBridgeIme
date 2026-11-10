//! 控制通道：Windows 命名管道上的 JSON 行协议（对应 `docs/service-api.md`）。
//!
//! ## 为什么是命名管道而不是 TCP 回环
//!
//! `docs/windows.md` 给了两个选项。命名管道更合适：
//!
//! - **天然按用户隔离**：管道名在 `\\.\pipe\` 全局命名空间里，但通过
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` + 默认安全描述符，只有当前用户（以及管理员）
//!   能连，不需要另做鉴权，也不占端口、不触发防火墙；
//! - **实例模型正好对应“多个界面/多个请求”**：每个连接一个管道实例，协议是
//!   “一行请求 → 一行应答”，与 Linux 端的 Unix socket 完全同构，界面侧
//!   `client` 代码几乎只是把 `UnixStream` 换成管道句柄；
//! - 连接断开能被立刻感知（管道断开即句柄读返回 0），`subscribe` 长连接同理。
//!
//! ## 传输细节
//!
//! - 消息字节流用**换行**分隔，UTF-8；与蓝牙侧帧格式一致，便于复用解析思路。
//! - `subscribe` 是长连接：先回一行 `{"ok":true,"status":{...}}`，之后每次状态变化
//!   推一行 `{"event":{...}}`，客户端关闭连接即可退订。
//! - 请求相互独立：每个连接一个任务，不做全局串行（`dispatch` 内部只锁状态）。

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};

use super::service::{self, Inner};

/// 控制通道管道名（与 `desktop/src-tauri/src/service/client.rs` 中的常量一致）。
pub const PIPE_NAME: &str = r"\\.\pipe\bluetooth-input-bridge-control";
/// 单行请求的最大长度（控制命令很短，64 KiB 足够且防止内存被灌爆）。
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// 创建一个管道实例（首个实例带 `first_pipe_instance`，用于单实例判定）。
fn create_instance(first: bool, inner: &Arc<Inner>) -> std::io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .pipe_mode(PipeMode::Byte)
        .access_inbound(true)
        .access_outbound(true)
        .max_instances(16)
        .first_pipe_instance(first);
    let server = options.create(PIPE_NAME)?;
    // 状态里的 serviceRunning 只在第一个实例时置位。
    if first {
        inner.update(|s| s.service_running = true);
    }
    Ok(server)
}

/// 接受连接的主循环（`stop` 置位后退出）。
pub async fn serve(inner: Arc<Inner>) -> Result<(), String> {
    let mut first = true;
    let mut last_logged = String::new();
    loop {
        if inner.stop.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let server = match create_instance(first, &inner) {
            Ok(server) => server,
            Err(error) => {
                if first {
                    return Err(format!("无法创建控制管道 {PIPE_NAME}：{error}"));
                }
                // 界面每秒轮询状态，每次都是一条新连接；同一类错误只记一次。
                log_once(&mut last_logged, format!("创建管道实例失败：{error}"));
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
        };
        first = false;
        if let Err(error) = server.connect().await {
            log_once(&mut last_logged, format!("等待控制通道连接失败：{error}"));
            continue;
        }
        // 每次 `status` 都是一条新连接：逐条记录会把日志刷满，这里只记“已就绪”。
        log_once(
            &mut last_logged,
            "控制通道已就绪（界面可以连了）".to_string(),
        );
        let task_inner = inner.clone();
        tokio::spawn(async move {
            handle_connection(server, task_inner).await;
        });
    }
    Ok(())
}

/// 同类日志只写一次（文本变了才写），避免每条管道连接/每轮重试都刷一行。
fn log_once(last: &mut String, message: String) {
    if last == &message {
        return;
    }
    service::log(&message);
    *last = message;
}

/// 处理一个连接：一次 `status` 之类的短请求，或一条 `subscribe` 长连接。
async fn handle_connection(server: NamedPipeServer, inner: Arc<Inner>) {
    let (reader, mut writer) = tokio::io::split(server);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        // 只读到 MAX_REQUEST_BYTES 为止：客户端一直不发换行时也不能让内存无上限增长。
        let read = {
            let mut limited = (&mut reader).take(MAX_REQUEST_BYTES as u64);
            limited.read_line(&mut line).await
        };
        match read {
            Ok(0) => break, // 客户端关闭
            Ok(_) => {}
            Err(error) => {
                service::log(&format!("读取控制请求失败：{error}"));
                break;
            }
        }
        if line.len() >= MAX_REQUEST_BYTES && !line.ends_with('\n') {
            // 超过上限还没读到行尾：回一条明确的错误再断开，后续字节不再读。
            let _ = write_json(&mut writer, &json!({ "ok": false, "error": "请求过长" })).await;
            break;
        }
        let request = line.trim();
        if request.is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(request) {
            Ok(value) => value,
            Err(_) => {
                let _ = write_json(
                    &mut writer,
                    &json!({ "ok": false, "error": "请求格式错误" }),
                )
                .await;
                continue;
            }
        };
        let command = parsed
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if command == "subscribe" {
            if write_json(
                &mut writer,
                &json!({ "ok": true, "status": inner.snapshot() }),
            )
            .await
            .is_err()
            {
                break;
            }
            subscribe_loop(&mut writer, &inner).await;
            break;
        }
        let reply = service::dispatch(inner.clone(), parsed).await;
        if write_json(&mut writer, &reply).await.is_err() {
            break;
        }
    }
}

/// `subscribe` 长连接：推送状态变化，直到服务停止、广播信道关闭或客户端断开。
///
/// 没有状态变化时 `recv()` 会一直等下去，因此这里用 `select!` 同时盯住停止标志：
/// 服务退出（`inner.stop`）时主动结束，不会留下一个只挂在长连接上的僵尸任务。
async fn subscribe_loop<W>(writer: &mut W, inner: &Arc<Inner>)
where
    W: AsyncWriteExt + Unpin,
{
    use std::sync::atomic::Ordering;

    let mut updates = service::subscribe(inner);
    // 每 500 ms 检查一次停止标志，同时保持 `recv()` 的响应性。
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = updates.recv() => match changed {
                Ok(status) => {
                    if write_json(writer, &json!({ "event": status })).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if inner.stop.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
    }
}

async fn write_json<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &Value,
) -> std::io::Result<()> {
    let mut text = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    text.push(b'\n');
    writer.write_all(&text).await?;
    writer.flush().await
}

/// 探测是否已有实例在监听（界面启动服务前的“已有实例”判定，也用于清理僵尸管道）。
pub async fn probe() -> bool {
    use tokio::net::windows::named_pipe::ClientOptions;

    // 能连上就说明有实例在跑；管道不存在/拒绝连接都当作“没有实例”。
    ClientOptions::new().open(PIPE_NAME).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_the_documented_one() {
        // 文档里写的是 \\.\pipe\bluetooth-input-bridge-control。
        assert_eq!(PIPE_NAME, r"\\.\pipe\bluetooth-input-bridge-control");
        assert!(PIPE_NAME.starts_with(r"\\."), "{PIPE_NAME}");
        assert!(PIPE_NAME.ends_with("bluetooth-input-bridge-control"));
    }

    #[test]
    fn request_limit_is_reasonable() {
        assert_eq!(MAX_REQUEST_BYTES, 64 * 1024);
    }
}
