use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

pub const SERVICE_UUID: &str = "0000e8b6-0000-1000-8000-00805f9b34fb";
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
pub struct TextFrame {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Ack {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Ack {
    pub fn success(id: String) -> Self {
        Self {
            kind: "ack",
            id,
            ok: true,
            error: None,
        }
    }
    pub fn failure(id: String, error: String) -> Self {
        Self {
            kind: "ack",
            id,
            ok: false,
            error: Some(error),
        }
    }
    pub fn is_ok(&self) -> bool {
        self.ok
    }

    /// `id` 字段不对外暴露，但会话结束时要按 id 交接“已注入未回执”的记录。
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn error_message(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// 单条会话里“文本帧 → 注入 → 回执”的全部状态。
///
/// 去重是必须的：安卓端一次只保留一条未回执消息，8 秒超时后重发同一个 `id`；如果
/// 每个重复都再注入一次，一次连接抖动就会把同一段文字打两遍。所以处理过的 `id` 要
/// 记下来，重复时直接回缓存回执、不再注入。
pub trait TextInjector: Send {
    fn inject(&mut self, text: &str) -> Result<(), String>;
}

/// 允许把 `Box<dyn TextInjector>` 直接交给 `Session`，会话层才能换注入器做测试。
impl TextInjector for Box<dyn TextInjector + '_> {
    fn inject(&mut self, text: &str) -> Result<(), String> {
        (**self).inject(text)
    }
}

impl TextInjector for Box<dyn TextInjector + Send + '_> {
    fn inject(&mut self, text: &str) -> Result<(), String> {
        (**self).inject(text)
    }
}

/// `id` 字段本来就不是 `pub`，但 `Session::preload` 需要按 id 建缓存，
/// 所以在同一模块里提供只读访问器，不扩大字段可见性。
fn ack_id(ack: &Ack) -> String {
    ack.id.clone()
}

pub struct Session<I: TextInjector> {
    injector: I,
    previous: HashMap<String, Ack>,
    order: VecDeque<String>,
}

impl<I: TextInjector> Session<I> {
    pub fn new(injector: I) -> Self {
        Self {
            injector,
            previous: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// 预置一条“已经处理过”的回执（换连接时继承旧会话的缓存用）。
    ///
    /// 只在旧会话里同一 `id` 的注入已经完成、但回执没写出去时调用：新会话收到同 `id`
    /// 时直接回这条缓存，不再注入，避免手机重发把同一段文字打两遍。
    pub fn preload(&mut self, ack: Ack) {
        let id = ack_id(&ack);
        if id.is_empty() || self.previous.contains_key(&id) {
            return;
        }
        self.order.push_back(id.clone());
        self.previous.insert(id, ack);
    }

    /// 处理一帧。
    ///
    /// 第二个返回值的语义：`Some(Option<String>)` 表示这一帧走了“新消息”路径，
    /// 里面的 `Some(text)` 是**这次真的注入进系统**的文本；`None` 表示命中历史缓存，
    /// 只回了缓存回执、没有注入。会话层据此统计“成功注入”次数，避免把重发记成注入。
    pub fn accept(&mut self, bytes: &[u8]) -> Result<(Ack, Option<Option<String>>), String> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err("消息超过 64 KiB".into());
        }
        let frame: TextFrame =
            serde_json::from_slice(bytes).map_err(|_| "JSON 格式错误".to_string())?;
        if frame.kind != "text" || frame.id.is_empty() || frame.id.len() > 128 {
            return Err("消息类型或 ID 无效".into());
        }
        if let Some(ack) = self.previous.get(&frame.id) {
            // 命中缓存：`None` 表示“这条不是新消息”，调用方据此不做注入计数。
            return Ok((ack.clone(), None));
        }
        let ack = if frame.text.is_empty() || frame.text.len() > MAX_TEXT_BYTES {
            Ack::failure(frame.id.clone(), "文本为空或超过 16 KiB".into())
        } else {
            match self.injector.inject(&frame.text) {
                Ok(()) => Ack::success(frame.id.clone()),
                Err(error) => Ack::failure(frame.id.clone(), error),
            }
        };
        let sent = ack.is_ok().then_some(frame.text);
        self.order.push_back(frame.id.clone());
        self.previous.insert(frame.id, ack.clone());
        if self.order.len() > 256 {
            if let Some(old) = self.order.pop_front() {
                self.previous.remove(&old);
            }
        }
        Ok((ack, Some(sent)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Recording(Arc<Mutex<Vec<String>>>);
    impl TextInjector for Recording {
        fn inject(&mut self, text: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(text.into());
            Ok(())
        }
    }

    #[test]
    fn duplicate_id_is_not_injected_twice() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut session = Session::new(Recording(received.clone()));
        let frame = r#"{"type":"text","id":"same","text":"中文"}"#.as_bytes();
        assert!(session.accept(frame).unwrap().0.is_ok());
        assert!(session.accept(frame).unwrap().0.is_ok());
        assert_eq!(*received.lock().unwrap(), vec!["中文"]);
    }

    #[test]
    fn rejects_oversized_text_without_injection() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut session = Session::new(Recording(received.clone()));
        let frame =
            serde_json::json!({"type":"text", "id":"big", "text":"x".repeat(MAX_TEXT_BYTES + 1)})
                .to_string();
        assert!(!session.accept(frame.as_bytes()).unwrap().0.is_ok());
        assert!(received.lock().unwrap().is_empty());
    }
}
