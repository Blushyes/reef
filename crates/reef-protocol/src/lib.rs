use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

// ─── Color ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Color {
    Named(String),           // "red", "green", "white", …
    Rgb([u8; 3]),            // [30, 30, 40]
}

impl Color {
    pub fn named(s: &str) -> Self { Self::Named(s.to_string()) }
    pub fn rgb(r: u8, g: u8, b: u8) -> Self { Self::Rgb([r, g, b]) }
}

// ─── StyledLine / Span ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Span {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
}

impl Span {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
    pub fn fg(mut self, c: Color) -> Self { self.fg = Some(c); self }
    pub fn bg(mut self, c: Color) -> Self { self.bg = Some(c); self }
    pub fn bold(mut self) -> Self { self.bold = Some(true); self }
    pub fn dim(mut self) -> Self { self.dim = Some(true); self }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
    /// 点击整行触发的命令（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_args: Option<serde_json::Value>,
}

impl StyledLine {
    pub fn new(spans: Vec<Span>) -> Self {
        Self { spans, ..Default::default() }
    }
    pub fn plain(text: impl Into<String>) -> Self {
        Self::new(vec![Span::new(text)])
    }
    pub fn on_click(mut self, command: impl Into<String>, args: serde_json::Value) -> Self {
        self.click_command = Some(command.into());
        self.click_args = Some(args);
        self
    }
}

// ─── Panel slot ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PanelSlot {
    Sidebar,
    Editor,
    Overlay,
    Statusbar,
}

// ─── reef.json manifest types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelDecl {
    pub id: String,
    pub title: String,
    pub slot: PanelSlot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindingDecl {
    pub key: String,
    pub command: String,
    #[serde(default = "default_when")]
    pub when: String,
}

fn default_when() -> String { "always".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDecl {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contributes {
    #[serde(default)]
    pub panels: Vec<PanelDecl>,
    #[serde(default)]
    pub keybindings: Vec<KeybindingDecl>,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub main: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub activation_events: Vec<String>,
    pub contributes: Contributes,
}

// ─── JSON-RPC 2.0 envelope ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    // For responses only
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcMessage {
    pub fn request(id: u64, method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: method.into(),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn notification(method: &str, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: method.into(),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn response(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: String::new(),
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn is_response(&self) -> bool {
        self.result.is_some() || self.error.is_some()
    }
}

// ─── Message params / results ────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeParams {
    pub reef_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    pub plugin_name: String,
    pub plugin_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenderParams {
    pub panel_id: String,
    pub width: u16,
    pub height: u16,
    pub focused: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenderResult {
    pub panel_id: String,
    pub lines: Vec<StyledLine>,
    #[serde(default)]
    pub total_lines: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventParams {
    pub panel_id: String,
    pub event: InputEvent,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventResult {
    pub consumed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum InputEvent {
    Key { key: String, modifiers: Vec<String> },
    Mouse { kind: String, button: Option<String>, column: u16, row: u16 },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommandParams {
    pub id: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenFileParams {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NotifyParams {
    pub message: String,
    pub level: String, // "info" | "warn" | "error"
}

// ─── Content-Length framing ──────────────────────────────────────────────────

/// Write one JSON-RPC message with Content-Length header.
pub fn write_message(writer: &mut impl Write, msg: &RpcMessage) -> io::Result<()> {
    let body = serde_json::to_string(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()
}

/// Read one JSON-RPC message (blocking).
pub fn read_message(reader: &mut impl BufRead) -> io::Result<RpcMessage> {
    // Read headers until blank line
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "plugin closed"));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(val) = line.strip_prefix("Content-Length: ") {
            content_length = val.trim().parse().ok();
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length")
    })?;

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    serde_json::from_slice(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
