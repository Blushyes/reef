mod git;

use git::{DiffMode, FileStatus, GitRepo};
use reef_protocol::{
    read_message, write_message, Color, InitializeResult, RenderResult, RpcMessage,
    Span, StyledLine,
};
use std::io::{self, BufReader, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    let mut state = PluginState::new();

    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(_) => break,
        };

        if msg.is_response() {
            continue; // host responding to our requests — ignore for now
        }

        match msg.method.as_str() {
            "reef/initialize" => {
                state.refresh();
                let result = serde_json::to_value(InitializeResult {
                    plugin_name: "git".to_string(),
                    plugin_version: "0.1.0".to_string(),
                })
                .unwrap();
                let resp = RpcMessage::response(msg.id.unwrap_or(0), result);
                let _ = write_message(&mut writer, &resp);
            }

            "reef/render" => {
                let id = msg.id.unwrap_or(0);
                let params = msg.params.as_ref();
                let panel_id = params
                    .and_then(|p| p.get("panel_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let width = params
                    .and_then(|p| p.get("width"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(40) as u16;
                let height = params
                    .and_then(|p| p.get("height"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(24) as u16;
                let focused = params
                    .and_then(|p| p.get("focused"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let lines = state.render(&panel_id, width, height, focused);
                let total = lines.len();
                let result = serde_json::to_value(RenderResult {
                    panel_id,
                    lines,
                    total_lines: total,
                })
                .unwrap();
                let resp = RpcMessage::response(id, result);
                let _ = write_message(&mut writer, &resp);
            }

            "reef/event" => {
                let id = msg.id.unwrap_or(0);
                let params = msg.params.as_ref();
                let consumed = state.handle_event(params, &mut writer);
                let result = serde_json::json!({ "consumed": consumed });
                let resp = RpcMessage::response(id, result);
                let _ = write_message(&mut writer, &resp);
            }

            "reef/command" => {
                let id = msg.id.unwrap_or(0);
                let params = msg.params.as_ref();
                let success = state.handle_command(params, &mut writer);
                let result = serde_json::json!({ "success": success });
                let resp = RpcMessage::response(id, result);
                let _ = write_message(&mut writer, &resp);
            }

            "reef/shutdown" => break,

            _ => {}
        }
    }
}

// ─── Plugin state ─────────────────────────────────────────────────────────────

struct PluginState {
    repo: Option<GitRepo>,
    staged: Vec<git::FileEntry>,
    unstaged: Vec<git::FileEntry>,
    selected: Option<SelectedFile>,
    staged_collapsed: bool,
    unstaged_collapsed: bool,
    scroll: usize,
    diff_mode: DiffMode,
    diff_scroll: usize,
}

struct SelectedFile {
    path: String,
    is_staged: bool,
}

impl PluginState {
    fn new() -> Self {
        let repo = GitRepo::open().ok();
        Self {
            repo,
            staged: Vec::new(),
            unstaged: Vec::new(),
            selected: None,
            staged_collapsed: false,
            unstaged_collapsed: false,
            scroll: 0,
            diff_mode: DiffMode::Compact,
            diff_scroll: 0,
        }
    }

    fn refresh(&mut self) {
        if let Some(ref repo) = self.repo {
            let (s, u) = repo.get_status();
            self.staged = s;
            self.unstaged = u;
        }
    }

    fn render(&mut self, panel_id: &str, width: u16, _height: u16, focused: bool) -> Vec<StyledLine> {
        match panel_id {
            "git.status" => self.render_status(width, focused),
            "git.diff" => self.render_diff(width),
            _ => vec![],
        }
    }

    fn render_status(&self, width: u16, _focused: bool) -> Vec<StyledLine> {
        let mut lines: Vec<StyledLine> = Vec::new();
        let max_path = (width as usize).saturating_sub(8);

        // Staged section
        if !self.staged.is_empty() {
            let arrow = if self.staged_collapsed { "›" } else { "⌄" };
            lines.push(StyledLine::new(vec![
                Span::new(format!("{} ", arrow)).fg(Color::named("white")),
                Span::new("暂存的更改").fg(Color::named("white")).bold(),
                Span::new(format!("  {}", self.staged.len())).fg(Color::named("green")),
            ]).on_click("git.toggleStaged", serde_json::Value::Null));

            if !self.staged_collapsed {
                for file in &self.staged {
                    lines.push(file_row(&file.path, file.status, true, max_path));
                }
            }
            lines.push(StyledLine::plain(""));
        }

        // Unstaged section
        let arrow = if self.unstaged_collapsed { "›" } else { "⌄" };
        lines.push(StyledLine::new(vec![
            Span::new(format!("{} ", arrow)).fg(Color::named("white")),
            Span::new("更改").fg(Color::named("white")).bold(),
            Span::new(format!("  {}", self.unstaged.len())).fg(Color::named("blue")),
        ]).on_click("git.toggleUnstaged", serde_json::Value::Null));

        if !self.unstaged_collapsed {
            for file in &self.unstaged {
                lines.push(file_row(&file.path, file.status, false, max_path));
            }
            if self.unstaged.is_empty() {
                lines.push(StyledLine::new(vec![
                    Span::new("  无文件").fg(Color::named("darkGray")),
                ]));
            }
        }

        lines
    }

    fn render_diff(&self, width: u16) -> Vec<StyledLine> {
        let Some(ref sel) = self.selected else {
            return vec![StyledLine::new(vec![
                Span::new("选择一个文件查看 diff").fg(Color::named("darkGray")),
            ])];
        };
        let Some(ref repo) = self.repo else { return vec![] };

        let context = match self.diff_mode {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        };
        let Some(diff) = repo.get_diff(&sel.path, sel.is_staged, context) else {
            return vec![StyledLine::plain("无 diff")];
        };

        let mut lines = vec![StyledLine::new(vec![
            Span::new(&diff.file_path).fg(Color::named("white")).bold(),
        ])];
        lines.push(StyledLine::new(vec![
            Span::new("─".repeat(width as usize)).fg(Color::named("darkGray")),
        ]));

        for hunk in &diff.hunks {
            lines.push(StyledLine::new(vec![
                Span::new(format!(" {}", hunk.header))
                    .fg(Color::named("cyan"))
                    .dim(),
            ]));
            for dl in &hunk.lines {
                let (prefix, fg, bg) = match dl.tag {
                    git::LineTag::Added   => ("+", Color::named("green"),   Some(Color::rgb(0, 40, 0))),
                    git::LineTag::Removed => ("-", Color::named("red"),     Some(Color::rgb(60, 0, 0))),
                    git::LineTag::Context => (" ", Color::named("gray"),    None),
                };
                let old_num = dl.old_lineno.map(|n| format!("{:>5}", n)).unwrap_or_else(|| "     ".to_string());
                let new_num = dl.new_lineno.map(|n| format!("{:>5}", n)).unwrap_or_else(|| "     ".to_string());

                let mut gutter = Span::new(format!(" {}  {} ", old_num, new_num))
                    .fg(Color::named("darkGray"));
                if let Some(ref b) = bg { gutter = gutter.bg(b.clone()); }

                let mut pfx_span = Span::new(format!("{} ", prefix)).fg(fg.clone());
                if let Some(ref b) = bg { pfx_span = pfx_span.bg(b.clone()); }

                let mut content = Span::new(dl.content.clone()).fg(fg);
                if let Some(b) = bg { content = content.bg(b); }

                lines.push(StyledLine::new(vec![gutter, pfx_span, content]));
            }
        }
        lines
    }

    fn handle_event(&mut self, params: Option<&serde_json::Value>, writer: &mut impl Write) -> bool {
        let event = params.and_then(|p| p.get("event"));
        let key = event
            .and_then(|e| e.get("key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match key {
            "s" => {
                if let Some(ref sel) = self.selected.as_ref().filter(|s| !s.is_staged) {
                    let path = sel.path.clone();
                    if let Some(ref repo) = self.repo {
                        let _ = repo.stage_file(&path);
                        self.refresh();
                        self.request_render(writer);
                    }
                }
                true
            }
            "u" => {
                if let Some(ref sel) = self.selected.as_ref().filter(|s| s.is_staged) {
                    let path = sel.path.clone();
                    if let Some(ref repo) = self.repo {
                        let _ = repo.unstage_file(&path);
                        self.refresh();
                        self.request_render(writer);
                    }
                }
                true
            }
            "r" => {
                self.refresh();
                self.request_render(writer);
                true
            }
            _ => false,
        }
    }

    fn handle_command(&mut self, params: Option<&serde_json::Value>, writer: &mut impl Write) -> bool {
        let id = params.and_then(|p| p.get("id")).and_then(|v| v.as_str()).unwrap_or("");
        let args = params.and_then(|p| p.get("args")).cloned().unwrap_or_default();

        match id {
            "git.selectFile" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let is_staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
                self.selected = Some(SelectedFile { path, is_staged });
                self.diff_scroll = 0;
                self.request_render(writer);
                true
            }
            "git.toggleStaged" => {
                self.staged_collapsed = !self.staged_collapsed;
                self.request_status_render(writer);
                true
            }
            "git.toggleUnstaged" => {
                self.unstaged_collapsed = !self.unstaged_collapsed;
                self.request_status_render(writer);
                true
            }
            "git.stage" => {
                if let Some(ref sel) = self.selected.as_ref().filter(|s| !s.is_staged) {
                    let path = sel.path.clone();
                    if let Some(ref repo) = self.repo {
                        let _ = repo.stage_file(&path);
                        self.refresh();
                        self.request_render(writer);
                    }
                }
                true
            }
            "git.unstage" => {
                if let Some(ref sel) = self.selected.as_ref().filter(|s| s.is_staged) {
                    let path = sel.path.clone();
                    if let Some(ref repo) = self.repo {
                        let _ = repo.unstage_file(&path);
                        self.refresh();
                        self.request_render(writer);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn request_render(&self, writer: &mut impl Write) {
        // Always refresh both panels — status list and diff view
        for panel_id in ["git.status", "git.diff"] {
            let msg = RpcMessage::notification(
                "reef/requestRender",
                serde_json::json!({ "panel_id": panel_id }),
            );
            let _ = write_message(writer, &msg);
        }
    }

    fn request_status_render(&self, writer: &mut impl Write) {
        let msg = RpcMessage::notification(
            "reef/requestRender",
            serde_json::json!({ "panel_id": "git.status" }),
        );
        let _ = write_message(writer, &msg);
    }
}

// ─── File row builder ─────────────────────────────────────────────────────────

fn file_row(path: &str, status: FileStatus, is_staged: bool, max_path: usize) -> StyledLine {
    let status_color = match status {
        FileStatus::Modified  => Color::named("yellow"),
        FileStatus::Added     => Color::named("green"),
        FileStatus::Deleted   => Color::named("red"),
        FileStatus::Renamed   => Color::named("cyan"),
        FileStatus::Untracked => Color::named("green"),
    };
    let status_label = status.label();
    let button = if is_staged { "−" } else { "+" };
    let button_color = if is_staged { Color::named("red") } else { Color::named("green") };

    let display_path = if path.len() > max_path {
        format!("...{}", &path[path.len().saturating_sub(max_path.saturating_sub(3))..])
    } else {
        path.to_string()
    };

    let select_cmd = if is_staged { "git.selectFile" } else { "git.selectFile" };
    let stage_cmd  = if is_staged { "git.unstage" } else { "git.stage" };

    StyledLine::new(vec![
        Span::new("  "),
        Span::new(display_path).fg(Color::named("white")),
        Span::new(format!(" {} ", status_label)).fg(status_color),
        Span::new(button).fg(button_color).bold(),
        Span::new(" "),
    ])
    .on_click(select_cmd, serde_json::json!({ "path": path, "staged": is_staged }))
}
