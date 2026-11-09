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

#[derive(Clone, Serialize)]
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

    pub fn error_message(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

pub trait TextInjector: Send {
    fn inject(&mut self, text: &str) -> Result<(), String>;
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

    pub fn accept(&mut self, bytes: &[u8]) -> Result<(Ack, Option<String>), String> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err("消息超过 64 KiB".into());
        }
        let frame: TextFrame =
            serde_json::from_slice(bytes).map_err(|_| "JSON 格式错误".to_string())?;
        if frame.kind != "text" || frame.id.is_empty() || frame.id.len() > 128 {
            return Err("消息类型或 ID 无效".into());
        }
        if let Some(ack) = self.previous.get(&frame.id) {
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
        Ok((ack, sent))
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
