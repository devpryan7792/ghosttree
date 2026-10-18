//! Minimal product-side JSON-RPC client for the daemon socket (SPEC §6.2).
//!
//! Newline-framed like the daemon; supports interleaved notifications and
//! responses, which is what a live dashboard needs: requests are written on
//! a dedicated half, frames are read one at a time with a configurable
//! timeout so caller loops can also poll for keypresses.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

/// Request/result client over the UDS transport.
pub struct RpcClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: i64,
    /// Notifications read while waiting for a specific response.
    notifications: Vec<Value>,
}

impl RpcClient {
    /// Connect to the daemon's unix socket.
    pub fn connect(socket: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let writer = stream.try_clone()?;
        Ok(Self {
            writer,
            reader: BufReader::new(stream),
            next_id: 1,
            notifications: Vec::new(),
        })
    }

    /// Bound how long `next_frame` waits for data (`None` = block).
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.reader.get_ref().set_read_timeout(timeout)
    }

    /// Notifications buffered by earlier `call`s / `next_frame`s.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.notifications)
    }

    /// Write one raw frame (any JSON object).
    pub fn send(&mut self, frame: &Value) -> io::Result<()> {
        let mut line = frame.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()
    }

    /// Request -> response, buffering any notifications seen meanwhile.
    pub fn call(&mut self, id: i64, method: &str, params: Value) -> io::Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        loop {
            match self.next_frame()? {
                Some(frame) if is_response(&frame, id) => {
                    if let Some(error) = frame.get("error") {
                        return Err(io::Error::other(format!("RPC {method} failed: {error}")));
                    }
                    return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
                }
                Some(frame) => {
                    if frame.get("method").is_some() {
                        self.notifications.push(frame);
                    }
                }
                None => continue,
            }
        }
    }

    /// Read the next frame; `Ok(None)` means the (optional) read timeout
    /// elapsed without data — the connection stays usable.
    pub fn next_frame(&mut self) -> io::Result<Option<Value>> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed the connection",
            )),
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                serde_json::from_str(trimmed)
                    .map(Some)
                    .map_err(|e| io::Error::other(format!("non-JSON frame from daemon: {e}")))
            }
            Err(e) if is_timeout(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Next request id (responses are matched by id).
    pub fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

fn is_response(frame: &Value, id: i64) -> bool {
    frame.get("method").is_none() && frame.get("id").and_then(Value::as_i64) == Some(id)
}

/// A read timeout surfaces as EAGAIN/EWOULDBLOCK (WouldBlock) or TimedOut
/// depending on platform timing; both mean "no data yet".
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline framing tests exercise only pure helpers; the socket path is
    /// covered by the daemon integration suite.
    #[test]
    fn is_response_matches_only_matching_ids() {
        let frame = json!({"jsonrpc": "2.0", "result": {}, "id": 3});
        assert!(is_response(&frame, 3));
        assert!(!is_response(&frame, 4));
        let notification = json!({"jsonrpc": "2.0", "method": "CandidateStateChanged"});
        assert!(!is_response(&notification, 3));
    }

    #[test]
    fn is_timeout_classifies_pending_reads() {
        assert!(is_timeout(&io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(is_timeout(&io::Error::from(io::ErrorKind::TimedOut)));
        assert!(!is_timeout(&io::Error::from(io::ErrorKind::NotFound)));
    }
}
