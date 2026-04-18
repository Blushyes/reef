//! Graph tab's left sidebar — DAG of commits with lane rendering and
//! ref-label chips.

use crate::app::App;
use crate::git::RefLabel;
use crate::git::graph::{GraphRow, LaneCell};
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    // Rebuild the graph when HEAD/refs moved. The cache lookup inside
    // `App::refresh_graph` guards against workdir-only changes triggering a
    // full revwalk on every keystroke.
    app.refresh_graph();

    if app.git_graph.rows.is_empty() {
        let line = Line::from(Span::styled(
            "  无 commit",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(line, area);
        return;
    }

    let show_meta = area.width >= 60;
    let max_subject = (area.width as usize).saturating_sub(if show_meta { 40 } else { 14 });
    let head_oid = app.repo.as_ref().and_then(|r| r.head_oid());

    // Clamp scroll and auto-scroll to keep the selected row in view.
    let total = app.git_graph.rows.len();
    let height = area.height as usize;
    let sel = app.git_graph.selected_idx;
    let mut scroll = app.git_graph.scroll;
    if sel < scroll {
        scroll = sel;
    } else if sel >= scroll + height && height > 0 {
        scroll = sel + 1 - height;
    }
    let max_scroll = total.saturating_sub(height);
    scroll = scroll.min(max_scroll);
    app.git_graph.scroll = scroll;

    let rows: Vec<&GraphRow> = app.git_graph.rows.iter().collect();
    for (i, row) in rows.iter().skip(scroll).take(height).enumerate() {
        let idx = scroll + i;
        let y = area.y + i as u16;
        let hover = crate::ui::hover::is_hover(app, area, y);
        let (line, click_x_w) = build_row_line(
            row,
            idx == app.git_graph.selected_idx,
            show_meta,
            max_subject,
            head_oid.as_deref(),
            &app.git_graph.ref_map,
            hover,
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
            let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
            if !oid.is_empty() {
                if let Some(idx) = app.git_graph.rows.iter().position(|r| r.commit.oid == oid) {
                    app.git_graph.selected_idx = idx;
                    app.git_graph.selected_commit = Some(oid.to_string());
                    app.commit_detail.scroll = 0;
                    app.load_commit_detail();
                }
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
    show_meta: bool,
    max_subject: usize,
    head_oid: Option<&str>,
    ref_map: &std::collections::HashMap<String, Vec<RefLabel>>,
    hover: bool,
) -> (Line<'static>, u16) {
    let oid = row.commit.oid.clone();
    let sel_bg = Color::Rgb(40, 60, 100);
    let is_head = head_oid == Some(oid.as_str());

    let mut spans: Vec<Span<'static>> = Vec::new();

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
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(" "));
        let rel = relative_time(row.commit.time);
        spans.push(Span::styled(
            format!("{:>4}", rel),
            Style::default().fg(Color::DarkGray),
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
    spans.push(Span::styled(subject, Style::default().fg(Color::White)));

    if selected {
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
                let style = crate::ui::hover::apply(s.style, true);
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
