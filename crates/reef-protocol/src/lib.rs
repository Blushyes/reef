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
    /// Per-span click command (overrides the line-level click_command for this span's region).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_args: Option<serde_json::Value>,
    /// Per-span double-click command (falls back to line-level dbl_click_command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbl_click_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbl_click_args: Option<serde_json::Value>,
}

impl Span {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
    pub fn fg(mut self, c: Color) -> Self { self.fg = Some(c); self }
    pub fn bg(mut self, c: Color) -> Self { self.bg = Some(c); self }
    pub fn bold(mut self) -> Self { self.bold = Some(true); self }
    pub fn dim(mut self) -> Self { self.dim = Some(true); self }
    pub fn on_click(mut self, command: impl Into<String>, args: serde_json::Value) -> Self {
        self.click_command = Some(command.into());
        self.click_args = Some(args);
        self
    }
    pub fn on_dbl_click(mut self, command: impl Into<String>, args: serde_json::Value) -> Self {
        self.dbl_click_command = Some(command.into());
        self.dbl_click_args = Some(args);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StyledLine {
    pub spans: Vec<Span>,
    /// 点击整行触发的命令（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_args: Option<serde_json::Value>,
    /// 双击整行触发的命令（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbl_click_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbl_click_args: Option<serde_json::Value>,
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
    pub fn on_dbl_click(mut self, command: impl Into<String>, args: serde_json::Value) -> Self {
        self.dbl_click_command = Some(command.into());
        self.dbl_click_args = Some(args);
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
    /// If set, this keybinding appears in the help panel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// First visible line (0-based). Plugin should return `height` lines starting here.
    #[serde(default)]
    pub scroll: u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ── Color ────────────────────────────────────────────────────────────────

    #[test]
    fn color_named_constructor() {
        assert_eq!(Color::named("red"), Color::Named("red".to_string()));
    }

    #[test]
    fn color_rgb_constructor() {
        assert_eq!(Color::rgb(1, 2, 3), Color::Rgb([1, 2, 3]));
    }

    #[test]
    fn color_named_serialization() {
        let json = serde_json::to_string(&Color::named("green")).unwrap();
        assert_eq!(json, "\"green\"");
    }

    #[test]
    fn color_rgb_serialization() {
        let json = serde_json::to_string(&Color::rgb(10, 20, 30)).unwrap();
        assert_eq!(json, "[10,20,30]");
    }

    // ── Span ─────────────────────────────────────────────────────────────────

    #[test]
    fn span_new_sets_text() {
        let s = Span::new("hello");
        assert_eq!(s.text, "hello");
        assert!(s.fg.is_none());
        assert!(s.bold.is_none());
    }

    #[test]
    fn span_builder_chain() {
        let s = Span::new("x")
            .fg(Color::named("red"))
            .bg(Color::named("blue"))
            .bold()
            .dim();
        assert_eq!(s.fg, Some(Color::named("red")));
        assert_eq!(s.bg, Some(Color::named("blue")));
        assert_eq!(s.bold, Some(true));
        assert_eq!(s.dim, Some(true));
    }

    #[test]
    fn span_on_click_sets_fields() {
        let s = Span::new("x").on_click("cmd", serde_json::json!({"k": "v"}));
        assert_eq!(s.click_command, Some("cmd".to_string()));
        assert_eq!(s.click_args, Some(serde_json::json!({"k": "v"})));
        assert!(s.dbl_click_command.is_none());
    }

    #[test]
    fn span_on_dbl_click_sets_fields() {
        let s = Span::new("x").on_dbl_click("dcmd", serde_json::json!(null));
        assert_eq!(s.dbl_click_command, Some("dcmd".to_string()));
        assert!(s.click_command.is_none());
    }

    #[test]
    fn span_default_all_none() {
        let s = Span::default();
        assert!(s.text.is_empty());
        assert!(s.fg.is_none());
        assert!(s.bold.is_none());
        assert!(s.italic.is_none());
        assert!(s.click_command.is_none());
        assert!(s.dbl_click_command.is_none());
    }

    // ── StyledLine ───────────────────────────────────────────────────────────

    #[test]
    fn styled_line_plain_single_span() {
        let line = StyledLine::plain("hello");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "hello");
        assert!(line.spans[0].fg.is_none());
        assert!(line.click_command.is_none());
    }

    #[test]
    fn styled_line_new_stores_spans() {
        let line = StyledLine::new(vec![Span::new("a"), Span::new("b")]);
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].text, "a");
        assert_eq!(line.spans[1].text, "b");
    }

    #[test]
    fn styled_line_on_click() {
        let line = StyledLine::plain("x").on_click("cmd", serde_json::json!(42));
        assert_eq!(line.click_command, Some("cmd".to_string()));
        assert_eq!(line.click_args, Some(serde_json::json!(42)));
        assert!(line.dbl_click_command.is_none());
    }

    #[test]
    fn styled_line_on_dbl_click() {
        let line = StyledLine::plain("x").on_dbl_click("dcmd", serde_json::json!(null));
        assert_eq!(line.dbl_click_command, Some("dcmd".to_string()));
        assert!(line.click_command.is_none());
    }

    // ── RpcMessage ───────────────────────────────────────────────────────────

    #[test]
    fn rpc_request_fields() {
        let msg = RpcMessage::request(5, "test/method", serde_json::json!({"key": "val"}));
        assert_eq!(msg.jsonrpc, "2.0");
        assert_eq!(msg.id, Some(5));
        assert_eq!(msg.method, "test/method");
        assert_eq!(msg.params, Some(serde_json::json!({"key": "val"})));
        assert!(msg.result.is_none());
        assert!(msg.error.is_none());
    }

    #[test]
    fn rpc_notification_no_id() {
        let msg = RpcMessage::notification("test/notify", serde_json::json!({}));
        assert_eq!(msg.jsonrpc, "2.0");
        assert!(msg.id.is_none());
        assert_eq!(msg.method, "test/notify");
        assert!(msg.result.is_none());
    }

    #[test]
    fn rpc_response_fields() {
        let msg = RpcMessage::response(7, serde_json::json!("ok"));
        assert_eq!(msg.id, Some(7));
        assert_eq!(msg.result, Some(serde_json::json!("ok")));
        assert!(msg.error.is_none());
        assert!(msg.params.is_none());
    }

    #[test]
    fn rpc_is_response_for_response_message() {
        assert!(RpcMessage::response(1, serde_json::json!(null)).is_response());
    }

    #[test]
    fn rpc_is_response_false_for_request() {
        assert!(!RpcMessage::request(1, "m", serde_json::json!({})).is_response());
    }

    #[test]
    fn rpc_is_response_false_for_notification() {
        assert!(!RpcMessage::notification("m", serde_json::json!({})).is_response());
    }

    // ── write_message / read_message ─────────────────────────────────────────

    #[test]
    fn write_then_read_roundtrip() {
        let original = RpcMessage::request(42, "reef/render", serde_json::json!({"panel_id": "test"}));
        let mut buf = Vec::new();
        write_message(&mut buf, &original).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.method, original.method);
        assert_eq!(decoded.params, original.params);
    }

    #[test]
    fn write_message_produces_content_length_header() {
        let msg = RpcMessage::notification("ping", serde_json::json!({}));
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("Content-Length: "), "expected Content-Length header");
        assert!(s.contains("\r\n\r\n"), "expected CRLF separator");
    }

    #[test]
    fn read_message_ignores_extra_headers() {
        let body = r#"{"jsonrpc":"2.0","method":"ping","params":null}"#;
        let raw = format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut cursor = Cursor::new(raw.into_bytes());
        let msg = read_message(&mut cursor).unwrap();
        assert_eq!(msg.method, "ping");
    }

    // ── Manifest / Decl serialization ────────────────────────────────────────

    #[test]
    fn rpc_error_fields() {
        let err = RpcError { code: -32600, message: "Invalid Request".to_string() };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("-32600"));
        assert!(json.contains("Invalid Request"));
    }

    #[test]
    fn panel_decl_slot_serialization() {
        let decl = PanelDecl {
            id: "git.status".into(),
            title: "Git".into(),
            slot: PanelSlot::Sidebar,
            icon: None,
        };
        let json = serde_json::to_value(&decl).unwrap();
        assert_eq!(json["slot"], "sidebar");
        assert_eq!(json["id"], "git.status");
    }

    #[test]
    fn panel_slot_variants_serialize() {
        assert_eq!(serde_json::to_value(PanelSlot::Sidebar).unwrap(), "sidebar");
        assert_eq!(serde_json::to_value(PanelSlot::Editor).unwrap(), "editor");
        assert_eq!(serde_json::to_value(PanelSlot::Overlay).unwrap(), "overlay");
        assert_eq!(serde_json::to_value(PanelSlot::Statusbar).unwrap(), "statusbar");
    }
}
