pub mod commit_detail_panel;
pub mod confirm_modal;
pub mod context_menu_panel;
pub mod db_preview;
pub mod diff_panel;
pub mod file_preview_panel;
pub mod file_tree_panel;
pub mod focus;
pub mod git_graph_panel;
pub mod git_status_panel;
pub mod global_search_panel;
pub mod highlight;
pub mod hosts_picker_panel;
pub mod hover;
pub mod layout;
pub mod mouse;
pub mod quick_open_panel;
pub mod search_tab;
pub mod selection;
pub mod settings_panel;
pub mod text;
pub mod theme;
pub mod toast;

use crate::app::{App, Tab, ViewMode};
use crate::i18n::{Msg, t};
use crate::ui::mouse::ClickAction;
use crate::ui::toast::ToastLevel;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

/// Tab-bar sidebar-toggle button glyphs. `⊟` (squared minus) renders
/// when the sidebar is visible; clicking collapses. `⊞` (squared plus)
/// renders when hidden; clicking expands. Exposed `pub` so tests scan
/// for the rendered glyph against this single source of truth.
pub const SIDEBAR_TOGGLE_GLYPH_VISIBLE: &str = "⊟";
pub const SIDEBAR_TOGGLE_GLYPH_HIDDEN: &str = "⊞";

pub fn render(f: &mut Frame, app: &mut App) {
    let size = f.area();
    app.hit_registry.clear();
    // Clear the preview hit cache each frame; the preview panel's own
    // `render` will repopulate it when the active tab renders the preview.
    // Without this, switching away from a preview-bearing tab would leave
    // `last_preview_rect` pointing at a now-hidden region and the mouse
    // handler would treat clicks on other panels as selection gestures.
    app.last_preview_rect = None;
    app.last_preview_content_origin = None;
    // Same story for the diff panel — both Git tab Diff and Graph tab's
    // 3-col diff column write into these slots during their own render.
    // If the active tab renders neither, wiping here keeps a stale rect
    // from steering mouse selection toward a hidden region.
    app.last_diff_rect = None;
    app.last_diff_hit = None;

    // Settings page is a full-screen takeover — render it instead of
    // the normal title/tab/body/status frame. The four-tab body still
    // gets its async work scheduled in the background; we just don't
    // draw it.
    if app.view_mode == ViewMode::Settings {
        settings_panel::render(f, app, size);
        return;
    }

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // body
            Constraint::Length(1), // status
        ])
        .split(size);

    render_title_bar(f, app, main_layout[0]);
    render_tab_bar(f, app, main_layout[1]);

    // Cache body width before layout math so `graph_uses_three_col` and
    // `normalize_active_panel` can see the current frame's geometry. The
    // title/tab/status bars are fixed-width siblings, so the body width
    // equals the frame width here.
    app.last_total_width = main_layout[2].width;
    app.normalize_active_panel();

    // Body: left (+ optional commit column for Graph 3-col) + right.
    // Width math goes through `App::graph_sidebar_width` /
    // `graph_three_col_widths` so `input::*` and `ui::render` agree on
    // where column boundaries fall — important for hit-testing and
    // h-scroll routing to land in the right column. When the sidebar is
    // hidden, `graph_sidebar_width` returns 0 and the left column drops
    // out of the layout entirely (no 0-width rect, no StartDragSplit zone
    // anchored at the screen edge). Graph 3-col mode without sidebar
    // degrades to [Commit | Diff] so the commit metadata column stays.
    let total_w = main_layout[2].width;
    let three_col = app.graph_uses_three_col();
    let sidebar_w = app.graph_sidebar_width(total_w);
    let has_sidebar = sidebar_w > 0;
    let body_layout = if three_col {
        let (_, commit_w, _) = app.graph_three_col_widths(total_w);
        if has_sidebar {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_w),
                    Constraint::Length(commit_w),
                    Constraint::Min(20),
                ])
                .split(main_layout[2])
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(commit_w), Constraint::Min(20)])
                .split(main_layout[2])
        }
    } else if has_sidebar {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_w), Constraint::Min(20)])
            .split(main_layout[2])
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20)])
            .split(main_layout[2])
    };

    // Index of the editor column in `body_layout`. With sidebar hidden the
    // sidebar slot is absent so the editor slides left by one.
    let editor_idx = if has_sidebar { 1 } else { 0 };

    // Drag zone on the sidebar | right split border — only meaningful when
    // the sidebar actually occupies a column.
    if has_sidebar {
        let split_x = body_layout[0].x + body_layout[0].width.saturating_sub(1);
        app.hit_registry.register(
            Rect::new(split_x, body_layout[0].y, 2, body_layout[0].height),
            ClickAction::StartDragSplit,
        );
    }
    // Second drag zone for the commit | diff boundary in 3-col mode. The
    // commit column sits right after the (optional) sidebar.
    if three_col {
        let commit_rect = body_layout[editor_idx];
        let mid_split_x = commit_rect.x + commit_rect.width.saturating_sub(1);
        app.hit_registry.register(
            Rect::new(mid_split_x, commit_rect.y, 2, commit_rect.height),
            ClickAction::StartDragGraphDiffSplit,
        );
    }

    match app.active_tab {
        Tab::Git => {
            if !app.backend.has_repo() {
                render_no_repo(f, app, body_layout[0]);
            } else {
                if has_sidebar {
                    render_git_sidebar(f, app, body_layout[0]);
                }
                render_git_editor(f, app, body_layout[editor_idx]);
            }
        }
        Tab::Files => {
            if has_sidebar {
                let focused = matches!(app.active_panel, crate::app::Panel::Files);
                file_tree_panel::render(f, app, body_layout[0], focused);
            }
            let focused = matches!(app.active_panel, crate::app::Panel::Diff);
            file_preview_panel::render(f, app, body_layout[editor_idx], focused);
        }
        Tab::Graph => {
            if has_sidebar {
                render_graph_sidebar(f, app, body_layout[0]);
            }
            render_graph_editor(f, app, body_layout[editor_idx]);
            if three_col {
                render_graph_diff_column(f, app, body_layout[editor_idx + 1]);
            }
        }
        Tab::Search => {
            if has_sidebar {
                search_tab::render_sidebar(f, app, body_layout[0]);
            }
            let focused = matches!(app.active_panel, crate::app::Panel::Diff);
            file_preview_panel::render(f, app, body_layout[editor_idx], focused);
        }
    }

    render_status_bar(f, app, main_layout[3]);

    if app.show_help {
        render_help(f, app, size);
    }

    // Render last so the palette overlays help if both are somehow active
    // (shouldn't happen in practice — opening one dismisses the other via
    // input-priority — but the ordering here is a belt-and-braces guard).
    // Global-search after quick-open: if both flags were somehow true the
    // later render wins on overlap, and having global-search on top matches
    // its priority in input dispatch.
    if app.quick_open.active {
        quick_open_panel::render(f, app, size);
    }
    if app.global_search.active {
        global_search_panel::render(f, app, size);
    }
    if app.hosts_picker.active {
        hosts_picker_panel::render(f, app, size);
    }
    // Context menu overlay renders last so it sits above the help
    // popup and any other in-panel chrome. The menu itself is scoped
    // to the Files tab but we don't gate on `active_tab` here: the
    // menu can only be opened from a right-click while the Files tab
    // is active, and `tree_edit.active` implicitly prevents concurrent
    // opens. Once shown, the menu should stay visible even if the
    // user tab-switches (unlikely — input is gated) so the render
    // check only looks at `tree_context_menu.active`.
    if app.tree_context_menu.active {
        context_menu_panel::render(f, app, size);
    }
    // ConfirmModal sits on top of everything — including context menu —
    // because once the user has committed to a destructive choice, the
    // confirm must be the only thing eligible to receive input.
    if app.confirm_modal.is_some() {
        confirm_modal::render(f, app, size);
    }
}

/// Full-width message shown in the Git tab when not inside a git repository.
fn render_no_repo(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.border));
    let msg = Paragraph::new(Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            t(Msg::NoRepoTitle),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            t(Msg::NoRepoHint),
            Style::default().fg(th.fg_secondary),
        )),
    ]))
    .alignment(ratatui::layout::Alignment::Center)
    .block(block);
    f.render_widget(msg, area);
}

/// Git tab's left sidebar — inline host-native status panel.
fn render_git_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(app.theme.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );
    let focused = matches!(app.active_panel, crate::app::Panel::Files);
    git_status_panel::render(f, app, padded, focused);
}

/// Git tab's right editor — host-native diff panel.
fn render_git_editor(f: &mut Frame, app: &mut App, area: Rect) {
    diff_panel::render(f, app, area);
}

/// Graph tab's left sidebar — inline commit-graph panel.
fn render_graph_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(app.theme.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );
    let focused = matches!(app.active_panel, crate::app::Panel::Files);
    git_graph_panel::render(f, app, padded, focused);
}

/// Graph tab's middle column — commit metadata + file tree. In 2-col
/// fallback this is the whole right pane and renders the inline diff
/// too (the `commit_detail_panel` consults `graph_uses_three_col` to
/// decide whether to append diff rows).
fn render_graph_editor(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    let focused = if app.graph_uses_three_col() {
        // In 3-col mode Panel::Commit owns the middle column.
        matches!(app.active_panel, crate::app::Panel::Commit)
    } else {
        matches!(app.active_panel, crate::app::Panel::Diff)
    };
    commit_detail_panel::render(f, app, inner, focused);
}

/// Graph tab's right column (3-col mode only) — the diff viewport for
/// the file selected inside the middle column. Delegates to the shared
/// `diff_panel::render_diff` so Git-tab Diff and this column share
/// rendering, search integration, and (after Stage 3) mouse selection.
fn render_graph_diff_column(f: &mut Frame, app: &mut App, area: Rect) {
    use ratatui::widgets::{Block, Padding};
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Cache the panel rect for mouse selection, parallel to the Git-tab
    // wrapper. Both tabs write into the same `last_diff_rect` slot — only
    // one of them renders Diff at a time.
    app.last_diff_rect = Some(inner);

    let Some(d) = app.commit_detail.file_diff.take() else {
        // In 3-col mode this only fires when `commit_file_diff_load.loading`
        // is still true. Show a quiet "loading…" banner so the column is
        // never blank between the click and the async result landing.
        if area.height >= 1 {
            let msg = Line::from(Span::styled(
                t(Msg::DiffLoading),
                Style::default().fg(app.theme.fg_secondary),
            ));
            let y = area.y + area.height / 2;
            f.render_widget(msg, Rect::new(area.x, y, area.width, 1));
        }
        return;
    };
    let selection = app.diff_selection;
    diff_panel::render_diff(
        f,
        inner,
        &d.diff,
        d.highlighted.as_ref(),
        app.commit_detail.diff_layout,
        app.commit_detail.diff_mode,
        app.theme,
        &app.search,
        crate::search::SearchTarget::GraphDiff,
        selection.as_ref(),
        &mut diff_panel::DiffView {
            scroll: &mut app.commit_detail.file_diff_scroll,
            h_scroll: &mut app.commit_detail.file_diff_h_scroll,
            sbs_left_h_scroll: &mut app.commit_detail.file_diff_sbs_left_h_scroll,
            sbs_right_h_scroll: &mut app.commit_detail.file_diff_sbs_right_h_scroll,
            last_view_h: &mut app.last_diff_view_h,
        },
        &mut app.last_diff_hit,
    );
    app.commit_detail.file_diff = Some(d);
}

fn render_tab_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let bg = th.chrome_bg;
    let tabs = Tab::ALL;

    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;

    for (i, tab) in tabs.iter().enumerate() {
        let label = tab.label();
        let is_active = app.active_tab == *tab;
        let style = if is_active {
            Style::default()
                .fg(th.chrome_active_fg)
                .bg(th.chrome_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.chrome_muted_fg).bg(bg)
        };
        let span = Span::styled(label, style);
        // Hit-registry uses terminal columns (display width), not bytes — tab
        // labels contain wide glyphs (📁, ⑂) so UnicodeWidthStr::width is
        // required to keep click zones aligned with the rendered text.
        let w = UnicodeWidthStr::width(label) as u16;

        app.hit_registry
            .register_row(x, area.y, w, ClickAction::SwitchTab(*tab));
        x += w;
        spans.push(span);

        // Separator between tabs
        if i < tabs.len() - 1 {
            spans.push(Span::styled(
                "│",
                Style::default().fg(th.chrome_muted_fg).bg(bg),
            ));
            x += 1;
        }
    }

    // Glyph mirrors current state so the icon doubles as state readout
    // (⊟ when visible, ⊞ when hidden). The click target is registered
    // tight to the glyph's columns so clicks on the surrounding hint
    // don't accidentally toggle.
    let keys_hint = t(Msg::TabBarHint);
    let button_text = if app.sidebar_visible {
        " ⊟ "
    } else {
        " ⊞ "
    };
    let button_w = UnicodeWidthStr::width(button_text) as u16;
    let consumed = x.saturating_sub(area.x) as usize;
    let right_chrome = button_w as usize + UnicodeWidthStr::width(keys_hint);
    let pad = (area.width as usize)
        .saturating_sub(consumed)
        .saturating_sub(right_chrome);
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    let button_x = x + pad as u16;
    app.hit_registry
        .register_row(button_x, area.y, button_w, ClickAction::ToggleSidebar);
    spans.push(Span::styled(
        button_text,
        Style::default()
            .fg(th.chrome_muted_fg)
            .bg(bg)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        keys_hint,
        Style::default().fg(th.chrome_muted_fg).bg(bg),
    ));

    f.render_widget(Line::from(spans), area);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let repo_name = if app.backend.has_repo() {
        app.workdir_name.as_str()
    } else {
        "—"
    };
    let branch = app.branch_name.as_str();

    let title = Line::from(vec![
        Span::styled(
            " reef ",
            Style::default()
                .fg(th.badge_fg)
                .bg(th.badge_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(th.chrome_bg)),
        Span::styled(
            repo_name,
            Style::default()
                .fg(th.chrome_fg)
                .bg(th.chrome_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if branch.is_empty() {
                String::new()
            } else {
                format!("  ⎇ {}", branch)
            },
            Style::default().fg(th.accent).bg(th.chrome_bg),
        ),
        Span::styled(
            " ".repeat(
                area.width
                    .saturating_sub(repo_name.len() as u16 + branch.len() as u16 + 10)
                    as usize,
            ),
            Style::default().bg(th.chrome_bg),
        ),
    ]);
    f.render_widget(title, area);
}

fn render_status_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;

    // Search prompt has the highest priority — while active it fully owns the
    // status row so the user can see what they're typing.
    if app.search.active {
        render_search_prompt(f, app, area);
        return;
    }

    // Post-search "n/N hint" indicator: when a search session is dormant but
    // still has matches, show a compact counter so the user remembers they
    // can keep stepping.
    if !app.search.active && !app.search.matches.is_empty() {
        render_search_dormant(f, app, area);
        return;
    }

    // Place-mode modal indicator: a loud badge in the accent color so the
    // user can't miss that a mode is active, plus a hint describing how
    // to commit or cancel. When a copy is actively running the hint
    // swaps to a copying indicator so the status bar proves the worker
    // is still alive on big transfers.
    if app.place_mode.active {
        let copying = app.file_copy_load.loading;
        let (badge_text, hint_text) = if copying {
            (" 📋 COPYING ", crate::i18n::place_mode_copying_banner())
        } else {
            (
                " 📋 PLACE ",
                crate::i18n::place_mode_status_hint().to_string(),
            )
        };
        let hint = Line::from(vec![
            Span::styled(
                badge_text,
                Style::default()
                    .fg(th.chrome_bg)
                    .bg(th.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(hint_text, Style::default().fg(th.accent).bg(th.chrome_bg)),
            Span::styled(
                " ".repeat(area.width.saturating_sub(80) as usize),
                Style::default().bg(th.chrome_bg),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Paste-conflict prompt: status bar becomes a yellow ⚠ prompt
    // walking the user through Replace / Skip / Keep both / Cancel.
    // Key routing lives in `input::handle_key_paste_conflict`.
    if let Some(prompt) = app.paste_conflict.as_ref()
        && let Some(item) = prompt.current()
    {
        let name = item
            .existing_at_dest
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let text = crate::i18n::paste_conflict_prompt(&name, prompt.pending_count());
        let hint = Line::from(vec![
            Span::styled(
                " ⚠ PASTE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(th.warn_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                text,
                Style::default()
                    .fg(th.fg_primary)
                    .bg(th.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(area.width.saturating_sub(80) as usize),
                Style::default().bg(th.chrome_bg),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Graph-tab visual-mode indicator. Shows whenever `selection_anchor`
    // is set — even when the range has collapsed to a single commit (user
    // just pressed V but hasn't moved yet) — so the user always knows why
    // arrows/clicks behave differently. Takes priority over the generic
    // status line so the exit hint (`Esc`) is always visible.
    if app.active_tab == crate::app::Tab::Graph && app.git_graph.in_visual_mode() {
        let (lo, hi) = app.git_graph.selected_range();
        let count = hi - lo + 1;
        let hint = Line::from(vec![
            Span::styled(
                crate::i18n::range_badge(count),
                Style::default()
                    .fg(th.chrome_bg)
                    .bg(th.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", t(Msg::StatusBarRangeHint)),
                Style::default().fg(th.accent).bg(th.chrome_bg),
            ),
            Span::styled(
                " ".repeat(area.width.saturating_sub(80) as usize),
                Style::default().bg(th.chrome_bg),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Show the most recent toast (push success/failure etc.) inline.
    let (notif, notif_color) = match app.toasts.last() {
        Some(t) => (
            format!("  {} ", t.message),
            match t.level {
                ToastLevel::Error => Color::Red,
                ToastLevel::Warn => Color::Yellow,
                ToastLevel::Info => Color::Cyan,
            },
        ),
        None => match app.activity_message() {
            Some(msg) => (format!("  {} ", msg), Color::Cyan),
            None => (String::new(), Color::Cyan),
        },
    };

    // Right-aligned panel chip showing which pane currently owns focus.
    // Always rendered in `accent` since it is by definition the focused
    // panel — the visual cue is the chip's presence and color, not its
    // change of state. Drops automatically when the row is too narrow.
    let chip = format!(" [{}] ", panel_chip_text(app.active_tab, app.active_panel));
    let chip_style = Style::default()
        .fg(th.accent)
        .bg(th.chrome_bg)
        .add_modifier(Modifier::BOLD);

    let hint_text = t(Msg::StatusBarHint);
    let hint_w = UnicodeWidthStr::width(hint_text) as u16;
    let chip_w = UnicodeWidthStr::width(chip.as_str()) as u16;
    let notif_w = UnicodeWidthStr::width(notif.as_str()) as u16;

    // Mouse-only settings entry — Ctrl+, fallback for terminals that
    // swallow the chord. Glyph mirrors `SettingsTitle` so the icon is
    // visually self-explanatory.
    let settings_btn = " ⚙ ";
    let settings_btn_w = UnicodeWidthStr::width(settings_btn) as u16;

    let pad_w = area
        .width
        .saturating_sub(notif_w)
        .saturating_sub(settings_btn_w)
        .saturating_sub(hint_w)
        .saturating_sub(chip_w);

    let settings_btn_x = area.x + notif_w + pad_w;
    app.hit_registry.register_row(
        settings_btn_x,
        area.y,
        settings_btn_w,
        ClickAction::OpenSettings,
    );

    let status = Line::from(vec![
        Span::styled(notif, Style::default().fg(notif_color).bg(th.chrome_bg)),
        Span::styled(
            " ".repeat(pad_w as usize),
            Style::default().bg(th.chrome_bg),
        ),
        Span::styled(
            settings_btn,
            Style::default()
                .fg(th.chrome_muted_fg)
                .bg(th.chrome_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            hint_text,
            Style::default().fg(th.chrome_muted_fg).bg(th.chrome_bg),
        ),
        Span::styled(chip, chip_style),
    ]);
    f.render_widget(status, area);
}

/// i18n short label for the currently-focused panel — drives the
/// status-bar chip. Derived from `(tab, panel)` so the label reflects
/// what the panel actually shows in this tab (e.g., `Panel::Files` is
/// "Files" in the Files/Git tabs but "Search" in the Search tab where
/// the left column is the query input). Pulled out as a pure fn (no
/// `&App` parameter) so the mapping is unit-testable.
fn panel_chip_text(tab: crate::app::Tab, panel: crate::app::Panel) -> &'static str {
    use crate::app::{Panel, Tab};
    match (tab, panel) {
        (Tab::Files, Panel::Files) => t(Msg::PanelFiles),
        (Tab::Files, Panel::Diff | Panel::Commit) => t(Msg::PanelPreview),
        (Tab::Search, Panel::Files) => t(Msg::PanelSearch),
        (Tab::Search, Panel::Diff | Panel::Commit) => t(Msg::PanelPreview),
        (Tab::Git, Panel::Files) => t(Msg::PanelFiles),
        (Tab::Git, Panel::Diff | Panel::Commit) => t(Msg::PanelDiff),
        (Tab::Graph, Panel::Files) => t(Msg::PanelGraph),
        (Tab::Graph, Panel::Commit) => t(Msg::PanelCommit),
        (Tab::Graph, Panel::Diff) => t(Msg::PanelDiff),
    }
}

fn render_search_prompt(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let prefix = if app.search.backwards { '?' } else { '/' };
    let query = app.search.query.as_str();

    // Build the right-side counter / status text.
    let right = match (
        app.search.matches.len(),
        app.search.current,
        app.search.wrap_msg,
        query.is_empty(),
    ) {
        (0, _, _, true) => String::new(),
        (0, _, _, false) => t(Msg::SearchNoMatch).to_string(),
        (n, Some(i), wrap, _) => crate::i18n::search_counter(i, n, wrap),
        _ => String::new(),
    };

    let prompt_text = format!("{}{}", prefix, query);
    let right_width = UnicodeWidthStr::width(right.as_str()) as u16;

    // Draw background fill for the whole row first.
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(fill, area);

    // Prompt on the left.
    let prompt_style = Style::default()
        .fg(th.fg_primary)
        .bg(th.chrome_bg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Line::from(Span::styled(prompt_text.clone(), prompt_style)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // Counter on the right.
    if right_width > 0 && right_width < area.width {
        let rx = area.x + area.width.saturating_sub(right_width);
        let right_style = Style::default().fg(th.fg_secondary).bg(th.chrome_bg);
        f.render_widget(
            Line::from(Span::styled(right, right_style)),
            Rect::new(rx, area.y, right_width, 1),
        );
    }

    // Blinking terminal cursor at the current insertion point — lets the user
    // see where new chars will land without a static `█` glyph.
    let prefix_w = 1u16; // '/' and '?' are always narrow.
    let cursor_w =
        UnicodeWidthStr::width(&app.search.query[..app.search.cursor.min(app.search.query.len())])
            as u16;
    let cursor_x = area.x + (prefix_w + cursor_w).min(area.width.saturating_sub(1));
    f.set_cursor_position((cursor_x, area.y));
}

fn render_search_dormant(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let prefix = if app.search.backwards { '?' } else { '/' };
    let counter = match app.search.current {
        Some(i) => crate::i18n::search_dormant_with_counter(
            prefix,
            &app.search.query,
            i,
            app.search.matches.len(),
        ),
        None => format!(" {}{} ", prefix, app.search.query),
    };
    let counter_w = UnicodeWidthStr::width(counter.as_str()) as u16;
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(fill, area);
    let style = Style::default().fg(th.fg_secondary).bg(th.chrome_bg);
    f.render_widget(
        Line::from(Span::styled(counter, style)),
        Rect::new(area.x, area.y, counter_w.min(area.width), 1),
    );
}

fn render_help(f: &mut Frame, app: &App, screen: Rect) {
    let th = app.theme;
    let core_entries: &[(&str, &str)] = &[
        ("q / Ctrl+C", t(Msg::HelpQuit)),
        ("Tab / Shift+Tab", t(Msg::HelpSwitchPanel)),
        ("Ctrl+Tab", t(Msg::HelpSwitchTab)),
        ("Esc", t(Msg::HelpEscBackOut)),
        ("1 … 9", t(Msg::HelpJumpTab)),
        ("↑ / k / Ctrl+P", t(Msg::HelpNavUp)),
        ("↓ / j / Ctrl+N", t(Msg::HelpNavDown)),
        ("PageUp", t(Msg::HelpPageUp)),
        ("PageDown", t(Msg::HelpPageDown)),
        ("← / →", t(Msg::HelpHScroll)),
        ("Shift+← / Shift+→", t(Msg::HelpHScrollFast)),
        ("V", t(Msg::HelpGraphVisualMode)),
        ("↑ / ↓ (visual)", t(Msg::HelpGraphRangeExtend)),
        ("PgUp / PgDn (visual)", t(Msg::HelpGraphRangeExtendFast)),
        ("Click (visual)", t(Msg::HelpGraphVisualClick)),
        ("Esc (visual)", t(Msg::HelpGraphRangeClear)),
        ("Shift+↑ / Shift+↓", t(Msg::HelpGraphShiftExtend)),
        ("Shift+Click", t(Msg::HelpGraphShiftClick)),
        ("Home / End", t(Msg::HelpHomeEnd)),
        (t(Msg::HelpKeyMouseHScroll), t(Msg::HelpMouseHScroll)),
        ("s / u", t(Msg::HelpStageUnstage)),
        ("d → y", t(Msg::HelpDiscard)),
        ("m", t(Msg::HelpDiffLayout)),
        ("f", t(Msg::HelpDiffMode)),
        ("t", t(Msg::HelpToggleView)),
        ("r", t(Msg::HelpRefresh)),
        ("h", t(Msg::HelpShowHelp)),
        ("Ctrl+B", t(Msg::HelpToggleSidebar)),
        ("Ctrl+,", t(Msg::HelpOpenSettings)),
        ("Space p", t(Msg::HelpQuickOpen)),
        ("Space f", t(Msg::HelpGlobalSearch)),
        (t(Msg::HelpKeyDragDrop), t(Msg::HelpDragDrop)),
        ("F2", t(Msg::HelpRenameEntry)),
        ("d / Del / ⌫", t(Msg::HelpDeleteEntry)),
        ("Shift+D / Shift+Del", t(Msg::HelpHardDeleteEntry)),
        (t(Msg::HelpKeyRightClick), t(Msg::HelpRightClickMenu)),
        (t(Msg::HelpKeyAnyKey), t(Msg::HelpAnyKey)),
    ];

    let popup_w = 72u16;
    let popup_h = core_entries.len() as u16 + 4;
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w.min(screen.width), popup_h.min(screen.height));

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            t(Msg::HelpTitle),
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let key_col = 24usize;
    let mut row_y = inner.y;

    for (key, desc) in core_entries {
        if row_y >= inner.y + inner.height {
            break;
        }
        let line = Line::from(vec![
            Span::styled(
                format!("{:<width$}", key, width = key_col),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(*desc, Style::default().fg(th.fg_primary)),
        ]);
        f.render_widget(line, Rect::new(inner.x, row_y, inner.width, 1));
        row_y += 1;
    }
}

#[cfg(test)]
mod panel_chip_tests {
    use super::*;
    use crate::app::{Panel, Tab};

    // Files tab → file tree on the left, preview on the right.
    #[test]
    fn files_tab_panels_match_their_role() {
        assert_eq!(
            panel_chip_text(Tab::Files, Panel::Files),
            t(Msg::PanelFiles)
        );
        assert_eq!(
            panel_chip_text(Tab::Files, Panel::Diff),
            t(Msg::PanelPreview)
        );
    }

    // Search tab → query input on the left, preview on the right.
    // Critical: Panel::Files in Search tab is NOT "Files" — it's the
    // search query column.
    #[test]
    fn search_tab_left_panel_labels_as_search_not_files() {
        assert_eq!(
            panel_chip_text(Tab::Search, Panel::Files),
            t(Msg::PanelSearch)
        );
        assert_eq!(
            panel_chip_text(Tab::Search, Panel::Diff),
            t(Msg::PanelPreview)
        );
    }

    // Git tab → file list on the left, diff (not preview) on the right.
    #[test]
    fn git_tab_right_panel_labels_as_diff_not_preview() {
        assert_eq!(panel_chip_text(Tab::Git, Panel::Files), t(Msg::PanelFiles));
        assert_eq!(panel_chip_text(Tab::Git, Panel::Diff), t(Msg::PanelDiff));
    }

    // Graph tab in 3-col mode is the only place all three Panels are
    // distinct — sidebar/middle/diff each get their own label.
    #[test]
    fn graph_tab_distinguishes_all_three_panels() {
        let g = panel_chip_text(Tab::Graph, Panel::Files);
        let c = panel_chip_text(Tab::Graph, Panel::Commit);
        let d = panel_chip_text(Tab::Graph, Panel::Diff);
        assert_eq!(g, t(Msg::PanelGraph));
        assert_eq!(c, t(Msg::PanelCommit));
        assert_eq!(d, t(Msg::PanelDiff));
        // And those three labels must be pairwise distinct so the chip
        // is actually informative across the cycle.
        assert_ne!(g, c);
        assert_ne!(c, d);
        assert_ne!(g, d);
    }

    // Defensive: Panel::Commit only legitimately appears in Graph 3-col,
    // but if it leaked into another tab via stale state, the label
    // should still resolve sensibly without panicking.
    #[test]
    fn stale_commit_panel_falls_back_per_tab() {
        // Files / Search treat it as preview (the right column).
        assert_eq!(
            panel_chip_text(Tab::Files, Panel::Commit),
            t(Msg::PanelPreview)
        );
        assert_eq!(
            panel_chip_text(Tab::Search, Panel::Commit),
            t(Msg::PanelPreview)
        );
        // Git treats it as diff.
        assert_eq!(panel_chip_text(Tab::Git, Panel::Commit), t(Msg::PanelDiff));
    }
}
