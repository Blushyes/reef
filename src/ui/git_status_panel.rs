//! Git tab's left sidebar.
//!
//! All state lives on `App` (staged_files/unstaged_files/selected_file/
//! diff_layout/diff_mode and the dedicated `App.git_status`). This module
//! is pure render + event/command dispatch: no git2 calls here, those all
//! go through `App`'s methods (`stage_file`, `confirm_discard`, `run_push`,
//! …) which keeps host side state coherent.

use crate::app::{App, Panel, SelectedFile};
use crate::git::tree::{self as gtree, Node};
use crate::git::{FileEntry, FileStatus};
use crate::ui::mouse::ClickAction;
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;

// ─── Public entry points ──────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    // Pull fresh git state on every render so an external `git add`,
    // `git commit`, `git reset`, etc. (which change `.git/index` or
    // `.git/HEAD` — both filtered out by the fs-watcher) still reflect in
    // the sidebar without the user having to press `r`. Matches the
    // plugin-era behaviour where the plugin refreshed on every render
    // request. `repo.statuses` is cheap enough on typical repos; we only
    // do it while the Git tab is being drawn.
    app.refresh_status();

    let theme = app.theme;
    let rows = build_rows(app, area.width, &theme);
    let total = rows.len();

    // Clamp scroll to a valid range so content can't be scrolled past its end.
    let max_scroll = total.saturating_sub(area.height as usize);
    if app.git_status.scroll > max_scroll {
        app.git_status.scroll = max_scroll;
    }
    let scroll = app.git_status.scroll;

    let visible = rows.iter().skip(scroll).take(area.height as usize);
    for (i, row) in visible.enumerate() {
        let y = area.y + i as u16;
        let hover = crate::ui::hover::is_hover(app, area, y);
        let spans: Vec<Span<'static>> = row
            .spans
            .iter()
            .map(|s| {
                Span::styled(
                    s.text.clone(),
                    crate::ui::hover::apply(s.style, hover, app.theme.hover_bg),
                )
            })
            .collect();
        f.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));

        // Register click zones. Span-level clicks win over the row-level click
        // within the span's range; for spans without their own click, fall back
        // to the row-level action (e.g. a whole file row → selectFile).
        let mut x = area.x;
        for span in &row.spans {
            let w = UnicodeWidthStr::width(span.text.as_str()) as u16;
            if w == 0 {
                continue;
            }
            let (cmd, args) = match (&span.click, &row.row_click) {
                (Some(c), _) => (Some(c.0.clone()), Some(c.1.clone())),
                (None, Some(r)) => (Some(r.0.clone()), Some(r.1.clone())),
                (None, None) => (None, None),
            };
            let (dbl, dbl_args) = match (&span.dbl, &row.row_dbl) {
                (Some(c), _) => (Some(c.0.clone()), Some(c.1.clone())),
                (None, Some(r)) => (Some(r.0.clone()), Some(r.1.clone())),
                (None, None) => (None, None),
            };
            if cmd.is_some() || dbl.is_some() {
                app.hit_registry.register_row(
                    x,
                    y,
                    w,
                    ClickAction::GitCommand {
                        command: cmd.unwrap_or_default(),
                        args: args.unwrap_or(Value::Null),
                        dbl_command: dbl,
                        dbl_args,
                    },
                );
            }
            x = x.saturating_add(w);
        }
    }
}

/// Return true when the key was consumed by this panel.
pub fn handle_key(app: &mut App, key: &str) -> bool {
    match key {
        "s" => {
            if let Some(ref sel) = app.selected_file.clone() {
                if !sel.is_staged {
                    app.stage_file(&sel.path);
                }
            }
            true
        }
        "u" => {
            if let Some(ref sel) = app.selected_file.clone() {
                if sel.is_staged {
                    app.unstage_file(&sel.path);
                }
            }
            true
        }
        "d" => {
            let path = app
                .selected_file
                .as_ref()
                .filter(|s| !s.is_staged)
                .and_then(|sel| {
                    app.unstaged_files
                        .iter()
                        .find(|f| f.path == sel.path)
                        .map(|f| f.path.clone())
                });
            if let Some(path) = path {
                app.git_status.confirm_discard = Some(path);
            }
            true
        }
        "y" => {
            if app.git_status.confirm_discard.is_some() {
                app.confirm_discard();
                true
            } else if app.git_status.confirm_force_push {
                app.git_status.confirm_force_push = false;
                app.run_push(true);
                true
            } else if app.git_status.confirm_push {
                app.git_status.confirm_push = false;
                app.run_push(false);
                true
            } else {
                false
            }
        }
        "n" | "Escape" => {
            if app.git_status.confirm_discard.is_some() {
                app.git_status.confirm_discard = None;
                true
            } else if app.git_status.confirm_force_push {
                app.git_status.confirm_force_push = false;
                true
            } else if app.git_status.confirm_push {
                app.git_status.confirm_push = false;
                true
            } else if app.git_status.push_error.is_some() {
                app.git_status.push_error = None;
                true
            } else {
                false
            }
        }
        "r" => {
            app.refresh_status();
            app.load_diff();
            true
        }
        "t" => {
            app.git_status.tree_mode = !app.git_status.tree_mode;
            crate::prefs::set(
                "status.tree_mode",
                if app.git_status.tree_mode {
                    "true"
                } else {
                    "false"
                },
            );
            true
        }
        _ => false,
    }
}

/// Return true when the command was handled by the status panel.
pub fn handle_command(app: &mut App, id: &str, args: &Value) -> bool {
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
            app.selected_file = Some(SelectedFile { path, is_staged });
            app.git_status.confirm_discard = None;
            app.diff_scroll = 0;
            app.diff_h_scroll = 0;
            app.load_diff();
            true
        }
        "git.toggleStaged" => {
            app.staged_collapsed = !app.staged_collapsed;
            true
        }
        "git.toggleUnstaged" => {
            app.unstaged_collapsed = !app.unstaged_collapsed;
            true
        }
        "git.toggleTree" => {
            handle_key(app, "t");
            true
        }
        "git.toggleDir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let is_staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !path.is_empty() {
                let key = gtree::collapsed_key(is_staged, path);
                if !app.git_status.collapsed_dirs.remove(&key) {
                    app.git_status.collapsed_dirs.insert(key);
                }
            }
            true
        }
        "git.stage" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    app.selected_file
                        .as_ref()
                        .filter(|s| !s.is_staged)
                        .map(|s| s.path.clone())
                });
            if let Some(path) = path {
                app.stage_file(&path);
            }
            true
        }
        "git.unstage" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    app.selected_file
                        .as_ref()
                        .filter(|s| s.is_staged)
                        .map(|s| s.path.clone())
                });
            if let Some(path) = path {
                app.unstage_file(&path);
            }
            true
        }
        "git.stageAll" => {
            app.stage_all();
            true
        }
        "git.unstageAll" => {
            app.unstage_all();
            true
        }
        "git.discardPrompt" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !path.is_empty() {
                app.selected_file = Some(SelectedFile {
                    path: path.clone(),
                    is_staged: false,
                });
                app.git_status.confirm_discard = Some(path);
                app.diff_scroll = 0;
                app.diff_h_scroll = 0;
                app.load_diff();
            }
            true
        }
        "git.discardConfirm" => {
            app.confirm_discard();
            true
        }
        "git.discardCancel" => {
            app.git_status.confirm_discard = None;
            true
        }
        "git.pushPrompt" => {
            if app.push_in_flight {
                return true;
            }
            app.git_status.confirm_push = true;
            app.git_status.confirm_force_push = false;
            app.git_status.push_error = None;
            true
        }
        "git.pushConfirm" => {
            app.git_status.confirm_push = false;
            app.run_push(false);
            true
        }
        "git.pushCancel" => {
            app.git_status.confirm_push = false;
            true
        }
        "git.forcePushPrompt" => {
            if app.push_in_flight {
                return true;
            }
            app.git_status.confirm_force_push = true;
            app.git_status.confirm_push = false;
            app.git_status.push_error = None;
            true
        }
        "git.forcePushConfirm" => {
            app.git_status.confirm_force_push = false;
            app.run_push(true);
            true
        }
        "git.forcePushCancel" => {
            app.git_status.confirm_force_push = false;
            true
        }
        "git.dismissPushError" => {
            app.git_status.push_error = None;
            true
        }
        "git.refresh" => {
            app.refresh_status();
            app.load_diff();
            true
        }
        _ => false,
    }
}

// ─── Row model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RowSpan {
    text: String,
    style: Style,
    /// Single-click command for just this span. Takes precedence over row-level.
    click: Option<(String, Value)>,
    dbl: Option<(String, Value)>,
}

#[derive(Debug, Default)]
struct Row {
    spans: Vec<RowSpan>,
    row_click: Option<(String, Value)>,
    row_dbl: Option<(String, Value)>,
}

impl RowSpan {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
            click: None,
            dbl: None,
        }
    }

    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            click: None,
            dbl: None,
        }
    }

    fn on_click(mut self, cmd: &str, args: Value) -> Self {
        self.click = Some((cmd.to_string(), args));
        self
    }
}

impl Row {
    fn new(spans: Vec<RowSpan>) -> Self {
        Self {
            spans,
            row_click: None,
            row_dbl: None,
        }
    }

    fn blank() -> Self {
        Row::new(vec![RowSpan::plain("")])
    }

    fn on_click(mut self, cmd: &str, args: Value) -> Self {
        self.row_click = Some((cmd.to_string(), args));
        self
    }

    fn on_dbl_click(mut self, cmd: &str, args: Value) -> Self {
        self.row_dbl = Some((cmd.to_string(), args));
        self
    }
}

// ─── Row builders ─────────────────────────────────────────────────────────────

fn build_rows(app: &App, width: u16, theme: &Theme) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let status = &app.git_status;
    // Slightly narrower budget to accommodate the ↺ discard button on unstaged rows.
    let max_path = (width as usize).saturating_sub(10);

    // Push-in-flight banner — shown while the worker thread is running.
    // Non-interactive: user just waits for tick() to drain the result.
    if app.push_in_flight {
        rows.push(Row::new(vec![RowSpan::styled(
            "  ⋯ 推送中…",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        rows.push(Row::blank());
    }

    // Push error banner
    if let Some(ref err) = status.push_error {
        let mut msg = err.clone();
        truncate_in_place(&mut msg, max_path);
        rows.push(
            Row::new(vec![
                RowSpan::styled(
                    "  ✖ 推送失败: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(msg, Style::default().fg(theme.fg_primary)),
                RowSpan::styled("  [关闭]", Style::default().fg(theme.fg_secondary)),
            ])
            .on_click("git.dismissPushError", Value::Null),
        );
        rows.push(Row::blank());
    }

    // Push / force-push confirmation banner
    if status.confirm_push || status.confirm_force_push {
        let force = status.confirm_force_push;
        let ahead = app
            .repo
            .as_ref()
            .and_then(|r| r.ahead_behind())
            .map(|(a, _)| a)
            .unwrap_or(0);

        if force {
            rows.push(Row::new(vec![
                RowSpan::styled(
                    "  ⚠ 强制推送？",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(
                    "（会覆盖远端，使用 --force-with-lease）",
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        } else {
            let msg = if ahead > 0 {
                format!("  推送 {ahead} 个提交到远端？")
            } else {
                "  推送到远端？".to_string()
            };
            rows.push(Row::new(vec![RowSpan::styled(
                msg,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            )]));
        }

        let (confirm_label, confirm_bg, confirm_cmd) = if force {
            (" 确认强制推送 ", Color::Red, "git.forcePushConfirm")
        } else {
            (" 确认推送 ", Color::Green, "git.pushConfirm")
        };
        let cancel_cmd = if force {
            "git.forcePushCancel"
        } else {
            "git.pushCancel"
        };
        rows.push(Row::new(vec![
            RowSpan::plain("  "),
            RowSpan::styled(
                confirm_label,
                Style::default()
                    .fg(Color::Black)
                    .bg(confirm_bg)
                    .add_modifier(Modifier::BOLD),
            )
            .on_click(confirm_cmd, Value::Null),
            RowSpan::plain("  "),
            RowSpan::styled(
                " 取消 ",
                Style::default()
                    .fg(theme.chrome_fg)
                    .bg(theme.chrome_muted_fg),
            )
            .on_click(cancel_cmd, Value::Null),
            RowSpan::plain("  "),
            RowSpan::styled("(y / Esc)", Style::default().fg(theme.fg_secondary)),
        ]));
        rows.push(Row::blank());
    }

    // Push indicator (only when tree is clean, no confirmation banner is
    // already shown, and no push is currently in flight).
    if app.staged_files.is_empty()
        && app.unstaged_files.is_empty()
        && !status.confirm_push
        && !status.confirm_force_push
        && !app.push_in_flight
    {
        if let Some((ahead, behind)) = app.repo.as_ref().and_then(|r| r.ahead_behind()) {
            if let Some(row) = push_indicator_row(ahead, behind) {
                rows.push(row);
                rows.push(Row::blank());
            }
        }
    }

    // Discard confirmation banner
    if let Some(ref path) = status.confirm_discard {
        let mut display = path.clone();
        truncate_in_place(&mut display, max_path);
        rows.push(Row::new(vec![
            RowSpan::styled(
                "  ⚠ 还原 ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            RowSpan::styled(
                display,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            RowSpan::styled("？（不可撤销）", Style::default().fg(Color::Yellow)),
        ]));
        rows.push(Row::new(vec![
            RowSpan::plain("  "),
            RowSpan::styled(
                " 确认还原 ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            )
            .on_click("git.discardConfirm", Value::Null),
            RowSpan::plain("  "),
            RowSpan::styled(
                " 取消 ",
                Style::default().fg(Color::White).bg(Color::DarkGray),
            )
            .on_click("git.discardCancel", Value::Null),
            RowSpan::plain("  "),
            RowSpan::styled("(y / Esc)", Style::default().fg(Color::DarkGray)),
        ]));
        rows.push(Row::blank());
    }

    // View mode toggle
    let mode_label = if status.tree_mode {
        "视图: 树形"
    } else {
        "视图: 列表"
    };
    rows.push(
        Row::new(vec![RowSpan::styled(
            mode_label,
            Style::default().fg(theme.fg_secondary),
        )])
        .on_click("git.toggleTree", Value::Null),
    );
    rows.push(Row::blank());

    // Staged section
    if !app.staged_files.is_empty() {
        rows.push(section_header(
            app.staged_collapsed,
            "暂存的更改",
            app.staged_files.len(),
            Color::Green,
            "git.toggleStaged",
            Some(("取消全部", "git.unstageAll", Color::Red)),
            width,
            theme,
        ));
        if !app.staged_collapsed {
            render_files(&mut rows, app, &app.staged_files, true, max_path, theme);
        }
        rows.push(Row::blank());
    }

    // Unstaged section
    let unstaged_button = if app.unstaged_files.is_empty() {
        None
    } else {
        Some(("暂存全部", "git.stageAll", Color::Green))
    };
    rows.push(section_header(
        app.unstaged_collapsed,
        "更改",
        app.unstaged_files.len(),
        Color::Blue,
        "git.toggleUnstaged",
        unstaged_button,
        width,
        theme,
    ));
    if !app.unstaged_collapsed {
        render_files(&mut rows, app, &app.unstaged_files, false, max_path, theme);
        if app.unstaged_files.is_empty() {
            rows.push(Row::new(vec![RowSpan::styled(
                "  无文件",
                Style::default().fg(theme.fg_secondary),
            )]));
        }
    }

    rows
}

/// Returns `Some(path)` when the app's current selection is for the matching
/// staged/unstaged list; otherwise `None`. Keeps the selection highlight from
/// leaking across sections (e.g. the previously-unstaged selection shouldn't
/// light up a same-named row under the staged header).
fn selected_path_for(app: &App, is_staged: bool) -> Option<String> {
    app.selected_file
        .as_ref()
        .filter(|s| s.is_staged == is_staged)
        .map(|s| s.path.clone())
}

fn render_files(
    rows: &mut Vec<Row>,
    app: &App,
    files: &[FileEntry],
    is_staged: bool,
    max_path: usize,
    theme: &Theme,
) {
    let status = &app.git_status;
    let sel_path = selected_path_for(app, is_staged);
    if status.tree_mode {
        let tree = gtree::build(files);
        walk_tree(
            rows,
            &tree,
            1,
            is_staged,
            max_path,
            &status.collapsed_dirs,
            &sel_path,
            theme,
        );
    } else {
        for file in files {
            let is_sel = sel_path.as_deref() == Some(file.path.as_str());
            rows.push(file_row(
                &file.path,
                &file.path,
                file.status,
                is_staged,
                max_path,
                is_sel,
                "  ",
                theme,
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_tree(
    rows: &mut Vec<Row>,
    tree: &BTreeMap<String, Node>,
    depth: usize,
    is_staged: bool,
    max_path: usize,
    collapsed: &std::collections::HashSet<String>,
    selected_path: &Option<String>,
    theme: &Theme,
) {
    let mut entries: Vec<(&String, &Node)> = tree.iter().collect();
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.1, Node::Dir { .. });
        let b_dir = matches!(b.1, Node::Dir { .. });
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });

    for (name, node) in entries {
        match node {
            Node::Dir { path, children } => {
                let key = gtree::collapsed_key(is_staged, path);
                let is_collapsed = collapsed.contains(&key);
                rows.push(dir_row(name, path, is_staged, depth, is_collapsed, theme));
                if !is_collapsed {
                    walk_tree(
                        rows,
                        children,
                        depth + 1,
                        is_staged,
                        max_path,
                        collapsed,
                        selected_path,
                        theme,
                    );
                }
            }
            Node::File(entry) => {
                let is_sel = selected_path.as_deref() == Some(entry.path.as_str());
                let basename = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                let indent = "  ".repeat(depth);
                rows.push(file_row(
                    &entry.path,
                    basename,
                    entry.status,
                    is_staged,
                    max_path,
                    is_sel,
                    &indent,
                    theme,
                ));
            }
        }
    }
}

fn dir_row(
    name: &str,
    path: &str,
    is_staged: bool,
    depth: usize,
    is_collapsed: bool,
    theme: &Theme,
) -> Row {
    let arrow = if is_collapsed { "›" } else { "⌄" };
    Row::new(vec![
        RowSpan::plain("  ".repeat(depth)),
        RowSpan::styled(
            format!("{} ", arrow),
            Style::default().fg(theme.fg_secondary),
        ),
        RowSpan::styled(
            format!("{}/", name),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .on_click(
        "git.toggleDir",
        serde_json::json!({ "path": path, "staged": is_staged }),
    )
}

#[allow(clippy::too_many_arguments)]
fn file_row(
    path: &str,
    display: &str,
    status: FileStatus,
    is_staged: bool,
    max_path: usize,
    is_selected: bool,
    indent: &str,
    theme: &Theme,
) -> Row {
    let status_color = match status {
        FileStatus::Modified => Color::Yellow,
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Untracked => Color::Green,
    };
    let status_label = status.label();
    let button = if is_staged { "−" } else { "+" };
    let button_color = if is_staged { Color::Red } else { Color::Green };
    let button_cmd = if is_staged {
        "git.unstage"
    } else {
        "git.stage"
    };

    let display_path = if display.chars().count() > max_path {
        let kept = max_path.saturating_sub(3);
        let start = display.chars().count().saturating_sub(kept);
        let trailing: String = display.chars().skip(start).collect();
        format!("...{}", trailing)
    } else {
        display.to_string()
    };

    let base_bg = if is_selected {
        Some(theme.selection_bg)
    } else {
        None
    };

    let mut spans: Vec<RowSpan> = vec![
        RowSpan::styled(indent.to_string(), apply_bg(Style::default(), base_bg)),
        RowSpan::styled(
            display_path,
            apply_bg(Style::default().fg(theme.fg_primary), base_bg),
        ),
        RowSpan::styled(
            format!(" {} ", status_label),
            apply_bg(Style::default().fg(status_color), base_bg),
        ),
        RowSpan {
            text: button.into(),
            style: apply_bg(
                Style::default()
                    .fg(button_color)
                    .add_modifier(Modifier::BOLD),
                base_bg,
            ),
            click: Some((button_cmd.to_string(), serde_json::json!({ "path": path }))),
            dbl: None,
        },
        RowSpan::styled(" ".to_string(), apply_bg(Style::default(), base_bg)),
    ];

    if !is_staged {
        spans.push(RowSpan {
            text: "↺".into(),
            style: apply_bg(Style::default().fg(Color::Red), base_bg),
            click: Some((
                "git.discardPrompt".to_string(),
                serde_json::json!({ "path": path }),
            )),
            dbl: None,
        });
        spans.push(RowSpan::styled(
            " ".to_string(),
            apply_bg(Style::default(), base_bg),
        ));
    }

    Row::new(spans)
        .on_click(
            "git.selectFile",
            serde_json::json!({ "path": path, "staged": is_staged }),
        )
        .on_dbl_click(button_cmd, serde_json::json!({ "path": path }))
}

fn apply_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(c) => style.bg(c),
        None => style,
    }
}

#[allow(clippy::too_many_arguments)]
fn section_header(
    collapsed: bool,
    label: &str,
    count: usize,
    count_color: Color,
    toggle_cmd: &str,
    action: Option<(&str, &str, Color)>,
    width: u16,
    theme: &Theme,
) -> Row {
    let arrow = if collapsed { "›" } else { "⌄" };
    let prefix = format!("{} ", arrow);
    let count_str = format!("  {}", count);
    let button_text = action.as_ref().map(|(t, _, _)| format!(" {} ", t));
    let used = prefix.width()
        + label.width()
        + count_str.width()
        + button_text.as_deref().map(str::width).unwrap_or(0);
    let padding = (width as usize).saturating_sub(used);

    let mut spans = vec![
        RowSpan::styled(prefix, Style::default().fg(theme.fg_primary)),
        RowSpan::styled(
            label.to_string(),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(count_str, Style::default().fg(count_color)),
    ];
    if padding > 0 {
        spans.push(RowSpan::plain(" ".repeat(padding)));
    }
    if let (Some(text), Some((_, cmd, color))) = (button_text, action) {
        spans.push(RowSpan {
            text,
            style: Style::default().fg(color).add_modifier(Modifier::BOLD),
            click: Some((cmd.to_string(), Value::Null)),
            dbl: None,
        });
    }

    Row::new(spans).on_click(toggle_cmd, Value::Null)
}

fn push_indicator_row(ahead: usize, behind: usize) -> Option<Row> {
    match (ahead, behind) {
        (0, 0) => None,
        (a, 0) => Some(Row::new(vec![
            RowSpan::plain("  "),
            RowSpan {
                text: format!(" ↑ 推送 ({a}) "),
                style: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                click: Some(("git.pushPrompt".into(), Value::Null)),
                dbl: None,
            },
        ])),
        (0, b) => Some(Row::new(vec![RowSpan::styled(
            format!("  ↓ 落后远端 {b} 次提交 — 请先 fetch/pull"),
            Style::default().fg(Color::Yellow),
        )])),
        (a, b) => Some(Row::new(vec![
            RowSpan::plain("  "),
            RowSpan {
                text: format!(" ⚠ 已分叉 ↑{a} ↓{b} — 强制推送 "),
                style: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                click: Some(("git.forcePushPrompt".into(), Value::Null)),
                dbl: None,
            },
        ])),
    }
}

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

// ─── Mouse scroll helper used by main.rs ──────────────────────────────────────

/// Apply a vertical scroll delta to the inline git.status panel. Used by the
/// mouse handler after M2 so the sidebar uses `App.git_status.scroll`
/// instead of the legacy `panel_scroll` map.
pub fn scroll(app: &mut App, delta: i32) {
    let s = &mut app.git_status.scroll;
    *s = if delta < 0 {
        s.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        s.saturating_add(delta as usize)
    };
}

/// Return true when the Git sidebar is currently focused — used by callers
/// that need to know whether to route keys here.
#[allow(dead_code)]
pub fn is_focused(app: &App) -> bool {
    matches!(app.active_panel, Panel::Files)
}
