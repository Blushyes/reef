use reef_git::git::{CommitDetail, DiffContent, FileStatus, GitRepo, LineTag, RefLabel};
use reef_git::writer::Writer;
use reef_git::{git, graph, prefs, tree, watcher};

use reef_protocol::{
    Color, InitializeResult, RenderResult, RpcMessage, Span, StyledLine, read_message,
};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader};
use unicode_width::UnicodeWidthStr;

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
                let visible: Vec<_> = lines
                    .into_iter()
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
    /// Set while a normal-push confirmation banner is shown. Mutually
    /// exclusive with `confirm_force_push` by UI convention — only one
    /// confirmation is ever active.
    confirm_push: bool,
    /// Set while a force-push confirmation banner is shown. Same y/Esc
    /// interception as discard.
    confirm_force_push: bool,
    /// Last push failure to surface in the status panel. Cleared by a
    /// successful push or an explicit dismiss. Kept as a plain string — we
    /// don't need structured error info, just what git told the user.
    push_error: Option<String>,

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
    /// Layout for the inline commit-file diff (mirrors Git-tab's `m` toggle).
    commit_diff_layout: DiffLayout,
    /// Mode for the inline commit-file diff (mirrors Git-tab's `f` toggle).
    commit_diff_mode: DiffMode,
    /// Tree vs. flat view for the Changed-files list in commit detail.
    commit_files_tree_mode: bool,
    /// Collapsed directories inside the commit-detail tree view. Kept
    /// separate from `collapsed_dirs` (status panel) so the two views don't
    /// share collapse state. Keyed by full path, no staged/unstaged prefix.
    commit_files_collapsed: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLayout {
    Unified,    // 上下
    SideBySide, // 左右
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Compact,  // 局部（3 行 context）
    FullFile, // 全量
}

impl DiffMode {
    fn context_lines(self) -> u32 {
        match self {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        }
    }
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
            confirm_push: false,
            confirm_force_push: false,
            push_error: None,
            graph_rows: Vec::new(),
            ref_map: HashMap::new(),
            graph_cache_key: None,
            graph_selected_idx: 0,
            selected_commit: None,
            commit_detail: None,
            commit_file_diff: None,
            commit_diff_layout: match prefs::load_commit_diff_layout() {
                "side_by_side" => DiffLayout::SideBySide,
                _ => DiffLayout::Unified,
            },
            commit_diff_mode: match prefs::load_commit_diff_mode() {
                "full_file" => DiffMode::FullFile,
                _ => DiffMode::Compact,
            },
            commit_files_tree_mode: prefs::load_commit_files_tree_mode(),
            commit_files_collapsed: HashSet::new(),
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
        self.selected_commit = rows
            .get(self.graph_selected_idx)
            .map(|r| r.commit.oid.clone());

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
        let context = self.commit_diff_mode.context_lines();
        self.commit_file_diff = match (&self.repo, &self.selected_commit) {
            (Some(repo), Some(oid)) => repo
                .get_commit_file_diff(oid, path, context)
                .map(|diff| (path.to_string(), diff)),
            _ => None,
        };
    }

    /// Reload the currently-selected commit file's diff (used after toggling
    /// `commit_diff_mode`, since that changes the context-lines argument).
    fn reload_commit_file_diff(&mut self) {
        let path = self.commit_file_diff.as_ref().map(|(p, _)| p.clone());
        if let Some(path) = path {
            self.load_commit_file_diff(&path);
        }
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
            Span::new(info.oid.clone())
                .fg(Color::named("yellow"))
                .bold(),
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
        let view_label = if self.commit_files_tree_mode {
            "树形"
        } else {
            "列表"
        };
        lines.push(StyledLine::new(vec![
            Span::new(format!("Changed files ({})", detail.files.len()))
                .fg(Color::named("green"))
                .bold(),
            Span::new(format!("  [{}]  t 切换", view_label))
                .fg(Color::named("darkGray"))
                .on_click("git.toggleCommitFilesView", serde_json::Value::Null),
        ]));

        let ctx = CommitFilesCtx {
            selected_file: self.commit_file_diff.as_ref().map(|(p, _)| p.as_str()),
            sel_bg: Color::rgb(40, 60, 100),
            max_path,
            commit_oid: &info.oid,
            collapsed: &self.commit_files_collapsed,
        };

        if self.commit_files_tree_mode {
            let nodes = tree::build(&detail.files);
            render_commit_file_tree(&nodes, 1, &ctx, &mut lines);
        } else {
            for file in &detail.files {
                lines.push(commit_file_row(file, &file.path, "  ", &ctx));
            }
        }

        // Selected file's diff (inline, below the file list).
        if let Some((path, diff)) = &self.commit_file_diff {
            lines.push(StyledLine::plain(""));
            lines.push(diff_header_line(
                path,
                self.commit_diff_layout,
                self.commit_diff_mode,
                width,
            ));
            lines.push(diff_separator_line(width));
            append_diff_lines(&mut lines, diff, self.commit_diff_layout, width);
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

        // ── Push error banner (dismissable) ──────────────────────────────────
        if let Some(ref err) = self.push_error {
            let mut msg = err.clone();
            // Keep to single-line; users can run `git push` manually for full output.
            truncate_in_place(&mut msg, max_path);
            lines.push(
                StyledLine::new(vec![
                    Span::new("  ✖ 推送失败: ").fg(Color::named("red")).bold(),
                    Span::new(msg).fg(Color::named("white")),
                    Span::new("  [关闭]").fg(Color::named("darkGray")),
                ])
                .on_click("git.dismissPushError", serde_json::Value::Null),
            );
            lines.push(StyledLine::plain(""));
        }

        // ── Push / force-push confirmation banner ────────────────────────────
        // Both share the same shape (prompt + confirm/cancel + keybinding hint);
        // only the wording and button styling differ.
        if self.confirm_push || self.confirm_force_push {
            let force = self.confirm_force_push;
            let ahead = self
                .repo
                .as_ref()
                .and_then(|r| r.ahead_behind())
                .map(|(a, _)| a)
                .unwrap_or(0);

            // Prompt line
            if force {
                lines.push(StyledLine::new(vec![
                    Span::new("  ⚠ 强制推送？")
                        .fg(Color::named("yellow"))
                        .bold(),
                    Span::new("（会覆盖远端，使用 --force-with-lease）").fg(Color::named("yellow")),
                ]));
            } else {
                let msg = if ahead > 0 {
                    format!("  推送 {ahead} 个提交到远端？")
                } else {
                    "  推送到远端？".to_string()
                };
                lines.push(StyledLine::new(vec![
                    Span::new(msg).fg(Color::named("white")).bold(),
                ]));
            }

            // Buttons
            let (confirm_label, confirm_bg, confirm_cmd) = if force {
                (" 确认强制推送 ", "red", "git.forcePushConfirm")
            } else {
                (" 确认推送 ", "green", "git.pushConfirm")
            };
            let cancel_cmd = if force {
                "git.forcePushCancel"
            } else {
                "git.pushCancel"
            };
            lines.push(StyledLine::new(vec![
                Span::new("  "),
                Span::new(confirm_label)
                    .fg(Color::named("black"))
                    .bg(Color::named(confirm_bg))
                    .bold()
                    .on_click(confirm_cmd, serde_json::Value::Null),
                Span::new("  "),
                Span::new(" 取消 ")
                    .fg(Color::named("white"))
                    .bg(Color::named("darkGray"))
                    .on_click(cancel_cmd, serde_json::Value::Null),
                Span::new("  "),
                Span::new("(y / Esc)").fg(Color::named("darkGray")),
            ]));
            lines.push(StyledLine::plain(""));
        }

        // ── Push indicator (only when working tree is clean) ─────────────────
        // Showing the button while there are uncommitted changes would be
        // misleading — VSCode's git extension only shows it when there's
        // actually nothing else the user should be looking at first.
        if self.staged.is_empty()
            && self.unstaged.is_empty()
            && !self.confirm_push
            && !self.confirm_force_push
        {
            if let Some((ahead, behind)) = self.repo.as_ref().and_then(|r| r.ahead_behind()) {
                if let Some(line) = push_indicator_line(ahead, behind) {
                    lines.push(line);
                    lines.push(StyledLine::plain(""));
                }
            }
        }

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
        let mode_label = if self.tree_mode {
            "视图: 树形"
        } else {
            "视图: 列表"
        };
        lines.push(
            StyledLine::new(vec![Span::new(mode_label).fg(Color::named("darkGray"))])
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
                let is_selected = self
                    .selected
                    .as_ref()
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

        if panel_id == "git.commitDetail" {
            return self.handle_commit_detail_key(key, writer);
        }

        match key {
            "s" => {
                if let Some(sel) = self.selected.as_ref().filter(|s| !s.is_staged) {
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
                if let Some(sel) = self.selected.as_ref().filter(|s| s.is_staged) {
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
                        if self
                            .selected
                            .as_ref()
                            .map(|s| s.path == path)
                            .unwrap_or(false)
                        {
                            self.selected = None;
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                    true
                } else if self.confirm_force_push {
                    self.confirm_force_push = false;
                    self.run_push(true, writer);
                    true
                } else if self.confirm_push {
                    self.confirm_push = false;
                    self.run_push(false, writer);
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
                } else if self.confirm_force_push {
                    self.confirm_force_push = false;
                    self.request_status_render(writer);
                    true
                } else if self.confirm_push {
                    self.confirm_push = false;
                    self.request_status_render(writer);
                    true
                } else if self.push_error.is_some() {
                    self.push_error = None;
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
        let id = params
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let args = params
            .and_then(|p| p.get("args"))
            .cloned()
            .unwrap_or_default();

        match id {
            "git.selectFile" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_staged = args
                    .get("staged")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
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
                let is_staged = args
                    .get("staged")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
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
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        self.selected
                            .as_ref()
                            .filter(|s| !s.is_staged)
                            .map(|s| s.path.clone())
                    });
                if let Some(path) = path {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.stage_file(&path);
                        if let Some(ref mut sel) = self.selected {
                            if sel.path == path {
                                sel.is_staged = true;
                            }
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            "git.unstage" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        self.selected
                            .as_ref()
                            .filter(|s| s.is_staged)
                            .map(|s| s.path.clone())
                    });
                if let Some(path) = path {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.unstage_file(&path);
                        if let Some(ref mut sel) = self.selected {
                            if sel.path == path {
                                sel.is_staged = false;
                            }
                        }
                        self.refresh();
                        self.notify_status_changed(writer);
                        self.request_status_render(writer);
                    }
                }
                true
            }
            "git.discardPrompt" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !path.is_empty() {
                    self.selected = Some(SelectedFile {
                        path: path.clone(),
                        is_staged: false,
                    });
                    self.confirm_discard = Some(path);
                    self.request_status_render(writer);
                }
                true
            }
            "git.discardConfirm" => {
                if let Some(path) = self.confirm_discard.take() {
                    if let Some(ref repo) = self.repo {
                        let _ = repo.restore_file(&path);
                        if self
                            .selected
                            .as_ref()
                            .map(|s| s.path == path)
                            .unwrap_or(false)
                        {
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
            "git.pushPrompt" => {
                self.confirm_push = true;
                self.confirm_force_push = false;
                self.push_error = None;
                self.request_status_render(writer);
                true
            }
            "git.pushConfirm" => {
                self.confirm_push = false;
                self.run_push(false, writer);
                true
            }
            "git.pushCancel" => {
                self.confirm_push = false;
                self.request_status_render(writer);
                true
            }
            "git.forcePushPrompt" => {
                self.confirm_force_push = true;
                self.confirm_push = false;
                self.push_error = None;
                self.request_status_render(writer);
                true
            }
            "git.forcePushConfirm" => {
                self.confirm_force_push = false;
                self.run_push(true, writer);
                true
            }
            "git.forcePushCancel" => {
                self.confirm_force_push = false;
                self.request_status_render(writer);
                true
            }
            "git.dismissPushError" => {
                self.push_error = None;
                self.request_status_render(writer);
                true
            }
            "git.stageAll" => {
                if let Some(ref repo) = self.repo {
                    let paths: Vec<String> = self.unstaged.iter().map(|f| f.path.clone()).collect();
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
                    let paths: Vec<String> = self.staged.iter().map(|f| f.path.clone()).collect();
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
            "git.graph.next" => {
                self.move_graph_selection(1, writer);
                true
            }
            "git.graph.prev" => {
                self.move_graph_selection(-1, writer);
                true
            }
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
            "git.toggleCommitDir" => {
                if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                    if !self.commit_files_collapsed.remove(path) {
                        self.commit_files_collapsed.insert(path.to_string());
                    }
                    self.request_commit_detail_render(writer);
                }
                true
            }
            "git.toggleCommitFilesView" => {
                self.commit_files_tree_mode = !self.commit_files_tree_mode;
                prefs::save_commit_files_tree_mode(self.commit_files_tree_mode);
                self.request_commit_detail_render(writer);
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

    /// Invoke `git push` (or `git push --force-with-lease` when `force`),
    /// store any error for display, and trigger a status re-render. Blocks
    /// the plugin's event loop for the duration of the push — acceptable
    /// because there's no meaningful work we could do concurrently anyway.
    fn run_push(&mut self, force: bool, writer: &Writer) {
        if let Some(ref repo) = self.repo {
            match repo.push(force) {
                Ok(()) => self.push_error = None,
                Err(e) => self.push_error = Some(e),
            }
            // Push updates remote-tracking refs on success, so the graph
            // cache needs to see the new state.
            self.graph_cache_key = None;
            self.refresh();
            self.notify_status_changed(writer);
            self.request_status_render(writer);
            self.request_graph_render(writer);
        }
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

    fn handle_commit_detail_key(&mut self, key: &str, writer: &Writer) -> bool {
        match key {
            "m" => {
                self.commit_diff_layout = match self.commit_diff_layout {
                    DiffLayout::Unified => DiffLayout::SideBySide,
                    DiffLayout::SideBySide => DiffLayout::Unified,
                };
                prefs::save_commit_diff_layout(match self.commit_diff_layout {
                    DiffLayout::Unified => "unified",
                    DiffLayout::SideBySide => "side_by_side",
                });
                self.request_commit_detail_render(writer);
                true
            }
            "f" => {
                self.commit_diff_mode = match self.commit_diff_mode {
                    DiffMode::Compact => DiffMode::FullFile,
                    DiffMode::FullFile => DiffMode::Compact,
                };
                prefs::save_commit_diff_mode(match self.commit_diff_mode {
                    DiffMode::Compact => "compact",
                    DiffMode::FullFile => "full_file",
                });
                self.reload_commit_file_diff();
                self.request_commit_detail_render(writer);
                true
            }
            "t" => {
                self.commit_files_tree_mode = !self.commit_files_tree_mode;
                prefs::save_commit_files_tree_mode(self.commit_files_tree_mode);
                self.request_commit_detail_render(writer);
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
        self.selected_commit = self.graph_rows.get(next).map(|r| r.commit.oid.clone());
        self.load_commit_detail();
        self.request_graph_render(writer);
        self.request_commit_detail_render(writer);
    }
}

// ─── Commit-detail file rows / tree walker ────────────────────────────────────

/// Shared rendering context for the Changed-files list in commit detail. Bundled
/// so `commit_file_row` / `render_commit_file_tree` stay below clippy's
/// too-many-arguments threshold.
struct CommitFilesCtx<'a> {
    selected_file: Option<&'a str>,
    sel_bg: Color,
    max_path: usize,
    commit_oid: &'a str,
    collapsed: &'a HashSet<String>,
}

fn commit_file_row(
    file: &git::FileEntry,
    display_path: &str,
    indent: &str,
    ctx: &CommitFilesCtx,
) -> StyledLine {
    let status_color = match file.status {
        FileStatus::Modified => "yellow",
        FileStatus::Added => "green",
        FileStatus::Deleted => "red",
        FileStatus::Renamed => "cyan",
        FileStatus::Untracked => "green",
    };
    let mut display = display_path.to_string();
    truncate_in_place(&mut display, ctx.max_path);

    let mut spans = vec![
        Span::new(indent.to_string()),
        Span::new(format!("{} ", file.status.label())).fg(Color::named(status_color)),
        Span::new(display).fg(Color::named("white")),
    ];
    if ctx.selected_file == Some(file.path.as_str()) {
        spans = spans.into_iter().map(|s| s.bg(ctx.sel_bg.clone())).collect();
    }
    StyledLine::new(spans).on_click(
        "git.selectCommitFile",
        serde_json::json!({ "oid": ctx.commit_oid, "path": file.path }),
    )
}

fn render_commit_file_tree(
    nodes: &std::collections::BTreeMap<String, tree::Node>,
    depth: usize,
    ctx: &CommitFilesCtx,
    out: &mut Vec<StyledLine>,
) {
    let mut entries: Vec<(&String, &tree::Node)> = nodes.iter().collect();
    // Dirs first, then files; case-insensitive name order within each group
    // — same convention as the status panel's tree view.
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.1, tree::Node::Dir { .. });
        let b_dir = matches!(b.1, tree::Node::Dir { .. });
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });

    for (name, node) in entries {
        match node {
            tree::Node::Dir { path, children } => {
                let is_collapsed = ctx.collapsed.contains(path);
                let arrow = if is_collapsed { "›" } else { "⌄" };
                let indent = "  ".repeat(depth);
                out.push(
                    StyledLine::new(vec![
                        Span::new(indent),
                        Span::new(format!("{} ", arrow)).fg(Color::named("darkGray")),
                        Span::new(format!("{}/", name))
                            .fg(Color::named("cyan"))
                            .bold(),
                    ])
                    .on_click(
                        "git.toggleCommitDir",
                        serde_json::json!({ "path": path }),
                    ),
                );
                if !is_collapsed {
                    render_commit_file_tree(children, depth + 1, ctx, out);
                }
            }
            tree::Node::File(entry) => {
                let basename = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                let indent = "  ".repeat(depth);
                out.push(commit_file_row(entry, basename, &indent, ctx));
            }
        }
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
        FileStatus::Modified => Color::named("yellow"),
        FileStatus::Added => Color::named("green"),
        FileStatus::Deleted => Color::named("red"),
        FileStatus::Renamed => Color::named("cyan"),
        FileStatus::Untracked => Color::named("green"),
    };
    let status_label = status.label();
    let button = if is_staged { "−" } else { "+" };
    let button_color = if is_staged {
        Color::named("red")
    } else {
        Color::named("green")
    };
    let button_cmd = if is_staged {
        "git.unstage"
    } else {
        "git.stage"
    };
    // Double-click anywhere on the row toggles staging (stage/unstage).
    let dbl_cmd = if is_staged {
        "git.unstage"
    } else {
        "git.stage"
    };

    let display_path = if display.len() > max_path {
        format!(
            "...{}",
            &display[display.len().saturating_sub(max_path.saturating_sub(3))..]
        )
    } else {
        display.to_string()
    };

    let sel_bg = Color::rgb(40, 60, 100);

    let mut spans = vec![
        Span::new(indent.to_string()),
        Span::new(display_path).fg(Color::named("white")),
        Span::new(format!(" {} ", status_label)).fg(status_color),
        Span::new(button)
            .fg(button_color)
            .bold()
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
        .on_click(
            "git.selectFile",
            serde_json::json!({ "path": path, "staged": is_staged }),
        )
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
    let used = prefix.width()
        + label.width()
        + count_str.width()
        + button_text.as_deref().map(str::width).unwrap_or(0);
    let padding = (width as usize).saturating_sub(used);

    let mut spans = vec![
        Span::new(prefix).fg(Color::named("white")),
        Span::new(label.to_string())
            .fg(Color::named("white"))
            .bold(),
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
            spans.push(Span::new(format!("{:<12}", author)).fg(Color::named("cyan")));
            spans.push(Span::new(" "));

            let rel = relative_time(row.commit.time);
            spans.push(Span::new(format!("{:>4}", rel)).fg(Color::named("darkGray")));
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
            spans = spans.into_iter().map(|s| s.bg(sel_bg.clone())).collect();
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
                if col < *from {
                    '├'
                } else {
                    '┤'
                }
            }
            graph::LaneCell::Fork { to } => {
                if col < *to {
                    '╭'
                } else {
                    '╮'
                }
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
/// Gutter is " NNNNN  NNNNN  " (old/new line numbers) — matches the host's
/// Git-tab diff panel so both views feel identical.
const DIFF_GUTTER_WIDTH: usize = 15;
/// Narrower single-side gutter used in the split view: " NNNNN ".
const SBS_GUTTER_WIDTH: usize = 7;

fn append_diff_lines(
    out: &mut Vec<StyledLine>,
    diff: &DiffContent,
    layout: DiffLayout,
    width: u16,
) {
    match layout {
        DiffLayout::Unified => append_unified_diff(out, diff, width),
        DiffLayout::SideBySide => append_side_by_side_diff(out, diff, width),
    }
}

fn append_unified_diff(out: &mut Vec<StyledLine>, diff: &DiffContent, width: u16) {
    let added_bg = Color::rgb(0, 40, 0);
    let removed_bg = Color::rgb(60, 0, 0);

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            out.push(StyledLine::new(vec![
                Span::new(format!(" {:>5}  {:>5}  ⋯", "", "")).fg(Color::named("darkGray")),
            ]));
        }
        out.push(StyledLine::new(vec![
            Span::new(format!(" {:>5}  {:>5}  {}", "", "", hunk.header))
                .fg(Color::named("cyan"))
                .dim(),
        ]));

        for line in &hunk.lines {
            let (prefix, fg, bg) = match line.tag {
                LineTag::Added => ("+", Color::named("green"), Some(added_bg.clone())),
                LineTag::Removed => ("-", Color::named("red"), Some(removed_bg.clone())),
                LineTag::Context => (" ", Color::named("white"), None),
            };
            let old_no = fmt_diff_lineno(line.old_lineno);
            let new_no = fmt_diff_lineno(line.new_lineno);

            let max_text = (width as usize).saturating_sub(DIFF_GUTTER_WIDTH);
            let content = truncate_to_display_width(&line.content, max_text);
            let pad = max_text.saturating_sub(UnicodeWidthStr::width(content));

            let mut gutter =
                Span::new(format!(" {}  {} ", old_no, new_no)).fg(Color::named("darkGray"));
            let mut marker = Span::new(format!("{} ", prefix)).fg(fg.clone());
            let mut text = Span::new(content.to_string()).fg(fg);
            let mut padding = Span::new(" ".repeat(pad));

            if let Some(bg) = bg {
                gutter = gutter.bg(bg.clone());
                marker = marker.bg(bg.clone());
                text = text.bg(bg.clone());
                padding = padding.bg(bg);
            }
            out.push(StyledLine::new(vec![gutter, marker, text, padding]));
        }
    }
}

fn append_side_by_side_diff(out: &mut Vec<StyledLine>, diff: &DiffContent, width: u16) {
    // Reserve 1 col for the center divider; split the rest evenly.
    let half = width.saturating_sub(1) / 2;
    let right_w = width.saturating_sub(half + 1);

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            out.push(StyledLine::new(vec![
                Span::new(format!(" {:>5}  ⋯", "")).fg(Color::named("darkGray")),
            ]));
        }
        out.push(StyledLine::new(vec![
            Span::new(format!(" {:>5}  {}", "", hunk.header))
                .fg(Color::named("cyan"))
                .dim(),
        ]));

        for row in pair_hunk_lines(hunk) {
            out.push(render_sbs_row(&row, half, right_w));
        }
    }
}

fn render_sbs_row(row: &SbsRow, half_w: u16, right_w: u16) -> StyledLine {
    let added_bg = Color::rgb(0, 40, 0);
    let removed_bg = Color::rgb(60, 0, 0);

    let mut spans = Vec::with_capacity(7);
    let (left_fg, left_bg) = side_style(row.left_tag, &added_bg, &removed_bg);
    let (right_fg, right_bg) = side_style(row.right_tag, &added_bg, &removed_bg);

    push_sbs_half(
        &mut spans,
        row.left_no,
        &row.left_text,
        left_fg,
        left_bg,
        half_w,
    );
    spans.push(Span::new("│").fg(Color::named("darkGray")));
    push_sbs_half(
        &mut spans,
        row.right_no,
        &row.right_text,
        right_fg,
        right_bg,
        right_w,
    );

    StyledLine::new(spans)
}

fn push_sbs_half(
    spans: &mut Vec<Span>,
    lineno: Option<u32>,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    width: u16,
) {
    let content_w = (width as usize).saturating_sub(SBS_GUTTER_WIDTH);
    let trimmed = truncate_to_display_width(text, content_w);
    let pad = content_w.saturating_sub(UnicodeWidthStr::width(trimmed));

    let mut gutter =
        Span::new(format!(" {} ", fmt_diff_lineno(lineno))).fg(Color::named("darkGray"));
    let mut body = Span::new(trimmed.to_string()).fg(fg);
    let mut padding = Span::new(" ".repeat(pad));
    if let Some(bg) = bg {
        gutter = gutter.bg(bg.clone());
        body = body.bg(bg.clone());
        padding = padding.bg(bg);
    }
    spans.push(gutter);
    spans.push(body);
    spans.push(padding);
}

fn side_style(tag: LineTag, added_bg: &Color, removed_bg: &Color) -> (Color, Option<Color>) {
    match tag {
        LineTag::Added => (Color::named("green"), Some(added_bg.clone())),
        LineTag::Removed => (Color::named("red"), Some(removed_bg.clone())),
        LineTag::Context => (Color::named("white"), None),
    }
}

struct SbsRow {
    left_tag: LineTag,
    left_no: Option<u32>,
    left_text: String,
    right_tag: LineTag,
    right_no: Option<u32>,
    right_text: String,
}

/// Pair consecutive removed/added lines into side-by-side rows. Unmatched
/// removals show an empty right column, unmatched additions show an empty
/// left column, context lines appear on both sides.
fn pair_hunk_lines(hunk: &crate::git::DiffHunk) -> Vec<SbsRow> {
    let mut rows = Vec::new();
    let mut pending_removed: Vec<(Option<u32>, String)> = Vec::new();

    for line in &hunk.lines {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, line.content.clone()));
            }
            LineTag::Added => {
                if let Some((old_no, old_text)) = (!pending_removed.is_empty())
                    .then(|| pending_removed.remove(0))
                {
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    });
                } else {
                    rows.push(SbsRow {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: String::new(),
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    });
                }
            }
            LineTag::Context => {
                for (old_no, old_text) in pending_removed.drain(..) {
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: String::new(),
                    });
                }
                rows.push(SbsRow {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: line.content.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: line.content.clone(),
                });
            }
        }
    }

    for (old_no, old_text) in pending_removed.drain(..) {
        rows.push(SbsRow {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: String::new(),
        });
    }

    rows
}

/// Header above the inline commit-file diff. Mirrors the Git-tab's
/// "path  [上下][局部]  m/f 切换" hint so the keybindings are discoverable.
fn diff_header_line(path: &str, layout: DiffLayout, mode: DiffMode, width: u16) -> StyledLine {
    let layout_label = match layout {
        DiffLayout::Unified => "上下",
        DiffLayout::SideBySide => "左右",
    };
    let mode_label = match mode {
        DiffMode::Compact => "局部",
        DiffMode::FullFile => "全量",
    };
    let tag_str = format!("  [{}][{}]  m/f 切换", layout_label, mode_label);
    let tag_w = UnicodeWidthStr::width(tag_str.as_str());
    let path_max = (width as usize).saturating_sub(tag_w);
    let path_display = truncate_to_display_width(path, path_max);

    StyledLine::new(vec![
        Span::new(path_display.to_string())
            .fg(Color::named("white"))
            .bold(),
        Span::new(tag_str).fg(Color::named("darkGray")),
    ])
}

fn diff_separator_line(width: u16) -> StyledLine {
    StyledLine::new(vec![
        Span::new("─".repeat(width as usize)).fg(Color::named("darkGray")),
    ])
}

fn fmt_diff_lineno(n: Option<u32>) -> String {
    n.map(|v| format!("{:>5}", v))
        .unwrap_or_else(|| "     ".to_string())
}

/// Truncate `s` to fit within `max_width` terminal columns (respects wide chars).
fn truncate_to_display_width(s: &str, max_width: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_width {
            return &s[..i];
        }
        width += cw;
    }
    s
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

/// Produce the status-panel push indicator for the given ahead/behind counts.
/// Returns `None` when local and remote are in sync (nothing to show).
///
/// Three visual states:
/// - `ahead > 0, behind == 0` → green "↑ 推送 (N)" button → `git.push`
/// - `ahead == 0, behind > 0` → read-only "落后 N 次提交" hint (no pull action
///   yet; pushing would fail so we don't offer a button)
/// - `ahead > 0, behind > 0`  → orange "⚠ 已分叉 ↑A ↓B — 强制推送" →
///   `git.forcePushPrompt`, which raises a confirmation banner
fn push_indicator_line(ahead: usize, behind: usize) -> Option<StyledLine> {
    match (ahead, behind) {
        (0, 0) => None,
        (a, 0) => Some(StyledLine::new(vec![
            Span::new("  "),
            Span::new(format!(" ↑ 推送 ({a}) "))
                .fg(Color::named("black"))
                .bg(Color::named("green"))
                .bold()
                .on_click("git.pushPrompt", serde_json::Value::Null),
        ])),
        (0, b) => Some(StyledLine::new(vec![
            Span::new(format!("  ↓ 落后远端 {b} 次提交 — 请先 fetch/pull"))
                .fg(Color::named("yellow")),
        ])),
        (a, b) => Some(StyledLine::new(vec![
            Span::new("  "),
            Span::new(format!(" ⚠ 已分叉 ↑{a} ↓{b} — 强制推送 "))
                .fg(Color::named("black"))
                .bg(Color::named("yellow"))
                .bold()
                .on_click("git.forcePushPrompt", serde_json::Value::Null),
        ])),
    }
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
/// unix timestamp. Seconds/minutes collapse to "now" / "Nm". Reads
/// `SystemTime::now()` for the reference clock; see `relative_time_at` for
/// a deterministic variant that accepts an explicit "now".
fn relative_time(author_time: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(author_time);
    relative_time_at(now, author_time)
}

/// Pure variant of `relative_time` — computes the display string from an
/// explicit `now_secs`. Exposed as a module-private helper so tests can
/// exercise every time-bucket branch deterministically.
fn relative_time_at(now_secs: i64, author_time: i64) -> String {
    let diff = (now_secs - author_time).max(0);
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
                RefLabel::Branch(n) => {
                    1u8.hash(&mut hasher);
                    n.hash(&mut hasher);
                }
                RefLabel::RemoteBranch(n) => {
                    2u8.hash(&mut hasher);
                    n.hash(&mut hasher);
                }
                RefLabel::Tag(n) => {
                    3u8.hash(&mut hasher);
                    n.hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{
        days_to_ymd, format_timestamp, hash_ref_map, lane_color_for, push_indicator_line,
        relative_time_at, render_lane_chars, truncate_in_place,
    };
    use reef_git::git::{CommitInfo, RefLabel};
    use reef_git::graph::{GraphRow, LaneCell};
    use reef_protocol::Color;
    use std::collections::HashMap;

    fn blank_commit() -> CommitInfo {
        CommitInfo {
            oid: String::new(),
            short_oid: String::new(),
            parents: vec![],
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            subject: String::new(),
        }
    }

    fn row(cells: Vec<LaneCell>, node_col: usize) -> GraphRow {
        GraphRow {
            cells,
            node_col,
            commit: blank_commit(),
        }
    }

    // ── truncate_in_place ────────────────────────────────────────────────────

    #[test]
    fn truncate_no_change_when_within_limit() {
        let mut s = "hello".to_string();
        truncate_in_place(&mut s, 8);
        assert_eq!(s, "hello");
    }

    #[test]
    fn truncate_no_change_at_exact_limit() {
        let mut s = "hello".to_string();
        truncate_in_place(&mut s, 5);
        assert_eq!(s, "hello");
    }

    #[test]
    fn truncate_over_limit_appends_ellipsis() {
        let mut s = "hello world".to_string();
        truncate_in_place(&mut s, 8);
        assert_eq!(s, "hello w…");
    }

    #[test]
    fn truncate_max_zero_clears_string() {
        let mut s = "hello".to_string();
        truncate_in_place(&mut s, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn truncate_multibyte_chars() {
        let mut s = "你好世界".to_string();
        truncate_in_place(&mut s, 3);
        assert_eq!(s, "你好…");
    }

    // ── days_to_ymd ──────────────────────────────────────────────────────────

    #[test]
    fn days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_one_day_after_epoch() {
        assert_eq!(days_to_ymd(1), (1970, 1, 2));
    }

    #[test]
    fn days_to_ymd_known_date_2020_01_01() {
        // 2020-01-01 = day 18262 since epoch
        assert_eq!(days_to_ymd(18262), (2020, 1, 1));
    }

    #[test]
    fn days_to_ymd_leap_day_2000_02_29() {
        // 2000-02-29 = day 11016 since epoch
        assert_eq!(days_to_ymd(11016), (2000, 2, 29));
    }

    // ── format_timestamp ─────────────────────────────────────────────────────

    #[test]
    fn format_timestamp_epoch() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn format_timestamp_known() {
        // 2020-01-01 00:00:00 UTC = 1577836800
        assert_eq!(format_timestamp(1577836800), "2020-01-01 00:00:00 UTC");
    }

    #[test]
    fn format_timestamp_with_time() {
        // 1970-01-01 01:02:03 UTC = 3723
        assert_eq!(format_timestamp(3723), "1970-01-01 01:02:03 UTC");
    }

    // ── lane_color_for ───────────────────────────────────────────────────────

    #[test]
    fn lane_color_for_col_zero() {
        assert_eq!(lane_color_for(0), Color::named("cyan"));
    }

    #[test]
    fn lane_color_for_cycles_back() {
        // Palette has 6 entries; col 6 should equal col 0
        assert_eq!(lane_color_for(6), lane_color_for(0));
    }

    #[test]
    fn lane_color_for_all_distinct_in_one_cycle() {
        let colors: Vec<Color> = (0..6).map(lane_color_for).collect();
        for i in 0..6 {
            for j in (i + 1)..6 {
                assert_ne!(colors[i], colors[j], "cols {} and {} should differ", i, j);
            }
        }
    }

    // ── hash_ref_map ─────────────────────────────────────────────────────────

    #[test]
    fn hash_ref_map_empty_is_stable() {
        let h1 = hash_ref_map(&HashMap::new());
        let h2 = hash_ref_map(&HashMap::new());
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_ref_map_same_content_same_hash() {
        let mut m1: HashMap<String, Vec<RefLabel>> = HashMap::new();
        m1.insert("abc".to_string(), vec![RefLabel::Head]);
        let mut m2 = m1.clone();
        m2.insert("abc".to_string(), vec![RefLabel::Head]);
        assert_eq!(hash_ref_map(&m1), hash_ref_map(&m2));
    }

    #[test]
    fn hash_ref_map_different_content_different_hash() {
        let mut m1: HashMap<String, Vec<RefLabel>> = HashMap::new();
        m1.insert("abc".to_string(), vec![RefLabel::Head]);
        let mut m2: HashMap<String, Vec<RefLabel>> = HashMap::new();
        m2.insert(
            "abc".to_string(),
            vec![RefLabel::Branch("main".to_string())],
        );
        assert_ne!(hash_ref_map(&m1), hash_ref_map(&m2));
    }

    // ── render_lane_chars ────────────────────────────────────────────────────

    #[test]
    fn render_lane_chars_empty_cells() {
        assert!(render_lane_chars(&row(vec![], 0)).is_empty());
    }

    #[test]
    fn render_lane_chars_single_node() {
        let r = row(vec![LaneCell::Node], 0);
        assert_eq!(render_lane_chars(&r), vec!['●']);
    }

    #[test]
    fn render_lane_chars_node_and_pass() {
        let r = row(vec![LaneCell::Node, LaneCell::Pass], 0);
        assert_eq!(render_lane_chars(&r), vec!['●', '│']);
    }

    #[test]
    fn render_lane_chars_pass_and_node() {
        let r = row(vec![LaneCell::Pass, LaneCell::Node], 1);
        assert_eq!(render_lane_chars(&r), vec!['│', '●']);
    }

    #[test]
    fn render_lane_chars_node_and_fork_right() {
        // Fork at col 1, to col 0 (node is to the left)
        let r = row(vec![LaneCell::Node, LaneCell::Fork { to: 0 }], 0);
        // Fork{to} where col(1) > to(0) → '╮'
        assert_eq!(render_lane_chars(&r), vec!['●', '╮']);
    }

    #[test]
    fn render_lane_chars_merge_fills_horizontal() {
        // Node at col 0, Empty at col 1, Merge{from:0} at col 2
        let r = row(
            vec![LaneCell::Node, LaneCell::Empty, LaneCell::Merge { from: 0 }],
            0,
        );
        let glyphs = render_lane_chars(&r);
        // Merge at col 2, from 0: col(2) > from(0) → '┤'; gap at col 1 filled with '─'
        assert_eq!(glyphs, vec!['●', '─', '┤']);
    }

    // ── ref_label_span ───────────────────────────────────────────────────────

    #[test]
    fn ref_label_span_head() {
        let span = super::ref_label_span(&RefLabel::Head);
        assert_eq!(span.text, " HEAD ");
        assert_eq!(span.fg, Some(Color::named("black")));
        assert_eq!(span.bg, Some(Color::named("cyan")));
        assert_eq!(span.bold, Some(true));
    }

    #[test]
    fn ref_label_span_branch() {
        let span = super::ref_label_span(&RefLabel::Branch("main".into()));
        assert_eq!(span.text, " main ");
        assert_eq!(span.bg, Some(Color::named("green")));
    }

    #[test]
    fn ref_label_span_remote_branch() {
        let span = super::ref_label_span(&RefLabel::RemoteBranch("origin/main".into()));
        assert_eq!(span.text, " origin/main ");
        assert_eq!(span.bg, Some(Color::named("darkGray")));
    }

    #[test]
    fn ref_label_span_tag() {
        let span = super::ref_label_span(&RefLabel::Tag("v1.0".into()));
        assert_eq!(span.text, " tag: v1.0 ");
        assert_eq!(span.bg, Some(Color::named("yellow")));
    }

    // ── append_diff_lines ────────────────────────────────────────────────────

    #[test]
    fn append_diff_lines_hunk_header_is_cyan() {
        use crate::git::{DiffContent, DiffHunk};
        let diff = DiffContent {
            file_path: "foo.rs".into(),
            hunks: vec![DiffHunk {
                header: "@@ -1,3 +1,3 @@".into(),
                lines: vec![],
            }],
        };
        let mut out = Vec::new();
        super::append_diff_lines(&mut out, &diff, super::DiffLayout::Unified, 80);
        assert_eq!(out.len(), 1);
        // Hunk header now lives in a gutter-padded cyan-dim span so it aligns
        // with the +/- rows below it.
        let span = &out[0].spans[0];
        assert!(
            span.text.ends_with("@@ -1,3 +1,3 @@"),
            "unexpected header span: {:?}",
            span.text
        );
        assert_eq!(span.fg, Some(Color::named("cyan")));
        assert_eq!(span.dim, Some(true));
    }

    // ── relative_time_at ─────────────────────────────────────────────────────

    #[test]
    fn relative_time_at_now() {
        // < 60s diff → "now"
        assert_eq!(relative_time_at(100, 100), "now");
        assert_eq!(relative_time_at(159, 100), "now"); // 59s diff
    }

    #[test]
    fn relative_time_at_minutes() {
        assert_eq!(relative_time_at(160, 100), "1m"); // 60s
        assert_eq!(relative_time_at(3699, 100), "59m"); // 3599s
    }

    #[test]
    fn relative_time_at_hours() {
        assert_eq!(relative_time_at(3700, 100), "1h"); // 3600s
        assert_eq!(relative_time_at(86499, 100), "23h"); // 86399s
    }

    #[test]
    fn relative_time_at_days() {
        assert_eq!(relative_time_at(86500, 100), "1d"); // 86400s
        assert_eq!(relative_time_at(2592000 + 99, 100), "29d"); // 30d boundary
    }

    #[test]
    fn relative_time_at_months() {
        assert_eq!(relative_time_at(2592000 + 100, 100), "1mo"); // 30d
        // 340 days / 30 = 11.33 → "11mo" (still < 365d year threshold)
        assert_eq!(relative_time_at(86_400 * 340 + 100, 100), "11mo");
    }

    #[test]
    fn relative_time_at_years() {
        assert_eq!(relative_time_at(86_400 * 365 + 100, 100), "1y");
        assert_eq!(relative_time_at(86_400 * 365 * 5 + 100, 100), "5y");
    }

    #[test]
    fn relative_time_at_future_clamps_to_now() {
        // author_time in the future → diff negative → clamped to 0 → "now"
        assert_eq!(relative_time_at(100, 500), "now");
    }

    #[test]
    fn append_diff_lines_colors_by_tag() {
        use crate::git::{DiffContent, DiffHunk, DiffLine, LineTag as GitLineTag};
        let diff = DiffContent {
            file_path: "foo.rs".into(),
            hunks: vec![DiffHunk {
                header: "@@ @@".into(),
                lines: vec![
                    DiffLine {
                        tag: GitLineTag::Added,
                        content: "add".into(),
                        old_lineno: None,
                        new_lineno: Some(1),
                    },
                    DiffLine {
                        tag: GitLineTag::Removed,
                        content: "rm".into(),
                        old_lineno: Some(1),
                        new_lineno: None,
                    },
                    DiffLine {
                        tag: GitLineTag::Context,
                        content: "ctx".into(),
                        old_lineno: Some(2),
                        new_lineno: Some(2),
                    },
                ],
            }],
        };
        let mut out = Vec::new();
        super::append_diff_lines(&mut out, &diff, super::DiffLayout::Unified, 80);
        // 1 header + 3 content lines. Each content row is 4 spans:
        // [gutter, "+/-/ ", content, padding].
        assert_eq!(out.len(), 4);

        let added = &out[1];
        assert_eq!(added.spans.len(), 4);
        assert_eq!(added.spans[1].text, "+ ");
        assert_eq!(added.spans[1].fg, Some(Color::named("green")));
        // Added rows tint the whole row dark-green (marker + text + padding).
        assert_eq!(added.spans[1].bg, Some(Color::rgb(0, 40, 0)));
        assert_eq!(added.spans[2].text, "add");
        assert_eq!(added.spans[2].bg, Some(Color::rgb(0, 40, 0)));
        // Gutter shows only the new line number for added rows.
        assert!(added.spans[0].text.contains("1"));

        let removed = &out[2];
        assert_eq!(removed.spans[1].text, "- ");
        assert_eq!(removed.spans[1].fg, Some(Color::named("red")));
        assert_eq!(removed.spans[1].bg, Some(Color::rgb(60, 0, 0)));
        assert_eq!(removed.spans[2].text, "rm");

        let context = &out[3];
        assert_eq!(context.spans[1].text, "  ");
        assert_eq!(context.spans[1].fg, Some(Color::named("white")));
        // Context rows have no background tint so they blend with the panel.
        assert_eq!(context.spans[1].bg, None);
        // Gutter shows both old and new line numbers for context rows.
        assert!(context.spans[0].text.contains("2"));
    }

    #[test]
    fn append_diff_lines_inserts_separator_between_hunks() {
        use crate::git::{DiffContent, DiffHunk};
        let diff = DiffContent {
            file_path: "foo.rs".into(),
            hunks: vec![
                DiffHunk {
                    header: "@@ -1 +1 @@".into(),
                    lines: vec![],
                },
                DiffHunk {
                    header: "@@ -10 +10 @@".into(),
                    lines: vec![],
                },
            ],
        };
        let mut out = Vec::new();
        super::append_diff_lines(&mut out, &diff, super::DiffLayout::Unified, 80);
        // header-1, separator, header-2
        assert_eq!(out.len(), 3);
        assert!(out[1].spans[0].text.contains("⋯"));
        assert_eq!(out[1].spans[0].fg, Some(Color::named("darkGray")));
    }

    #[test]
    fn append_diff_lines_side_by_side_pairs_remove_and_add() {
        use crate::git::{DiffContent, DiffHunk, DiffLine, LineTag as GitLineTag};
        let diff = DiffContent {
            file_path: "foo.rs".into(),
            hunks: vec![DiffHunk {
                header: "@@ @@".into(),
                lines: vec![
                    DiffLine {
                        tag: GitLineTag::Removed,
                        content: "old".into(),
                        old_lineno: Some(1),
                        new_lineno: None,
                    },
                    DiffLine {
                        tag: GitLineTag::Added,
                        content: "new".into(),
                        old_lineno: None,
                        new_lineno: Some(1),
                    },
                ],
            }],
        };
        let mut out = Vec::new();
        super::append_diff_lines(&mut out, &diff, super::DiffLayout::SideBySide, 80);
        // 1 header + 1 paired row; the paired row renders a single line with a
        // `│` divider between the removed (left) and added (right) halves.
        assert_eq!(out.len(), 2);
        let row = &out[1];
        let has_divider = row.spans.iter().any(|s| s.text == "│");
        assert!(has_divider, "side-by-side row should contain a divider span");
        let joined: String = row.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("old"));
        assert!(joined.contains("new"));
    }

    #[test]
    fn diff_header_line_surfaces_layout_and_mode_hints() {
        let line =
            super::diff_header_line("foo.rs", super::DiffLayout::SideBySide, super::DiffMode::FullFile, 80);
        let joined: String = line.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.starts_with("foo.rs"));
        assert!(joined.contains("[左右]"));
        assert!(joined.contains("[全量]"));
        assert!(joined.contains("m/f"));
    }

    // ── push_indicator_line ──────────────────────────────────────────────────

    #[test]
    fn push_indicator_in_sync_returns_none() {
        assert!(push_indicator_line(0, 0).is_none());
    }

    #[test]
    fn push_indicator_ahead_is_green_push_button() {
        let line = push_indicator_line(3, 0).expect("ahead → button");
        // Find the clickable span (has text content with ↑).
        let btn = line
            .spans
            .iter()
            .find(|s| s.text.contains("↑"))
            .expect("↑ span present");
        assert!(
            btn.text.contains("3"),
            "ahead count in label: {:?}",
            btn.text
        );
        assert_eq!(btn.bg, Some(Color::named("green")));
        // Click raises the confirmation banner rather than pushing immediately,
        // symmetric with git.forcePushPrompt.
        assert_eq!(btn.click_command.as_deref(), Some("git.pushPrompt"));
    }

    #[test]
    fn push_indicator_behind_is_readonly_yellow_hint() {
        let line = push_indicator_line(0, 2).expect("behind → hint");
        // No clickable span — this is read-only info; pulling is out of scope.
        let has_click = line.spans.iter().any(|s| s.click_command.is_some());
        assert!(!has_click, "behind-only should have no click handler");
        let text: String = line.spans.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("落后"));
        assert!(text.contains("2"));
    }

    #[test]
    fn push_indicator_diverged_triggers_force_push_prompt() {
        let line = push_indicator_line(1, 1).expect("diverged → force button");
        let btn = line
            .spans
            .iter()
            .find(|s| s.text.contains("分叉"))
            .expect("分叉 span present");
        assert_eq!(btn.bg, Some(Color::named("yellow")));
        assert_eq!(btn.click_command.as_deref(), Some("git.forcePushPrompt"));
        assert!(btn.text.contains("↑1"));
        assert!(btn.text.contains("↓1"));
    }
}
