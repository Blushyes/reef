//! Minimal plugin binary used ONLY by integration tests. Not shipped.
//!
//! Behavior:
//!   - responds to `reef/initialize` with a fixed `InitializeResult`
//!   - responds to `reef/render` with a stub `RenderResult`
//!   - echoes any other method's params back as `result`
//!   - exits cleanly on `reef/shutdown` notification or EOF

use reef_protocol::{InitializeResult, RenderResult, RpcMessage, read_message, write_message};
use std::io::{self, BufReader};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();

    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(_) => break, // EOF or bad frame → exit
        };

        if msg.is_response() {
            continue; // ignore responses to host-originated pings
        }

        match msg.method.as_str() {
            "reef/shutdown" => break,

            "reef/initialize" => {
                let id = msg.id.unwrap_or(0);
                let result = serde_json::to_value(InitializeResult {
                    plugin_name: "echo".to_string(),
                    plugin_version: "0.0.1".to_string(),
                })
                .unwrap();
                write_message(&mut stdout, &RpcMessage::response(id, result))?;
            }

            "reef/render" => {
                let id = msg.id.unwrap_or(0);
                let panel_id = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("panel_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = serde_json::to_value(RenderResult {
                    panel_id,
                    lines: Vec::new(),
                    total_lines: 0,
                })
                .unwrap();
                write_message(&mut stdout, &RpcMessage::response(id, result))?;
            }

            _ => {
                // Echo: reply to requests with their own params
                if let Some(id) = msg.id {
                    let result = msg.params.unwrap_or(serde_json::Value::Null);
                    write_message(&mut stdout, &RpcMessage::response(id, result))?;
                }
            }
        }
    }

    Ok(())
}
