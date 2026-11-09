//! Client for the service control socket.
//!
//! Protocol: one JSON request line, one JSON reply line over a Unix socket.
//! `subscribe` replies once and then streams `{"event": <status>}` lines until
//! the client disconnects.

use crate::ServiceStatus;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::getuid() };
    let name = if base.starts_with("/run/user") {
        "bluetooth-input-bridge".to_string()
    } else {
        format!("bluetooth-input-bridge-{uid}")
    };
    base.join(name).join("control.sock")
}

pub struct Reply {
    pub ok: bool,
    pub status: Option<ServiceStatus>,
    pub error: Option<String>,
}

fn parse_reply(line: &str) -> Result<Reply, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| format!("服务响应格式错误: {error}"))?;
    Ok(Reply {
        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
        status: value
            .get("status")
            .and_then(|status| serde_json::from_value(status.clone()).ok()),
        error: value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

async fn connect() -> Result<UnixStream, String> {
    UnixStream::connect(socket_path())
        .await
        .map_err(|error| format!("后台服务未运行: {error}"))
}

pub async fn call(command: &str, payload: Value) -> Result<Reply, String> {
    let mut stream = connect().await?;
    let mut fields = match payload {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    fields.insert("cmd".to_string(), Value::String(command.to_string()));
    let mut line = serde_json::to_vec(&Value::Object(fields)).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let count = reader
        .read_line(&mut response)
        .await
        .map_err(|error| error.to_string())?;
    if count == 0 {
        return Err("服务没有返回状态".into());
    }
    parse_reply(&response)
}

fn require_status(reply: Reply) -> Result<ServiceStatus, String> {
    if reply.ok {
        reply.status.ok_or_else(|| "服务响应缺少状态".to_string())
    } else {
        Err(reply.error.unwrap_or_else(|| "服务返回失败".into()))
    }
}

fn require_ok(reply: Reply) -> Result<(), String> {
    if reply.ok {
        Ok(())
    } else {
        Err(reply.error.unwrap_or_else(|| "服务返回失败".into()))
    }
}

pub async fn status() -> Result<ServiceStatus, String> {
    require_status(call("status", json!({})).await?)
}

pub async fn set_registration(enabled: bool) -> Result<ServiceStatus, String> {
    require_status(call("setRegistration", json!({ "enabled": enabled })).await?)
}

pub async fn set_autostart(enabled: bool) -> Result<ServiceStatus, String> {
    require_status(call("setAutostart", json!({ "enabled": enabled })).await?)
}

pub async fn enable_pairing() -> Result<(), String> {
    require_ok(call("enablePairing", json!({})).await?)
}

pub async fn send_test(text: &str) -> Result<(), String> {
    require_ok(call("sendTest", json!({ "text": text })).await?)
}

/// Connect, send `subscribe`, and return the initial status plus a reader that
/// yields `{"event": <status>}` lines.
pub async fn subscribe() -> Result<(ServiceStatus, BufReader<UnixStream>), String> {
    let mut stream = connect().await?;
    stream
        .write_all(b"{\"cmd\":\"subscribe\"}\n")
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .map_err(|error| error.to_string())?
        == 0
    {
        return Err("服务订阅没有响应".into());
    }
    let reply = parse_reply(&line)?;
    let status = require_status(reply)?;
    Ok((status, reader))
}
