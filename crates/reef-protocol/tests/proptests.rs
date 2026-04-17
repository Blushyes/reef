//! Property-based tests for protocol framing and RPC message roundtrip.

use proptest::prelude::*;
use reef_protocol::{RpcMessage, read_message, write_message};
use std::io::Cursor;

/// Arbitrary request messages. Methods are restricted to ASCII to keep tests fast.
fn any_rpc_message() -> impl Strategy<Value = RpcMessage> {
    prop_oneof![
        // Request
        (any::<u64>(), "[a-zA-Z/]{1,20}", any::<i64>())
            .prop_map(|(id, m, n)| { RpcMessage::request(id, &m, serde_json::json!({ "n": n })) }),
        // Notification
        ("[a-zA-Z/]{1,20}", any::<bool>()).prop_map(|(m, flag)| {
            RpcMessage::notification(&m, serde_json::json!({ "flag": flag }))
        }),
        // Response
        (any::<u64>(), any::<String>())
            .prop_map(|(id, s)| { RpcMessage::response(id, serde_json::json!(s)) }),
    ]
}

proptest! {
    /// write → read must produce an equivalent message.
    #[test]
    fn rpc_roundtrip_preserves_identity(msg in any_rpc_message()) {
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("write succeeds");
        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).expect("read succeeds");

        prop_assert_eq!(decoded.id, msg.id);
        prop_assert_eq!(decoded.method, msg.method);
        prop_assert_eq!(decoded.params, msg.params);
        prop_assert_eq!(decoded.result, msg.result);
    }

    /// read_message must never panic on arbitrary byte input. It may return
    /// an error, but must not unwind.
    #[test]
    fn read_message_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let mut cursor = Cursor::new(bytes);
        let _ = read_message(&mut cursor); // may be Err — must not panic
    }

    /// Any Content-Length-framed JSON produced by serde_json is readable.
    #[test]
    fn read_message_handles_valid_framing(method in "[a-z]{1,10}") {
        let body = format!(r#"{{"jsonrpc":"2.0","method":"{}","params":null}}"#, method);
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut cursor = Cursor::new(framed.into_bytes());
        let msg = read_message(&mut cursor).expect("valid framing parses");
        prop_assert_eq!(msg.method, method);
    }
}
