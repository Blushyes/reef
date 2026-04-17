//! End-to-end RPC handshake test: spawn the real `echo-plugin` binary and
//! verify request/response cycle + graceful shutdown via `PluginProcess`.

use reef_host::plugin::process::PluginProcess;
use std::time::{Duration, Instant};

const ECHO_PLUGIN: &str = env!("CARGO_BIN_EXE_echo-plugin");

/// Poll `drain_messages` up to `timeout`, returning as soon as at least one
/// message has arrived.
fn wait_for_message(proc: &PluginProcess, timeout: Duration) -> Vec<reef_protocol::RpcMessage> {
    let start = Instant::now();
    loop {
        let msgs = proc.drain_messages();
        if !msgs.is_empty() {
            return msgs;
        }
        if start.elapsed() > timeout {
            return msgs;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn initialize_request_returns_response() {
    let mut proc = PluginProcess::spawn("echo", ECHO_PLUGIN).expect("spawn echo plugin");
    let id = proc
        .send_request(
            "reef/initialize",
            serde_json::json!({ "reef_version": "test" }),
        )
        .expect("send initialize");

    let msgs = wait_for_message(&proc, Duration::from_secs(3));
    assert!(!msgs.is_empty(), "plugin should respond within timeout");
    let response = msgs.iter().find(|m| m.id == Some(id) && m.is_response());
    let response = response.expect("matching response present");
    let result = response.result.as_ref().expect("response has result");
    assert_eq!(result["plugin_name"], "echo");
    assert_eq!(result["plugin_version"], "0.0.1");
}

#[test]
fn render_request_returns_render_result() {
    let mut proc = PluginProcess::spawn("echo", ECHO_PLUGIN).expect("spawn");
    let id = proc
        .send_request(
            "reef/render",
            serde_json::json!({
                "panel_id": "test.panel",
                "width": 80u16,
                "height": 24u16,
                "focused": true,
            }),
        )
        .expect("send render");
    let msgs = wait_for_message(&proc, Duration::from_secs(3));
    let response = msgs
        .iter()
        .find(|m| m.id == Some(id) && m.is_response())
        .expect("response arrived");
    let result = response.result.as_ref().unwrap();
    assert_eq!(result["panel_id"], "test.panel");
    assert!(result["lines"].is_array());
}

#[test]
fn unique_request_ids_assigned() {
    let mut proc = PluginProcess::spawn("echo", ECHO_PLUGIN).expect("spawn");
    let id1 = proc
        .send_request("reef/custom", serde_json::json!({"n": 1}))
        .unwrap();
    let id2 = proc
        .send_request("reef/custom", serde_json::json!({"n": 2}))
        .unwrap();
    assert_ne!(id1, id2);

    // Collect both responses (echo plugin replies to any request)
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen = std::collections::HashSet::new();
    while Instant::now() < deadline && seen.len() < 2 {
        for msg in proc.drain_messages() {
            if let Some(id) = msg.id {
                if msg.is_response() && (id == id1 || id == id2) {
                    seen.insert(id);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(seen.contains(&id1));
    assert!(seen.contains(&id2));
}

#[test]
fn notification_produces_no_response() {
    let mut proc = PluginProcess::spawn("echo", ECHO_PLUGIN).expect("spawn");
    proc.send_notification("reef/custom", serde_json::json!({"k": "v"}))
        .unwrap();
    // Give the plugin time to process — no response should come back.
    std::thread::sleep(Duration::from_millis(150));
    let msgs = proc.drain_messages();
    assert!(
        msgs.is_empty(),
        "notification must not trigger response, got {:?}",
        msgs
    );
}
