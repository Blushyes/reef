//! Keyboard + mouse dispatch. `main.rs` delegates everything below the
//! event-drain loop to `handle_key` and `handle_mouse` here, so the binary
//! entry point stays focused on terminal bootstrap.
//!
//! The one exception is the `v` (select mode toggle) and `show_help`
//! dismiss — those stay inline in `main.rs` because the first needs
//! `execute!(terminal.backend_mut(), ...)` to flip crossterm's mouse
//! capture mode, and both are simple enough that splitting them out would
//! just add indirection.

use crate::app::{App, Panel, Tab};
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use std::time::{Duration, Instant};

pub const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

// ─── Keyboard ────────────────────────────────────────────────────────────────

pub fn handle_key(key: KeyEvent, app: &mut App) {
    // Global keys (work on all tabs)
    match key.code {
        KeyCode::Char('q') => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char(c) if matches!(c, '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            if let Some(&tab) = Tab::ALL.get(idx) {
                app.active_tab = tab;
            }
            return;
        }
        KeyCode::Tab => {
            let tabs = Tab::ALL;
            let cur = tabs.iter().position(|&t| t == app.active_tab).unwrap_or(0);
            app.active_tab = tabs[(cur + 1) % tabs.len()];
            return;
        }
        KeyCode::BackTab => {
            app.active_panel = match app.active_panel {
                Panel::Files => Panel::Diff,
                Panel::Diff => Panel::Files,
            };
            return;
        }
        KeyCode::Char('h') => {
            app.show_help = true;
            return;
        }
        _ => {}
    }

    match app.active_tab {
        Tab::Git => handle_key_git(key, app),
        Tab::Files => handle_key_files(key, app),
        Tab::Graph => handle_key_graph(key, app),
    }
}

fn handle_key_graph(key: KeyEvent, app: &mut App) {
    use ui::{commit_detail_panel, git_graph_panel};
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "k");
            }
            Panel::Diff => commit_detail_panel::scroll(app, -1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "j");
            }
            Panel::Diff => commit_detail_panel::scroll(app, 1),
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "k");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, -20),
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "j");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, 20),
        },
        KeyCode::Char('r') => {
            // `r` on the graph sidebar = force a graph cache refresh
            app.git_graph.cache_key = None;
            app.refresh_graph();
        }
        // m/f/t target the commit-detail panel regardless of focus.
        KeyCode::Char('m') => {
            commit_detail_panel::handle_key(app, "m");
        }
        KeyCode::Char('f') => {
            commit_detail_panel::handle_key(app, "f");
        }
        KeyCode::Char('t') => {
            commit_detail_panel::handle_key(app, "t");
        }
        _ => {}
    }
}

fn handle_key_git(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff => {
                app.diff_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => app.navigate_files(10),
            Panel::Diff => {
                app.diff_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = usize::MAX; // render 自动钳到实际最大值
        }
        KeyCode::Char('s') => {
            ui::git_status_panel::handle_key(app, "s");
        }
        KeyCode::Char('u') => {
            ui::git_status_panel::handle_key(app, "u");
        }
        KeyCode::Char('d') => {
            ui::git_status_panel::handle_key(app, "d");
        }
        KeyCode::Char('y') => {
            ui::git_status_panel::handle_key(app, "y");
        }
        KeyCode::Char('n') => {
            ui::git_status_panel::handle_key(app, "n");
        }
        KeyCode::Esc => {
            ui::git_status_panel::handle_key(app, "Escape");
        }
        KeyCode::Char('r') => {
            ui::git_status_panel::handle_key(app, "r");
        }
        KeyCode::Char('t') => {
            ui::git_status_panel::handle_key(app, "t");
        }
        KeyCode::Char('m') => {
            app.toggle_diff_layout();
        }
        KeyCode::Char('f') => {
            app.toggle_diff_mode();
        }
        KeyCode::Char('e') | KeyCode::Enter => {
            // Edit the currently selected changed file. Ignore if nothing's
            // selected (empty status) or the repo's gone. A Deleted-status
            // file will be recreated by the editor if the user writes — same
            // behaviour you'd get running `$EDITOR path/to/deleted` in a shell.
            if let (Some(repo), Some(sel)) = (&app.repo, &app.selected_file) {
                if let Some(workdir) = repo.workdir_path() {
                    app.pending_edit = Some(workdir.join(&sel.path));
                }
            }
        }
        _ => {}
    }
}

fn handle_key_files(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = usize::MAX;
        }
        KeyCode::Enter => {
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx) {
                if entry.is_dir {
                    app.file_tree.toggle_expand(idx);
                    app.load_preview();
                } else {
                    // File: hand off to the main loop, which owns the
                    // terminal and can suspend it around `$EDITOR`.
                    app.pending_edit = Some(app.file_tree.root.join(&entry.path));
                }
            }
        }
        KeyCode::Char('r') => {
            app.refresh_file_tree();
        }
        KeyCode::Char('e') => {
            // Explicit alias for "edit selected entry". Unlike Enter, this
            // never expands a directory — on a dir it's a no-op.
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx) {
                if !entry.is_dir {
                    app.pending_edit = Some(app.file_tree.root.join(&entry.path));
                }
            }
        }
        _ => {}
    }
}

// ─── Mouse ───────────────────────────────────────────────────────────────────

pub fn handle_mouse<B: Backend>(mouse: MouseEvent, app: &mut App, terminal: &Terminal<B>) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            let is_double = matches!(
                app.last_click,
                Some((t, c, r))
                    if c == mouse.column
                        && r == mouse.row
                        && now.duration_since(t) < DOUBLE_CLICK_WINDOW
            );

            if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row) {
                // On double-click: if the region carries a dbl action, swap to it
                // and run through handle_action so host-side side effects fire.
                let effective = if is_double {
                    if let ui::mouse::ClickAction::GitCommand {
                        dbl_command: Some(ref cmd),
                        ref dbl_args,
                        ..
                    } = action
                    {
                        ui::mouse::ClickAction::GitCommand {
                            command: cmd.clone(),
                            args: dbl_args.clone().unwrap_or(serde_json::Value::Null),
                            dbl_command: None,
                            dbl_args: None,
                        }
                    } else {
                        action
                    }
                } else {
                    action
                };
                app.handle_action(effective);
            }

            // Reset tracking on every genuine second click so triple-clicks don't chain.
            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_split = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.dragging_split {
                let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
                if total_width > 0 {
                    let percent = (mouse.column * 100 / total_width).clamp(10, 80);
                    app.split_percent = percent;
                }
            }
        }
        MouseEventKind::ScrollUp => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            // Shift + 滚轮 = 横向滚动（兼容不发 ScrollLeft/Right 的终端）
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, -3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        ui::git_status_panel::scroll(app, -3);
                    } else {
                        app.diff_scroll = app.diff_scroll.saturating_sub(3);
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll = app.tree_scroll.saturating_sub(3);
                    } else {
                        app.preview_scroll = app.preview_scroll.saturating_sub(3);
                    }
                }
                Tab::Graph => {
                    if is_left {
                        ui::git_graph_panel::scroll(app, -3);
                    } else {
                        ui::commit_detail_panel::scroll(app, -3);
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, 3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        ui::git_status_panel::scroll(app, 3);
                    } else {
                        app.diff_scroll += 3;
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll += 3;
                    } else {
                        app.preview_scroll += 3;
                    }
                }
                Tab::Graph => {
                    if is_left {
                        ui::git_graph_panel::scroll(app, 3);
                    } else {
                        ui::commit_detail_panel::scroll(app, 3);
                    }
                }
            }
        }
        MouseEventKind::ScrollLeft => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, -3);
        }
        MouseEventKind::ScrollRight => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, 3);
        }
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);
        }
        _ => {}
    }
}

/// Apply a horizontal-scroll delta (in display columns) to the preview / diff
/// panel under the cursor. Events landing on the left sidebar are ignored.
/// Graph tab doesn't support horizontal scroll yet — the commit-detail rows
/// already truncate to the panel width; if long-line viewing becomes a real
/// need, wire up `CommitDetailState.diff_h_scroll` here.
fn apply_horizontal_scroll(app: &mut App, column: u16, total_width: u16, delta: i32) {
    let split_x = total_width * app.split_percent / 100;
    let is_left = column < split_x;
    if is_left {
        return;
    }
    let target = match app.active_tab {
        Tab::Files => &mut app.preview_h_scroll,
        Tab::Git => &mut app.diff_h_scroll,
        Tab::Graph => return,
    };
    *target = if delta < 0 {
        target.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        target.saturating_add(delta as usize)
    };
}
