//! SQLite preview body — the rendering path for `PreviewBody::Database`.
//!
//! Split out from [`crate::ui::file_preview_panel`] because the body
//! is significantly heavier than the Text / Image / Binary peers
//! (multi-pane layout with tables list + data grid + clickable
//! pagination chips + a goto-page input prompt) and the helpers
//! (column-width math, affinity colors, page-chip windowing) all
//! belong with it. The dispatch in `file_preview_panel::render`
//! calls into [`render`] for the Database arm; everything else here
//! is private.
//!
//! Public helpers `natural_column_widths` / `total_table_width` are
//! consumed by [`crate::app::DbPreviewState::recompute_layout`] so
//! the cache stays the single source of truth for the data pane's
//! layout. They live here (with the rest of the column-width logic)
//! rather than on App, but stay `pub(crate)` so the boundary's
//! intentional.

use crate::app::App;
use crate::i18n::{Msg, t};
use crate::ui::text::clip_spans;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use reef_sqlite_preview::{ColumnInfo, DatabaseInfo, SqliteValue, TableSummary};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width reserved for the left-side tables list when the panel is
/// wide enough to show both panes. Below `MIN_TWO_PANE_WIDTH` we drop
/// the list entirely and just show the data — the `[`/`]` keybinding
/// still cycles tables, the user just doesn't see the list.
const TABLES_PANEL_WIDTH: u16 = 22;
/// Below this panel width we skip rendering the tables list and let
/// the data area consume the full width.
const MIN_TWO_PANE_WIDTH: u16 = 50;
/// Cell separator between data columns. 3 chars wide; matches the
/// vertical-rule glyph used elsewhere in Reef's diff layout.
const COL_SEP: &str = " │ ";

/// SQLite preview body. Layout (panel width permitting):
///
/// ```text
/// filename                                       ← card header
/// ────────────────────────────────────────       ← separator
/// sqlite · 12 tables · 4.1 MB                    ← meta line
///                                                ← spacer
///  tables           id   │ name      │ email     ← tables list + column header
/// ▶users (3)        1    │ alice     │ a@x.io
///  posts (42)       2    │ bob       │ NULL
///  sessions (1)     3    │ carol     │ c@x.io
/// page 1 / 1 · row 1-3 / 3                       ← footer
/// ```
///
/// `preview_scroll` doubles as the in-page row offset — Up/Down/
/// Ctrl+P/N mutate it, the data pane slices `current_rows` from
/// there. PgUp/PgDn flip whole pages via `db_navigate`. The
/// renderer clamps preview_scroll on every frame so an over-scroll
/// past the page bottom snaps back rather than rendering an empty
/// view.
pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    path: &str,
    info: &DatabaseInfo,
) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let mut y = crate::ui::file_preview_panel::render_card_header(f, area, path, &th);

    // Meta line — same shape as the binary card's `application/x · N B`
    // so the eye picks it up in the same place across formats.
    if y < max_y {
        let meta = format!(
            "sqlite · {} {} · {}",
            info.tables.len(),
            t(Msg::DbTablesHeader),
            crate::file_tree::human_bytes(info.bytes_on_disk),
        );
        f.render_widget(
            Line::from(Span::styled(meta, Style::default().fg(th.fg_secondary))),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    // Spacer.
    if y < max_y {
        y += 1;
    }

    // Empty database — single centred line, skip the panes entirely.
    if info.tables.is_empty() {
        if y < max_y {
            let msg = t(Msg::DbEmpty);
            let w = UnicodeWidthStr::width(msg) as u16;
            let cy = y + (max_y - y) / 2;
            let cx = area.x + area.width.saturating_sub(w) / 2;
            f.render_widget(
                Line::from(Span::styled(msg, Style::default().fg(th.fg_secondary))),
                Rect::new(cx, cy, area.width.saturating_sub(cx - area.x), 1),
            );
        }
        return;
    }

    // Read pagination state when present and pointing at the same
    // file. State is rebuilt by `apply_worker_result` on every preview
    // land, so a missing or stale state means we just sat down on a
    // fresh `.db` — fall back to `info.initial_page` defaults.
    let state = app.db_preview_state.as_ref().filter(|s| s.path == path);
    let selected_idx = state
        .map(|s| s.selected_table)
        .unwrap_or(info.selected_table)
        .min(info.tables.len() - 1);
    let selected_table = &info.tables[selected_idx];
    let rows: &[Vec<SqliteValue>] = state
        .map(|s| s.current_rows.as_slice())
        .unwrap_or(info.initial_page.rows.as_slice());
    let page_index: u64 = state.map(|s| s.page).unwrap_or(0);
    let rows_per_page: u64 = state
        .map(|s| s.rows_per_page as u64)
        .unwrap_or(rows.len().max(1) as u64);

    // Clamp `preview_scroll` against the number of rows we actually
    // have on this page. Up/Down/Ctrl+P/N just mutate the field
    // unconditionally; we snap back here so an over-scroll past the
    // bottom doesn't leave the data pane empty.
    let max_scroll = rows.len().saturating_sub(1);
    if app.preview_scroll > max_scroll {
        app.preview_scroll = max_scroll;
    }
    let row_offset = app.preview_scroll;

    // Reserve last row for the page footer; the body sits between
    // `y` and `body_max_y`. Skipping the footer if there's only one
    // row left avoids a half-rendered card on a 6-row-tall panel.
    let body_max_y = max_y.saturating_sub(1);

    // Layout: tables list on the left when the panel is wide enough,
    // data area takes everything else.
    let two_pane = area.width >= MIN_TWO_PANE_WIDTH;
    let tables_w = if two_pane { TABLES_PANEL_WIDTH } else { 0 };
    let tables_x = area.x;
    let data_x = area.x + tables_w + if two_pane { 1 } else { 0 };
    let data_w = area
        .width
        .saturating_sub(tables_w + if two_pane { 1 } else { 0 });

    if two_pane {
        render_tables_list(
            f,
            &th,
            Rect::new(tables_x, y, tables_w, body_max_y - y),
            &info.tables,
            selected_idx,
        );
    }

    // Column widths come from the cache on `db_preview_state` —
    // recomputed only when current_rows / selected_table change, not
    // on every render. This makes h-scroll cheap: a single keypress
    // is O(1) work in the render path (clip_spans on visible rows
    // only), no O(rows × cols × avg_str_len) re-walk per frame. When
    // state is missing (the rare race window between preview-land
    // and the apply_worker_result hook) we fall back to computing on
    // the fly so the table still draws.
    let owned_widths: Vec<usize>;
    let owned_total_w: usize;
    let (col_widths, total_table_w) = match state {
        Some(s) if !s.col_widths.is_empty() => (s.col_widths.as_slice(), s.total_table_w),
        _ => {
            owned_widths = natural_column_widths(&selected_table.columns, rows);
            owned_total_w = total_table_width(&owned_widths);
            (owned_widths.as_slice(), owned_total_w)
        }
    };
    let max_h_scroll = total_table_w.saturating_sub(data_w as usize);
    if app.preview_h_scroll > max_h_scroll {
        app.preview_h_scroll = max_h_scroll;
    }
    let h_scroll = app.preview_h_scroll;

    render_data_pane(
        f,
        &th,
        Rect::new(data_x, y, data_w, body_max_y - y),
        &selected_table.columns,
        col_widths,
        &rows[row_offset.min(rows.len())..],
        h_scroll,
    );

    // Footer — page chips + row range + selected-table indicator,
    // or the goto-input prompt when the user has invoked `g`.
    if body_max_y < max_y {
        if let Some(buf) = app.db_goto_input.as_ref() {
            // Prompt style — bold prompt label, regular digits, plus
            // a block-cursor glyph at the end so the typing focus is
            // visually obvious without us having to drive a real
            // terminal cursor (which would conflict with the rest of
            // the TUI's draw cycle).
            let prompt_line = Line::from(vec![
                Span::styled(
                    format!("{}: ", t(Msg::DbGotoPagePrompt)),
                    Style::default()
                        .fg(th.fg_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(buf.clone(), Style::default().fg(th.fg_primary)),
                Span::styled(
                    "▏",
                    Style::default()
                        .fg(th.fg_primary)
                        .add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  {}", t(Msg::DbGotoPageHint)),
                    Style::default().fg(th.fg_secondary),
                ),
            ]);
            f.render_widget(prompt_line, Rect::new(area.x, body_max_y, area.width, 1));
        } else {
            render_pagination_footer(
                f,
                app,
                Rect::new(area.x, body_max_y, area.width, 1),
                &th,
                FooterContext {
                    table: selected_table,
                    selected_idx,
                    table_count: info.tables.len(),
                    page_index,
                    rows_per_page,
                },
            );
        }
    }
}

/// Bundle of SQLite-state-derived params for the pagination footer.
/// Five fields that all flow from `(info, state)` at the call site —
/// grouped into one borrow rather than spread across positional args
/// so the rendering signature stays comfortably under clippy's
/// `too_many_arguments` threshold and the call site reads as a
/// labeled struct literal rather than a positional argument list.
struct FooterContext<'a> {
    table: &'a TableSummary,
    selected_idx: usize,
    table_count: usize,
    page_index: u64,
    rows_per_page: u64,
}

/// Footer with chip-style pagination buttons. Layout:
///
/// ```text
/// ‹  1  …  4  [5]  6  …  50  ›    ·  row 201-250 / 2500  ·  users (1/3)
/// ```
///
/// Each numeric chip plus the `‹` / `›` arrows is registered with the
/// hit-test registry so a left-click dispatches the matching
/// `ClickAction`. Ellipsis chips and the trailing text are
/// non-interactive. When pages overflow the panel width we still
/// always show first / last + the active page; intermediate chips
/// drop off into the ellipsis on whichever side is longer.
fn render_pagination_footer(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    theme: &crate::ui::theme::Theme,
    ctx: FooterContext<'_>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let total_pages = if ctx.rows_per_page == 0 {
        1
    } else {
        ctx.table.row_count.div_ceil(ctx.rows_per_page).max(1)
    };
    let current = ctx.page_index + 1;
    let chips = build_page_chips(current, total_pages);

    let mut x = area.x;
    let max_x = area.x + area.width;

    // "page  " prefix label — non-interactive, just an anchor word so
    // the chip strip reads as a sentence rather than a row of glyphs.
    let prefix = format!("{}  ", t(Msg::DbPageLabel));
    let prefix_w = UnicodeWidthStr::width(prefix.as_str()) as u16;
    if prefix_w < max_x - x {
        f.render_widget(
            Line::from(Span::styled(
                prefix,
                Style::default().fg(theme.fg_secondary),
            )),
            Rect::new(x, area.y, prefix_w, 1),
        );
        x += prefix_w;
    }

    // Render the prev/next + numeric chips. Each chip carries an
    // optional `ClickAction` — None means the chip is inert (active
    // page, ellipsis, or boundary-disabled arrow) and we skip the
    // hit-region register.
    for (i, chip) in chips.iter().enumerate() {
        if x >= max_x {
            break;
        }
        let label = chip.label();
        let label_w = UnicodeWidthStr::width(label.as_str()) as u16;
        let span_text = if i + 1 < chips.len() {
            // Two-space gap between chips so the click target stays
            // visually separated from its neighbours and the
            // hit-region's right edge has a bit of slack.
            format!("{label}  ")
        } else {
            label
        };
        let span_w = UnicodeWidthStr::width(span_text.as_str()) as u16;
        let style = chip.style(theme);
        let render_w = span_w.min(max_x - x);
        f.render_widget(
            Line::from(Span::styled(span_text.clone(), style)),
            Rect::new(x, area.y, render_w, 1),
        );
        if let Some(action) = chip.action() {
            // Register only the label cells, not the trailing space.
            // Width capped at remaining row.
            let hit_w = label_w.min(max_x - x);
            if hit_w > 0 {
                app.hit_registry.register_row(x, area.y, hit_w, action);
            }
        }
        x += span_w;
    }

    // Trailing text: row range + table indicator. Mirrors the old
    // single-string footer so users keep the same context.
    if x >= max_x {
        return;
    }
    let row_start = if ctx.table.row_count > 0 {
        ctx.page_index * ctx.rows_per_page + 1
    } else {
        0
    };
    let row_end = (ctx.page_index * ctx.rows_per_page + ctx.rows_per_page).min(ctx.table.row_count);
    let suffix = format!(
        " ·  {} {}-{} / {}  ·  {} ({}/{})",
        t(Msg::DbRowsLabel),
        row_start,
        row_end,
        ctx.table.row_count,
        ctx.table.name,
        ctx.selected_idx + 1,
        ctx.table_count,
    );
    let suffix_w = UnicodeWidthStr::width(suffix.as_str()) as u16;
    let render_w = suffix_w.min(max_x - x);
    f.render_widget(
        Line::from(Span::styled(
            suffix,
            Style::default().fg(theme.fg_secondary),
        )),
        Rect::new(x, area.y, render_w, 1),
    );
}

/// One pagination chip — a prev/next arrow, a page number, an active
/// (current) page indicator, or an ellipsis spacer. `action()` is
/// `Some` exactly when the chip is clickable.
#[derive(Debug, PartialEq, Eq)]
enum PageChip {
    Prev { enabled: bool },
    Next { enabled: bool },
    Page(u64),
    Active(u64),
    Ellipsis,
}

impl PageChip {
    fn label(&self) -> String {
        match self {
            PageChip::Prev { .. } => "‹".to_string(),
            PageChip::Next { .. } => "›".to_string(),
            PageChip::Page(n) => n.to_string(),
            PageChip::Active(n) => format!("[{n}]"),
            PageChip::Ellipsis => "…".to_string(),
        }
    }

    fn action(&self) -> Option<crate::ui::mouse::ClickAction> {
        use crate::ui::mouse::ClickAction;
        match self {
            PageChip::Prev { enabled: true } => Some(ClickAction::DbPrevPage),
            PageChip::Next { enabled: true } => Some(ClickAction::DbNextPage),
            PageChip::Page(n) => Some(ClickAction::DbGotoPage(*n)),
            // Active/Ellipsis/disabled-arrow are inert — clicking a
            // disabled chip should do nothing, not jump back to the
            // current page (which would needlessly reissue the RPC).
            _ => None,
        }
    }

    fn style(&self, theme: &crate::ui::theme::Theme) -> Style {
        match self {
            PageChip::Active(_) => Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
            PageChip::Page(_) => Style::default().fg(theme.fg_primary),
            PageChip::Prev { enabled } | PageChip::Next { enabled } => {
                if *enabled {
                    Style::default().fg(theme.fg_primary)
                } else {
                    Style::default()
                        .fg(theme.fg_secondary)
                        .add_modifier(Modifier::DIM)
                }
            }
            PageChip::Ellipsis => Style::default().fg(theme.fg_secondary),
        }
    }
}

/// Build the chip list for `(current, total)`. Always shows the first
/// and last page as anchors; collapses long intermediate runs to an
/// ellipsis. Examples:
///
/// - `(1, 1)` → `‹ [1] ›`
/// - `(3, 5)` → `‹ 1 2 [3] 4 5 ›`
/// - `(5, 50)` → `‹ 1 … 4 [5] 6 … 50 ›`
/// - `(1, 50)` → `‹ [1] 2 … 50 ›`
/// - `(50, 50)` → `‹ 1 … 49 [50] ›`
fn build_page_chips(current: u64, total: u64) -> Vec<PageChip> {
    let mut out = Vec::new();
    out.push(PageChip::Prev {
        enabled: current > 1,
    });

    if total <= 7 {
        // Compact: render every page as its own chip.
        for p in 1..=total {
            out.push(if p == current {
                PageChip::Active(p)
            } else {
                PageChip::Page(p)
            });
        }
    } else {
        // Sliding 3-wide window around `current`, anchored by first
        // and last page chips. Window edges clamp inside [2, total-1]
        // so the "always show 1 and total" guarantee never produces
        // a duplicate chip.
        let near_lo = current.saturating_sub(1).max(2);
        let near_hi = (current + 1).min(total - 1);

        // First page anchor.
        if current == 1 {
            out.push(PageChip::Active(1));
        } else {
            out.push(PageChip::Page(1));
        }
        // Left ellipsis when there's a gap between page 1 and the
        // window. `near_lo > 2` means at least one page is missing.
        if near_lo > 2 {
            out.push(PageChip::Ellipsis);
        }
        // Window pages, skipping 1 / total which are already anchors.
        for p in near_lo..=near_hi {
            if p == 1 || p == total {
                continue;
            }
            out.push(if p == current {
                PageChip::Active(p)
            } else {
                PageChip::Page(p)
            });
        }
        // Right ellipsis.
        if near_hi < total - 1 {
            out.push(PageChip::Ellipsis);
        }
        // Last page anchor.
        if current == total {
            out.push(PageChip::Active(total));
        } else {
            out.push(PageChip::Page(total));
        }
    }

    out.push(PageChip::Next {
        enabled: current < total,
    });
    out
}

/// Render the left-side tables list. The selected row is prefixed
/// with `▶`, others with a leading space; both name and row count are
/// right-truncated to fit the panel width (22 chars by default — see
/// [`TABLES_PANEL_WIDTH`]).
fn render_tables_list(
    f: &mut Frame,
    theme: &crate::ui::theme::Theme,
    area: Rect,
    tables: &[TableSummary],
    selected_idx: usize,
) {
    if area.width < 4 || area.height < 1 {
        return;
    }
    // Header row.
    let header = clip_or_pad(t(Msg::DbTablesHeader), area.width as usize - 1);
    f.render_widget(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                header,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Rect::new(area.x, area.y, area.width, 1),
    );
    // Body — one row per table, scroll-clamped at the bottom.
    let body_h = area.height.saturating_sub(1);
    let visible = (body_h as usize).min(tables.len().saturating_sub(scroll_offset_for(
        selected_idx,
        tables.len(),
        body_h as usize,
    )));
    let scroll = scroll_offset_for(selected_idx, tables.len(), body_h as usize);
    for i in 0..visible {
        let idx = scroll + i;
        let t_summary = &tables[idx];
        let is_sel = idx == selected_idx;
        let prefix = if is_sel { "▶" } else { " " };
        let label = format!("{} ({})", t_summary.name, t_summary.row_count);
        let body = clip_or_pad(&label, area.width as usize - 1);
        let style = if is_sel {
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg_secondary)
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(prefix.to_string(), style),
                Span::styled(body, style),
            ]),
            Rect::new(area.x, area.y + 1 + i as u16, area.width, 1),
        );
    }
}

/// First-visible-row index for a scrolling list of `total` items in
/// `viewport` rows when `selected` should be visible. Keeps selected
/// roughly in view without aggressive recentering — selection just
/// past the bottom edge scrolls one row at a time, like a typical
/// vim-style listing.
fn scroll_offset_for(selected: usize, total: usize, viewport: usize) -> usize {
    if total <= viewport || viewport == 0 {
        return 0;
    }
    if selected < viewport {
        0
    } else {
        (selected + 1).saturating_sub(viewport)
    }
}

/// Render the right-side data area: a two-row header (column names
/// plus affinity-colored type tags) followed by `rows` of cells.
/// Columns are pre-sized in `col_widths` (natural widths from the
/// max of name / type tag / all current-page row cells); within each
/// row we pad cells to those widths and join with ` │ `. The whole
/// row is then handed to [`clip_spans`] with `h_scroll` so any
/// overflow past `area.width` becomes a horizontal scroll instead
/// of a per-cell `…` truncation. NULL is shown italic dimmed, BLOB
/// as `<blob N B>` italic dimmed, TEXT preserves the reader-side `…`
/// suffix when the value was reader-truncated at
/// `MAX_TEXT_CELL_CHARS`.
fn render_data_pane(
    f: &mut Frame,
    theme: &crate::ui::theme::Theme,
    area: Rect,
    columns: &[ColumnInfo],
    col_widths: &[usize],
    rows: &[Vec<SqliteValue>],
    h_scroll: usize,
) {
    if area.width < 4 || area.height < 1 || columns.is_empty() {
        return;
    }

    let header_style = Style::default()
        .fg(theme.fg_primary)
        .add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(theme.fg_secondary);

    // Row 0 — column names (bold, primary fg).
    let mut name_tokens: Vec<(Style, String)> = Vec::with_capacity(columns.len() * 2);
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            name_tokens.push((sep_style, COL_SEP.to_string()));
        }
        let w = col_widths.get(i).copied().unwrap_or(0);
        name_tokens.push((header_style, pad_to_width(&col.name, w)));
    }
    let name_spans = clip_spans(&name_tokens, h_scroll, area.width as usize);
    f.render_widget(
        Line::from(name_spans),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // Row 1 — affinity-colored type tags. Skipped when the panel is
    // only one row tall (rare, but degrade gracefully rather than
    // overflowing into a body row that doesn't exist).
    if area.height >= 2 {
        let mut type_tokens: Vec<(Style, String)> = Vec::with_capacity(columns.len() * 2);
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                type_tokens.push((sep_style, COL_SEP.to_string()));
            }
            let w = col_widths.get(i).copied().unwrap_or(0);
            let aff = affinity_of(&col.decl_type);
            let style = Style::default().fg(affinity_color(aff, theme));
            type_tokens.push((style, pad_to_width(affinity_short(aff), w)));
        }
        let type_spans = clip_spans(&type_tokens, h_scroll, area.width as usize);
        f.render_widget(
            Line::from(type_spans),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }

    // Data rows start under both header rows when the panel is tall
    // enough; on a 1-row-only panel the data simply doesn't render.
    let body_y = if area.height >= 2 {
        area.y + 2
    } else {
        area.y + 1
    };
    let body_end = area.y + area.height;
    let body_h = body_end.saturating_sub(body_y) as usize;

    if rows.is_empty() {
        // "(no rows)" centred in the body slot.
        if body_h >= 1 {
            let msg = t(Msg::DbNoRows);
            let w = UnicodeWidthStr::width(msg) as u16;
            let cy = body_y + (body_h as u16).saturating_sub(1) / 2;
            let cx = area.x + area.width.saturating_sub(w) / 2;
            f.render_widget(
                Line::from(Span::styled(msg, Style::default().fg(theme.fg_secondary))),
                Rect::new(cx, cy, area.width.saturating_sub(cx - area.x), 1),
            );
        }
        return;
    }

    for (i, row) in rows.iter().take(body_h).enumerate() {
        let mut tokens: Vec<(Style, String)> = Vec::with_capacity(row.len() * 2);
        for (c_idx, value) in row.iter().enumerate() {
            if c_idx > 0 {
                tokens.push((sep_style, COL_SEP.to_string()));
            }
            let w = col_widths.get(c_idx).copied().unwrap_or(0);
            tokens.push((
                cell_style(value, theme),
                pad_to_width(&value.to_string(), w),
            ));
        }
        let spans = clip_spans(&tokens, h_scroll, area.width as usize);
        f.render_widget(
            Line::from(spans),
            Rect::new(area.x, body_y + i as u16, area.width, 1),
        );
    }
}

/// Style for a `SqliteValue` — what color/weight to render with.
/// Paired with `value.to_string()` (the `Display` impl on
/// `SqliteValue`) at every render call site, keeping width
/// measurement and rendering on the same string.
fn cell_style(v: &SqliteValue, theme: &crate::ui::theme::Theme) -> Style {
    match v {
        SqliteValue::Null | SqliteValue::Blob { .. } => Style::default()
            .fg(theme.fg_secondary)
            .add_modifier(Modifier::ITALIC),
        SqliteValue::Integer(_) | SqliteValue::Real(_) | SqliteValue::Text { .. } => {
            Style::default().fg(theme.fg_primary)
        }
    }
}

/// Per-column display width based on header label, the type-tag
/// rendered in the second header row, and every cell in `rows` (the
/// full current page, not just the visible slice). Floored at 1 so
/// a column with an empty header + all-NULL never collapses to zero
/// width and hides its separator. No upper cap — long TEXT cells
/// make their column wide and the user h-scrolls, per the project's
/// "no per-cell `…` truncation" rule.
pub(crate) fn natural_column_widths(
    columns: &[ColumnInfo],
    rows: &[Vec<SqliteValue>],
) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .map(|(c_idx, col)| {
            let mut w = UnicodeWidthStr::width(col.name.as_str());
            // The type-tag row uses the affinity short label
            // ("INT", "TEXT", …). Tiny columns whose name is just
            // "id" or "n" widen to fit "INT" — accepted, the type
            // info is worth the extra few cells.
            let type_w = UnicodeWidthStr::width(affinity_short(affinity_of(&col.decl_type)));
            if type_w > w {
                w = type_w;
            }
            for row in rows {
                if let Some(cell) = row.get(c_idx) {
                    let cw = UnicodeWidthStr::width(cell.to_string().as_str());
                    if cw > w {
                        w = cw;
                    }
                }
            }
            w.max(1)
        })
        .collect()
}

/// SQLite type affinity buckets. The five-class system spec'd in
/// <https://sqlite.org/datatype3.html#determination_of_column_affinity>
/// plus a synthetic [`Affinity::None`] for columns that were declared
/// without any type at all — SQLite treats those as `BLOB`, but
/// labelling an undeclared column "BLOB" is misleading to the user
/// (they almost never meant to store binary blobs), so we render
/// those with a blank type tag instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Real,
    Blob,
    Numeric,
    None,
}

/// Map a declared SQL type to its SQLite affinity. The match order
/// follows the spec verbatim — `INT` is checked before `CHAR` so
/// `INTEGER` doesn't accidentally hit the TEXT branch via "INT" not
/// matching but later code mistaking it. The `to_ascii_uppercase`
/// upfront is the standard approach (declared types are
/// case-insensitive in SQLite).
fn affinity_of(decl_type: &str) -> Affinity {
    if decl_type.trim().is_empty() {
        return Affinity::None;
    }
    let u = decl_type.to_ascii_uppercase();
    if u.contains("INT") {
        return Affinity::Integer;
    }
    if u.contains("CHAR") || u.contains("CLOB") || u.contains("TEXT") {
        return Affinity::Text;
    }
    if u.contains("BLOB") {
        return Affinity::Blob;
    }
    if u.contains("REAL") || u.contains("FLOA") || u.contains("DOUB") {
        return Affinity::Real;
    }
    Affinity::Numeric
}

/// 3-to-4-char label rendered in the type header row. Short
/// abbreviations keep the column-width tax tolerable on narrow
/// columns; the affinity-color makes the bucket distinguishable
/// without spelling out "INTEGER" / "NUMERIC".
fn affinity_short(a: Affinity) -> &'static str {
    match a {
        Affinity::Integer => "INT",
        Affinity::Text => "TEXT",
        Affinity::Real => "REAL",
        Affinity::Blob => "BLOB",
        Affinity::Numeric => "NUM",
        Affinity::None => "",
    }
}

/// Color for a type tag, picked per affinity + theme polarity. Hand-
/// chosen RGB values rather than ratatui's named colors so the hue
/// stays consistent across terminals that remap palette entries
/// (Solarized, Nord, …). Five distinct hues: Integer/Numeric on the
/// blue/purple side (numeric feel), Text on green (string convention
/// in editors), Real on amber (decimal/float feel), Blob/None on
/// secondary gray (low information density).
fn affinity_color(a: Affinity, theme: &crate::ui::theme::Theme) -> ratatui::style::Color {
    use ratatui::style::Color;
    match (a, theme.is_dark) {
        (Affinity::Integer, true) => Color::Rgb(130, 170, 255),
        (Affinity::Integer, false) => Color::Rgb(0, 92, 197),
        (Affinity::Text, true) => Color::Rgb(140, 220, 130),
        (Affinity::Text, false) => Color::Rgb(34, 134, 58),
        (Affinity::Real, true) => Color::Rgb(230, 200, 130),
        (Affinity::Real, false) => Color::Rgb(155, 100, 20),
        (Affinity::Numeric, true) => Color::Rgb(180, 170, 230),
        (Affinity::Numeric, false) => Color::Rgb(110, 80, 180),
        (Affinity::Blob, _) => theme.fg_secondary,
        (Affinity::None, _) => theme.fg_secondary,
    }
}

/// Total display width of the data table — sum of column widths plus
/// `COL_SEP` between adjacent columns. Used to clamp `preview_h_scroll`
/// so the user can't scroll past the right edge.
pub(crate) fn total_table_width(col_widths: &[usize]) -> usize {
    if col_widths.is_empty() {
        return 0;
    }
    let sep_w = UnicodeWidthStr::width(COL_SEP);
    col_widths.iter().sum::<usize>() + sep_w * (col_widths.len() - 1)
}

/// Right-pad `s` with spaces until it occupies exactly `target_w`
/// display columns. If `s` is already wider it's returned as-is —
/// callers pass natural widths so this branch shouldn't fire.
fn pad_to_width(s: &str, target_w: usize) -> String {
    let cur = UnicodeWidthStr::width(s);
    if cur >= target_w {
        s.to_string()
    } else {
        let pad = target_w - cur;
        let mut out = String::with_capacity(s.len() + pad);
        out.push_str(s);
        for _ in 0..pad {
            out.push(' ');
        }
        out
    }
}

/// Trim or right-pad a string to exactly `width` display columns. The
/// truncated form ends with `…`. Width is unicode-display-width, not
/// char count, so CJK glyphs (2 cols each) align correctly.
fn clip_or_pad(s: &str, width: usize) -> String {
    let total = UnicodeWidthStr::width(s);
    if total <= width {
        let pad = width - total;
        let mut out = String::with_capacity(s.len() + pad);
        out.push_str(s);
        for _ in 0..pad {
            out.push(' ');
        }
        return out;
    }
    // Truncate, leaving room for the ellipsis.
    let mut out = String::new();
    let mut acc = 0usize;
    for ch in s.chars() {
        let chw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + chw + 1 > width {
            break;
        }
        out.push(ch);
        acc += chw;
    }
    out.push('…');
    acc += 1;
    while acc < width {
        out.push(' ');
        acc += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_integer_matches_int_substring() {
        assert_eq!(affinity_of("INTEGER"), Affinity::Integer);
        assert_eq!(affinity_of("int"), Affinity::Integer);
        assert_eq!(affinity_of("BIGINT"), Affinity::Integer);
        assert_eq!(affinity_of("TINYINT"), Affinity::Integer);
        // "INT" wins over later branches even when the string would
        // otherwise hit them — matches SQLite's left-to-right rule
        // ordering.
        assert_eq!(affinity_of("INT8"), Affinity::Integer);
    }

    #[test]
    fn affinity_text_matches_char_clob_text() {
        assert_eq!(affinity_of("TEXT"), Affinity::Text);
        assert_eq!(affinity_of("VARCHAR(255)"), Affinity::Text);
        assert_eq!(affinity_of("CHARACTER"), Affinity::Text);
        assert_eq!(affinity_of("CLOB"), Affinity::Text);
        assert_eq!(affinity_of("nvarchar"), Affinity::Text);
    }

    #[test]
    fn affinity_real_matches_real_floa_doub() {
        assert_eq!(affinity_of("REAL"), Affinity::Real);
        assert_eq!(affinity_of("FLOAT"), Affinity::Real);
        assert_eq!(affinity_of("DOUBLE"), Affinity::Real);
        assert_eq!(affinity_of("double precision"), Affinity::Real);
    }

    #[test]
    fn affinity_blob_explicit_only() {
        assert_eq!(affinity_of("BLOB"), Affinity::Blob);
        // Empty decl_type → None (we deviate from SQLite's "no
        // declared type → BLOB" rule for display friendliness).
        assert_eq!(affinity_of(""), Affinity::None);
        assert_eq!(affinity_of("   "), Affinity::None);
    }

    #[test]
    fn affinity_numeric_fallback() {
        // Anything that doesn't match the prior buckets falls into
        // NUMERIC — datetime, decimal, custom user types.
        assert_eq!(affinity_of("DATETIME"), Affinity::Numeric);
        assert_eq!(affinity_of("DECIMAL(10,2)"), Affinity::Numeric);
        assert_eq!(affinity_of("BOOLEAN"), Affinity::Numeric);
        assert_eq!(affinity_of("CUSTOM_THING"), Affinity::Numeric);
    }

    // ── build_page_chips ─────────────────────────────────────────────

    #[test]
    fn page_chips_single_page_only_disabled_arrows() {
        // 1 / 1 — both arrows disabled, single Active chip.
        let chips = build_page_chips(1, 1);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: false },
                PageChip::Active(1),
                PageChip::Next { enabled: false },
            ]
        );
    }

    #[test]
    fn page_chips_compact_below_threshold() {
        // total ≤ 7 → every page rendered as its own chip, no
        // ellipses. Active marks `current`.
        let chips = build_page_chips(3, 5);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: true },
                PageChip::Page(1),
                PageChip::Page(2),
                PageChip::Active(3),
                PageChip::Page(4),
                PageChip::Page(5),
                PageChip::Next { enabled: true },
            ]
        );
    }

    #[test]
    fn page_chips_threshold_boundary_exactly_seven() {
        // 7 pages → still compact (boundary check `total <= 7`).
        let chips = build_page_chips(4, 7);
        let want = vec![
            PageChip::Prev { enabled: true },
            PageChip::Page(1),
            PageChip::Page(2),
            PageChip::Page(3),
            PageChip::Active(4),
            PageChip::Page(5),
            PageChip::Page(6),
            PageChip::Page(7),
            PageChip::Next { enabled: true },
        ];
        assert_eq!(chips, want);
    }

    #[test]
    fn page_chips_window_in_middle() {
        // 50 pages, current = 25 → first/last anchors + 3-wide
        // window around current + ellipses on both sides.
        let chips = build_page_chips(25, 50);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: true },
                PageChip::Page(1),
                PageChip::Ellipsis,
                PageChip::Page(24),
                PageChip::Active(25),
                PageChip::Page(26),
                PageChip::Ellipsis,
                PageChip::Page(50),
                PageChip::Next { enabled: true },
            ]
        );
    }

    #[test]
    fn page_chips_at_first_page_no_left_ellipsis() {
        // current = 1 — no ellipsis between page 1 and the window
        // (window collapses to pages 2-2 so 1, 2, …, last is the
        // shape). Prev arrow disabled.
        let chips = build_page_chips(1, 50);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: false },
                PageChip::Active(1),
                PageChip::Page(2),
                PageChip::Ellipsis,
                PageChip::Page(50),
                PageChip::Next { enabled: true },
            ]
        );
    }

    #[test]
    fn page_chips_at_last_page_no_right_ellipsis() {
        // Mirror of the previous test for the right edge.
        let chips = build_page_chips(50, 50);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: true },
                PageChip::Page(1),
                PageChip::Ellipsis,
                PageChip::Page(49),
                PageChip::Active(50),
                PageChip::Next { enabled: false },
            ]
        );
    }

    #[test]
    fn page_chips_window_adjacent_to_anchors_collapses_ellipsis() {
        // current = 3 with total 50 → near_lo = 2, near_hi = 4. The
        // left ellipsis would be between page 1 and page 2 — there's
        // no gap, so no ellipsis. Right gap exists → right ellipsis.
        let chips = build_page_chips(3, 50);
        assert_eq!(
            chips,
            vec![
                PageChip::Prev { enabled: true },
                PageChip::Page(1),
                PageChip::Page(2),
                PageChip::Active(3),
                PageChip::Page(4),
                PageChip::Ellipsis,
                PageChip::Page(50),
                PageChip::Next { enabled: true },
            ]
        );
    }

    #[test]
    fn natural_widths_factor_in_type_tag() {
        // A column named "n" with INTEGER type would be 1 cell wide
        // by the old rules, but the type-tag row needs 3 cells for
        // "INT" — col_w must reach 3 to keep the header readable.
        let columns = vec![ColumnInfo {
            name: "n".to_string(),
            decl_type: "INTEGER".to_string(),
        }];
        let rows: Vec<Vec<SqliteValue>> = vec![vec![SqliteValue::Integer(1)]];
        let widths = natural_column_widths(&columns, &rows);
        assert_eq!(widths, vec![3]);
    }
}
