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
use reef_sqlite_preview::{ColumnInfo, DatabaseInfoV2, DbObject, DbObjectKind, SqliteValue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Minimum + maximum width clamp for the dynamically-sized sidebar.
/// Picked so a one-letter schema with `▾ a` fits, and a deeply-nested
/// `temp.complicated_long_table_name (10k)` doesn't eat the whole
/// panel. The renderer computes the natural width per frame from the
/// currently expanded objects and clamps into this range.
const SIDEBAR_MIN_WIDTH: u16 = 18;
const SIDEBAR_MAX_WIDTH: u16 = 32;
/// Below this panel width we drop the sidebar entirely and let the
/// data area consume the full width. The user can still cycle objects
/// via `[`/`]` — they just don't see the sidebar.
const MIN_TWO_PANE_WIDTH: u16 = 50;
/// Cell separator between data columns. Two spaces — the modern
/// "borderless table" look. Plenty of breathing room without a
/// vertical-rule glyph that visually approximates a frame line.
const COL_SEP: &str = "  ";
/// Width to pad the row-count column to in the sidebar so counts
/// across all object rows in a schema right-align. 5 covers `99999`
/// and the `1.2k` / `10.5k` shortened forms.
const SIDEBAR_COUNT_WIDTH: usize = 5;

/// SQLite preview body. Layout (panel width permitting):
///
/// ```text
/// fixture.db                                             ← card header
/// ─────────────────────────────────────────────────      ← separator
/// sqlite · 5 tables · 20.0 KB                            ← meta line
///                                                        ← spacer
/// ▾ main · fixture.db   id    email          name        ← sidebar + data header
///   Tables (2)          INT   TEXT           TEXT         ← (type chips)
/// ▎▸ users              1     alice@…        Alice        ← selected row (accent bar)
///  ▸ posts              2     bob@…          Bob
///   Views (1)           3     carol@…        Carol
///  ▸ active_users
///   Indexes (2)
///  ▸ users_email_idx
/// ▸ temp
/// page  ‹ [1] › · row 1-3 / 3 · users (1/2)              ← footer
/// ```
///
/// The data area replaces itself with a "detail card" when the
/// current selection is an Index or Trigger — see
/// [`render_detail_pane`] for that shape.
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
    info: &DatabaseInfoV2,
    focused: bool,
) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let mut y =
        crate::ui::file_preview_panel::render_card_header(f, area, path, &th, focused, None);

    let total_objects: usize = info.schemas.iter().map(|s| s.objects.len()).sum();
    let row_bearing_total: usize = info.iter_row_bearing().count();

    // Meta line — same shape as the binary card's `application/x · N B`
    // so the eye picks it up in the same place across formats.
    if y < max_y {
        let meta = format!(
            "sqlite · {} {} · {}",
            row_bearing_total,
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
    if total_objects == 0 {
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

    // Navigation state for this `.db`. Cloned up front so the borrow
    // releases before the pane renderers take `&mut app`. The clone is
    // now small (no schema graph copy — that lives in `info`); the only
    // heap fields left are the current page's rows + cached col_widths
    // + the `expanded` set, all <1 KB in practice.
    let state: Option<crate::app::DbPreviewState> = app
        .db_preview_state
        .as_ref()
        .filter(|s| s.path == path)
        .cloned();
    let state_ref = state.as_ref();

    // Locate the currently-selected object across every schema. Falls
    // back to the first row-bearing object in any schema when state
    // is missing or the selection is stale.
    let selected_object: Option<&DbObject> = state_ref
        .and_then(|s| info.lookup(&s.selection))
        .or_else(|| info.iter_row_bearing().next());
    let rows: &[Vec<SqliteValue>] = state_ref
        .map(|s| s.current_rows.as_slice())
        .unwrap_or(info.initial_page.rows.as_slice());
    let page_index: u64 = state_ref.map(|s| s.page).unwrap_or(0);
    let rows_per_page: u64 = state_ref
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

    let two_pane = area.width >= MIN_TWO_PANE_WIDTH;
    let sidebar_rows = if two_pane {
        build_sidebar_rows(info, state_ref)
    } else {
        Vec::new()
    };
    let sidebar_w = if two_pane {
        sidebar_width_for_rows(&sidebar_rows)
    } else {
        0
    };
    let sidebar_x = area.x;
    let data_x = area.x + sidebar_w + if two_pane { 1 } else { 0 };
    let data_w = area
        .width
        .saturating_sub(sidebar_w + if two_pane { 1 } else { 0 });

    if two_pane {
        render_objects_pane(
            f,
            app,
            &th,
            Rect::new(sidebar_x, y, sidebar_w, body_max_y - y),
            &sidebar_rows,
            state_ref,
        );
    }

    let data_rect = Rect::new(data_x, y, data_w, body_max_y - y);

    match selected_object {
        Some(o) if o.kind.has_rows() => {
            // Column widths come from the cache on `db_preview_state` —
            // recomputed only when current_rows / selected change, not
            // on every render. This makes h-scroll cheap: a single
            // keypress is O(1) work in the render path (clip_spans on
            // visible rows only), no O(rows × cols × avg_str_len)
            // re-walk per frame. When state is missing (the rare race
            // window between preview-land and the apply_worker_result
            // hook) we fall back to computing on the fly so the table
            // still draws.
            let owned_widths: Vec<usize>;
            let owned_total_w: usize;
            let (col_widths, total_table_w) = match state_ref {
                Some(s) if !s.col_widths.is_empty() => (s.col_widths.as_slice(), s.total_table_w),
                _ => {
                    owned_widths = natural_column_widths(&o.columns, rows);
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
                data_rect,
                &o.columns,
                col_widths,
                &rows[row_offset.min(rows.len())..],
                h_scroll,
            );

            // Pagination footer.
            if body_max_y < max_y {
                if let Some(buf) = app.db_goto_input.as_ref() {
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
                    let footer_idx = position_in_row_bearing(info, o).unwrap_or(0);
                    render_pagination_footer(
                        f,
                        app,
                        Rect::new(area.x, body_max_y, area.width, 1),
                        &th,
                        FooterContext {
                            object: o,
                            selected_idx: footer_idx,
                            table_count: row_bearing_total,
                            page_index,
                            rows_per_page,
                        },
                    );
                }
            }
        }
        Some(o) => {
            // Non-row object (Index / Trigger): render structural
            // detail in the data area; footer just shows the object's
            // identity since there's nothing to paginate.
            let detail = state_ref.and_then(|s| s.detail.as_ref());
            render_detail_pane(f, &th, data_rect, o, detail);
            if body_max_y < max_y {
                let label = format!("{} · {}.{}", o.kind.as_master_type(), o.schema, o.name);
                f.render_widget(
                    Line::from(Span::styled(label, Style::default().fg(th.fg_secondary))),
                    Rect::new(area.x, body_max_y, area.width, 1),
                );
            }
        }
        None => {
            // No row-bearing object anywhere — render an explanatory
            // line in the data area. The sidebar still lets the user
            // pick an Index / Trigger to see its detail.
            let msg = t(Msg::DbNoRows);
            if data_rect.height >= 1 {
                let w = UnicodeWidthStr::width(msg) as u16;
                let cy = data_rect.y + data_rect.height / 2;
                let cx = data_rect.x + data_rect.width.saturating_sub(w) / 2;
                f.render_widget(
                    Line::from(Span::styled(msg, Style::default().fg(th.fg_secondary))),
                    Rect::new(cx, cy, data_rect.width.saturating_sub(cx - data_rect.x), 1),
                );
            }
        }
    }
}

/// Index of `target` within the flat list of row-bearing objects
/// (used by the pagination footer to render `(i/N)`).
fn position_in_row_bearing(
    info: &reef_sqlite_preview::DatabaseInfoV2,
    target: &DbObject,
) -> Option<usize> {
    info.iter_row_bearing()
        .position(|o| o.schema == target.schema && o.name == target.name && o.kind == target.kind)
}

/// Render the detail pane for an Index or Trigger (and, for symmetry,
/// Table / View — though the latter two normally show their data grid
/// instead). Falls back to a "Loading…" placeholder when
/// `state.detail` hasn't been populated yet.
fn render_detail_pane(
    f: &mut Frame,
    theme: &crate::ui::theme::Theme,
    area: Rect,
    object: &DbObject,
    detail: Option<&reef_sqlite_preview::DbObjectDetail>,
) {
    if area.height < 1 || area.width < 4 {
        return;
    }
    let mut y = area.y;
    let max_y = area.y + area.height;
    let bold = Style::default()
        .fg(theme.fg_primary)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.fg_secondary);

    // Title row — object name with its kind as a tail tag.
    if y < max_y {
        let title = format!("{}  ", object.name);
        let kind_tag = format!("[{}]", object.kind.as_master_type());
        f.render_widget(
            Line::from(vec![
                Span::styled(title, bold),
                Span::styled(
                    kind_tag,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    if y < max_y {
        // A single underline for visual closure of the title row.
        let dash = "─".repeat(area.width as usize);
        f.render_widget(
            Line::from(Span::styled(dash, dim)),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }

    // Loading placeholder when the detail RPC hasn't landed yet.
    let Some(detail) = detail else {
        if y < max_y {
            f.render_widget(
                Line::from(Span::styled(
                    "Loading…",
                    Style::default()
                        .fg(theme.fg_secondary)
                        .add_modifier(Modifier::ITALIC),
                )),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        return;
    };

    use reef_sqlite_preview::DbObjectDetail as D;
    match detail {
        D::Index {
            unique,
            columns,
            partial_where,
            tbl_name,
            ..
        } => {
            // Chip row: UNIQUE / PARTIAL when applicable.
            if y < max_y {
                let mut chips: Vec<Span> = Vec::new();
                if *unique {
                    chips.push(Span::styled(
                        " UNIQUE ",
                        Style::default()
                            .fg(theme.badge_fg)
                            .bg(theme.badge_bg)
                            .add_modifier(Modifier::BOLD),
                    ));
                    chips.push(Span::raw("  "));
                }
                if partial_where.is_some() {
                    chips.push(Span::styled(
                        " PARTIAL ",
                        Style::default()
                            .fg(theme.badge_fg)
                            .bg(theme.warn_bg)
                            .add_modifier(Modifier::BOLD),
                    ));
                    chips.push(Span::raw("  "));
                }
                if !chips.is_empty() {
                    f.render_widget(Line::from(chips), Rect::new(area.x, y, area.width, 1));
                    y += 1;
                }
            }
            if y < max_y {
                f.render_widget(
                    Line::from(vec![
                        Span::styled("Table: ", dim),
                        Span::styled(tbl_name.clone(), bold),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            if y < max_y && !columns.is_empty() {
                f.render_widget(
                    Line::from(vec![
                        Span::styled("Columns: ", dim),
                        Span::styled(columns.join(", "), bold),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            if let Some(w) = partial_where
                && y < max_y
            {
                f.render_widget(
                    Line::from(vec![
                        Span::styled("WHERE: ", dim),
                        Span::styled(w.clone(), Style::default().fg(theme.fg_primary)),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
        }
        D::Trigger {
            timing,
            event,
            tbl_name,
            sql,
        } => {
            // Header: AFTER INSERT ON users
            if y < max_y {
                let header = format!(
                    "{} {} ON {}",
                    trigger_timing_label(*timing),
                    trigger_event_label(*event),
                    tbl_name,
                );
                f.render_widget(
                    Line::from(Span::styled(header, bold)),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            if y < max_y {
                y += 1; // spacer before body
            }
            // SQL body, capped at 20 lines.
            const MAX_BODY_LINES: usize = 20;
            let total: Vec<&str> = sql.lines().collect();
            let to_render = total.iter().take(MAX_BODY_LINES);
            for line in to_render {
                if y >= max_y {
                    break;
                }
                let body_str = clip_or_pad(line, area.width as usize);
                f.render_widget(
                    Line::from(Span::styled(
                        body_str,
                        Style::default().fg(theme.fg_primary),
                    )),
                    Rect::new(area.x, y, area.width, 1),
                );
                y += 1;
            }
            if total.len() > MAX_BODY_LINES && y < max_y {
                let more = format!("… +{} lines", total.len() - MAX_BODY_LINES);
                f.render_widget(
                    Line::from(Span::styled(more, dim)),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
        }
        D::Table { create_sql } | D::View { create_sql } => {
            if let Some(sql) = create_sql {
                let total: Vec<&str> = sql.lines().collect();
                for line in total.iter().take(20) {
                    if y >= max_y {
                        break;
                    }
                    let body_str = clip_or_pad(line, area.width as usize);
                    f.render_widget(
                        Line::from(Span::styled(
                            body_str,
                            Style::default().fg(theme.fg_primary),
                        )),
                        Rect::new(area.x, y, area.width, 1),
                    );
                    y += 1;
                }
            } else if y < max_y {
                f.render_widget(
                    Line::from(Span::styled("No CREATE SQL", dim)),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
        }
    }
}

fn trigger_timing_label(t: reef_sqlite_preview::TriggerTiming) -> &'static str {
    match t {
        reef_sqlite_preview::TriggerTiming::Before => "BEFORE",
        reef_sqlite_preview::TriggerTiming::After => "AFTER",
        reef_sqlite_preview::TriggerTiming::InsteadOf => "INSTEAD OF",
        reef_sqlite_preview::TriggerTiming::Unknown => "?",
    }
}

fn trigger_event_label(e: reef_sqlite_preview::TriggerEvent) -> &'static str {
    match e {
        reef_sqlite_preview::TriggerEvent::Insert => "INSERT",
        reef_sqlite_preview::TriggerEvent::Update => "UPDATE",
        reef_sqlite_preview::TriggerEvent::Delete => "DELETE",
        reef_sqlite_preview::TriggerEvent::Unknown => "?",
    }
}

/// Bundle of SQLite-state-derived params for the pagination footer.
/// Five fields that all flow from `(info, state)` at the call site —
/// grouped into one borrow rather than spread across positional args
/// so the rendering signature stays comfortably under clippy's
/// `too_many_arguments` threshold and the call site reads as a
/// labeled struct literal rather than a positional argument list.
struct FooterContext<'a> {
    object: &'a DbObject,
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
    // Row count missing (views, indexes, triggers) → degrade footer to
    // "page 1 / 1" with no row range, since we can't paginate something
    // we haven't counted.
    let row_count = ctx.object.row_count.unwrap_or(0);
    let total_pages = if ctx.rows_per_page == 0 {
        1
    } else {
        row_count.div_ceil(ctx.rows_per_page).max(1)
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
    let row_start = if row_count > 0 {
        ctx.page_index * ctx.rows_per_page + 1
    } else {
        0
    };
    let row_end = (ctx.page_index * ctx.rows_per_page + ctx.rows_per_page).min(row_count);
    let suffix = format!(
        " ·  {} {}-{} / {}  ·  {} ({}/{})",
        t(Msg::DbRowsLabel),
        row_start,
        row_end,
        row_count,
        ctx.object.name,
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

/// One row in the objects sidebar's flat-list-with-scroll model.
/// Grouped sidebars are easier to render as a single flat list (so
/// scroll math stays trivial) than to keep section nesting at runtime.
#[derive(Debug, Clone, Copy)]
enum SidebarRow<'a> {
    /// `▾ main` / `▸ temp` / `▸ aux  · file.db` header. Clickable —
    /// toggles the schema's expanded/collapsed state.
    SchemaHeader {
        name: &'a str,
        expanded: bool,
        file: Option<&'a str>,
    },
    /// `Tables (12)` / `Indexes (3)` etc. subsection label. Not
    /// clickable; purely a visual divider inside an expanded schema.
    Subsection { label: &'static str, count: usize },
    /// One object row. Clickable — switches the data grid / detail
    /// pane to this object.
    Object(&'a DbObject),
}

/// Build the flat row list for the objects sidebar. Iteration order
/// is the same as `info.schemas` (which mirrors `PRAGMA database_list`
/// — main first, then temp, then attached). Inside an expanded schema
/// we group by kind in the canonical order; empty subsections are
/// elided.
fn build_sidebar_rows<'a>(
    info: &'a reef_sqlite_preview::DatabaseInfoV2,
    state: Option<&'a crate::app::DbPreviewState>,
) -> Vec<SidebarRow<'a>> {
    let mut rows = Vec::new();
    for schema in &info.schemas {
        let expanded = state
            .map(|s| s.expanded.contains(&schema.name))
            .unwrap_or(schema.kind == reef_sqlite_preview::SchemaKind::Main);
        rows.push(SidebarRow::SchemaHeader {
            name: &schema.name,
            expanded,
            file: schema.file.as_deref(),
        });
        if !expanded {
            continue;
        }
        for kind in [
            DbObjectKind::Table,
            DbObjectKind::View,
            DbObjectKind::Index,
            DbObjectKind::Trigger,
        ] {
            let objs: Vec<&DbObject> = schema.objects.iter().filter(|o| o.kind == kind).collect();
            if objs.is_empty() {
                continue;
            }
            rows.push(SidebarRow::Subsection {
                label: kind.section_label(),
                count: objs.len(),
            });
            for o in objs {
                rows.push(SidebarRow::Object(o));
            }
        }
    }
    rows
}

/// Natural sidebar width from a pre-built row list. Clamped into
/// `[SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH]` so the min keeps `▸ main`
/// readable and the max stops a long attached-db name from eating the
/// data pane.
fn sidebar_width_for_rows(rows: &[SidebarRow<'_>]) -> u16 {
    let mut max_w: usize = SIDEBAR_MIN_WIDTH as usize;
    for row in rows {
        let w = match row {
            SidebarRow::SchemaHeader { name, file, .. } => {
                let mut w = 2 + UnicodeWidthStr::width(*name);
                if let Some(f) = file
                    && !f.is_empty()
                {
                    let file_short = short_file_label(f);
                    w += 3 + UnicodeWidthStr::width(file_short.as_str());
                }
                w
            }
            SidebarRow::Subsection { label, count } => {
                2 + UnicodeWidthStr::width(*label) + 1 + 2 + count.to_string().len()
            }
            SidebarRow::Object(o) => {
                let name_w = UnicodeWidthStr::width(o.name.as_str());
                let virt_w = if o.is_virtual { 2 } else { 0 };
                4 + name_w + virt_w + 1 + SIDEBAR_COUNT_WIDTH
            }
        };
        if w > max_w {
            max_w = w;
        }
    }
    max_w.min(SIDEBAR_MAX_WIDTH as usize) as u16
}

/// Shorten a `PRAGMA database_list.file` path to the bare filename,
/// because the full absolute path almost never fits in a sidebar
/// column. Falls back to the trailing path component.
fn short_file_label(file: &str) -> String {
    std::path::Path::new(file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string())
}

/// Format a row count for sidebar display, trimming to 4 chars max.
/// 1234 → "1234", 12345 → "12.3k", 123456 → " 123k". `None` → "—".
fn format_row_count(count: Option<u64>) -> String {
    match count {
        None => "—".to_string(),
        Some(n) if n < 10_000 => n.to_string(),
        Some(n) if n < 1_000_000 => format!("{:.1}k", n as f64 / 1000.0),
        Some(n) => format!("{}k", n / 1000),
    }
}

/// Render the grouped objects sidebar — schemas + their tables /
/// views / indexes / triggers. Registers `DbToggleSchema` on schema
/// headers and `DbSelectObject` on object rows; subsection labels
/// are inert.
fn render_objects_pane(
    f: &mut Frame,
    app: &mut App,
    theme: &crate::ui::theme::Theme,
    area: Rect,
    rows: &[SidebarRow<'_>],
    state: Option<&crate::app::DbPreviewState>,
) {
    if area.width < 4 || area.height < 1 || rows.is_empty() {
        return;
    }

    // Anchor scroll on the row that contains the current selection so
    // an off-screen object scrolls into view rather than getting lost.
    let selected_row_idx = state
        .and_then(|s| {
            rows.iter().position(|r| match r {
                SidebarRow::Object(o) => o.name == s.selection.name && o.kind == s.selection.kind,
                _ => false,
            })
        })
        .unwrap_or(0);
    let body_h = area.height as usize;
    let scroll = scroll_offset_for(selected_row_idx, rows.len(), body_h);

    for (i, row) in rows.iter().skip(scroll).take(body_h).enumerate() {
        let y = area.y + i as u16;
        match row {
            SidebarRow::SchemaHeader {
                name,
                expanded,
                file,
            } => {
                let icon = if *expanded { "▾ " } else { "▸ " };
                let mut text = format!("{icon}{name}");
                if let Some(f) = file
                    && !f.is_empty()
                {
                    let short = short_file_label(f);
                    text.push_str(" · ");
                    text.push_str(&short);
                }
                let body = clip_or_pad(&text, area.width as usize);
                let style = Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD);
                f.render_widget(
                    Line::from(Span::styled(body, style)),
                    Rect::new(area.x, y, area.width, 1),
                );
                app.hit_registry.register_row(
                    area.x,
                    y,
                    area.width,
                    crate::ui::mouse::ClickAction::DbToggleSchema((*name).to_string()),
                );
            }
            SidebarRow::Subsection { label, count } => {
                // Subsection labels get a subtle accent so the eye can
                // chunk the sidebar at a glance — the label is the
                // section marker (Tables / Views / Indexes / Triggers).
                // Bold + accent fg with the count in dimmer secondary
                // fg so the magnitude information stays visible without
                // competing for attention.
                let indent = "  ";
                let count_str = format!(" ({count})");
                let max = area.width as usize;
                let count_w = UnicodeWidthStr::width(count_str.as_str());
                let label_avail = max.saturating_sub(indent.len() + count_w);
                let label_w = UnicodeWidthStr::width(*label).min(label_avail);
                // Clip (but don't pad) the label so the count stays
                // adjacent.
                let label_render: String = if UnicodeWidthStr::width(*label) <= label_avail {
                    (*label).to_string()
                } else {
                    clip_or_pad(label, label_avail).trim_end().to_string()
                };
                let trailing = max.saturating_sub(indent.len() + label_w + count_w);
                f.render_widget(
                    Line::from(vec![
                        Span::raw(indent.to_string()),
                        Span::styled(
                            label_render,
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(count_str, Style::default().fg(theme.fg_secondary)),
                        Span::raw(" ".repeat(trailing)),
                    ]),
                    Rect::new(area.x, y, area.width, 1),
                );
            }
            SidebarRow::Object(o) => {
                let is_selected =
                    state.is_some_and(|s| o.name == s.selection.name && o.kind == s.selection.kind);
                render_object_row(f, app, theme, area, y, o, is_selected);
            }
        }
    }
}

/// Render one object row in the sidebar. Layout:
///
/// ```text
///   ▸ users           1.2k      ← unselected
/// ▎ ▸ active_users    —         ← selected (accent bar on the left)
///   ▸ notes ⓥ          0
/// ```
fn render_object_row(
    f: &mut Frame,
    app: &mut App,
    theme: &crate::ui::theme::Theme,
    area: Rect,
    y: u16,
    object: &DbObject,
    is_selected: bool,
) {
    if area.width == 0 {
        return;
    }
    let row_style = if is_selected {
        Style::default()
            .fg(theme.fg_primary)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg_secondary)
    };
    // Accent column only gets a non-default background on the selected
    // row — otherwise we leave it transparent so it inherits the panel
    // bg instead of painting a dark gutter against the main area
    // (chrome_bg was visibly darker than the panel and showed as a
    // black strip on every row).
    let accent_style = if is_selected {
        Style::default().fg(theme.accent).bg(theme.selection_bg)
    } else {
        Style::default()
    };

    // Object rows don't get an expand caret — they're terminal nodes,
    // and the `▸ → ▾` toggle pattern is reserved for schema headers
    // where it actually does something.
    let accent_glyph = if is_selected { "▎" } else { " " };
    let virt_suffix = if object.is_virtual { " ⓥ" } else { "" };
    let name_with_virt = format!("{}{}", object.name, virt_suffix);
    let count_str = format_row_count(object.row_count);
    let count_w = SIDEBAR_COUNT_WIDTH;
    let avail_for_name =
        (area.width as usize).saturating_sub(1 /*accent*/ + 3 /*indent*/ + 1 /*gap*/ + count_w);
    let name_disp = clip_or_pad(&name_with_virt, avail_for_name);
    let count_disp = format!("{count_str:>count_w$}");

    let spans = vec![
        Span::styled(accent_glyph.to_string(), accent_style),
        Span::styled("   ".to_string(), row_style),
        Span::styled(name_disp, row_style),
        Span::styled(" ".to_string(), row_style),
        Span::styled(count_disp, row_style),
    ];
    f.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));
    app.hit_registry.register_row(
        area.x,
        y,
        area.width,
        crate::ui::mouse::ClickAction::DbSelectObject {
            schema: object.schema.clone(),
            name: object.name.clone(),
            kind: object.kind,
        },
    );
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

/// Render the right-side data area in the modern "borderless table"
/// style: a two-row header (column names + affinity-colored type
/// chips) followed by `rows` of cells with zebra striping. Columns
/// are pre-sized in `col_widths` and joined with two spaces — no
/// vertical-rule glyphs anywhere, so the table relies on alignment
/// and color rather than a visible frame.
///
/// - Headers: bold + bright; type row tinted by affinity (INT blue,
///   TEXT green, REAL amber, etc.).
/// - Data rows: alternating `theme.hover_bg` on odd rows for a subtle
///   zebra stripe.
/// - Integers / Reals: right-aligned within their column so digits
///   stack cleanly.
/// - NULL: italic dim, left-aligned. BLOB: italic dim `<blob N B>`.
/// - Reader-truncated TEXT preserves its trailing `…` from the wire
///   payload.
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

    let sep_bg_default = Style::default(); // separator inherits row bg below

    // Row 0 — column names. Tinted by affinity so each name pairs
    // visually with the type chip directly below it (green for TEXT,
    // blue for INT, etc.). Bold to keep the name as the primary
    // emphasis; the chip below uses the same hue but unbolded so the
    // hierarchy reads as "name (bold) / type (lighter)".
    let mut name_tokens: Vec<(Style, String)> = Vec::with_capacity(columns.len() * 2);
    for (i, col) in columns.iter().enumerate() {
        if i > 0 {
            name_tokens.push((sep_bg_default, COL_SEP.to_string()));
        }
        let w = col_widths.get(i).copied().unwrap_or(0);
        let aff = affinity_of(&col.decl_type);
        let style = Style::default()
            .fg(affinity_header_color(aff, theme))
            .add_modifier(Modifier::BOLD);
        name_tokens.push((style, pad_to_width(&col.name, w)));
    }
    let name_spans = clip_spans(&name_tokens, h_scroll, area.width as usize);
    f.render_widget(
        Line::from(name_spans),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // Row 1 — type label, same hue as the name above, no bold so the
    // name carries the visual weight and the type sits as a quieter
    // annotation. Empty for Affinity::None columns (rendered as
    // spaces) since there's no useful tag to show.
    if area.height >= 2 {
        let mut type_tokens: Vec<(Style, String)> = Vec::with_capacity(columns.len() * 2);
        for (i, col) in columns.iter().enumerate() {
            if i > 0 {
                type_tokens.push((sep_bg_default, COL_SEP.to_string()));
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

    // Data rows start under both header rows.
    let body_y = if area.height >= 2 {
        area.y + 2
    } else {
        area.y + 1
    };
    let body_end = area.y + area.height;
    let body_h = body_end.saturating_sub(body_y) as usize;

    if rows.is_empty() {
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
        // Zebra stripe: odd rows get `theme.hover_bg`. Renders the
        // whole row's worth of cells (including padding / separators)
        // with that background so the stripe is unbroken across the
        // grid.
        let row_bg = if i % 2 == 1 {
            Some(theme.hover_bg)
        } else {
            None
        };
        let bg_style = if let Some(bg) = row_bg {
            Style::default().bg(bg)
        } else {
            Style::default()
        };
        // Fill the row's background first by rendering a span of
        // spaces — clip_spans on the cell tokens below paints over
        // it, but any gaps (e.g. when h_scroll positions us past the
        // last column) keep the stripe visible.
        if row_bg.is_some() {
            f.render_widget(
                Line::from(Span::styled(" ".repeat(area.width as usize), bg_style)),
                Rect::new(area.x, body_y + i as u16, area.width, 1),
            );
        }

        let mut tokens: Vec<(Style, String)> = Vec::with_capacity(row.len() * 2);
        for (c_idx, value) in row.iter().enumerate() {
            if c_idx > 0 {
                tokens.push((bg_style, COL_SEP.to_string()));
            }
            let w = col_widths.get(c_idx).copied().unwrap_or(0);
            let mut cell_style_v = cell_style(value, theme);
            if let Some(bg) = row_bg {
                cell_style_v = cell_style_v.bg(bg);
            }
            // Right-align numerics so digits stack cleanly down the
            // column; left-align text / null / blob.
            let padded = if value_is_numeric(value) {
                pad_left_to_width(&value.to_string(), w)
            } else {
                pad_to_width(&value.to_string(), w)
            };
            tokens.push((cell_style_v, padded));
        }
        let spans = clip_spans(&tokens, h_scroll, area.width as usize);
        f.render_widget(
            Line::from(spans),
            Rect::new(area.x, body_y + i as u16, area.width, 1),
        );
    }
}

/// `true` for Integer / Real — the right-align targets in the data
/// grid. Text / Null / Blob stay left-aligned.
fn value_is_numeric(v: &SqliteValue) -> bool {
    matches!(v, SqliteValue::Integer(_) | SqliteValue::Real(_))
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

/// Same as [`affinity_color`] but maps Blob / None to `fg_primary`
/// instead of the secondary gray — used for the bold column-name
/// row, where an untyped column shouldn't read as washed-out.
fn affinity_header_color(a: Affinity, theme: &crate::ui::theme::Theme) -> ratatui::style::Color {
    match a {
        Affinity::Blob | Affinity::None => theme.fg_primary,
        other => affinity_color(other, theme),
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

/// Pad `s` with spaces to occupy exactly `target_w` display columns.
/// If `s` is already wider it's returned as-is — callers pass natural
/// widths so the wider branch shouldn't fire. Right-aligned variant
/// (left-padding) used for numeric cells in the data grid.
fn pad_to_width(s: &str, target_w: usize) -> String {
    pad_aligned(s, target_w, Align::Left)
}

fn pad_left_to_width(s: &str, target_w: usize) -> String {
    pad_aligned(s, target_w, Align::Right)
}

#[derive(Copy, Clone)]
enum Align {
    Left,
    Right,
}

fn pad_aligned(s: &str, target_w: usize, align: Align) -> String {
    let cur = UnicodeWidthStr::width(s);
    if cur >= target_w {
        return s.to_string();
    }
    let pad = target_w - cur;
    let mut out = String::with_capacity(s.len() + pad);
    match align {
        Align::Left => {
            out.push_str(s);
            out.extend(std::iter::repeat_n(' ', pad));
        }
        Align::Right => {
            out.extend(std::iter::repeat_n(' ', pad));
            out.push_str(s);
        }
    }
    out
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
            notnull: false,
            pk: false,
        }];
        let rows: Vec<Vec<SqliteValue>> = vec![vec![SqliteValue::Integer(1)]];
        let widths = natural_column_widths(&columns, &rows);
        assert_eq!(widths, vec![3]);
    }
}
