//! A minimal Chrome DevTools Protocol client: JSON messages over one WebSocket.
//!
//! A reader thread owns the socket for reading and blocks on it; calls write their frames
//! through a second handle to the same socket, so a call never waits for the reader. Pages are
//! attached with `flatten`, so every session's messages share this one connection.

use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tungstenite::Message;

/// Handles a session's events: `(method, params)`.
pub type Listener = Arc<dyn Fn(&str, &Json) + Send + Sync>;

#[derive(Clone)]
pub struct Connection(Arc<Inner>);

struct Inner {
    writer: Mutex<TcpStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Json, String>>>>,
    listeners: Mutex<HashMap<String, Listener>>,
    closed: AtomicBool,
}

impl Connection {
    /// Connect to a browser's DevTools WebSocket (`ws://127.0.0.1:<port>/devtools/browser/<id>`).
    pub fn connect(ws_url: &str) -> Result<Connection, String> {
        let address = ws_url
            .strip_prefix("ws://")
            .and_then(|rest| rest.split('/').next())
            .ok_or_else(|| format!("not a DevTools URL: {ws_url}"))?;
        let stream = TcpStream::connect(address).map_err(|e| e.to_string())?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (mut socket, _) = tungstenite::client::client_with_config(ws_url, stream, Some(config))
            .map_err(|e| format!("DevTools handshake failed: {e}"))?;
        let conn = Connection(Arc::new(Inner {
            writer: Mutex::new(writer),
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            listeners: Mutex::default(),
            closed: AtomicBool::new(false),
        }));
        let inner = conn.0.clone();
        std::thread::Builder::new()
            .name("klausur-cdp".into())
            .spawn(move || {
                while let Ok(message) = socket.read() {
                    if let Message::Text(text) = message
                        && let Ok(msg) = serde_json::from_str::<Json>(text.as_str())
                    {
                        inner.dispatch(msg);
                    }
                }
                inner.closed.store(true, Ordering::SeqCst);
                for (_, tx) in inner.pending.lock().unwrap().drain() {
                    let _ = tx.send(Err("Chrome closed the connection".into()));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(conn)
    }

    /// Call a method and wait for its result. `session` targets a page; `None` is the browser.
    pub fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Json,
        timeout: Duration,
    ) -> Result<Json, String> {
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.0.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(id, session, method, params) {
            self.0.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                self.0.pending.lock().unwrap().remove(&id);
                Err(format!(
                    "Chrome didn't answer `{method}` within {timeout:?}"
                ))
            }
        }
    }

    /// Send a method without waiting for its result, e.g. from an event listener, which runs
    /// on the reader thread and so can't wait.
    pub fn send(&self, session: Option<&str>, method: &str, params: Json) {
        let id = self.0.next_id.fetch_add(1, Ordering::SeqCst);
        let _ = self.write(id, session, method, params);
    }

    pub fn listen(&self, session: &str, listener: Listener) {
        self.0
            .listeners
            .lock()
            .unwrap()
            .insert(session.to_string(), listener);
    }

    pub fn forget(&self, session: &str) {
        self.0.listeners.lock().unwrap().remove(session);
    }

    fn write(
        &self,
        id: u64,
        session: Option<&str>,
        method: &str,
        params: Json,
    ) -> Result<(), String> {
        if self.0.closed.load(Ordering::SeqCst) {
            return Err("Chrome closed the connection".into());
        }
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let frame = client_frame(msg.to_string().as_bytes(), id as u32);
        let mut writer = self.0.writer.lock().unwrap();
        writer
            .write_all(&frame)
            .map_err(|e| format!("couldn't send `{method}` to Chrome: {e}"))
    }
}

impl Inner {
    fn dispatch(&self, msg: Json) {
        if let Some(id) = msg.get("id").and_then(Json::as_u64) {
            let Some(tx) = self.pending.lock().unwrap().remove(&id) else {
                return;
            };
            let result = match msg.get("error") {
                Some(e) => Err(e
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("unknown error")
                    .to_string()),
                None => Ok(msg.get("result").cloned().unwrap_or(Json::Null)),
            };
            let _ = tx.send(result);
            return;
        }
        let (Some(method), Some(session)) = (
            msg.get("method").and_then(Json::as_str),
            msg.get("sessionId").and_then(Json::as_str),
        ) else {
            return;
        };
        let listener = self.listeners.lock().unwrap().get(session).cloned();
        if let Some(listener) = listener {
            listener(method, msg.get("params").unwrap_or(&Json::Null));
        }
    }
}

/// Encode one masked text frame, as a WebSocket client must send it (RFC 6455 §5.2). Chrome
/// is on localhost, so the mask only has to be present, not unpredictable.
fn client_frame(payload: &[u8], seed: u32) -> Vec<u8> {
    let mask = seed.wrapping_mul(0x9E37_79B9).to_be_bytes();
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x81); // FIN, text
    match payload.len() {
        n if n < 126 => out.push(0x80 | n as u8),
        n if n <= u16::MAX as usize => {
            out.push(0x80 | 126);
            out.extend((n as u16).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend((n as u64).to_be_bytes());
        }
    }
    out.extend(mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_masked_with_the_right_length() {
        for len in [0, 125, 126, 70_000] {
            let payload = vec![b'x'; len];
            let frame = client_frame(&payload, 7);
            let (header, mask_at) = match len {
                0..=125 => (2, 2),
                126..=65535 => (4, 4),
                _ => (10, 10),
            };
            assert_eq!(frame.len(), header + 4 + len);
            assert_eq!(frame[0], 0x81);
            assert_ne!(frame[1] & 0x80, 0, "mask bit");
            let mask = &frame[mask_at..mask_at + 4];
            let decoded: Vec<u8> = frame[mask_at + 4..]
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ mask[i % 4])
                .collect();
            assert_eq!(decoded, payload);
        }
    }
}
