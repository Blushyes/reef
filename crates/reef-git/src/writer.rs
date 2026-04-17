use reef_protocol::{RpcMessage, write_message};
use std::io::{Stdout, Write};
use std::sync::{Arc, Mutex};

/// Thread-safe sink for JSON-RPC messages. Holds a mutex so the fs-watcher
/// thread and the main loop can both emit without interleaving frames.
///
/// The inner writer is a boxed trait object so tests can inject in-memory
/// buffers (`Vec<u8>`) or channel-backed sinks instead of `Stdout`.
#[derive(Clone)]
pub struct Writer {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Writer {
    /// Construct from stdout — the production path.
    pub fn new(stdout: Stdout) -> Self {
        Self::from_writer(stdout)
    }

    /// Construct from any `Write + Send` — used by tests to capture output.
    pub fn from_writer<W: Write + Send + 'static>(w: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Box::new(w))),
        }
    }

    pub fn send(&self, msg: &RpcMessage) {
        if let Ok(mut w) = self.inner.lock() {
            let _ = write_message(&mut *w, msg);
        }
    }
}
