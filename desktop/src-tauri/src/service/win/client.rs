//! 控制通道客户端：命名管道版 `service/client.rs`（界面侧）。
//!
//! 协议与 Linux 端完全一致（`docs/service-api.md`）：一行请求 → 一行应答；
//! `subscribe` 是长连接。区别只在传输层——`NamedPipeClient` 替代 `UnixStream`，
//! 所以上层命令实现只差一个 `cfg`。

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

use super::control::PIPE_NAME;
use crate::ServiceStatus;

/// 一条应答（`{"ok":…,"status":…,"error":…}`）。
#[derive(Debug, Clone)]
pub struct Reply {
    pub ok: bool,
    pub status: Option<ServiceStatus>,
    pub error: Option<String>,
}

/// 解析一行应答。
fn parse_reply(line: &str) -> Result<Reply, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        ok: bool,
        #[serde(default)]
        status: Option<ServiceStatus>,
        #[serde(default)]
        error: Option<String>,
    }
    let raw: Raw =
        serde_json::from_str(line.trim()).map_err(|error| format!("服务响应格式错误: {error}"))?;
    Ok(Reply {
        ok: raw.ok,
        status: raw.status,
        error: raw.error,
    })
}

async fn connect() -> Result<NamedPipeClient, String> {
    // 服务刚启动时管道实例可能还没建好，短暂重试。
    let mut last = String::new();
    for _ in 0..10 {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(client) => return Ok(client),
            Err(error) => {
                last = error.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(format!("后台服务未运行: {last}"))
}

/// 等一行应答的上限。
///
/// 服务端正常时是毫秒级；这里只是兜底：服务进程若因驱动/蓝牙协议栈卡在阻塞调用
/// 里（例如注销 SDP），界面不能跟着无限转圈——超时就给用户一个能看懂的失败，
/// 而不是把界面卡死。
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 发一条命令并等待一行应答。
pub async fn call(command: &str, payload: Value) -> Result<Reply, String> {
    let client = connect().await?;
    let (reader, mut writer) = tokio::io::split(client);
    let mut request = payload;
    if let Value::Object(map) = &mut request {
        map.insert("cmd".into(), Value::String(command.into()));
    }
    let mut line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    line.push(b'\n');
    writer
        .write_all(&line)
        .await
        .map_err(|error| format!("写入控制通道失败: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("写入控制通道失败: {error}"))?;

    let mut reader = BufReader::new(reader);
    let text = read_reply_line(&mut reader, command).await?;
    parse_reply(&text)
}

/// 读一行应答，超过 [`REPLY_TIMEOUT`] 就放弃（并给出中文原因）。
async fn read_reply_line<R>(reader: &mut BufReader<R>, command: &str) -> Result<String, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut text = String::new();
    let read = tokio::time::timeout(REPLY_TIMEOUT, reader.read_line(&mut text))
        .await
        .map_err(|_| {
            format!(
                "后台服务在 {} 秒内没有回复（{}），可能正在处理上一条命令或蓝牙协议栈无响应",
                REPLY_TIMEOUT.as_secs(),
                command
            )
        })?
        .map_err(|error| format!("读取服务响应失败: {error}"))?;
    if read == 0 {
        return Err("服务没有返回状态".into());
    }
    Ok(text)
}

/// 要求应答成功并带状态（失败时把中文错误抛给界面）。
fn require_ok(reply: Reply) -> Result<ServiceStatus, String> {
    match (reply.ok, reply.status) {
        (true, Some(status)) => Ok(status),
        (true, None) => Err("服务没有返回状态".into()),
        (false, _) => Err(reply.error.unwrap_or_else(|| "服务返回失败".into())),
    }
}

/// 当前状态。
pub async fn status() -> Result<ServiceStatus, String> {
    let reply = call("status", json!({})).await?;
    require_ok(reply)
}

/// 服务注册开关。
pub async fn set_registration(enabled: bool) -> Result<ServiceStatus, String> {
    let reply = call("setRegistration", json!({ "enabled": enabled })).await?;
    require_ok(reply)
}

/// 开机自启开关。
pub async fn set_autostart(enabled: bool) -> Result<ServiceStatus, String> {
    let reply = call("setAutostart", json!({ "enabled": enabled })).await?;
    require_ok(reply)
}

/// 打开配对引导。
pub async fn enable_pairing() -> Result<(), String> {
    let reply = call("enablePairing", json!({})).await?;
    require_ok(reply).map(|_| ())
}

/// 测试输入（走同一条注入链路，不经过蓝牙）。
pub async fn send_test(text: &str) -> Result<(), String> {
    let reply = call("sendTest", json!({ "text": text })).await?;
    require_ok(reply).map(|_| ())
}

/// 订阅状态：返回初始状态与后续推送的读取器。
pub async fn subscribe() -> Result<(ServiceStatus, BufReader<ReadHalf<NamedPipeClient>>), String> {
    let client = connect().await?;
    let (reader, mut writer) = tokio::io::split(client);
    writer
        .write_all(b"{\"cmd\":\"subscribe\"}\n")
        .await
        .map_err(|error| format!("写入控制通道失败: {error}"))?;
    writer
        .flush()
        .await
        .map_err(|error| format!("写入控制通道失败: {error}"))?;
    let mut reader = BufReader::new(reader);
    let text = read_reply_line(&mut reader, "subscribe").await?;
    let reply = parse_reply(&text)?;
    let status = require_ok(reply)?;
    Ok((status, reader))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ok_reply() {
        let reply = parse_reply(
            r#"{"ok":true,"status":{"serviceRunning":true,"listening":true,"registrationEnabled":true,"autostartEnabled":false,"connectedDevice":"OPPO","connectedAddress":"AA:BB:CC:DD:EE:FF","receivedCount":3,"injectionMethod":"SendInput + KEYEVENTF_UNICODE"}}"#,
        )
        .expect("应可解析");
        assert!(reply.ok);
        let status = reply.status.expect("应带状态");
        assert_eq!(status.received_count, 3);
        assert_eq!(status.injection_method, "SendInput + KEYEVENTF_UNICODE");
    }

    #[test]
    fn parses_error_reply() {
        let reply = parse_reply(r#"{"ok":false,"error":"测试文字为空"}"#).expect("应可解析");
        assert!(!reply.ok);
        assert_eq!(reply.error.as_deref(), Some("测试文字为空"));
        assert!(require_ok(reply).is_err());
    }

    #[test]
    fn rejects_garbage() {
        let error = parse_reply("not json").unwrap_err();
        assert!(error.contains("服务响应格式错误"), "{error}");
    }

    #[test]
    fn commands_carry_the_cmd_field_on_the_wire() {
        let mut request = json!({ "enabled": true });
        if let Value::Object(map) = &mut request {
            map.insert("cmd".into(), Value::String("setRegistration".into()));
        }
        assert_eq!(request["cmd"], "setRegistration");
        assert_eq!(request["enabled"], true);
    }

    #[tokio::test]
    async fn missing_reply_line_reports_a_chinese_error() {
        // 对端一直不发数据（管道没有关闭）时，read_reply_line 必须靠超时之外的
        // “读到 0 字节”路径给出明确原因，而不是返回空串让上层误判。
        let mut reader = BufReader::new(&b""[..]);
        let error = read_reply_line(&mut reader, "status").await.unwrap_err();
        assert_eq!(error, "服务没有返回状态");
    }

    #[tokio::test]
    async fn reply_line_is_read_once() {
        let mut reader = BufReader::new(&b"{\"ok\":true}\n{{\"ok\":false}\n"[..]);
        let text = read_reply_line(&mut reader, "status").await.expect("可读");
        assert_eq!(text.trim(), "{\"ok\":true}");
        assert!(parse_reply(&text).expect("可解析").ok);
    }
}
