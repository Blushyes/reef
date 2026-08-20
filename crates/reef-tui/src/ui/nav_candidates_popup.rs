//! Inline Peek view for multi-result code navigation.

use std::borrow::Cow;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use reef_app::{LocationSurface, NavCandidateKind, NavCandidatesPopup};
use reef_core::preview::{PreviewBody, PreviewDocument};
use unicode_width::UnicodeWidthStr;

use crate::TuiApp as App;
use crate::i18n::{Msg, t};
use crate::ui::mouse::ClickAction;
use crate::ui::text::{clip_spans, overlay_match_highlight};

const PREFERRED_HEIGHT: u16 = 18;
const MIN_USEFUL_HEIGHT: u16 = 8;
const PREVIEW_PERCENT: u16 = 68;
const COMPACT_WIDTH: u16 = 82;
const COMPACT_PATH_WIDTH: usize = 36;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    app.nav_peek_preview_rect = None;
    app.nav_peek_tree_rect = None;
    app.nav_peek_visible_rows = 1;
    match app.engine.nav_peek_mode() {
        reef_app::NavPeekMode::Expanded => render_expanded(f, app, screen),
        reef_app::NavPeekMode::Compact => render_compact(f, app, screen),
    }
}

fn render_popup_surface(f: &mut Frame, area: Rect, block: Block<'_>, background: Color) -> Rect {
    let inner = block.inner(area);
    f.render_widget(Clear, inner);
    f.render_widget(
        Block::default().style(Style::default().bg(background)),
        inner,
    );
    f.render_widget(block, area);
    inner
}

fn render_expanded(f: &mut Frame, app: &mut App, screen: Rect) {
    let Some(popup) = app.engine.nav_candidates() else {
        return;
    };
    if popup.candidates.is_empty() {
        return;
    }

    let host = peek_host_rect(app, &popup, screen);
    let area = peek_area(host, app.nav_peek_anchor_row);
    if area.width < 12 || area.height < 4 {
        return;
    }

    for row in screen.y..screen.y + screen.height {
        app.hit_registry
            .register_row(screen.x, row, screen.width, ClickAction::NavCandidatesClose);
    }
    app.hit_registry
        .register(area, ClickAction::NavCandidatesCapture);

    let th = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.accent));
    let inner = render_popup_surface(f, area, block, th.chrome_bg);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(PREVIEW_PERCENT),
            Constraint::Percentage(100 - PREVIEW_PERCENT),
        ])
        .split(rows[1]);
    let viewport_rows = popup.visible_rows(columns[1].height.saturating_sub(1) as usize);
    reconcile_nav_candidates_viewport(app, &popup, viewport_rows);
    let Some(popup) = app.engine.nav_candidates() else {
        return;
    };
    render_header(f, app, rows[0], &popup);
    render_preview(f, app, columns[0], &popup);
    render_tree(f, app, columns[1], &popup);
}

fn render_compact(f: &mut Frame, app: &mut App, screen: Rect) {
    let Some(popup) = app.engine.nav_candidates() else {
        return;
    };
    if popup.candidates.is_empty() || screen.width < 4 || screen.height < 3 {
        return;
    }

    let total = popup.candidates.len();
    let max_visible = popup.compact_visible_rows(usize::MAX);
    let width = screen.width.min(COMPACT_WIDTH);
    let height = screen.height.min(max_visible as u16 + 2);
    let visible = popup.compact_visible_rows(height.saturating_sub(2) as usize);
    reconcile_nav_candidates_viewport(app, &popup, visible);
    let Some(popup) = app.engine.nav_candidates() else {
        return;
    };
    let scroll = popup.scroll.min(total.saturating_sub(visible));
    let scrollable = total > visible;
    let scrollbar_width = u16::from(scrollable);
    let x = app
        .nav_peek_anchor_col
        .clamp(screen.x, screen.x + screen.width.saturating_sub(width));
    let y = app
        .nav_peek_anchor_row
        .clamp(screen.y, screen.y + screen.height.saturating_sub(height));
    let area = Rect::new(x, y, width, height);
    app.nav_peek_tree_rect = Some(area);

    for row in screen.y..screen.y + screen.height {
        app.hit_registry
            .register_row(screen.x, row, screen.width, ClickAction::NavCandidatesClose);
    }
    app.hit_registry
        .register(area, ClickAction::NavCandidatesCapture);

    let th = app.theme;
    let kind = match popup.kind {
        NavCandidateKind::Definitions => t(Msg::NavDefinitions),
        NavCandidateKind::References => t(Msg::NavReferences),
    };
    let title = if scrollable {
        format!(
            " {} — {}–{}/{} ",
            popup.symbol,
            scroll + 1,
            (scroll + visible).min(total),
            total
        )
    } else {
        format!(" {} — {kind} ({total}) ", popup.symbol)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.border))
        .title(Span::styled(title, Style::default().fg(th.fg_secondary)));
    let inner = render_popup_surface(f, area, block, th.chrome_bg);

    let body_width = inner.width.saturating_sub(scrollbar_width);
    for (row_in_view, candidate) in popup
        .candidates
        .iter()
        .skip(scroll)
        .take(visible)
        .enumerate()
    {
        let candidate_index = scroll + row_in_view;
        let y = inner.y + row_in_view as u16;
        let selected = candidate_index == popup.selected;
        let hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .is_some_and(|column| column >= inner.x && column < inner.x + body_width);
        let background = if selected || hovered {
            th.selection_bg
        } else {
            th.chrome_bg
        };
        let location = compact_location(candidate, &popup.current_path);
        let location_width = location.width() + 3;
        let snippet = truncate_end(
            &candidate.snippet,
            (body_width as usize).saturating_sub(location_width),
        );
        let used = location_width + snippet.width();
        let padding = (body_width as usize).saturating_sub(used);
        let snippet_style = Style::default()
            .fg(th.fg_primary)
            .bg(background)
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let symbol_style = Style::default()
            .fg(th.accent)
            .bg(th.search_match)
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(
            format!(" {location}  "),
            Style::default()
                .fg(th.accent)
                .bg(background)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(compact_snippet_spans(
            &snippet,
            &popup.symbol,
            snippet_style,
            symbol_style,
        ));
        spans.push(Span::styled(
            " ".repeat(padding),
            Style::default().bg(background),
        ));
        f.render_widget(Line::from(spans), Rect::new(inner.x, y, body_width, 1));
        app.hit_registry.register_row(
            inner.x,
            y,
            body_width,
            ClickAction::NavCandidateSelect(candidate_index),
        );
    }

    if scrollable {
        render_compact_scrollbar(f, app, inner, total, visible, scroll);
    }
}

fn reconcile_nav_candidates_viewport(
    app: &mut App,
    popup: &NavCandidatesPopup,
    viewport_rows: usize,
) {
    let viewport_rows = viewport_rows.max(1);
    app.nav_peek_visible_rows = viewport_rows;
    let view = (popup.selected, viewport_rows);
    if app.nav_peek_reconciled_view == Some(view) {
        return;
    }
    app.engine
        .dispatch(reef_app::AppCommand::ReconcileNavCandidatesViewport { viewport_rows });
    app.nav_peek_reconciled_view = Some(view);
}

fn render_compact_scrollbar(
    f: &mut Frame,
    app: &App,
    area: Rect,
    total: usize,
    visible: usize,
    scroll: usize,
) {
    let Some(thumb) = scrollbar_thumb(total, visible, scroll) else {
        return;
    };
    let x = area.x + area.width.saturating_sub(1);
    for row in 0..visible {
        let (glyph, color) = if thumb.contains(&row) {
            ("█", app.theme.accent)
        } else {
            ("│", app.theme.border)
        };
        f.render_widget(
            Line::from(Span::styled(
                glyph,
                Style::default().fg(color).bg(app.theme.chrome_bg),
            )),
            Rect::new(x, area.y + row as u16, 1, 1),
        );
    }
}

fn compact_location(candidate: &reef_core::nav::Location, current_path: &Path) -> String {
    let path = candidate.path.as_deref().unwrap_or(current_path);
    format!(
        "{}:{}",
        truncate_start(&path.to_string_lossy(), COMPACT_PATH_WIDTH),
        candidate.line + 1
    )
}

fn compact_snippet_spans<'a>(
    snippet: &'a str,
    symbol: &str,
    normal_style: Style,
    symbol_style: Style,
) -> Vec<Span<'a>> {
    if symbol.is_empty() {
        return vec![Span::styled(snippet, normal_style)];
    }
    let mut spans = Vec::new();
    let mut cursor = 0;
    for (start, matched) in snippet.match_indices(symbol) {
        if start > cursor {
            spans.push(Span::styled(&snippet[cursor..start], normal_style));
        }
        let end = start + matched.len();
        spans.push(Span::styled(&snippet[start..end], symbol_style));
        cursor = end;
    }
    if cursor < snippet.len() {
        spans.push(Span::styled(&snippet[cursor..], normal_style));
    }
    if spans.is_empty() {
        spans.push(Span::styled(snippet, normal_style));
    }
    spans
}

fn peek_host_rect(app: &App, popup: &NavCandidatesPopup, screen: Rect) -> Rect {
    let candidate = match popup.origin.surface {
        LocationSurface::FilePreview | LocationSurface::SearchPreview => app.last_preview_rect,
        LocationSurface::GitDiff { .. } | LocationSurface::GraphDiff { .. } => app.last_diff_rect,
    };
    candidate
        .filter(|rect| rect.width > 0 && rect.height > 0)
        .unwrap_or(screen)
}

fn peek_area(host: Rect, anchor_row: u16) -> Rect {
    let height = host.height.min(PREFERRED_HEIGHT);
    if height == 0 {
        return host;
    }
    let bottom = host.y.saturating_add(host.height);
    let below = bottom.saturating_sub(anchor_row);
    let y = if anchor_row >= host.y && below >= MIN_USEFUL_HEIGHT.min(height) {
        anchor_row
    } else {
        bottom.saturating_sub(height)
    };
    Rect::new(host.x, y, host.width, height.min(bottom.saturating_sub(y)))
}

fn render_header(f: &mut Frame, app: &mut App, area: Rect, popup: &NavCandidatesPopup) {
    if area.width == 0 {
        return;
    }
    let th = app.theme;
    let selected_path = popup.selected_path();
    let file = selected_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| selected_path.to_string_lossy());
    let parent = selected_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy());
    let kind = match popup.kind {
        NavCandidateKind::Definitions => t(Msg::NavDefinitions),
        NavCandidateKind::References => t(Msg::NavReferences),
    };
    let meta = format!("{} — {} ({})", popup.symbol, kind, popup.candidates.len());

    let mut spans = vec![Span::styled(
        format!(" {file}"),
        Style::default()
            .fg(th.fg_primary)
            .bg(th.chrome_bg)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(parent) = parent {
        spans.push(Span::styled(
            format!("  {parent}"),
            Style::default().fg(th.fg_secondary).bg(th.chrome_bg),
        ));
    }
    spans.push(Span::styled(
        format!("  › {meta}"),
        Style::default().fg(th.fg_secondary).bg(th.chrome_bg),
    ));
    f.render_widget(Line::from(spans), area);

    let close_x = area.x + area.width.saturating_sub(2);
    f.render_widget(
        Line::from(Span::styled(
            "× ",
            Style::default().fg(th.fg_primary).bg(th.chrome_bg),
        )),
        Rect::new(close_x, area.y, area.width.min(2), 1),
    );
    app.hit_registry.register_row(
        close_x,
        area.y,
        area.width.min(2),
        ClickAction::NavCandidatesClose,
    );
}

fn render_preview(f: &mut Frame, app: &mut App, area: Rect, popup: &NavCandidatesPopup) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    app.nav_peek_preview_rect = Some(area);
    app.nav_peek_preview_max_scroll = 0;
    let th = app.theme;
    let frame = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(th.accent))
        .style(Style::default().bg(th.chrome_bg));
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let Some(candidate) = popup.candidates.get(popup.selected) else {
        return;
    };
    let selected_path = popup.selected_path();
    let preview = app.engine.nav_preview_content();
    let matching = preview
        .as_deref()
        .filter(|preview| Path::new(&preview.path) == selected_path);
    match matching {
        Some(preview) => render_preview_document(f, app, inner, preview, candidate),
        None if app.engine.nav_preview_loading() => {
            render_preview_message(f, inner, t(Msg::NavPreviewLoading), th.fg_secondary)
        }
        None => render_preview_message(
            f,
            inner,
            app.engine
                .nav_preview_error()
                .unwrap_or_else(|| t(Msg::NavPreviewUnavailable)),
            th.fg_secondary,
        ),
    }
}

fn render_preview_document(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    preview: &PreviewDocument,
    candidate: &reef_core::nav::Location,
) {
    let PreviewBody::Text(text) = &preview.body else {
        render_preview_message(f, area, "Preview unavailable", app.theme.fg_secondary);
        return;
    };
    let height = area.height as usize;
    let target = (
        preview.path.clone(),
        candidate.line,
        candidate.byte_range.start,
    );
    if app.nav_peek_preview_target.as_ref() != Some(&target) {
        app.nav_peek_preview_scroll = preview_start(text.lines.len(), candidate.line, height);
        app.nav_peek_preview_target = Some(target);
    }
    app.nav_peek_preview_max_scroll = text.lines.len().saturating_sub(height);
    app.nav_peek_preview_scroll = app
        .nav_peek_preview_scroll
        .min(app.nav_peek_preview_max_scroll);
    let start = app.nav_peek_preview_scroll;
    let gutter_width = text.lines.len().max(1).ilog10() as usize + 2;
    let scrollable = text.lines.len() > height;
    let scrollbar_width = usize::from(scrollable);
    let content_width = (area.width as usize)
        .saturating_sub(gutter_width)
        .saturating_sub(scrollbar_width);

    for (visible_row, line) in text.lines.iter().skip(start).take(height).enumerate() {
        let line_index = start + visible_row;
        let y = area.y + visible_row as u16;
        let gutter = Span::styled(
            format!("{:>width$} ", line_index + 1, width = gutter_width - 1),
            Style::default()
                .fg(app.theme.fg_secondary)
                .bg(app.theme.chrome_bg),
        );
        let base_tokens: Vec<(Style, Cow<'_, str>)> = text
            .highlighted
            .as_ref()
            .and_then(|highlighted| highlighted.get(line_index))
            .map(|tokens| {
                tokens
                    .iter()
                    .map(|token| {
                        (
                            crate::ui::highlight::to_ratatui_style(token.style)
                                .bg(app.theme.chrome_bg),
                            Cow::Borrowed(token.text.as_str()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![(
                    Style::default()
                        .fg(app.theme.fg_primary)
                        .bg(app.theme.chrome_bg),
                    Cow::Borrowed(line.as_str()),
                )]
            });
        let tokens = if line_index == candidate.line
            && candidate.byte_range.start < candidate.byte_range.end
        {
            overlay_match_highlight(
                base_tokens,
                std::slice::from_ref(&candidate.byte_range),
                Some(candidate.byte_range.clone()),
                app.theme.search_match,
                app.theme.search_current,
            )
        } else {
            base_tokens
        };
        let mut spans = vec![gutter];
        spans.extend(clip_spans(&tokens, 0, content_width));
        f.render_widget(
            Line::from(spans),
            Rect::new(
                area.x,
                y,
                area.width.saturating_sub(scrollbar_width as u16),
                1,
            ),
        );
    }
    if scrollable {
        render_preview_scrollbar(
            f,
            app,
            area,
            text.lines.len(),
            height,
            app.nav_peek_preview_scroll,
        );
    }
}

fn render_preview_scrollbar(
    f: &mut Frame,
    app: &App,
    area: Rect,
    total_rows: usize,
    visible_rows: usize,
    scroll: usize,
) {
    let Some(thumb) = scrollbar_thumb(total_rows, visible_rows, scroll) else {
        return;
    };
    let x = area.x + area.width.saturating_sub(1);
    for row in 0..visible_rows {
        let (glyph, color) = if thumb.contains(&row) {
            ("█", app.theme.accent)
        } else {
            ("│", app.theme.border)
        };
        f.render_widget(
            Line::from(Span::styled(
                glyph,
                Style::default().fg(color).bg(app.theme.chrome_bg),
            )),
            Rect::new(x, area.y + row as u16, 1, 1),
        );
    }
}

fn scrollbar_thumb(
    total_rows: usize,
    visible_rows: usize,
    scroll: usize,
) -> Option<std::ops::Range<usize>> {
    if visible_rows == 0 || total_rows <= visible_rows {
        return None;
    }
    let thumb_len = ((visible_rows * visible_rows) / total_rows)
        .max(1)
        .min(visible_rows);
    let track = visible_rows - thumb_len;
    let max_scroll = total_rows - visible_rows;
    let thumb_start = scroll.min(max_scroll) * track / max_scroll;
    Some(thumb_start..thumb_start + thumb_len)
}

fn preview_start(line_count: usize, target: usize, height: usize) -> usize {
    let target = target.min(line_count.saturating_sub(1));
    target
        .saturating_sub(height / 2)
        .min(line_count.saturating_sub(height))
}

fn render_preview_message(f: &mut Frame, area: Rect, message: &str, color: ratatui::style::Color) {
    if area.height == 0 {
        return;
    }
    f.render_widget(
        Line::from(Span::styled(
            format!("  {message}"),
            Style::default().fg(color),
        )),
        Rect::new(area.x, area.y + area.height / 2, area.width, 1),
    );
}

fn render_tree(f: &mut Frame, app: &mut App, area: Rect, popup: &NavCandidatesPopup) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    app.nav_peek_tree_rect = Some(area);
    let th = app.theme;
    let frame = Block::default()
        .borders(Borders::TOP | Borders::LEFT)
        .border_style(Style::default().fg(th.accent))
        .style(Style::default().bg(th.chrome_bg));
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let visible = popup.visible_rows(inner.height as usize);
    let visible_start = popup
        .scroll
        .min(popup.tree_row_count().saturating_sub(visible));
    let visible_end = visible_start + visible;
    let mut tree_row = 0;
    for (group_index, group) in popup.groups.iter().enumerate() {
        if tree_row >= visible_start && tree_row < visible_end {
            let y = inner.y + (tree_row - visible_start) as u16;
            render_group_row(f, app, inner, y, group_index, group);
        }
        tree_row += 1;
        if !group.expanded {
            continue;
        }
        for candidate_index in group.candidate_range.clone() {
            if tree_row >= visible_start && tree_row < visible_end {
                let y = inner.y + (tree_row - visible_start) as u16;
                if let Some(candidate) = popup.candidates.get(candidate_index) {
                    render_candidate_row(f, app, inner, y, candidate_index, candidate, popup);
                }
            }
            tree_row += 1;
        }
    }
}

fn render_group_row(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    y: u16,
    group_index: usize,
    group: &reef_app::NavCandidateGroup,
) {
    let th = app.theme;
    let file = group
        .path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| group.path.to_string_lossy());
    let parent = group
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy());
    let marker = if group.expanded { "⌄" } else { "›" };
    let mut spans = vec![Span::styled(
        format!(" {marker} {file}"),
        Style::default()
            .fg(th.fg_primary)
            .bg(th.chrome_bg)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(parent) = parent {
        spans.push(Span::styled(
            format!("  {parent}"),
            Style::default().fg(th.fg_secondary).bg(th.chrome_bg),
        ));
    }
    spans.push(Span::styled(
        format!("  {}", group.candidate_range.len()),
        Style::default().fg(th.fg_secondary).bg(th.chrome_bg),
    ));
    f.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));
    app.hit_registry.register_row(
        area.x,
        y,
        area.width,
        ClickAction::NavCandidateGroupToggle(group_index),
    );
}

fn render_candidate_row(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    y: u16,
    candidate_index: usize,
    candidate: &reef_core::nav::Location,
    popup: &NavCandidatesPopup,
) {
    let th = app.theme;
    let selected = candidate_index == popup.selected;
    let hovered = app.hover_row == Some(y)
        && app
            .hover_col
            .is_some_and(|column| column >= area.x && column < area.x + area.width);
    let background = if selected {
        th.selection_bg
    } else if hovered {
        th.hover_bg
    } else {
        th.chrome_bg
    };
    let line_number = format!("  {:>4}  ", candidate.line + 1);
    let snippet_width = (area.width as usize).saturating_sub(line_number.width());
    let mut spans = vec![Span::styled(
        line_number,
        Style::default().fg(th.fg_secondary).bg(background),
    )];
    let snippet = truncate_end(&candidate.snippet, snippet_width);
    spans.extend(exact_snippet_spans(
        &snippet,
        candidate.snippet_match_range.clone(),
        Style::default().fg(th.fg_primary).bg(background),
        Style::default()
            .fg(th.accent)
            .bg(th.search_match)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));
    app.hit_registry.register_row(
        area.x,
        y,
        area.width,
        ClickAction::NavCandidateSelect(candidate_index),
    );
}

fn exact_snippet_spans(
    snippet: &str,
    match_range: std::ops::Range<usize>,
    normal_style: Style,
    match_style: Style,
) -> Vec<Span<'_>> {
    if match_range.is_empty()
        || snippet.get(match_range.clone()).is_none()
        || !snippet.is_char_boundary(match_range.start)
        || !snippet.is_char_boundary(match_range.end)
    {
        return vec![Span::styled(snippet, normal_style)];
    }
    let mut spans = Vec::with_capacity(3);
    if match_range.start > 0 {
        spans.push(Span::styled(&snippet[..match_range.start], normal_style));
    }
    spans.push(Span::styled(&snippet[match_range.clone()], match_style));
    if match_range.end < snippet.len() {
        spans.push(Span::styled(&snippet[match_range.end..], normal_style));
    }
    spans
}

fn truncate_end(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for character in text.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width + 1 > max_width {
            break;
        }
        out.push(character);
        width += character_width;
    }
    out.push('…');
    out
}

fn truncate_start(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let suffix_width = max_width - 1;
    let mut width = 0;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > suffix_width {
            break;
        }
        width += character_width;
        start = index;
    }
    format!("…{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn popup_surface_preserves_background_behind_border_cells() {
        let backend = TestBackend::new(7, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                frame.render_widget(
                    Block::default().style(Style::default().bg(Color::Blue)),
                    frame.area(),
                );
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Green));
                render_popup_surface(frame, Rect::new(1, 1, 5, 4), block, Color::Red);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(
            (buffer[(1, 1)].bg, buffer[(2, 2)].bg),
            (Color::Blue, Color::Red)
        );
    }

    #[test]
    fn peek_area_uses_anchor_when_enough_space_remains() {
        let host = Rect::new(10, 2, 100, 30);

        assert_eq!(peek_area(host, 12), Rect::new(10, 12, 100, 18));
    }

    #[test]
    fn peek_area_moves_up_when_anchor_is_near_bottom() {
        let host = Rect::new(10, 2, 100, 20);

        assert_eq!(peek_area(host, 20), Rect::new(10, 4, 100, 18));
    }

    #[test]
    fn preview_start_centers_target_and_clamps_at_end() {
        assert_eq!(preview_start(100, 98, 10), 90);
    }

    #[test]
    fn scrollbar_thumb_starts_at_top_for_zero_scroll() {
        assert_eq!(scrollbar_thumb(100, 10, 0), Some(0..1));
    }

    #[test]
    fn scrollbar_thumb_reaches_bottom_at_max_scroll() {
        assert_eq!(scrollbar_thumb(100, 10, 90), Some(9..10));
    }

    #[test]
    fn scrollbar_thumb_is_absent_when_content_fits() {
        assert_eq!(scrollbar_thumb(10, 10, 0), None);
    }

    #[test]
    fn truncate_end_respects_unicode_display_width() {
        assert_eq!(truncate_end("abc界def", 6), "abc界…");
    }

    #[test]
    fn compact_location_preserves_filename_when_path_is_wide() {
        let candidate = reef_core::nav::Location {
            path: Some(
                "a/very/long/workspace/path/with/many/components/navigation/engine.rs".into(),
            ),
            line: 41,
            byte_range: 0..4,
            snippet: "ReefApp::new()".to_owned(),
            snippet_match_range: 0..4,
        };

        let location = compact_location(&candidate, Path::new("unused.rs"));

        assert!(location.starts_with('…') && location.ends_with("engine.rs:42"));
    }

    #[test]
    fn compact_snippet_spans_highlight_every_symbol_occurrence() {
        let normal = Style::default().fg(ratatui::style::Color::White);
        let highlighted = Style::default().fg(ratatui::style::Color::Yellow);

        let spans = compact_snippet_spans(
            "crate::diff::{DiffLine, Vec<DiffLine>}",
            "DiffLine",
            normal,
            highlighted,
        );

        assert_eq!(
            spans
                .iter()
                .map(|span| (span.content.as_ref(), span.style))
                .collect::<Vec<_>>(),
            vec![
                ("crate::diff::{", normal),
                ("DiffLine", highlighted),
                (", Vec<", normal),
                ("DiffLine", highlighted),
                (">}", normal),
            ]
        );
    }

    #[test]
    fn exact_snippet_spans_highlight_the_candidate_occurrence() {
        let normal = Style::default().fg(ratatui::style::Color::White);
        let highlighted = Style::default().fg(ratatui::style::Color::Yellow);
        let snippet = "target(target())";

        let spans = exact_snippet_spans(snippet, 7..13, normal, highlighted);

        assert_eq!(
            spans
                .iter()
                .map(|span| (span.content.as_ref(), span.style))
                .collect::<Vec<_>>(),
            vec![
                ("target(", normal),
                ("target", highlighted),
                ("())", normal)
            ]
        );
    }
}
