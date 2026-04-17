mod git;
mod graph;
mod prefs;
mod tree;
mod watcher;

use git::{CommitDetail, DiffContent, FileStatus, GitRepo, LineTag, RefLabel};

use reef_protocol::{
    read_message, write_message, Color, InitializeResult, RenderResult, RpcMessage,
    Span, StyledLine,
};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, Stdout};
use std::sync::{Arc, Mutex};
use unicode_width::UnicodeWidthStr;

/// Thread-safe stdout wrapper. Holds a mutex so the fs-watcher thread and the
/// main loop can both emit JSON-RPC messages without interleaving frames.
#[derive(Clone)]
pub struct Writer {
    inner: Arc<Mutex<Stdout>>,
}

impl Writer {
    pub fn new(stdout: Stdout) -> Self {
        Self {
            inner: Arc::new(Mutex::new(stdout)),
        }
    }

    pub fn send(&self, msg: &RpcMessage) {
        if let Ok(mut w) = self.inner.lock() {
            let _ = write_message(&mut *w, msg);
        }
    }
}

fn main() {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let writer = Writer::new(io::stdout());

    let mut state = PluginState::new();

    if let Some(ref repo) = state.repo {
        if let Some(workdir) = repo.workdir() {
            let workdir = workdir.to_path_buf();
            let gitdir = repo.gitdir().to_path_buf();
            watcher::spawn(workdir, gitdir, writer.clone());
        }
    }

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
                writer.send(&RpcMessage::response(msg.id.unwrap_or(0), result));
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

                let lines = match panel_id.as_str() {
                    "git.graph" => {
                        state.refresh_graph();
                        state.render_graph(width)
                    }
                    "git.commitDetail" => state.render_commit_detail(width),
                    _ => {
                        // Pull fresh git state on every render so fs-watcher-triggered
                        // re-renders pick up the latest working tree / index changes.
                        state.refresh();
                        state.render_status(width)
                    }
                };
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
                writer.send(&RpcMessage::response(id, result));
            }

            "reef/event" => {
                let id = msg.id.unwrap_or(0);
                let params = msg.params.as_ref();
                let consumed = state.handle_event(params, &writer);
                let result = serde_json::json!({ "consumed": consumed });
                writer.send(&RpcMessage::response(id, result));
            }

            "reef/command" => {
                let id = msg.id.unwrap_or(0);
                let params = msg.params.as_ref();
                let success = state.handle_command(params, &writer);
                let result = serde_json::json!({ "success": success });
                writer.send(&RpcMessage::response(id, result));
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
    /// Path of the unstaged file pending discard confirmation. While set, the
    /// status panel shows a confirmation banner and `y`/`Esc` are intercepted.
    confirm_discard: Option<String>,

    // ── Graph panel state ──
    graph_rows: Vec<graph::GraphRow>,
    ref_map: HashMap<String, Vec<RefLabel>>,
    /// (HEAD oid, refs-hash). refresh_graph skips rebuild when unchanged, so
    /// working-tree fs events (which fire `statusChanged`) don't trigger a
    /// full revwalk.
    graph_cache_key: Option<(String, u64)>,
    graph_selected_idx: usize,
    selected_commit: Option<String>,
    commit_detail: Option<CommitDetail>,
    /// (file path, diff) for the currently-selected file inside the
    /// currently-selected commit. None means "no file selected, show only
    /// the commit summary".
    commit_file_diff: Option<(String, DiffContent)>,
}

const GRAPH_COMMIT_LIMIT: usize = 500;

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
            tree_mode: prefs::load_tree_mode(),
            collapsed_dirs: HashSet::new(),
            confirm_discard: None,
            graph_rows: Vec::new(),
            ref_map: HashMap::new(),
            graph_cache_key: None,
            graph_selected_idx: 0,
            selected_commit: None,
            commit_detail: None,
            commit_file_diff: None,
        }
    }

    fn refresh(&mut self) {
        if let Some(ref repo) = self.repo {
            let (s, u) = repo.get_status();
            self.staged = s;
            self.unstaged = u;
        }
    }

    /// Rebuild the commit graph iff HEAD or any ref moved since the last build.
    /// Fs events that only touch the working tree don't invalidate the cache.
    fn refresh_graph(&mut self) {
        let Some(ref repo) = self.repo else {
            self.graph_rows.clear();
            self.ref_map.clear();
            self.graph_cache_key = None;
            return;
        };

        let head = repo.head_oid().unwrap_or_default();
        let refs = repo.list_refs();
        let refs_hash = hash_ref_map(&refs);
        let key = (head, refs_hash);

        if self.graph_cache_key.as_ref() == Some(&key) {
            // Nothing ref-y changed; reuse cached rows & refs.
            return;
        }

        let commits = repo.list_commits(GRAPH_COMMIT_LIMIT);
        let rows = graph::build_graph(&commits);

        // Clamp selection if the graph got shorter (e.g. reset --hard).
        if self.graph_selected_idx >= rows.len() {
            self.graph_selected_idx = rows.len().saturating_sub(1);
        }
        // Sync selected_commit to the row at the current index.
        self.selected_commit = rows.get(self.graph_selected_idx).map(|r| r.commit.oid.clone());

        self.graph_rows = rows;
        self.ref_map = refs;
        self.graph_cache_key = Some(key);

        // Re-load detail for the newly-selected commit (idx may have shifted
        // if the graph shrank, or selected_commit may no longer exist).
        self.load_commit_detail();
    }

    /// (Re)load commit detail for `selected_commit`. Clears detail + any
    /// previously-selected file diff whenever the target commit changes.
    fn load_commit_detail(&mut self) {
        self.commit_detail = match (&self.repo, &self.selected_commit) {
            (Some(repo), Some(oid)) => repo.get_commit(oid),
            _ => None,
        };
        self.commit_file_diff = None;
    }

    fn load_commit_file_diff(&mut self, path: &str) {
        self.commit_file_diff = match (&self.repo, &self.selected_commit) {
            (Some(repo), Some(oid)) => repo
                .get_commit_file_diff(oid, path, 3)
                .map(|diff| (path.to_string(), diff)),
            _ => None,
        };
    }

    fn render_commit_detail(&self, width: u16) -> Vec<StyledLine> {
        let Some(detail) = &self.commit_detail else {
            return vec![StyledLine::new(vec![
                Span::new("  选择一个 commit 查看详情").fg(Color::named("darkGray")),
            ])];
        };

        let info = &detail.info;
        let max_msg = (width as usize).saturating_sub(4);
        let max_path = (width as usize).saturating_sub(6);
        let mut lines = Vec::new();

        lines.push(StyledLine::new(vec![
            Span::new("commit ").fg(Color::named("darkGray")),
            Span::new(info.oid.clone()).fg(Color::named("yellow")).bold(),
        ]));
        lines.push(StyledLine::new(vec![
            Span::new("Author: ").fg(Color::named("darkGray")),
            Span::new(format!("{} <{}>", info.author_name, info.author_email))
                .fg(Color::named("white")),
        ]));
        lines.push(StyledLine::new(vec![
            Span::new("Date:   ").fg(Color::named("darkGray")),
            Span::new(format_timestamp(info.time)).fg(Color::named("white")),
        ]));

        // Inline ref labels if the commit is named by HEAD/branches/tags.
        if let Some(labels) = self.ref_map.get(&info.oid) {
            let mut spans: Vec<Span> = vec![Span::new("Refs:   ").fg(Color::named("darkGray"))];
            for label in labels {
                spans.push(ref_label_span(label));
                spans.push(Span::new(" "));
            }
            lines.push(StyledLine::new(spans));
        }

        lines.push(StyledLine::plain(""));

        // Commit message (indented 4 cols, one line per newline)
        for raw in detail.message.lines() {
            let mut msg = raw.to_string();
            truncate_in_place(&mut msg, max_msg);
            lines.push(StyledLine::new(vec![
                Span::new("    "),
                Span::new(msg).fg(Color::named("white")),
            ]));
        }

        lines.push(StyledLine::plain(""));
        lines.push(StyledLine::new(vec![
            Span::new(format!("Changed files ({})", detail.files.len()))
                .fg(Color::named("green"))
                .bold(),
        ]));

        let selected_file = self.commit_file_diff.as_ref().map(|(p, _)| p.as_str());
        let sel_bg = Color::rgb(40, 60, 100);

        for file in &detail.files {
            let status_color = match file.status {
                FileStatus::Modified => "yellow",
                FileStatus::Added => "green",
                FileStatus::Deleted => "red",
                FileStatus::Renamed => "cyan",
                FileStatus::Untracked => "green",
            };
            let mut path = file.path.clone();
            truncate_in_place(&mut path, max_path);

            let is_selected = selected_file == Some(file.path.as_str());
            let mut spans = vec![
                Span::new("  "),
                Span::new(format!("{} ", file.status.label())).fg(Color::named(status_color)),
                Span::new(path).fg(Color::named("white")),
            ];
            if is_selected {
                spans = spans.into_iter().map(|s| s.bg(sel_bg.clone())).collect();
            }
            lines.push(
                StyledLine::new(spans).on_click(
                    "git.selectCommitFile",
                    serde_json::json!({ "oid": info.oid, "path": file.path }),
                ),
            );
        }

        // Selected file's diff (inline, below the file list).
        if let Some((_, diff)) = &self.commit_file_diff {
            lines.push(StyledLine::plain(""));
            append_diff_lines(&mut lines, diff);
        }

        lines
    }

    fn render_graph(&self, width: u16) -> Vec<StyledLine> {
        if self.graph_rows.is_empty() {
            return vec![StyledLine::new(vec![
                Span::new("  无 commit").fg(Color::named("darkGray")),
            ])];
        }

        let show_meta = width >= 60;
        let max_subject = (width as usize).saturating_sub(if show_meta { 40 } else { 14 });
        let head_oid = self.repo.as_ref().and_then(|r| r.head_oid());

        let mut lines = Vec::with_capacity(self.graph_rows.len());
        for (idx, row) in self.graph_rows.iter().enumerate() {
            lines.push(self.graph_row_line(
                row,
                idx == self.graph_selected_idx,
                show_meta,
                max_subject,
                head_oid.as_deref(),
            ));
        }
        lines
    }

    fn render_status(&self, width: u16) -> Vec<StyledLine> {
        let mut lines: Vec<StyledLine> = Vec::new();
        // Slightly narrower budget to accommodate the extra ↺ discard button on unstaged rows.
        let max_path = (width as usize).saturating_sub(10);

        // ── Confirmation banner ──────────────────────────────────────────────
        if let Some(ref path) = self.confirm_discard {
            let mut display = path.clone();
            truncate_in_place(&mut display, max_path);
            lines.push(StyledLine::new(vec![
                Span::new("  ⚠ 还原 ").fg(Color::named("yellow")).bold(),
                Span::new(display).fg(Color::named("white")).bold(),
                Span::new("？（不可撤销）").fg(Color::named("yellow")),
            ]));
            lines.push(StyledLine::new(vec![
                Span::new("  "),
                Span::new(" 确认还原 ")
                    .fg(Color::named("black"))
                    .bg(Color::named("red"))
                    .bold()
                    .on_click("git.discardConfirm", serde_json::Value::Null),
                Span::new("  "),
                Span::new(" 取消 ")
                    .fg(Color::named("white"))
                    .bg(Color::named("darkGray"))
                    .on_click("git.discardCancel", serde_json::Value::Null),
                Span::new("  "),
                Span::new("(y / Esc)").fg(Color::named("darkGray")),
            ]));
            lines.push(StyledLine::plain(""));
        }

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
            lines.push(section_header(
                self.staged_collapsed,
                "暂存的更改",
                self.staged.len(),
                Color::named("green"),
                "git.toggleStaged",
                Some(("取消全部", "git.unstageAll", Color::named("red"))),
                width,
            ));

            if !self.staged_collapsed {
                self.render_files(&self.staged, true, max_path, &mut lines);
            }
            lines.push(StyledLine::plain(""));
        }

        // Unstaged section
        let unstaged_button = if self.unstaged.is_empty() {
            None
        } else {
            Some(("暂存全部", "git.stageAll", Color::named("green")))
        };
        lines.push(section_header(
            self.unstaged_collapsed,
            "更改",
            self.unstaged.len(),
            Color::named("blue"),
            "git.toggleUnstaged",
            unstaged_button,
            width,
        ));

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

    fn handle_event(&mut self, params: Option<&serde_json::Value>, writer: &Writer) -> bool {
        let panel_id = params
            .and_then(|p| p.get("panel_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let event = params.and_then(|p| p.get("event"));
        let key = event
            .and_then(|e| e.get("key"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if panel_id == "git.graph" {
            return self.handle_graph_key(key, writer);
        }

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
            "d" => {
                // Enter discard confirmation for the selected unstaged tracked file.
                let path = self
                    .selected
                    .as_ref()
                    .filter(|s| !s.is_staged)
                    .and_then(|sel| {
                        self.unstaged
                            .iter()
                            .find(|f| f.path == sel.path)
                            .map(|f| f.path.clone())
                    });
                if let Some(path) = path {
                    self.confirm_discard = Some(path);
                    self.request_status_render(writer);
                }
                true
            }
            "y" => {
                if let Some(path) = self.confirm_discard.take() {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.restore_file(&path);
                        if self.selected.as_ref().map(|s| s.path == path).unwrap_or(false) {
                            self.selected = None;
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                    true
                } else {
                    false
                }
            }
            "n" | "Escape" => {
                if self.confirm_discard.is_some() {
                    self.confirm_discard = None;
                    self.request_status_render(writer);
                    true
                } else {
                    false
                }
            }
            "r" => {
                self.refresh();
                self.notify_status_changed(writer);
                self.request_status_render(writer);
                true
            }
            "t" => {
                self.tree_mode = !self.tree_mode;
                prefs::save_tree_mode(self.tree_mode);
                self.request_status_render(writer);
                true
            }
            _ => false,
        }
    }

    fn handle_command(&mut self, params: Option<&serde_json::Value>, writer: &Writer) -> bool {
        let id = params.and_then(|p| p.get("id")).and_then(|v| v.as_str()).unwrap_or("");
        let args = params.and_then(|p| p.get("args")).cloned().unwrap_or_default();

        match id {
            "git.selectFile" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let is_staged = args.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
                self.selected = Some(SelectedFile { path, is_staged });
                self.confirm_discard = None;
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
                prefs::save_tree_mode(self.tree_mode);
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
            "git.discardPrompt" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !path.is_empty() {
                    self.selected = Some(SelectedFile { path: path.clone(), is_staged: false });
                    self.confirm_discard = Some(path);
                    self.request_status_render(writer);
                }
                true
            }
            "git.discardConfirm" => {
                if let Some(path) = self.confirm_discard.take() {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.restore_file(&path);
                        if self.selected.as_ref().map(|s| s.path == path).unwrap_or(false) {
                            self.selected = None;
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            "git.discardCancel" => {
                self.confirm_discard = None;
                self.request_status_render(writer);
                true
            }
            "git.stageAll" => {
                if let Some(ref repo) = self.repo {
                    let paths: Vec<String> =
                        self.unstaged.iter().map(|f| f.path.clone()).collect();
                    for p in &paths {
                        let _ = repo.stage_file(p);
                    }
                    if let Some(ref mut sel) = self.selected {
                        if paths.iter().any(|p| p == &sel.path) {
                            sel.is_staged = true;
                        }
                    }
                    self.refresh();
                    self.notify_status_changed(writer);
                    self.request_status_render(writer);
                }
                true
            }
            "git.unstageAll" => {
                if let Some(ref repo) = self.repo {
                    let paths: Vec<String> =
                        self.staged.iter().map(|f| f.path.clone()).collect();
                    for p in &paths {
                        let _ = repo.unstage_file(p);
                    }
                    if let Some(ref mut sel) = self.selected {
                        if paths.iter().any(|p| p == &sel.path) {
                            sel.is_staged = false;
                        }
                    }
                    self.refresh();
                    self.notify_status_changed(writer);
                    self.request_status_render(writer);
                }
                true
            }
            "git.graph.next" => { self.move_graph_selection(1, writer); true }
            "git.graph.prev" => { self.move_graph_selection(-1, writer); true }
            "git.selectCommit" => {
                let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
                if !oid.is_empty() {
                    if let Some(idx) = self.graph_rows.iter().position(|r| r.commit.oid == oid) {
                        self.graph_selected_idx = idx;
                        self.selected_commit = Some(oid.to_string());
                        self.load_commit_detail();
                        self.request_graph_render(writer);
                        self.request_commit_detail_render(writer);
                    }
                }
                true
            }
            "git.selectCommitFile" => {
                let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if !oid.is_empty() && !path.is_empty() {
                    // If a different commit was clicked, switch commit first.
                    if self.selected_commit.as_deref() != Some(oid) {
                        if let Some(idx) = self.graph_rows.iter().position(|r| r.commit.oid == oid)
                        {
                            self.graph_selected_idx = idx;
                        }
                        self.selected_commit = Some(oid.to_string());
                        self.load_commit_detail();
                    }
                    self.load_commit_file_diff(path);
                    self.request_commit_detail_render(writer);
                }
                true
            }
            "git.graph.refresh" => {
                self.graph_cache_key = None;
                self.refresh_graph();
                self.request_graph_render(writer);
                true
            }
            _ => false,
        }
    }

    fn request_status_render(&self, writer: &Writer) {
        writer.send(&RpcMessage::notification(
            "reef/requestRender",
            serde_json::json!({ "panel_id": "git.status" }),
        ));
    }

    fn request_graph_render(&self, writer: &Writer) {
        writer.send(&RpcMessage::notification(
            "reef/requestRender",
            serde_json::json!({ "panel_id": "git.graph" }),
        ));
    }

    fn request_commit_detail_render(&self, writer: &Writer) {
        writer.send(&RpcMessage::notification(
            "reef/requestRender",
            serde_json::json!({ "panel_id": "git.commitDetail" }),
        ));
    }

    fn notify_status_changed(&self, writer: &Writer) {
        writer.send(&RpcMessage::notification(
            "reef/statusChanged",
            serde_json::json!({}),
        ));
    }

    fn handle_graph_key(&mut self, key: &str, writer: &Writer) -> bool {
        match key {
            "j" | "ArrowDown" => {
                self.move_graph_selection(1, writer);
                true
            }
            "k" | "ArrowUp" => {
                self.move_graph_selection(-1, writer);
                true
            }
            _ => false,
        }
    }

    fn move_graph_selection(&mut self, delta: i32, writer: &Writer) {
        if self.graph_rows.is_empty() {
            return;
        }
        let last = self.graph_rows.len() - 1;
        let current = self.graph_selected_idx as i32;
        let next = (current + delta).clamp(0, last as i32) as usize;
        if next == self.graph_selected_idx {
            return;
        }
        self.graph_selected_idx = next;
        self.selected_commit = self
            .graph_rows
            .get(next)
            .map(|r| r.commit.oid.clone());
        self.load_commit_detail();
        self.request_graph_render(writer);
        self.request_commit_detail_render(writer);
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
    // Double-click anywhere on the row toggles staging (stage/unstage).
    let dbl_cmd = if is_staged { "git.unstage" } else { "git.stage" };

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

    // Discard button — only for unstaged files.
    if !is_staged {
        spans.push(
            Span::new("↺")
                .fg(Color::named("red"))
                .on_click("git.discardPrompt", serde_json::json!({ "path": path })),
        );
        spans.push(Span::new(" "));
    }

    if is_selected {
        spans = spans.into_iter().map(|s| s.bg(sel_bg.clone())).collect();
    }

    StyledLine::new(spans)
        .on_click("git.selectFile", serde_json::json!({ "path": path, "staged": is_staged }))
        .on_dbl_click(dbl_cmd, serde_json::json!({ "path": path }))
}

// ─── Section header builder ───────────────────────────────────────────────────

/// Build a collapsible section header with an optional right-aligned action button.
fn section_header(
    collapsed: bool,
    label: &str,
    count: usize,
    count_color: Color,
    toggle_cmd: &str,
    action: Option<(&str, &str, Color)>,
    width: u16,
) -> StyledLine {
    let arrow = if collapsed { "›" } else { "⌄" };
    let prefix = format!("{} ", arrow);
    let count_str = format!("  {}", count);

    // Compute right-side padding so the action button sits at the panel's edge.
    let button_text = action.as_ref().map(|(t, _, _)| format!(" {} ", t));
    let used = prefix.width() + label.width() + count_str.width()
        + button_text.as_deref().map(str::width).unwrap_or(0);
    let padding = (width as usize).saturating_sub(used);

    let mut spans = vec![
        Span::new(prefix).fg(Color::named("white")),
        Span::new(label.to_string()).fg(Color::named("white")).bold(),
        Span::new(count_str).fg(count_color),
    ];
    if padding > 0 {
        spans.push(Span::new(" ".repeat(padding)));
    }
    if let (Some(text), Some((_, cmd, color))) = (button_text, action) {
        spans.push(
            Span::new(text)
                .fg(color)
                .bold()
                .on_click(cmd, serde_json::Value::Null),
        );
    }

    StyledLine::new(spans).on_click(toggle_cmd, serde_json::Value::Null)
}

// ─── Graph row rendering ──────────────────────────────────────────────────────

impl PluginState {
    fn graph_row_line(
        &self,
        row: &graph::GraphRow,
        selected: bool,
        show_meta: bool,
        max_subject: usize,
        head_oid: Option<&str>,
    ) -> StyledLine {
        let oid = row.commit.oid.clone();
        let sel_bg = Color::rgb(40, 60, 100);
        let is_head = head_oid == Some(oid.as_str());

        let mut spans: Vec<Span> = Vec::new();

        // Per-col glyph with horizontal fill for merge/fork connectors.
        let glyphs = render_lane_chars(row);
        for (col, ch) in glyphs.iter().enumerate() {
            let color = lane_color_for(col);
            let mut span = Span::new(ch.to_string()).fg(color);
            if col == row.node_col && *ch == '●' && is_head {
                span = span.bold();
            } else if *ch == '●' {
                span = span.dim();
            }
            spans.push(span);
        }
        spans.push(Span::new(" "));

        // Short oid
        spans.push(
            Span::new(row.commit.short_oid.clone())
                .fg(Color::named("yellow"))
                .dim(),
        );
        spans.push(Span::new(" "));

        if show_meta {
            let mut author = row.commit.author_name.clone();
            truncate_in_place(&mut author, 12);
            spans.push(
                Span::new(format!("{:<12}", author)).fg(Color::named("cyan")),
            );
            spans.push(Span::new(" "));

            let rel = relative_time(row.commit.time);
            spans.push(
                Span::new(format!("{:>4}", rel)).fg(Color::named("darkGray")),
            );
            spans.push(Span::new(" "));
        }

        // Ref labels (HEAD, branches, tags) before subject
        if let Some(labels) = self.ref_map.get(&oid) {
            for label in labels {
                spans.push(ref_label_span(label));
                spans.push(Span::new(" "));
            }
        }

        // Subject
        let mut subject = row.commit.subject.clone();
        truncate_in_place(&mut subject, max_subject);
        spans.push(Span::new(subject).fg(Color::named("white")));

        if selected {
            spans = spans
                .into_iter()
                .map(|s| s.bg(sel_bg.clone()))
                .collect();
        }

        StyledLine::new(spans).on_click("git.selectCommit", serde_json::json!({ "oid": oid }))
    }
}

/// Compute the per-column glyph for a row, filling horizontal connectors
/// (`─`) across Empty cells that sit between a Merge/Fork cell and the
/// node column it links to.
fn render_lane_chars(row: &graph::GraphRow) -> Vec<char> {
    let mut glyphs: Vec<char> = row
        .cells
        .iter()
        .enumerate()
        .map(|(col, cell)| match cell {
            graph::LaneCell::Empty => ' ',
            graph::LaneCell::Pass => '│',
            graph::LaneCell::Node => '●',
            graph::LaneCell::Merge { from } => {
                if col < *from { '├' } else { '┤' }
            }
            graph::LaneCell::Fork { to } => {
                if col < *to { '╭' } else { '╮' }
            }
        })
        .collect();

    // Fill horizontal connectors between each merge/fork cell and its target
    for (col, cell) in row.cells.iter().enumerate() {
        let target = match cell {
            graph::LaneCell::Merge { from } => Some(*from),
            graph::LaneCell::Fork { to } => Some(*to),
            _ => None,
        };
        let Some(target) = target else { continue };
        let (lo, hi) = if col < target {
            (col + 1, target)
        } else {
            (target + 1, col)
        };
        for k in lo..hi {
            if matches!(row.cells.get(k), Some(graph::LaneCell::Empty)) {
                glyphs[k] = '─';
            }
        }
    }

    glyphs
}

/// Render a DiffContent into styled lines: one header span per hunk, then
/// per-line `+`/`-`/` ` with green/red/default coloring. Minimal version — no
/// line numbers (plain unified patch look).
fn append_diff_lines(out: &mut Vec<StyledLine>, diff: &DiffContent) {
    for hunk in &diff.hunks {
        out.push(StyledLine::new(vec![
            Span::new(hunk.header.clone()).fg(Color::named("cyan")),
        ]));
        for line in &hunk.lines {
            let (prefix, fg) = match line.tag {
                LineTag::Added => ("+", "green"),
                LineTag::Removed => ("-", "red"),
                LineTag::Context => (" ", "white"),
            };
            out.push(StyledLine::new(vec![
                Span::new(prefix.to_string()).fg(Color::named(fg)),
                Span::new(line.content.clone()).fg(Color::named(fg)),
            ]));
        }
    }
}

fn ref_label_span(label: &RefLabel) -> Span {
    let (text, fg, bg) = match label {
        RefLabel::Head => (" HEAD ".to_string(), "black", "cyan"),
        RefLabel::Branch(n) => (format!(" {} ", n), "black", "green"),
        RefLabel::RemoteBranch(n) => (format!(" {} ", n), "white", "darkGray"),
        RefLabel::Tag(n) => (format!(" tag: {} ", n), "black", "yellow"),
    };
    Span::new(text)
        .fg(Color::named(fg))
        .bg(Color::named(bg))
        .bold()
}

/// Truncate a string (in-place) to at most `max` Unicode chars, appending `…`.
fn truncate_in_place(s: &mut String, max: usize) {
    if max == 0 {
        s.clear();
        return;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return;
    }
    let kept: String = chars.into_iter().take(max.saturating_sub(1)).collect();
    *s = format!("{}…", kept);
}

fn lane_color_for(col: usize) -> Color {
    // Rotate through a small palette so parallel lanes get distinct colors.
    const PALETTE: &[&str] = &["cyan", "magenta", "green", "yellow", "blue", "red"];
    Color::named(PALETTE[col % PALETTE.len()])
}

/// Format a unix timestamp (seconds since epoch, UTC) as "YYYY-MM-DD HH:MM:SS UTC".
/// Uses Howard Hinnant's civil_from_days conversion; no external deps.
fn format_timestamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// Civil-from-days algorithm (Howard Hinnant). Days are counted from 1970-01-01.
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Compute a short relative-time string ("2h", "3d", "5mo", "1y") from a
/// unix timestamp. Seconds/minutes collapse to "now" / "Nm".
fn relative_time(author_time: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(author_time);
    let diff = (now - author_time).max(0);
    if diff < 60 {
        "now".into()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}d", diff / 86_400)
    } else if diff < 86_400 * 365 {
        format!("{}mo", diff / (86_400 * 30))
    } else {
        format!("{}y", diff / (86_400 * 365))
    }
}

/// Stable hash of the ref map — used as part of the graph cache key.
fn hash_ref_map(map: &HashMap<String, Vec<RefLabel>>) -> u64 {
    let mut entries: Vec<(&String, &Vec<RefLabel>)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (oid, labels) in entries {
        oid.hash(&mut hasher);
        for label in labels {
            match label {
                RefLabel::Head => 0u8.hash(&mut hasher),
                RefLabel::Branch(n) => { 1u8.hash(&mut hasher); n.hash(&mut hasher); }
                RefLabel::RemoteBranch(n) => { 2u8.hash(&mut hasher); n.hash(&mut hasher); }
                RefLabel::Tag(n) => { 3u8.hash(&mut hasher); n.hash(&mut hasher); }
            }
        }
    }
    hasher.finish()
}
