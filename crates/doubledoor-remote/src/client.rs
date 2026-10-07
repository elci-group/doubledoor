//! A `doubledoor.protocol/v1` client over a transport.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use thiserror::Error;

use doubledoor_protocol::rpc::{ErrorObject, PROTOCOL};

use crate::config::Remote;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("remote `{0}` is not registered")]
    Unknown(String),
    #[error("remote configuration: {0}")]
    Config(String),
    #[error("cannot reach remote: {0}")]
    Unreachable(String),
    #[error("remote protocol violation: {0}")]
    Protocol(String),
    /// The remote answered with a structured error.
    #[error("remote error [{}]: {}", .0.kind, .0.message)]
    Remote(ErrorObject),
}

/// One protocol connection to a remote Doubledoor.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<String>,
    next: u64,
    timeout: Duration,
    /// Notifications received while waiting for responses.
    pub notifications: Vec<Value>,
    /// What the remote granted at `hello`.
    pub hello: Value,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    /// Start the remote's `rpc` over the transport and negotiate.
    pub fn connect(
        remote: &Remote,
        capabilities: &[&str],
        timeout: Duration,
    ) -> Result<Self, RemoteError> {
        let argv = remote.rpc_command().map_err(RemoteError::Config)?;
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| RemoteError::Config("empty command".into()))?;
        let mut child = Command::new(prog)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| RemoteError::Unreachable(format!("{prog}: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RemoteError::Unreachable("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RemoteError::Unreachable("no stdout".into()))?;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(l) = line else { return };
                if tx.send(l).is_err() {
                    return;
                }
            }
        });
        let mut c = Client {
            child,
            stdin,
            rx,
            next: 1,
            timeout,
            notifications: Vec::new(),
            hello: Value::Null,
        };
        c.hello = c.call(
            "hello",
            json!({ "protocol_versions": [PROTOCOL], "capabilities": capabilities, "agent": { "name": "doubledoor-remote" } }),
        )?;
        Ok(c)
    }

    /// Send one request; return its result or the remote's error.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, RemoteError> {
        let id = self.next;
        self.next += 1;
        let req = json!({ "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}")
            .and_then(|()| self.stdin.flush())
            .map_err(|e| RemoteError::Unreachable(e.to_string()))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let v = self.next_message(deadline)?;
            if v.get("notification").is_some() {
                self.notifications.push(v);
                continue;
            }
            if v["id"] != json!(id) {
                continue;
            }
            return if v["ok"] == true {
                Ok(v["result"].clone())
            } else {
                let err: ErrorObject = serde_json::from_value(v["error"].clone())
                    .map_err(|e| RemoteError::Protocol(format!("malformed error: {e}")))?;
                Err(RemoteError::Remote(err))
            };
        }
    }

    /// Next JSON message (response or notification) before `deadline`.
    pub fn next_message(&mut self, deadline: Instant) -> Result<Value, RemoteError> {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = self.rx.recv_timeout(left).map_err(|_| {
            RemoteError::Unreachable("no response from remote (timed out or closed)".into())
        })?;
        serde_json::from_str(&line)
            .map_err(|e| RemoteError::Protocol(format!("non-JSON line from remote ({e})")))
    }
}
