//! Full-screen renderer for the Settings page. Activation goes through
//! Enter (not click) so a stray click can't silently flip a pref the
//! user wasn't aiming at.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::i18n::{Msg, t};
use crate::settings::{ItemValue, SettingItem, SettingSection, current_value};
use crate::ui::mouse::ClickAction;

const LABEL_COL_WIDTH: u16 = 28;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            t(Msg::SettingsTitle),
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(2, 2, 1, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 || inner.width < 30 {
        return;
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    let desc_y = inner.y + inner.height.saturating_sub(2);
    let body_h = inner.height.saturating_sub(2);
    let body_rect = Rect::new(inner.x, inner.y, inner.width, body_h);

    let selected = app.settings.selected();
    render_body(f, app, body_rect, selected);

    let editing_editor = app.settings.editor_edit.is_some();
    if editing_editor {
        render_inline_editor_prompt(f, app, Rect::new(inner.x, desc_y, inner.width, 1));
    } else {
        let desc = t(selected.description());
        let line = Line::from(Span::styled(
            format!("  {desc}"),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(line, Rect::new(inner.x, desc_y, inner.width, 1));
    }

    let footer_msg = if editing_editor {
        Msg::SettingsEditorEditHint
    } else {
        Msg::SettingsFooterHint
    };
    let footer = Line::from(Span::styled(
        t(footer_msg),
        Style::default().fg(th.chrome_muted_fg),
    ));
    f.render_widget(footer, Rect::new(inner.x, footer_y, inner.width, 1));
}

fn render_body(f: &mut Frame, app: &mut App, area: Rect, selected: SettingItem) {
    let th = app.theme;
    let mut y = area.y;
    let max_y = area.y + area.height;
    let mut last_section: Option<SettingSection> = None;
    let value_col = (area.width.saturating_sub(2))
        .saturating_sub(LABEL_COL_WIDTH)
        .max(20);

    for (idx, item) in SettingItem::ALL.iter().enumerate() {
        if y >= max_y {
            break;
        }
        let section = item.section();
        if Some(section) != last_section {
            if last_section.is_some() {
                y += 1;
                if y >= max_y {
                    break;
                }
            }
            let header = Line::from(Span::styled(
                t(section.label()).to_uppercase(),
                Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
            ));
            f.render_widget(header, Rect::new(area.x, y, area.width, 1));
            y += 1;
            last_section = Some(section);
            if y >= max_y {
                break;
            }
        }

        let row_rect = Rect::new(area.x, y, area.width, 1);
        render_row(f, app, row_rect, value_col, *item, *item == selected);
        app.hit_registry
            .register(row_rect, ClickAction::SettingsRow(idx));
        y += 1;
    }
}

fn render_row(
    f: &mut Frame,
    app: &App,
    rect: Rect,
    value_col: u16,
    item: SettingItem,
    selected: bool,
) {
    let th = app.theme;
    let row_bg = if selected {
        th.selection_bg
    } else {
        th.chrome_bg
    };
    let chevron = if selected { " › " } else { "   " };

    let label_text = t(item.label());
    let (value_text, value_style) = format_value(&current_value(item, app), th, selected);

    let consumed = UnicodeWidthStr::width(chevron) + UnicodeWidthStr::width(label_text);
    let pad_str = " ".repeat((value_col as usize).saturating_sub(consumed));

    // ratatui has no "row background" primitive; a full-width styled
    // space is the standard workaround so the selected row's bg sweeps
    // past the value pill to the right edge.
    let bg_line = Line::from(Span::styled(
        " ".repeat(rect.width as usize),
        Style::default().bg(row_bg),
    ));
    f.render_widget(bg_line, rect);

    let label_style = Style::default()
        .fg(th.fg_primary)
        .bg(row_bg)
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let chevron_style = Style::default()
        .fg(th.accent)
        .bg(row_bg)
        .add_modifier(Modifier::BOLD);

    let line = Line::from(vec![
        Span::styled(chevron, chevron_style),
        Span::styled(label_text, label_style),
        Span::styled(pad_str, Style::default().bg(row_bg)),
        Span::styled(value_text, value_style.bg(row_bg)),
    ]);
    f.render_widget(line, rect);
}

fn format_value(value: &ItemValue, th: crate::ui::theme::Theme, selected: bool) -> (String, Style) {
    let strong = if selected {
        Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(th.fg_primary)
    };
    match value {
        // Bools carry no good/bad semantics — colouring `on` green
        // would imply "on is desirable", which isn't true for tree-mode
        // toggles. Off-state dim + on-state accent reads as "current
        // selection" instead.
        ItemValue::Bool(b) => {
            let label = if *b {
                t(Msg::SettingsValueOn)
            } else {
                t(Msg::SettingsValueOff)
            };
            let style = if *b {
                strong
            } else {
                Style::default().fg(th.fg_secondary)
            };
            (format!("[{label}]"), style)
        }
        ItemValue::Choice(label) => (format!("[{label}]"), strong),
        ItemValue::Text(s) if s.is_empty() => (
            t(Msg::SettingsEditorPlaceholder).to_string(),
            Style::default()
                .fg(th.fg_secondary)
                .add_modifier(Modifier::ITALIC),
        ),
        ItemValue::Text(s) => (s.clone(), strong),
    }
}

fn render_inline_editor_prompt(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let Some(edit) = app.settings.editor_edit.as_ref() else {
        return;
    };
    let prompt = "  › ";
    let line = Line::from(vec![
        Span::styled(
            prompt,
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(edit.buffer.clone(), Style::default().fg(th.fg_primary)),
    ]);
    f.render_widget(line, area);

    let prompt_w = UnicodeWidthStr::width(prompt) as u16;
    let buffer_w =
        UnicodeWidthStr::width(&edit.buffer[..edit.cursor.min(edit.buffer.len())]) as u16;
    let cursor_x = area.x + (prompt_w + buffer_w).min(area.width.saturating_sub(1));
    f.set_cursor_position((cursor_x, area.y));
}
