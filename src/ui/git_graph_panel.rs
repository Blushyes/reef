//! Graph tab's left sidebar — DAG of commits with lane rendering and
//! ref-label chips.

use crate::app::App;
use crate::git::RefLabel;
use crate::git::graph::{GraphRow, LaneCell};
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::mouse::ClickAction;
use crate::ui::text::overlay_match_highlight;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    if app.git_graph.rows.is_empty() {
        let line = Line::from(Span::styled(
            t(Msg::NoCommits),
            Style::default().fg(app.theme.fg_secondary),
        ));
        f.render_widget(line, area);
        return;
    }

    let show_meta = area.width >= 60;
    let max_subject = (area.width as usize).saturating_sub(if show_meta { 40 } else { 14 });
    let head_oid = app
        .git_graph
        .cache_key
        .as_ref()
        .map(|(head, _)| head.as_str());

    // Clamp scroll to a valid range. Auto-scroll into view only when the
    // selection actually moved since the last render — running this every
    // frame meant mouse-wheel scroll (which only changes `scroll`, not
    // `selected_idx`) got snapped back to the selected commit on the next
    // tick. Graph-tab equivalent of #10 / follow-up to #13.
    let total = app.git_graph.rows.len();
    let height = area.height as usize;
    let max_scroll = total.saturating_sub(height);
    let mut scroll = app.git_graph.scroll.min(max_scroll);

    let sel = app.git_graph.selected_idx;
    if app.git_graph.last_rendered_selected != Some(sel) {
        if sel < scroll {
            scroll = sel;
        } else if sel >= scroll + height && height > 0 {
            scroll = sel + 1 - height;
        }
        scroll = scroll.min(max_scroll);
        app.git_graph.last_rendered_selected = Some(sel);
    }
    app.git_graph.scroll = scroll;

    let rows: Vec<&GraphRow> = app.git_graph.rows.iter().collect();
    let theme = app.theme;
    // Range highlight bounds. Collapsed range (lo == hi) = single-select;
    // `in_range` check below only fires when the user actually Shift-extended.
    let (range_lo, range_hi) = app.git_graph.selected_range();
    let is_range = app.git_graph.is_range();
    let anchor_idx = app.git_graph.selection_anchor;
    for (i, row) in rows.iter().skip(scroll).take(height).enumerate() {
        let idx = scroll + i;
        let y = area.y + i as u16;
        let hover = crate::ui::hover::is_hover(app, area, y);
        let (search_ranges, search_cur) = app.search.ranges_on_row(SearchTarget::CommitGraph, idx);
        let in_range = is_range && idx >= range_lo && idx <= range_hi;
        let is_anchor = is_range && Some(idx) == anchor_idx;
        let (line, click_x_w) = build_row_line(
            row,
            idx == app.git_graph.selected_idx,
            in_range,
            is_anchor,
            show_meta,
            max_subject,
            head_oid,
            &app.git_graph.ref_map,
            hover,
            &theme,
            &search_ranges,
            search_cur,
        );
        f.render_widget(line, Rect::new(area.x, y, area.width, 1));

        // Row-level click → selectCommit.
        app.hit_registry.register_row(
            area.x,
            y,
            click_x_w,
            ClickAction::GitCommand {
                command: "git.selectCommit".into(),
                args: serde_json::json!({ "oid": row.commit.oid }),
                dbl_command: None,
                dbl_args: None,
            },
        );
    }
}

pub fn handle_key(app: &mut App, key: &str) -> bool {
    match key {
        "j" | "ArrowDown" => {
            app.move_graph_selection(1);
            true
        }
        "k" | "ArrowUp" => {
            app.move_graph_selection(-1);
            true
        }
        _ => false,
    }
}

pub fn handle_command(app: &mut App, id: &str, args: &Value) -> bool {
    match id {
        "git.graph.next" => {
            app.move_graph_selection(1);
            true
        }
        "git.graph.prev" => {
            app.move_graph_selection(-1);
            true
        }
        "git.selectCommit" => {
            // Click on a graph row. In visual mode (anchor set) it moves
            // the endpoint, keeping the anchor — this is the primary way
            // to build a range in terminals that intercept Shift+Click.
            // Out of visual mode, single-select behaviour as before.
            let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
            if !oid.is_empty()
                && let Some(idx) = app.git_graph.find_row_by_oid(oid)
            {
                if app.git_graph.in_visual_mode() {
                    let delta = idx as i32 - app.git_graph.selected_idx as i32;
                    app.extend_graph_selection(delta);
                } else {
                    app.git_graph.selected_idx = idx;
                    app.git_graph.selected_commit = Some(oid.to_string());
                    app.commit_detail.range_detail = None;
                    app.commit_detail.scroll = 0;
                    app.load_commit_detail();
                }
            }
            true
        }
        "git.graph.focusCommit" => {
            // Range-list "zoom-in" click: always collapse to single-select
            // on the target commit, regardless of visual mode. Emitted by
            // the commit_detail_panel range-header commit list.
            let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
            if !oid.is_empty()
                && let Some(idx) = app.git_graph.find_row_by_oid(oid)
            {
                app.git_graph.selected_idx = idx;
                app.git_graph.selected_commit = Some(oid.to_string());
                app.git_graph.selection_anchor = None;
                app.commit_detail.range_detail = None;
                app.commit_detail.scroll = 0;
                app.load_commit_detail();
            }
            true
        }
        "git.graph.refresh" => {
            app.git_graph.cache_key = None;
            app.refresh_graph();
            true
        }
        _ => false,
    }
}

pub fn scroll(app: &mut App, delta: i32) {
    let s = &mut app.git_graph.scroll;
    *s = if delta < 0 {
        s.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        s.saturating_add(delta as usize)
    };
}

// ─── Row builder ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn build_row_line(
    row: &GraphRow,
    selected: bool,
    in_range: bool,
    is_anchor: bool,
    show_meta: bool,
    max_subject: usize,
    head_oid: Option<&str>,
    ref_map: &std::collections::HashMap<String, Vec<RefLabel>>,
    hover: bool,
    theme: &crate::ui::theme::Theme,
    search_ranges: &[Range<usize>],
    search_current: Option<Range<usize>>,
) -> (Line<'static>, u16) {
    let oid = row.commit.oid.clone();
    let sel_bg = theme.selection_bg;
    let is_head = head_oid == Some(oid.as_str());

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Anchor indicator: a slim accent bar at the very left, so the user can
    // always see where the range started even when the cursor has drifted
    // far away. Costs one cell of horizontal space on the anchor row only.
    if is_anchor && !selected {
        spans.push(Span::styled(
            "▌".to_string(),
            Style::default().fg(theme.accent),
        ));
    }

    let glyphs = render_lane_chars(row);
    for (col, ch) in glyphs.iter().enumerate() {
        let color = lane_color_for(col);
        let mut style = Style::default().fg(color);
        if col == row.node_col && *ch == '●' && is_head {
            style = style.add_modifier(Modifier::BOLD);
        } else if *ch == '●' {
            style = style.add_modifier(Modifier::DIM);
        }
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::raw(" "));

    spans.push(Span::styled(
        row.commit.short_oid.clone(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw(" "));

    if show_meta {
        let mut author = row.commit.author_name.clone();
        truncate_in_place(&mut author, 12);
        spans.push(Span::styled(
            format!("{:<12}", author),
            Style::default().fg(theme.accent),
        ));
        spans.push(Span::raw(" "));
        let rel = relative_time(row.commit.time);
        spans.push(Span::styled(
            format!("{:>4}", rel),
            Style::default().fg(theme.fg_secondary),
        ));
        spans.push(Span::raw(" "));
    }

    if let Some(labels) = ref_map.get(&oid) {
        for label in labels {
            spans.push(ref_label_span(label));
            spans.push(Span::raw(" "));
        }
    }

    let mut subject = row.commit.subject.clone();
    truncate_in_place(&mut subject, max_subject);
    let subject_style = Style::default().fg(theme.fg_primary);
    // `collect_rows(CommitGraph)` uses `row.commit.subject`; overlay ranges
    // are byte offsets into that exact string. Truncation happens after the
    // fact so short subjects have unchanged offsets; long ones get clipped
    // with `…` and matches past the clip are implicitly dropped when the
    // text shrinks (our overlay walks the shorter `subject` and naturally
    // covers only the surviving bytes).
    if search_ranges.is_empty() {
        spans.push(Span::styled(subject, subject_style));
    } else {
        let sub_tokens = vec![(subject_style, subject)];
        let overlaid = overlay_match_highlight(
            sub_tokens,
            search_ranges,
            search_current,
            theme.search_match,
            theme.search_current,
        );
        for (style, text) in overlaid {
            spans.push(Span::styled(text, style));
        }
    }

    if selected {
        // All selected rows wear `selection_bg`; when the selection is
        // part of an active range the cursor additionally picks up BOLD
        // so it reads as the focused commit within its peers. BOLD alone
        // isn't applied outside range mode to keep single-select look
        // unchanged from pre-visual-mode versions.
        spans = spans
            .into_iter()
            .map(|s| {
                let mut style = s.style.bg(sel_bg);
                if in_range {
                    style = style.add_modifier(Modifier::BOLD);
                }
                Span::styled(s.content.into_owned(), style)
            })
            .collect();
    } else if in_range {
        // Range peers share the cursor's `selection_bg` so the whole bundle
        // reads as one highlighted block. Earlier we tried `hover_bg` here
        // to dim non-cursor rows, but on dark themes that's Rgb(40,40,50) —
        // visually indistinguishable from the default background. Using the
        // same `selection_bg` with a bold cursor gives stronger affordance
        // for "these are the commits being merged".
        spans = spans
            .into_iter()
            .map(|s| Span::styled(s.content.into_owned(), s.style.bg(sel_bg)))
            .collect();
    } else if hover {
        // Apply hover wash only when not selected — the brighter sel_bg
        // should stay visible when the cursor is on the selected row.
        spans = spans
            .into_iter()
            .map(|s| {
                let style = crate::ui::hover::apply(s.style, true, theme.hover_bg);
                Span::styled(s.content.into_owned(), style)
            })
            .collect();
    }

    let total_width: u16 = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()) as u16)
        .sum();

    (Line::from(spans), total_width)
}

pub fn render_lane_chars(row: &GraphRow) -> Vec<char> {
    let mut glyphs: Vec<char> = row
        .cells
        .iter()
        .enumerate()
        .map(|(col, cell)| match cell {
            LaneCell::Empty => ' ',
            LaneCell::Pass => '│',
            LaneCell::Node => '●',
            LaneCell::Merge { from } => {
                if col < *from {
                    '├'
                } else {
                    '┤'
                }
            }
            LaneCell::Fork { to } => {
                if col < *to {
                    '╭'
                } else {
                    '╮'
                }
            }
        })
        .collect();

    for (col, cell) in row.cells.iter().enumerate() {
        let target = match cell {
            LaneCell::Merge { from } => Some(*from),
            LaneCell::Fork { to } => Some(*to),
            _ => None,
        };
        let Some(target) = target else { continue };
        let (lo, hi) = if col < target {
            (col + 1, target)
        } else {
            (target + 1, col)
        };
        for k in lo..hi {
            if matches!(row.cells.get(k), Some(LaneCell::Empty)) {
                glyphs[k] = '─';
            }
        }
    }

    glyphs
}

fn lane_color_for(col: usize) -> Color {
    const PALETTE: &[Color] = &[
        Color::Cyan,
        Color::Magenta,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Red,
    ];
    PALETTE[col % PALETTE.len()]
}

pub fn ref_label_span(label: &RefLabel) -> Span<'static> {
    let (text, fg, bg) = match label {
        RefLabel::Head => (" HEAD ".to_string(), Color::Black, Color::Cyan),
        RefLabel::Branch(n) => (format!(" {} ", n), Color::Black, Color::Green),
        RefLabel::RemoteBranch(n) => (format!(" {} ", n), Color::White, Color::DarkGray),
        RefLabel::Tag(n) => (format!(" tag: {} ", n), Color::Black, Color::Yellow),
    };
    Span::styled(
        text,
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
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

fn relative_time(author_time: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(author_time);
    relative_time_at(now, author_time)
}

pub fn relative_time_at(now_secs: i64, author_time: i64) -> String {
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
