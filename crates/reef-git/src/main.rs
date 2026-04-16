mod git;
mod tree;

use git::{FileStatus, GitRepo};

use reef_protocol::{
    read_message, write_message, Color, InitializeResult, RenderResult, RpcMessage,
    Span, StyledLine,
};
use std::collections::HashSet;
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
            continue;
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
                let scroll = params
                    .and_then(|p| p.get("scroll"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;

                let lines = state.render_status(width);
                let total = lines.len();
                let visible: Vec<_> = lines.into_iter()
                    .skip(scroll as usize)
                    .take(height as usize)
                    .collect();
                let result = serde_json::to_value(RenderResult {
                    panel_id,
                    lines: visible,
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
    tree_mode: bool,
    collapsed_dirs: HashSet<String>,
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
            tree_mode: false,
            collapsed_dirs: HashSet::new(),
        }
    }

    fn refresh(&mut self) {
        if let Some(ref repo) = self.repo {
            let (s, u) = repo.get_status();
            self.staged = s;
            self.unstaged = u;
        }
    }

    fn render_status(&self, width: u16) -> Vec<StyledLine> {
        let mut lines: Vec<StyledLine> = Vec::new();
        let max_path = (width as usize).saturating_sub(8);

        // View mode toggle
        let mode_label = if self.tree_mode { "视图: 树形" } else { "视图: 列表" };
        lines.push(
            StyledLine::new(vec![
                Span::new(mode_label).fg(Color::named("darkGray")),
            ])
            .on_click("git.toggleTree", serde_json::Value::Null),
        );
        lines.push(StyledLine::plain(""));

        // Staged section
        if !self.staged.is_empty() {
            let arrow = if self.staged_collapsed { "›" } else { "⌄" };
            lines.push(StyledLine::new(vec![
                Span::new(format!("{} ", arrow)).fg(Color::named("white")),
                Span::new("暂存的更改").fg(Color::named("white")).bold(),
                Span::new(format!("  {}", self.staged.len())).fg(Color::named("green")),
            ]).on_click("git.toggleStaged", serde_json::Value::Null));

            if !self.staged_collapsed {
                self.render_files(&self.staged, true, max_path, &mut lines);
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
            self.render_files(&self.unstaged, false, max_path, &mut lines);
            if self.unstaged.is_empty() {
                lines.push(StyledLine::new(vec![
                    Span::new("  无文件").fg(Color::named("darkGray")),
                ]));
            }
        }

        lines
    }

    fn render_files(
        &self,
        files: &[git::FileEntry],
        is_staged: bool,
        max_path: usize,
        out: &mut Vec<StyledLine>,
    ) {
        if self.tree_mode {
            let t = tree::build(files);
            let selected = self.selected.as_ref();
            let mut renderer = |entry: &git::FileEntry, depth: usize, out: &mut Vec<StyledLine>| {
                let is_selected = selected
                    .map(|s| s.path == entry.path && s.is_staged == is_staged)
                    .unwrap_or(false);
                let indent = "  ".repeat(depth);
                let basename = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                out.push(file_row(
                    &entry.path,
                    basename,
                    entry.status,
                    is_staged,
                    max_path,
                    is_selected,
                    &indent,
                ));
            };
            tree::flatten(&t, is_staged, &self.collapsed_dirs, out, &mut renderer);
        } else {
            for file in files {
                let is_selected = self.selected.as_ref()
                    .map(|s| s.path == file.path && s.is_staged == is_staged)
                    .unwrap_or(false);
                out.push(file_row(
                    &file.path,
                    &file.path,
                    file.status,
                    is_staged,
                    max_path,
                    is_selected,
                    "  ",
                ));
            }
        }
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
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
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
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            "r" => {
                self.refresh();
                self.notify_status_changed(writer);
                self.request_status_render(writer);
                true
            }
            "t" => {
                self.tree_mode = !self.tree_mode;
                self.request_status_render(writer);
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
                self.request_status_render(writer);
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
            "git.toggleTree" => {
                self.tree_mode = !self.tree_mode;
                self.request_status_render(writer);
                true
            }
            "git.toggleDir" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let is_staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
                if !path.is_empty() {
                    let key = tree::collapsed_key(is_staged, path);
                    if self.collapsed_dirs.contains(&key) {
                        self.collapsed_dirs.remove(&key);
                    } else {
                        self.collapsed_dirs.insert(key);
                    }
                    self.request_status_render(writer);
                }
                true
            }
            "git.stage" => {
                let path = args.get("path").and_then(|v| v.as_str()).map(|s| s.to_string())
                    .or_else(|| self.selected.as_ref().filter(|s| !s.is_staged).map(|s| s.path.clone()));
                if let Some(path) = path {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.stage_file(&path);
                        if let Some(ref mut sel) = self.selected {
                            if sel.path == path { sel.is_staged = true; }
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            "git.unstage" => {
                let path = args.get("path").and_then(|v| v.as_str()).map(|s| s.to_string())
                    .or_else(|| self.selected.as_ref().filter(|s| s.is_staged).map(|s| s.path.clone()));
                if let Some(path) = path {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.unstage_file(&path);
                        if let Some(ref mut sel) = self.selected {
                            if sel.path == path { sel.is_staged = false; }
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn request_status_render(&self, writer: &mut impl Write) {
        let msg = RpcMessage::notification(
            "reef/requestRender",
            serde_json::json!({ "panel_id": "git.status" }),
        );
        let _ = write_message(writer, &msg);
    }

    fn notify_status_changed(&self, writer: &mut impl Write) {
        let msg = RpcMessage::notification("reef/statusChanged", serde_json::json!({}));
        let _ = write_message(writer, &msg);
    }
}

// ─── File row builder ─────────────────────────────────────────────────────────

fn file_row(
    path: &str,
    display: &str,
    status: FileStatus,
    is_staged: bool,
    max_path: usize,
    is_selected: bool,
    indent: &str,
) -> StyledLine {
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
    let button_cmd  = if is_staged { "git.unstage" } else { "git.stage" };

    let display_path = if display.len() > max_path {
        format!("...{}", &display[display.len().saturating_sub(max_path.saturating_sub(3))..])
    } else {
        display.to_string()
    };

    let sel_bg = Color::rgb(40, 60, 100);

    let mut spans = vec![
        Span::new(indent.to_string()),
        Span::new(display_path).fg(Color::named("white")),
        Span::new(format!(" {} ", status_label)).fg(status_color),
        Span::new(button).fg(button_color).bold()
            .on_click(button_cmd, serde_json::json!({ "path": path })),
        Span::new(" "),
    ];

    if is_selected {
        spans = spans.into_iter().map(|s| s.bg(sel_bg.clone())).collect();
    }

    StyledLine::new(spans)
        .on_click("git.selectFile", serde_json::json!({ "path": path, "staged": is_staged }))
}
