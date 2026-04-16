use reef_protocol::{read_message, write_message, RpcMessage};
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// One live plugin subprocess.
pub struct PluginProcess {
    pub name: String,
    stdin: ChildStdin,
    rx: Receiver<RpcMessage>,
    _child: Child,
    next_id: u64,
}

impl PluginProcess {
    /// Spawn the plugin executable and start a background reader thread.
    pub fn spawn(name: &str, exe: &str) -> std::io::Result<Self> {
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // plugin stderr → host terminal (for debugging)
            .spawn()?;

        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");

        let (tx, rx): (Sender<RpcMessage>, Receiver<RpcMessage>) = mpsc::channel();
        let plugin_name = name.to_string();

        thread::Builder::new()
            .name(format!("reef-plugin-{}", plugin_name))
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader) {
                        Ok(msg) => {
                            if tx.send(msg).is_err() {
                                break; // host dropped the receiver, exit
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::UnexpectedEof {
                                eprintln!("[reef] plugin '{}' reader error: {}", plugin_name, e);
                            }
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            name: name.to_string(),
            stdin,
            rx,
            _child: child,
            next_id: 1,
        })
    }

    /// Send a request (with id). Returns the id assigned.
    pub fn send_request(&mut self, method: &str, params: serde_json::Value) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = RpcMessage::request(id, method, params);
        write_message(&mut self.stdin, &msg)?;
        Ok(id)
    }

    /// Send a notification (no id, no response expected).
    pub fn send_notification(&mut self, method: &str, params: serde_json::Value) -> std::io::Result<()> {
        let msg = RpcMessage::notification(method, params);
        write_message(&mut self.stdin, &msg)
    }

    /// Send a response back to the plugin (for plugin→host requests).
    pub fn send_response(&mut self, id: u64, result: serde_json::Value) -> std::io::Result<()> {
        let msg = RpcMessage::response(id, result);
        write_message(&mut self.stdin, &msg)
    }

    /// Non-blocking: drain all pending messages from the reader thread.
    pub fn drain_messages(&self) -> Vec<RpcMessage> {
        let mut msgs = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            msgs.push(msg);
        }
        msgs
    }
}
