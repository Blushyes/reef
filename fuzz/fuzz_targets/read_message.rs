#![no_main]

use libfuzzer_sys::fuzz_target;
use reef_protocol::read_message;
use std::io::Cursor;

// Fuzz the Content-Length framed JSON-RPC parser. Any byte input must either
// parse successfully or return an Err — never panic, OOM, or hang.
fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    let _ = read_message(&mut cursor);
});
