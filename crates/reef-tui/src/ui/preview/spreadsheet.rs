use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use reef_app::{AppCommand, AppPanel as Panel, AppTab as Tab};
use reef_core::preview::{PreviewBody, PreviewDocument, spreadsheet::WorkbookPreview};

use crate::{
    TuiApp,
    keymap::{Command, InputScope, Keymap},
};

const CELL_WIDTH: u16 = 18;
const GUTTER: u16 = 7;

/// Terminal selection and viewport geometry, reset on accepted source/sheet identity.
#[derive(Default)]
pub(crate) struct SpreadsheetViewState {
    identity: Option<(String, u64, usize)>,
    row: usize,
    column: usize,
    visible_columns: usize,
}

fn sync_identity(app: &mut TuiApp, path: &str) {
    let identity = (
        path.to_owned(),
        app.engine.preview_source_revision(),
        app.engine.spreadsheet_sheet(),
    );
    if app.spreadsheet_view.identity.as_ref() != Some(&identity) {
        app.spreadsheet_view = SpreadsheetViewState {
            identity: Some(identity),
            ..Default::default()
        };
    }
}

pub(crate) fn select_cell(app: &mut TuiApp, row: usize, column: usize) {
    let Some(preview) = app.engine.preview_content() else {
        return;
    };
    let PreviewBody::Spreadsheet(workbook) = &preview.body else {
        return;
    };
    let Some(sheet) = workbook.sheets.get(app.engine.spreadsheet_sheet()) else {
        return;
    };
    if sheet
        .rows
        .get(row)
        .and_then(|cells| cells.get(column))
        .is_none()
    {
        return;
    }
    sync_identity(app, &preview.path);
    app.spreadsheet_view.row = row;
    app.spreadsheet_view.column = column;
}

pub(crate) fn handle_key(key: KeyEvent, app: &mut TuiApp) -> bool {
    if !matches!(app.engine.active_tab(), Tab::Files | Tab::Search)
        || app.engine.active_panel() != Panel::Diff
    {
        return false;
    }
    let Some(preview) = app.engine.preview_content() else {
        return false;
    };
    let PreviewBody::Spreadsheet(workbook) = &preview.body else {
        return false;
    };
    let Some(command) = Keymap::resolve(InputScope::SpreadsheetPreview, &key) else {
        return false;
    };
    sync_identity(app, &preview.path);
    let index = app.engine.spreadsheet_sheet();
    if let Command::SpreadsheetSheet(delta) = command {
        let next = index
            .saturating_add_signed(delta as isize)
            .min(workbook.sheets.len().saturating_sub(1));
        app.engine
            .dispatch(AppCommand::SelectSpreadsheetSheet(next));
        return true;
    }
    let Some(sheet) = workbook.sheets.get(index) else {
        return true;
    };
    let height = usize::from(app.layout.last_preview_view_h).max(1);
    let state = &mut app.spreadsheet_view;
    match command {
        Command::SpreadsheetMove(row, column) => {
            state.row = state.row.saturating_add_signed(row as isize);
            state.column = state.column.saturating_add_signed(column as isize);
        }
        Command::PageUp => state.row = state.row.saturating_sub(height),
        Command::PageDown => state.row = state.row.saturating_add(height),
        Command::ScrollTop => state.row = 0,
        Command::ScrollBottom => state.row = sheet.rows.len().saturating_sub(1),
        Command::SpreadsheetCopy => {
            if let Some(value) = sheet
                .rows
                .get(state.row)
                .and_then(|row| row.get(state.column))
            {
                app.copy_text_to_clipboard(value.clone());
            }
            return true;
        }
        _ => return false,
    }
    state.row = state.row.min(sheet.rows.len().saturating_sub(1));
    state.column = state.column.min(sheet.columns.len().saturating_sub(1));
    let first_column = app.engine.preview_h_scroll() / usize::from(CELL_WIDTH);
    let first_column = if state.column < first_column {
        state.column
    } else if state.column >= first_column + state.visible_columns.max(1) {
        state.column + 1 - state.visible_columns.max(1)
    } else {
        first_column
    };
    app.engine.dispatch(AppCommand::SetPreviewHorizontalScroll(
        first_column * usize::from(CELL_WIDTH),
    ));
    let scroll = app.engine.preview_scroll();
    let target = if state.row < scroll {
        state.row
    } else if state.row >= scroll + height {
        state.row + 1 - height
    } else {
        scroll
    };
    app.engine
        .dispatch(AppCommand::SetPreviewVerticalScroll(target));
    true
}

pub(in crate::ui) fn render(
    frame: &mut Frame,
    app: &mut TuiApp,
    area: Rect,
    preview: &PreviewDocument,
    workbook: &WorkbookPreview,
    focused: bool,
) {
    sync_identity(app, &preview.path);
    app.last_preview_content_origin = None;
    let top =
        super::chrome::render_card_header(frame, area, &preview.path, &app.theme, focused, None);
    let Some(sheet) = workbook.sheets.get(app.engine.spreadsheet_sheet()) else {
        frame.render_widget(
            Paragraph::new("No worksheets"),
            Rect::new(area.x, top, area.width, area.bottom().saturating_sub(top)),
        );
        return;
    };
    if area.bottom().saturating_sub(top) < 4 {
        return;
    }
    let subtitle = format!(
        "{} ({}/{}) · {} × {}{}  [ ] sheets · y copy",
        sheet.name,
        app.engine.spreadsheet_sheet() + 1,
        workbook.sheets.len(),
        sheet.total_rows,
        sheet.total_columns,
        if sheet.truncated {
            " · truncated preview"
        } else {
            ""
        }
    );
    frame.render_widget(
        Line::from(subtitle).style(Style::default().fg(app.theme.fg_secondary)),
        Rect::new(area.x, top, area.width, 1),
    );
    let body_top = top + 2;
    let height = area.bottom().saturating_sub(body_top + 3);
    app.layout.last_preview_view_h = height;
    app.engine.dispatch(AppCommand::ClampPreviewVerticalScroll(
        sheet.rows.len().saturating_sub(height as usize),
    ));
    let state = &mut app.spreadsheet_view;
    state.visible_columns = usize::from(area.width.saturating_sub(GUTTER) / CELL_WIDTH).max(1);
    app.engine
        .dispatch(AppCommand::ClampPreviewHorizontalScroll(
            sheet.columns.len().saturating_sub(state.visible_columns) * usize::from(CELL_WIDTH),
        ));
    let first_column = app.engine.preview_h_scroll() / usize::from(CELL_WIDTH);
    let normal = Style::default().fg(app.theme.fg_primary);
    let secondary = Style::default().fg(app.theme.fg_secondary);
    for (offset, label) in sheet
        .columns
        .iter()
        .skip(first_column)
        .take(state.visible_columns)
        .enumerate()
    {
        let x = area.x + GUTTER + offset as u16 * CELL_WIDTH;
        if x >= area.right() {
            break;
        }
        let width = CELL_WIDTH.min(area.right() - x);
        frame.render_widget(
            Line::from(label.as_str()).style(secondary),
            Rect::new(x, top + 1, width, 1),
        );
        for (line, row) in sheet
            .rows
            .iter()
            .enumerate()
            .skip(app.engine.preview_scroll())
            .take(height as usize)
        {
            let y = body_top + (line - app.engine.preview_scroll()) as u16;
            let column = first_column + offset;
            app.hit_registry.register(
                Rect::new(x, y, width.saturating_sub(1), 1),
                crate::ui::mouse::ClickAction::SpreadsheetSelectCell { row: line, column },
            );
            let value = row[column].replace(['\n', '\r', '\t'], " ");
            let style = if state.row == line && state.column == column {
                normal.bg(app.theme.selection_bg)
            } else {
                normal
            };
            frame.render_widget(
                Paragraph::new(value).style(style),
                Rect::new(x, y, width.saturating_sub(1), 1),
            );
        }
    }
    for offset in 0..height {
        let row = app.engine.preview_scroll() + usize::from(offset);
        if row >= sheet.rows.len() {
            break;
        }
        frame.render_widget(
            Line::from(format!("{:>6}", sheet.start_row as usize + row + 1)).style(secondary),
            Rect::new(area.x, body_top + offset, GUTTER.min(area.width), 1),
        );
    }
    let value = sheet
        .rows
        .get(state.row)
        .and_then(|row| row.get(state.column));
    let detail = value
        .map(|value| {
            format!(
                "{}{}: {}",
                sheet.columns[state.column],
                sheet.start_row as usize + state.row + 1,
                value
            )
        })
        .unwrap_or_else(|| "Empty worksheet".into());
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .style(normal),
        Rect::new(area.x, area.bottom() - 3, area.width, 3),
    );
}
