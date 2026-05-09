//! Git tab's left sidebar.
//!
//! All state lives on `App` (staged_files/unstaged_files/selected_file/
//! diff_layout/diff_mode and the dedicated `App.git_status`). This module
//! is pure render + event/command dispatch: no git2 calls here, those all
//! go through `App`'s methods (`stage_file`, `confirm_discard`, `run_push`,
//! …) which keeps host side state coherent.

use crate::app::{App, DiscardTarget, Panel, PushOperation, SelectedFile, Tab};
use crate::backend::{normalize_repo_root_rel, repo_key};
use crate::git::tree::{self as gtree, Node};
use crate::git::{FileEntry, FileStatus};
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::mouse::ClickAction;
use crate::ui::text::overlay_match_highlight;
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

// ─── Public entry points ──────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
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
        let (ranges, cur) = match row.search_row_idx {
            Some(idx) => app.search.ranges_on_row(SearchTarget::GitStatus, idx),
            None => (Vec::new(), None),
        };
        let spans: Vec<Span<'static>> = if ranges.is_empty() {
            row.spans
                .iter()
                .map(|s| {
                    Span::styled(
                        s.text.clone(),
                        crate::ui::hover::apply(s.style, hover, app.theme.hover_bg),
                    )
                })
                .collect()
        } else {
            // Search rows in `collect_rows(GitStatus)` are raw `file.path`
            // strings. The rendered row has a leading indent span before the
            // filename span, so we overlay just the filename portion with
            // ranges in path-local coords. For tree-mode rows the filename
            // span actually holds `basename` or a "..."-truncated string;
            // those cases fall through to the raw-string path below and any
            // ranges pointing past the truncation boundary are simply
            // ignored by the overlay walker.
            build_spans_with_path_overlay(
                row,
                &ranges,
                cur,
                hover,
                theme.search_match,
                theme.search_current,
                app.theme.hover_bg,
            )
        };
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
            if !can_perform_git_writes(app) {
                return true;
            }
            if let Some(ref sel) = app.selected_file.clone() {
                if !sel.is_staged {
                    app.stage_file(&sel.path);
                }
            }
            true
        }
        "u" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            if let Some(ref sel) = app.selected_file.clone() {
                if sel.is_staged {
                    app.unstage_file(&sel.path);
                }
            }
            true
        }
        "d" => {
            if !can_perform_git_writes(app) {
                return true;
            }
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
                app.git_status.confirm_discard = Some(DiscardTarget::File(path));
            }
            true
        }
        "y" => {
            if app.git_status.confirm_discard.is_some() {
                if !can_perform_git_writes(app) {
                    return true;
                }
                app.confirm_discard();
                true
            } else if app.git_status.confirm_force_push {
                if !can_perform_git_writes(app) {
                    return true;
                }
                app.git_status.confirm_force_push = false;
                app.run_push(true);
                true
            } else if app.git_status.confirm_push {
                if !can_perform_git_writes(app) {
                    return true;
                }
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
            } else if app.git_status.branch_dropdown_open {
                app.git_status.branch_dropdown_open = false;
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
            app.toggle_status_tree_mode();
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
        "git.selectRepo" => {
            let Some(raw) = args.get("repo").and_then(|v| v.as_str()) else {
                return true;
            };
            let Ok(repo_root_rel) = normalize_repo_root_rel(Path::new(raw)) else {
                return true;
            };
            let exists = app
                .repo_catalog
                .repos
                .iter()
                .any(|repo| repo.repo_root_rel == repo_root_rel);
            if exists {
                app.repo_catalog.selected_git_repo = Some(repo_root_rel.clone());
                crate::prefs::set("status.selected_repo", &repo_key(&repo_root_rel));
                app.selected_file = None;
                app.diff_content = None;
                app.diff_scroll = 0;
                app.diff_h_scroll = 0;
                app.git_status.confirm_discard = None;
                app.git_status.branch_dropdown_open = false;
                app.staged_files.clear();
                app.unstaged_files.clear();
                app.git_status.ahead_behind = None;
                app.git_graph.rows.clear();
                app.git_graph.ref_map.clear();
                app.git_graph.cache_key = None;
                app.git_graph.selected_idx = 0;
                app.git_graph.selected_commit = None;
                app.git_graph.selection_anchor = None;
                app.commit_detail.detail = None;
                app.commit_detail.range_detail = None;
                app.commit_detail.file_diff = None;
                app.refresh_status();
                app.graph_load.invalidate_stale();
            }
            true
        }
        "git.checkoutBranch" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            let branch = args.get("branch").and_then(|v| v.as_str()).unwrap_or("");
            if !branch.is_empty() {
                app.git_status.branch_dropdown_open = false;
                app.checkout_branch(branch);
            }
            true
        }
        "git.toggleBranchDropdown" => {
            app.git_status.branch_dropdown_open = !app.git_status.branch_dropdown_open;
            true
        }
        "git.openBranchCreate" => {
            app.open_branch_create_dialog();
            true
        }
        "git.branchCreateFromCurrent" => {
            app.start_branch_create_from_current();
            true
        }
        "git.branchCreateChooseBase" => {
            app.start_branch_create_choose_base();
            true
        }
        "git.branchCreateBase" => {
            let idx = args.get("idx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            app.select_branch_create_base(idx);
            true
        }
        "git.branchCreateSubmit" => {
            app.submit_branch_create_dialog();
            true
        }
        "git.branchCreateCancel" => {
            app.cancel_branch_create_dialog();
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
            if !can_perform_git_writes(app) {
                return true;
            }
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
            if !can_perform_git_writes(app) {
                return true;
            }
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
            if !can_perform_git_writes(app) {
                return true;
            }
            app.stage_all();
            true
        }
        "git.unstageAll" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            app.unstage_all();
            true
        }
        "git.discardPrompt" => {
            if !can_perform_git_writes(app) {
                return true;
            }
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
                app.git_status.confirm_discard = Some(DiscardTarget::File(path));
                app.diff_scroll = 0;
                app.diff_h_scroll = 0;
                app.load_diff();
            }
            true
        }
        "git.discardFolderPrompt" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !path.is_empty() {
                app.git_status.confirm_discard = Some(DiscardTarget::Folder { is_staged, path });
            }
            true
        }
        "git.stageFolder" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                app.stage_folder(path);
            }
            true
        }
        "git.unstageFolder" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                app.unstage_folder(path);
            }
            true
        }
        "git.discardAllPrompt" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            let is_staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let has_any = if is_staged {
                !app.staged_files.is_empty()
            } else {
                !app.unstaged_files.is_empty()
            };
            if has_any {
                app.git_status.confirm_discard = Some(DiscardTarget::Section { is_staged });
            }
            true
        }
        "git.discardConfirm" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            app.confirm_discard();
            true
        }
        "git.discardCancel" => {
            app.git_status.confirm_discard = None;
            true
        }
        "git.pushPrompt" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            if app.push_in_flight {
                return true;
            }
            app.git_status.confirm_push = true;
            app.git_status.confirm_force_push = false;
            app.git_status.push_error = None;
            true
        }
        "git.pushConfirm" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            app.git_status.confirm_push = false;
            app.run_push(false);
            true
        }
        "git.pushCancel" => {
            app.git_status.confirm_push = false;
            true
        }
        "git.forcePushPrompt" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            if app.push_in_flight {
                return true;
            }
            app.git_status.confirm_force_push = true;
            app.git_status.confirm_push = false;
            app.git_status.push_error = None;
            true
        }
        "git.forcePushConfirm" => {
            if !can_perform_git_writes(app) {
                return true;
            }
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
        "git.publishBranch" => {
            if !can_perform_git_writes(app) || app.push_in_flight {
                return true;
            }
            app.git_status.push_error = None;
            app.run_publish_branch();
            true
        }
        "git.dismissPullError" => {
            app.git_status.pull_error = None;
            true
        }
        "git.pull" => {
            if !can_perform_git_writes(app) || app.pull_in_flight {
                return true;
            }
            app.git_status.pull_error = None;
            app.run_pull();
            true
        }
        "git.refresh" => {
            app.refresh_status();
            app.load_diff();
            true
        }
        "git.commitFocus" => {
            app.git_status.commit_editing = true;
            true
        }
        "git.commitBlur" => {
            app.git_status.commit_editing = false;
            true
        }
        "git.commitSubmit" => {
            if !can_perform_git_writes(app) {
                return true;
            }
            app.run_commit();
            true
        }
        "git.dismissCommitError" => {
            app.git_status.commit_error = None;
            true
        }
        "git.revealInTree" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                let rel = std::path::PathBuf::from(path);
                app.set_active_tab(Tab::Files);
                app.file_tree.reveal(&rel);
                app.refresh_file_tree_with_target(Some(rel.clone()));
                app.load_preview_for_path(rel);
            }
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
    /// When this row is a file row, its index into the unified
    /// staged-then-unstaged file list — lines up with `collect_rows` in
    /// `crate::search` for `SearchTarget::GitStatus`. Lets the render loop
    /// overlay match highlights on the filename span.
    search_row_idx: Option<usize>,
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
            search_row_idx: None,
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

    fn with_search_row(mut self, idx: usize) -> Self {
        self.search_row_idx = Some(idx);
        self
    }
}

// ─── Row builders ─────────────────────────────────────────────────────────────

fn build_rows(app: &App, width: u16, theme: &Theme) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let status = &app.git_status;
    // Slightly narrower budget to accommodate the ↗ open and ↺ discard buttons.
    let max_path = (width as usize).saturating_sub(12);

    push_repo_selector(&mut rows, app, max_path, theme);
    push_branch_selector(&mut rows, app, max_path, theme);

    let allow_stage_writes = can_perform_git_writes(app);
    let allow_discard_writes = allow_stage_writes;
    let allow_git_writes = allow_stage_writes;
    if app.repo_catalog.selected_git_repo.is_none()
        && !app.backend.has_repo()
        && !app.repo_catalog.discover_load.loading
    {
        return rows;
    }

    // Push-in-flight banner — shown while the worker thread is running.
    // Non-interactive: user just waits for tick() to drain the result.
    if allow_git_writes && app.push_in_flight {
        rows.push(Row::new(vec![RowSpan::styled(
            if app.push_in_flight_kind == PushOperation::PublishBranch {
                t(Msg::PublishingBranchHint)
            } else {
                t(Msg::PushingHint)
            },
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        rows.push(Row::blank());
    }

    if allow_git_writes && app.pull_in_flight {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::PullingHint),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        rows.push(Row::blank());
    }

    // Push error banner
    if allow_git_writes && let Some(ref err) = status.push_error {
        let mut msg = err.clone();
        truncate_in_place(&mut msg, max_path);
        rows.push(
            Row::new(vec![
                RowSpan::styled(
                    if status.push_error_kind == PushOperation::PublishBranch {
                        t(Msg::PublishBranchFailedPrefix)
                    } else {
                        t(Msg::PushFailedPrefix)
                    },
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(msg, Style::default().fg(theme.fg_primary)),
                RowSpan::styled(
                    t(Msg::DismissClose),
                    Style::default().fg(theme.fg_secondary),
                ),
            ])
            .on_click("git.dismissPushError", Value::Null),
        );
        rows.push(Row::blank());
    }

    if allow_git_writes && let Some(ref err) = status.pull_error {
        let mut msg = err.clone();
        truncate_in_place(&mut msg, max_path);
        rows.push(
            Row::new(vec![
                RowSpan::styled(
                    t(Msg::PullFailedPrefix),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(msg, Style::default().fg(theme.fg_primary)),
                RowSpan::styled(
                    t(Msg::DismissClose),
                    Style::default().fg(theme.fg_secondary),
                ),
            ])
            .on_click("git.dismissPullError", Value::Null),
        );
        rows.push(Row::blank());
    }

    // Push / force-push confirmation banner
    if allow_git_writes && (status.confirm_push || status.confirm_force_push) {
        let force = status.confirm_force_push;
        let ahead = app.git_status.ahead_behind.map(|(a, _)| a).unwrap_or(0);

        if force {
            rows.push(Row::new(vec![
                RowSpan::styled(
                    t(Msg::ForcePushPrompt),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(t(Msg::ForcePushWarning), Style::default().fg(Color::Yellow)),
            ]));
        } else {
            let msg = if ahead > 0 {
                crate::i18n::push_n_commits_prompt(ahead)
            } else {
                t(Msg::PushToRemote).to_string()
            };
            rows.push(Row::new(vec![RowSpan::styled(
                msg,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            )]));
        }

        let (confirm_label, confirm_bg, confirm_cmd) = if force {
            (t(Msg::ConfirmForcePush), Color::Red, "git.forcePushConfirm")
        } else {
            (t(Msg::ConfirmPush), Color::Green, "git.pushConfirm")
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
                t(Msg::Cancel),
                Style::default()
                    .fg(theme.chrome_fg)
                    .bg(theme.chrome_muted_fg),
            )
            .on_click(cancel_cmd, Value::Null),
            RowSpan::plain("  "),
            RowSpan::styled(t(Msg::YEscHint), Style::default().fg(theme.fg_secondary)),
        ]));
        rows.push(Row::blank());
    }

    // Dangerous divergent push stays as an explicit warning row. Normal Push
    // and Pull live on the commit action row so primary Git actions share one
    // horizontal control strip.
    if allow_git_writes
        && !status.confirm_push
        && !status.confirm_force_push
        && !app.push_in_flight
        && !app.pull_in_flight
    {
        if let Some((ahead, behind)) = app.git_status.ahead_behind {
            if let Some(row) = force_push_indicator_row(ahead, behind) {
                rows.push(row);
                rows.push(Row::blank());
            }
        }
    }

    // Commit in-flight banner — while the worker is committing, the
    // input is read-only (commit_in_flight gates run_commit) and the
    // UI shows a spinner.
    if allow_git_writes && app.commit_in_flight {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::CommittingHint),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
        rows.push(Row::blank());
    }

    // Commit error banner — mirrors push_error. Sticks around until
    // the user clicks [dismiss] or starts a new commit attempt.
    if allow_git_writes && let Some(ref err) = status.commit_error {
        let mut msg = err.clone();
        truncate_in_place(&mut msg, max_path);
        rows.push(
            Row::new(vec![
                RowSpan::styled(
                    t(Msg::CommitFailedPrefix),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                RowSpan::styled(msg, Style::default().fg(theme.fg_primary)),
                RowSpan::styled(
                    t(Msg::DismissClose),
                    Style::default().fg(theme.fg_secondary),
                ),
            ])
            .on_click("git.dismissCommitError", Value::Null),
        );
        rows.push(Row::blank());
    }

    if allow_git_writes {
        // Commit message box + Commit button. Anchored just above the
        // staged/unstaged sections to mirror VSCode's Source Control
        // layout. Editing is routed through `git.commit*` commands so
        // mouse and keyboard paths share the same buffer mutation code.
        push_commit_box(&mut rows, app, width as usize, theme);
        rows.push(Row::blank());
    }

    // View mode toggle
    let mode_label = if status.tree_mode {
        t(Msg::ViewModeTree)
    } else {
        t(Msg::ViewModeList)
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
        let staged_actions = if !allow_stage_writes {
            Vec::new()
        } else if matches!(
            status.confirm_discard,
            Some(DiscardTarget::Section { is_staged: true })
        ) && allow_discard_writes
        {
            section_confirm_actions(theme)
        } else {
            let mut actions = vec![HeaderAction {
                label: t(Msg::UnstageAll).to_string(),
                cmd: "git.unstageAll".into(),
                args: Value::Null,
                color: Color::Red,
            }];
            if allow_discard_writes {
                actions.push(HeaderAction {
                    label: t(Msg::DiscardAll).to_string(),
                    cmd: "git.discardAllPrompt".into(),
                    args: serde_json::json!({ "staged": true }),
                    color: Color::Red,
                });
            }
            actions
        };
        rows.push(section_header(
            app.staged_collapsed,
            t(Msg::StagedChanges),
            app.staged_files.len(),
            Color::Green,
            "git.toggleStaged",
            &staged_actions,
            width,
            theme,
        ));
        if !app.staged_collapsed {
            render_files(
                &mut rows,
                app,
                &app.staged_files,
                true,
                max_path,
                theme,
                0,
                allow_stage_writes,
                allow_discard_writes,
            );
        }
        rows.push(Row::blank());
    }

    // Unstaged section
    let unstaged_actions: Vec<HeaderAction> =
        if !allow_stage_writes || app.unstaged_files.is_empty() {
            Vec::new()
        } else if matches!(
            status.confirm_discard,
            Some(DiscardTarget::Section { is_staged: false })
        ) && allow_discard_writes
        {
            section_confirm_actions(theme)
        } else {
            let mut actions = vec![HeaderAction {
                label: t(Msg::StageAll).to_string(),
                cmd: "git.stageAll".into(),
                args: Value::Null,
                color: Color::Green,
            }];
            if allow_discard_writes {
                actions.push(HeaderAction {
                    label: t(Msg::DiscardAll).to_string(),
                    cmd: "git.discardAllPrompt".into(),
                    args: serde_json::json!({ "staged": false }),
                    color: Color::Red,
                });
            }
            actions
        };
    rows.push(section_header(
        app.unstaged_collapsed,
        t(Msg::Changes),
        app.unstaged_files.len(),
        Color::Blue,
        "git.toggleUnstaged",
        &unstaged_actions,
        width,
        theme,
    ));
    if !app.unstaged_collapsed {
        render_files(
            &mut rows,
            app,
            &app.unstaged_files,
            false,
            max_path,
            theme,
            app.staged_files.len(),
            allow_stage_writes,
            allow_discard_writes,
        );
        if app.unstaged_files.is_empty() {
            rows.push(Row::new(vec![RowSpan::styled(
                t(Msg::NoFiles),
                Style::default().fg(theme.fg_secondary),
            )]));
        }
    }

    rows
}

fn selected_repo_allows_legacy_writes(app: &App) -> bool {
    if !app.backend.has_repo() {
        return false;
    }
    app.repo_catalog.selected_git_repo.as_deref() == Some(Path::new("."))
        || (app.repo_catalog.selected_git_repo.is_none()
            && app.repo_catalog.repos.is_empty()
            && !app.repo_catalog.discover_load.loading)
}

fn can_perform_git_writes(app: &App) -> bool {
    selected_repo_allows_legacy_writes(app) || app.repo_catalog.selected_git_repo.is_some()
}

fn push_repo_selector(rows: &mut Vec<Row>, app: &App, max_path: usize, theme: &Theme) {
    rows.push(Row::new(vec![RowSpan::styled(
        t(Msg::Repository),
        Style::default()
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD),
    )]));

    if app.repo_catalog.discover_load.loading {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::RepoScanning),
            Style::default().fg(theme.fg_secondary),
        )]));
        rows.push(Row::blank());
        return;
    }

    if app.repo_catalog.repos.is_empty() {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::NoReposFound),
            Style::default().fg(theme.fg_secondary),
        )]));
        rows.push(Row::blank());
        return;
    }

    if app.repo_catalog.selected_git_repo.is_none() {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::RepoSelectPrompt),
            Style::default().fg(Color::Yellow),
        )]));
    }

    for repo in &app.repo_catalog.repos {
        let key = repo_key(&repo.repo_root_rel);
        let selected =
            app.repo_catalog.selected_git_repo.as_deref() == Some(repo.repo_root_rel.as_path());
        let marker = if selected { "* " } else { "  " };
        let mut label = key.clone();
        truncate_in_place(&mut label, max_path.saturating_sub(2));
        let color = if selected {
            theme.fg_primary
        } else {
            theme.fg_secondary
        };
        rows.push(
            Row::new(vec![
                RowSpan::styled(marker, Style::default().fg(Color::Green)),
                RowSpan::styled(label, Style::default().fg(color)),
            ])
            .on_click("git.selectRepo", serde_json::json!({ "repo": key })),
        );
    }

    if app.repo_catalog.truncated {
        rows.push(Row::new(vec![RowSpan::styled(
            "  …",
            Style::default().fg(theme.fg_secondary),
        )]));
    }

    rows.push(Row::blank());
}

fn push_branch_selector(rows: &mut Vec<Row>, app: &App, max_path: usize, theme: &Theme) {
    if app.repo_catalog.selected_git_repo.is_none() && !app.backend.has_repo() {
        return;
    }
    if app.branch_name.is_empty() && app.git_status.branches.is_empty() {
        return;
    }

    let branches: Vec<String> = app
        .git_status
        .branches
        .iter()
        .filter(|branch| branch.as_str() != app.branch_name)
        .take(6)
        .cloned()
        .collect();
    let has_choices = app.branch_name != "(detached)";
    let arrow = if app.git_status.branch_dropdown_open {
        " ▴"
    } else {
        " ▾"
    };
    let branch_prefix = format!("{}: ", t(Msg::Branch));
    let branch_prefix_width = branch_prefix.width();
    let mut spans = vec![
        RowSpan::styled(
            branch_prefix,
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(
            app.branch_name.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if has_choices {
        spans.push(RowSpan::styled(
            arrow,
            Style::default()
                .fg(theme.fg_secondary)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let row = Row::new(spans);
    rows.push(if has_choices {
        row.on_click("git.toggleBranchDropdown", Value::Null)
    } else {
        row
    });

    if app.git_status.branch_dropdown_open && has_choices {
        rows.push(
            Row::new(vec![
                RowSpan::plain(" ".repeat(branch_prefix_width)),
                RowSpan::styled("+ ", Style::default().fg(Color::Green)),
                RowSpan::styled(
                    crate::i18n::branch_create_menu_item(),
                    Style::default()
                        .fg(theme.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
            .on_click("git.openBranchCreate", Value::Null),
        );
        for branch in branches {
            let mut label = branch.clone();
            truncate_in_place(&mut label, max_path.saturating_sub(2).max(1));
            rows.push(
                Row::new(vec![
                    RowSpan::plain(" ".repeat(branch_prefix_width)),
                    RowSpan::styled("› ", Style::default().fg(theme.fg_secondary)),
                    RowSpan::styled(
                        label,
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
                .on_click(
                    "git.checkoutBranch",
                    serde_json::json!({ "branch": branch }),
                ),
            );
        }
    }
    rows.push(Row::blank());
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

/// `search_base` is the offset applied to each file's `search_row_idx` —
/// staged files start at 0 and unstaged files start at `staged_files.len()`,
/// mirroring the ordering in `crate::search::collect_rows(GitStatus)`.
fn render_files(
    rows: &mut Vec<Row>,
    app: &App,
    files: &[FileEntry],
    is_staged: bool,
    max_path: usize,
    theme: &Theme,
    search_base: usize,
    allow_stage_actions: bool,
    allow_discard_actions: bool,
) {
    let status = &app.git_status;
    let sel_path = selected_path_for(app, is_staged);
    let pending_discard = status.confirm_discard.as_ref();
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
            files,
            search_base,
            pending_discard,
            allow_stage_actions,
            allow_discard_actions,
        );
    } else {
        let ctx = FileRowCtx {
            is_staged,
            max_path,
            theme,
            pending_discard: status.confirm_discard.as_ref(),
            allow_stage_actions,
            allow_discard_actions,
        };
        for (i, file) in files.iter().enumerate() {
            let is_sel = sel_path.as_deref() == Some(file.path.as_str());
            rows.push(
                file_row(file, &file.path, "  ", is_sel, &ctx).with_search_row(search_base + i),
            );
        }
    }
}

/// `flat_files` is the linear ordering used by `collect_rows(GitStatus)` —
/// we look each file up here to compute its `search_row_idx` regardless of
/// tree nesting.
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
    flat_files: &[FileEntry],
    search_base: usize,
    pending_discard: Option<&DiscardTarget>,
    allow_stage_actions: bool,
    allow_discard_actions: bool,
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
                rows.push(dir_row(
                    name,
                    path,
                    is_staged,
                    depth,
                    is_collapsed,
                    theme,
                    pending_discard,
                    allow_stage_actions,
                    allow_discard_actions,
                ));
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
                        flat_files,
                        search_base,
                        pending_discard,
                        allow_stage_actions,
                        allow_discard_actions,
                    );
                }
            }
            Node::File(entry) => {
                let is_sel = selected_path.as_deref() == Some(entry.path.as_str());
                let basename = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                let indent = "  ".repeat(depth);
                let ctx = FileRowCtx {
                    is_staged,
                    max_path,
                    theme,
                    pending_discard,
                    allow_stage_actions,
                    allow_discard_actions,
                };
                let mut row = file_row(entry, basename, &indent, is_sel, &ctx);
                if let Some(pos) = flat_files.iter().position(|f| f.path == entry.path) {
                    row = row.with_search_row(search_base + pos);
                }
                rows.push(row);
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
    pending_discard: Option<&DiscardTarget>,
    allow_stage_actions: bool,
    allow_discard_actions: bool,
) -> Row {
    let arrow = if is_collapsed { "›" } else { "⌄" };
    let (stage_btn, stage_color, stage_cmd) = if is_staged {
        ("−", Color::Red, "git.unstageFolder")
    } else {
        ("+", Color::Green, "git.stageFolder")
    };
    let confirming = matches!(
        pending_discard,
        Some(DiscardTarget::Folder { is_staged: ps, path: pp })
            if *ps == is_staged && pp == path
    );
    let mut spans = vec![
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
    ];
    if allow_stage_actions {
        // Per-folder stage/unstage button. Mirrors the file-row `+`/`−`
        // semantics, scoped to every file under this directory prefix.
        spans.push(RowSpan::plain(" "));
        spans.push(RowSpan {
            text: stage_btn.into(),
            style: Style::default()
                .fg(stage_color)
                .add_modifier(Modifier::BOLD),
            click: Some((stage_cmd.into(), serde_json::json!({ "path": path }))),
            dbl: None,
        });
        spans.push(RowSpan::plain(" "));
        if allow_discard_actions && confirming {
            push_inline_confirm_spans(&mut spans, theme, None);
        } else if allow_discard_actions {
            // Per-folder revert button. Clicking this stages a Folder discard
            // target; the root-row click (below) still toggles expand/collapse.
            spans.push(RowSpan {
                text: "↺".into(),
                style: Style::default().fg(Color::Red),
                click: Some((
                    "git.discardFolderPrompt".into(),
                    serde_json::json!({ "path": path, "staged": is_staged }),
                )),
                dbl: None,
            });
        }
    }
    Row::new(spans).on_click(
        "git.toggleDir",
        serde_json::json!({ "path": path, "staged": is_staged }),
    )
}

/// Per-render constants shared by every file row in a section: the section's
/// staged-ness, the width budget for the path column, and the theme. Kept
/// as a borrowed struct so callers don't have to repack args on every row.
struct FileRowCtx<'a> {
    is_staged: bool,
    max_path: usize,
    theme: &'a Theme,
    /// `Some` when a discard confirmation is awaiting acceptance — used by
    /// `file_row` / `dir_row` to swap the row's `↺` button for inline
    /// `✓`/`✕` buttons on the row whose target matches.
    pending_discard: Option<&'a DiscardTarget>,
    allow_stage_actions: bool,
    allow_discard_actions: bool,
}

fn file_row(
    file: &FileEntry,
    display: &str,
    indent: &str,
    is_selected: bool,
    ctx: &FileRowCtx<'_>,
) -> Row {
    let status_color = match file.status {
        FileStatus::Modified => Color::Yellow,
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Untracked => Color::Green,
    };
    let status_label = file.status.label();
    let button = if ctx.is_staged { "−" } else { "+" };
    let button_color = if ctx.is_staged {
        Color::Red
    } else {
        Color::Green
    };
    let button_cmd = if ctx.is_staged {
        "git.unstage"
    } else {
        "git.stage"
    };

    // Tighter path budget here — numstat and status chrome share the row,
    // so add their widths to the 10-char reservation from `build_rows`.
    let add_label = if file.additions > 0 {
        format!("+{}", file.additions)
    } else {
        String::new()
    };
    let del_label = if file.deletions > 0 {
        format!("-{}", file.deletions)
    } else {
        String::new()
    };
    let numstat_w = add_label.width()
        + del_label.width()
        + if !add_label.is_empty() && !del_label.is_empty() {
            1
        } else {
            0
        }
        + if !add_label.is_empty() || !del_label.is_empty() {
            1
        } else {
            0
        };
    let path_budget = ctx.max_path.saturating_sub(numstat_w);

    let display_path = if display.chars().count() > path_budget {
        let kept = path_budget.saturating_sub(3);
        let start = display.chars().count().saturating_sub(kept);
        let trailing: String = display.chars().skip(start).collect();
        format!("...{}", trailing)
    } else {
        display.to_string()
    };

    let base_bg = if is_selected {
        Some(ctx.theme.selection_bg)
    } else {
        None
    };

    let mut spans: Vec<RowSpan> = vec![
        RowSpan::styled(indent.to_string(), apply_bg(Style::default(), base_bg)),
        RowSpan::styled(
            display_path,
            apply_bg(Style::default().fg(ctx.theme.fg_primary), base_bg),
        ),
    ];

    if !add_label.is_empty() {
        spans.push(RowSpan::styled(
            format!(" {}", add_label),
            apply_bg(Style::default().fg(Color::Green), base_bg),
        ));
    }
    if !del_label.is_empty() {
        spans.push(RowSpan::styled(
            format!(" {}", del_label),
            apply_bg(Style::default().fg(Color::Red), base_bg),
        ));
    }

    spans.push(RowSpan::styled(
        format!(" {} ", status_label),
        apply_bg(Style::default().fg(status_color), base_bg),
    ));
    if ctx.allow_stage_actions {
        spans.push(RowSpan {
            text: "↗".into(),
            style: apply_bg(Style::default().fg(Color::Blue), base_bg),
            click: Some((
                "git.revealInTree".to_string(),
                serde_json::json!({ "path": file.path }),
            )),
            dbl: None,
        });
        spans.push(RowSpan::styled(
            " ".to_string(),
            apply_bg(Style::default(), base_bg),
        ));
        spans.push(RowSpan {
            text: button.into(),
            style: apply_bg(
                Style::default()
                    .fg(button_color)
                    .add_modifier(Modifier::BOLD),
                base_bg,
            ),
            click: Some((
                button_cmd.to_string(),
                serde_json::json!({ "path": file.path }),
            )),
            dbl: None,
        });
        spans.push(RowSpan::styled(
            " ".to_string(),
            apply_bg(Style::default(), base_bg),
        ));
    }

    if ctx.allow_discard_actions && !ctx.is_staged {
        let confirming = matches!(
            ctx.pending_discard,
            Some(DiscardTarget::File(p)) if p == &file.path
        );
        if confirming {
            push_inline_confirm_spans(&mut spans, ctx.theme, base_bg);
        } else {
            spans.push(RowSpan {
                text: "↺".into(),
                style: apply_bg(Style::default().fg(Color::Red), base_bg),
                click: Some((
                    "git.discardPrompt".to_string(),
                    serde_json::json!({ "path": file.path }),
                )),
                dbl: None,
            });
            spans.push(RowSpan::styled(
                " ".to_string(),
                apply_bg(Style::default(), base_bg),
            ));
        }
    }

    let row = Row::new(spans).on_click(
        "git.selectFile",
        serde_json::json!({ "path": file.path, "staged": ctx.is_staged }),
    );
    if ctx.allow_stage_actions {
        row.on_dbl_click(button_cmd, serde_json::json!({ "path": file.path }))
    } else {
        row
    }
}

fn apply_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(c) => style.bg(c),
        None => style,
    }
}

/// One header-level button: `(label, command, args, color)`. The args are
/// owned so callers can stamp in `{ "staged": true }` for the discard-all
/// variant without extra plumbing.
struct HeaderAction {
    label: String,
    cmd: String,
    args: Value,
    color: Color,
}

#[allow(clippy::too_many_arguments)]
fn section_header(
    collapsed: bool,
    label: &str,
    count: usize,
    count_color: Color,
    toggle_cmd: &str,
    actions: &[HeaderAction],
    width: u16,
    theme: &Theme,
) -> Row {
    let arrow = if collapsed { "›" } else { "⌄" };
    let prefix = format!("{} ", arrow);
    let count_str = format!("  {}", count);
    let button_texts: Vec<String> = actions.iter().map(|a| format!(" {} ", a.label)).collect();
    let buttons_w: usize = button_texts.iter().map(|s| s.width()).sum();
    let used = prefix.width() + label.width() + count_str.width() + buttons_w;
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
    for (text, action) in button_texts.into_iter().zip(actions.iter()) {
        spans.push(RowSpan {
            text,
            style: Style::default()
                .fg(action.color)
                .add_modifier(Modifier::BOLD),
            click: Some((action.cmd.clone(), action.args.clone())),
            dbl: None,
        });
    }

    Row::new(spans).on_click(toggle_cmd, Value::Null)
}

/// Render the VSCode-style commit message box.
///
/// Layout (single column, the right-hand diff pane stays untouched):
///   ┌─ Commit message ─┐
///   │ subject…          │
///   │ (body line …)     │
///   └───────────────────┘
///   [ ✓ Commit ]  (Ctrl+Enter · Esc)
///
/// - Empty + unfocused: shows a dimmed placeholder string so the box
///   communicates its purpose before the user clicks.
/// - Focused: the caret is drawn as a reverse-video block at
///   `commit_cursor` so the user can see where typing will land.
/// - Multi-line messages are split on `\n` and rendered as
///   successive rows. Long single lines are truncated with `…` to
///   stay inside the sidebar width; the full buffer is preserved in
///   state and ships to `git commit -F -` verbatim.
fn push_commit_box(rows: &mut Vec<Row>, app: &App, max_path: usize, theme: &Theme) {
    let msg = &app.git_status.commit_message;
    let editing = app.git_status.commit_editing;
    let cursor = app.git_status.commit_cursor.min(msg.len());
    let has_text = !msg.trim().is_empty();
    let border_color = if editing {
        theme.accent
    } else {
        theme.fg_secondary
    };
    // Top border
    rows.push(
        Row::new(vec![RowSpan::styled(
            format!(" ┌ {} ", t(Msg::CommitMessagePlaceholder)),
            Style::default().fg(border_color),
        )])
        .on_click("git.commitFocus", Value::Null),
    );

    // Trailing pad width so the whole interior of the box is a click
    // target for `git.commitFocus`, not just the `│` and caret cells.
    let pad_to = |used: usize| -> String { " ".repeat(max_path.saturating_sub(used)) };

    if !has_text && !editing {
        let prefix = " │ ";
        let used = UnicodeWidthStr::width(prefix);
        rows.push(
            Row::new(vec![
                RowSpan::styled(prefix.to_string(), Style::default().fg(border_color)),
                RowSpan::plain(pad_to(used)),
            ])
            .on_click("git.commitFocus", Value::Null),
        );
    } else {
        // Walk the buffer line by line, emitting one row each. Track
        // a running byte offset so we can decide which line holds
        // the caret and compute its intra-line column.
        let mut offset: usize = 0;
        let lines: Vec<&str> = msg.split('\n').collect();
        let last_idx = lines.len().saturating_sub(1);
        for (i, line) in lines.iter().enumerate() {
            let line_start = offset;
            let line_end = offset + line.len();
            let cursor_in_line = editing
                && cursor >= line_start
                && (cursor < line_end
                    || (cursor == line_end && (i == last_idx || cursor == msg.len())));
            // Truncate overly long lines so the box stays inside
            // the sidebar budget. Budget is in *display columns*
            // (via UnicodeWidthStr) not chars, so a CJK-heavy
            // message line doesn't overflow the sidebar — each
            // ideograph counts as 2 cells on most terminals.
            let budget = max_path.saturating_sub(4).max(1);
            let mut spans = vec![RowSpan::styled(
                " │ ".to_string(),
                Style::default().fg(border_color),
            )];
            if cursor_in_line {
                let col_bytes = cursor - line_start;
                let (display, before_chars) = commit_line_view(line, col_bytes, budget);
                let (before, after) = split_chars(&display, before_chars);
                spans.push(RowSpan::plain(before));
                // Caret glyph: reverse-video space so it shows at
                // end-of-line too. When there's a char under the
                // caret, render it with reverse-video so the user
                // sees which char they're on.
                let (caret, rest) = split_chars(&after, 1);
                let caret_glyph = if caret.is_empty() {
                    " ".to_string()
                } else {
                    caret
                };
                spans.push(RowSpan::styled(
                    caret_glyph,
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
                spans.push(RowSpan::plain(rest));
            } else {
                let display = truncate_to_width(line, budget);
                if display.is_empty() {
                    spans.push(RowSpan::plain(" "));
                } else {
                    spans.push(RowSpan::plain(display));
                }
            }
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.text.as_str()))
                .sum();
            spans.push(RowSpan::plain(pad_to(used)));
            rows.push(Row::new(spans).on_click("git.commitFocus", Value::Null));
            offset = line_end + 1; // +1 for the consumed '\n'
        }
    }

    // Bottom border
    rows.push(
        Row::new(vec![RowSpan::styled(
            " └".to_string(),
            Style::default().fg(border_color),
        )])
        .on_click("git.commitFocus", Value::Null),
    );

    // Action row: [✓ Commit] + hint. Button is dimmed when the
    // draft or staged tree is empty — click still works but the
    // toast/banner tells the user why nothing happened.
    let enabled = has_text && !app.staged_files.is_empty() && !app.commit_in_flight;
    let (btn_fg, btn_bg) = if enabled {
        (Color::Black, Color::Green)
    } else {
        (theme.chrome_fg, theme.chrome_muted_fg)
    };
    let mut action_spans = vec![
        RowSpan::plain("  "),
        RowSpan {
            text: t(Msg::CommitButton).to_string(),
            style: Style::default()
                .fg(btn_fg)
                .bg(btn_bg)
                .add_modifier(Modifier::BOLD),
            click: Some(("git.commitSubmit".into(), Value::Null)),
            dbl: None,
        },
    ];

    if let Some((ahead, behind)) = app.git_status.ahead_behind {
        if ahead > 0 && behind == 0 && !app.push_in_flight {
            action_spans.push(RowSpan::plain("  "));
            action_spans.push(
                RowSpan::styled(
                    crate::i18n::push_button(ahead),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
                .on_click("git.pushPrompt", Value::Null),
            );
        }
    }

    if app.should_offer_publish_branch() && !app.push_in_flight {
        action_spans.push(RowSpan::plain("  "));
        action_spans.push(
            RowSpan::styled(
                crate::i18n::publish_branch_button(),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
            .on_click("git.publishBranch", Value::Null),
        );
    }

    if !app.pull_in_flight
        && let Some((_, behind)) = app.git_status.ahead_behind
    {
        action_spans.push(RowSpan::plain("  "));
        action_spans.push(
            RowSpan::styled(
                crate::i18n::pull_button(behind),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .on_click("git.pull", Value::Null),
        );
    }

    action_spans.push(RowSpan::plain("  "));
    action_spans.push(RowSpan::styled(
        t(Msg::CommitHint).to_string(),
        Style::default().fg(theme.fg_secondary),
    ));

    rows.push(Row::new(action_spans).on_click("git.commitSubmit", Value::Null));
}

/// Split `s` into `(first n chars, rest)` in char units so truncation
/// respects grapheme-ish boundaries instead of slicing mid-UTF-8.
/// Used by `push_commit_box` to draw the caret glyph between
/// before/after runs.
fn split_chars(s: &str, n: usize) -> (String, String) {
    let mut chars = s.chars();
    let before: String = (&mut chars).take(n).collect();
    let after: String = chars.collect();
    (before, after)
}

/// Return the visible slice of a commit-message line plus the cursor's char
/// offset inside that slice. Unlike `truncate_to_width`, this follows the
/// cursor so moving right reveals text after the ellipsis instead of pinning
/// the view to the start of the line.
fn commit_line_view(line: &str, cursor_byte: usize, max_cols: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;

    if max_cols == 0 {
        return (String::new(), 0);
    }
    if UnicodeWidthStr::width(line) <= max_cols {
        let cursor_chars = line[..cursor_byte.min(line.len())].chars().count();
        return (line.to_string(), cursor_chars);
    }
    if max_cols <= 2 {
        let display = truncate_to_width(line, max_cols);
        let cursor_chars = line[..cursor_byte.min(line.len())].chars().count();
        return (display, cursor_chars.min(max_cols));
    }

    let chars: Vec<char> = line.chars().collect();
    let cursor_char = line[..cursor_byte.min(line.len())]
        .chars()
        .count()
        .min(chars.len());
    let prefix_width: usize = chars[..cursor_char]
        .iter()
        .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
        .sum();

    let mut start = 0usize;
    if prefix_width >= max_cols {
        start = cursor_char;
        let mut used = 0usize;
        // Reserve one column for the leading ellipsis and one for either the
        // cursor cell or trailing ellipsis. This keeps the insertion point on
        // screen when the user navigates into the right side of a long line.
        let cap_left = max_cols.saturating_sub(2);
        while start > 0 {
            let w = UnicodeWidthChar::width(chars[start - 1]).unwrap_or(0);
            if used + w > cap_left {
                break;
            }
            used += w;
            start -= 1;
        }
    }

    let mut out = String::new();
    let mut used = 0usize;
    if start > 0 {
        out.push('…');
        used += 1;
    }

    let mut cursor_display = out.chars().count();
    let mut idx = start;
    while idx < chars.len() {
        if idx == cursor_char {
            cursor_display = out.chars().count();
        }
        let w = UnicodeWidthChar::width(chars[idx]).unwrap_or(0);
        let reserve_trailing = usize::from(idx + 1 < chars.len());
        if used + w + reserve_trailing > max_cols {
            break;
        }
        out.push(chars[idx]);
        used += w;
        idx += 1;
    }
    if cursor_char >= idx {
        cursor_display = out.chars().count();
    }
    if idx < chars.len() {
        out.push('…');
    }

    (out, cursor_display)
}

/// Clip `s` so its rendered width is ≤ `max_cols` display columns
/// (via `UnicodeWidthStr`). Appends an `…` when truncation happens.
/// Used by `push_commit_box` instead of char-count clipping so
/// wide-column glyphs (CJK, emoji) don't overflow the sidebar.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_cols == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    // Reserve one column for the trailing ellipsis.
    let cap = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > cap {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn force_push_indicator_row(ahead: usize, behind: usize) -> Option<Row> {
    match (ahead, behind) {
        (0, 0) => None,
        (_, 0) => None,
        (0, _) => None,
        (a, b) => Some(Row::new(vec![
            RowSpan::plain("  "),
            RowSpan {
                text: crate::i18n::diverged_force_push(a, b),
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

/// Apply the search overlay to just the filename span (index 1) of a file row.
/// Leaves indent/status/button spans untouched so their colored chrome stays
/// intact. Ranges are in the raw `file.path` byte-offset space; when the row
/// is showing a truncated basename (tree view or "..."-prefixed path) the
/// overlay call still runs with those ranges — mismatched ranges produce no
/// visible highlight rather than a miscolored span.
fn build_spans_with_path_overlay(
    row: &Row,
    ranges: &[std::ops::Range<usize>],
    current: Option<std::ops::Range<usize>>,
    hover: bool,
    match_bg: Color,
    current_bg: Color,
    hover_bg: Color,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(row.spans.len() + 2);
    for (i, s) in row.spans.iter().enumerate() {
        if i == 1 {
            // Filename span — overlay here.
            let tokens = vec![(s.style, s.text.clone())];
            let overlaid =
                overlay_match_highlight(tokens, ranges, current.clone(), match_bg, current_bg);
            for (style, text) in overlaid {
                out.push(Span::styled(
                    text,
                    crate::ui::hover::apply(style, hover, hover_bg),
                ));
            }
        } else {
            out.push(Span::styled(
                s.text.clone(),
                crate::ui::hover::apply(s.style, hover, hover_bg),
            ));
        }
    }
    out
}

/// Append `✓` (confirm — destructive, red) and `✕` (cancel — subdued)
/// inline-button spans for the pending discard. Used by `file_row` and
/// `dir_row` to swap the row's `↺` for the two-button confirm widget
/// without inserting a separate banner. `base_bg` mirrors the row's
/// selection background so the buttons sit on the highlight when the
/// row is selected.
fn push_inline_confirm_spans(spans: &mut Vec<RowSpan>, theme: &Theme, base_bg: Option<Color>) {
    spans.push(RowSpan {
        text: "✓".into(),
        style: apply_bg(
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            base_bg,
        ),
        click: Some(("git.discardConfirm".to_string(), Value::Null)),
        dbl: None,
    });
    spans.push(RowSpan::styled(
        " ".to_string(),
        apply_bg(Style::default(), base_bg),
    ));
    spans.push(RowSpan {
        text: "✕".into(),
        style: apply_bg(
            Style::default()
                .fg(theme.fg_secondary)
                .add_modifier(Modifier::BOLD),
            base_bg,
        ),
        click: Some(("git.discardCancel".to_string(), Value::Null)),
        dbl: None,
    });
}

/// Section-header twin of `push_inline_confirm_spans`: when a
/// `DiscardTarget::Section` is awaiting confirmation, the section's
/// trailing actions ("Stage All" / "Discard All") are swapped for these
/// two so the user accepts/rejects the operation in place. `Stage All`
/// stays hidden during the pending state to keep the cluster unambiguous.
fn section_confirm_actions(theme: &Theme) -> Vec<HeaderAction> {
    vec![
        HeaderAction {
            label: "✓".to_string(),
            cmd: "git.discardConfirm".into(),
            args: Value::Null,
            color: Color::Red,
        },
        HeaderAction {
            label: "✕".to_string(),
            cmd: "git.discardCancel".into(),
            args: Value::Null,
            color: theme.fg_secondary,
        },
    ]
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
