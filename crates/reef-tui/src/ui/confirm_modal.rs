//! Generic centered confirm modal — `[Cancel] [Primary]`.
//!
//! Callers build a `ConfirmModal` (body text + two `FnOnce` callbacks) and
//! hand it to `App::show_confirm`. `App::fire_confirm_primary` /
//! `fire_confirm_cancel` `take()` the modal before calling the closure, so
//! the closure receives a clean `&mut App` and can even re-`show_confirm`
//! itself — that's the "keep open while a prior op is still running" retry
//! path used by `execute_tree_delete`.
//!
//! Rendered last in `ui::render` so it floats above other overlays; mouse +
//! keyboard are gated in `input::*` so events never reach the panels
//! underneath. A full-screen `ConfirmModalCancel` hit zone is registered
//! first and button zones last — reverse-order hit-testing makes outside
//! clicks dismiss while button clicks resolve normally.

use crate::app::App;
use crate::ui::hover;
use crate::ui::layout::center_rect;
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear};
use unicode_width::UnicodeWidthStr;

pub struct ConfirmModal {
    pub title: String,
    pub tone: ModalTone,
    /// Body text. Split on `'\n'` for multi-line rendering.
    pub body: String,
    pub primary_label: String,
    pub cancel_label: String,
    /// Keys that fire `on_confirm`. Typically `['y', 'Y']`. Empty means
    /// "only the mouse can confirm" — Esc / N / C still cancel.
    pub confirm_keys: Vec<char>,
    pub on_confirm: Box<dyn FnOnce(&mut App)>,
    pub on_cancel: Box<dyn FnOnce(&mut App)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalTone {
    Default,
    Danger,
}

const MIN_W: u16 = 44;
const MAX_W: u16 = 64;
const SIDE_PAD: u16 = 4;
const BUTTON_GAP: u16 = 3;
const PAD_TOP: u16 = 1;
const PAD_BOTTOM: u16 = 1;
const TITLE_TO_BODY: u16 = 1;
const BODY_TO_BUTTONS: u16 = 1;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let Some(modal) = app.confirm_modal.as_ref() else {
        return;
    };
    let th = app.theme;

    let body_lines: Vec<&str> = modal.body.lines().collect();
    let body_lines = if body_lines.is_empty() {
        vec![""]
    } else {
        body_lines
    };

    let hint = crate::i18n::confirm_modal_hint();
    let hint_w = UnicodeWidthStr::width(hint.as_str()) as u16;

    let body_max_w = body_lines
        .iter()
        .map(|s| UnicodeWidthStr::width(*s) as u16)
        .max()
        .unwrap_or(0);
    let title_w = UnicodeWidthStr::width(modal.title.as_str()) as u16;
    let cancel_label_w = UnicodeWidthStr::width(modal.cancel_label.as_str()) as u16;
    let primary_label_w = UnicodeWidthStr::width(modal.primary_label.as_str()) as u16;
    // `  label  ` chip = label + 4 cols of left/right padding.
    let cancel_btn_w = cancel_label_w + 4;
    let primary_btn_w = primary_label_w + 4;
    let buttons_w = cancel_btn_w + BUTTON_GAP + primary_btn_w;

    let content_w = body_max_w.max(buttons_w).max(hint_w).max(title_w);
    // On extremely narrow terminals (< MIN_W) `MAX_W.min(screen.width)` can
    // dip below `MIN_W`, which would make `clamp(min, max)` panic. Compute
    // the upper bound first and floor the lower bound to it.
    let max_w = MAX_W.min(screen.width);
    let min_w = MIN_W.min(max_w);
    let popup_w = (content_w + SIDE_PAD * 2).clamp(min_w, max_w);

    // Vertical layout: top_pad | title | gap | body | gap | buttons | hint | bottom_pad
    let popup_h = (PAD_TOP
        + 1                       // title
        + TITLE_TO_BODY
        + body_lines.len() as u16
        + BODY_TO_BUTTONS
        + 1                       // buttons
        + 1                       // hint
        + PAD_BOTTOM)
        .min(screen.height);

    let area = center_rect(screen, popup_w, popup_h);

    // Fallthrough cancel: every screen row gets a cancel hit zone. Button
    // zones registered later shadow it via reverse-order hit-testing.
    for sy in screen.y..screen.y + screen.height {
        app.hit_registry
            .register_row(screen.x, sy, screen.width, ClickAction::ConfirmModalCancel);
    }

    // Wipe stale glyphs and paint the card surface in one shot. The
    // chrome_active_bg is one shade lighter than the underlying panels'
    // chrome_bg, which provides the "lift" without a line-drawing border.
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(th.chrome_active_bg)),
        area,
    );

    let card_bg = th.chrome_active_bg;
    let title_accent = match modal.tone {
        ModalTone::Danger => th.removed_accent,
        ModalTone::Default => th.accent,
    };

    let inner_x = area.x + SIDE_PAD;
    let inner_w = area.width.saturating_sub(SIDE_PAD * 2);

    // Title row, left-aligned within the inner content column. Pure
    // foreground accent — no bg banner, no border integration.
    let title_y = area.y + PAD_TOP;
    if title_y < area.y + area.height {
        f.render_widget(
            Line::from(Span::styled(
                modal.title.clone(),
                Style::default()
                    .fg(title_accent)
                    .bg(card_bg)
                    .add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner_x, title_y, inner_w, 1),
        );
    }

    // Body lines.
    let body_start_y = title_y + 1 + TITLE_TO_BODY;
    for (i, line) in body_lines.iter().enumerate() {
        let row_y = body_start_y + i as u16;
        if row_y >= area.y + area.height {
            break;
        }
        f.render_widget(
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(th.fg_primary).bg(card_bg),
            )),
            Rect::new(inner_x, row_y, inner_w, 1),
        );
    }

    let buttons_y = body_start_y + body_lines.len() as u16 + BODY_TO_BUTTONS;
    let hint_y = buttons_y + 1;

    // Buttons centered within `inner`.
    let buttons_start_x = inner_x + inner_w.saturating_sub(buttons_w) / 2;
    let cancel_x = buttons_start_x;
    let primary_x = buttons_start_x + cancel_btn_w + BUTTON_GAP;

    // Buttons + hint are skipped when `popup_h` got clamped against an
    // extremely short terminal. The modal isn't usable at that point
    // anyway — the user just sees the title + body — but bailing out
    // here avoids an off-bounds `Rect::new` panic.
    if buttons_y < area.y + area.height {
        let cancel_hovered = hover::is_hover(
            app,
            Rect::new(cancel_x, buttons_y, cancel_btn_w, 1),
            buttons_y,
        );
        let primary_hovered = hover::is_hover(
            app,
            Rect::new(primary_x, buttons_y, primary_btn_w, 1),
            buttons_y,
        );

        // Cancel: recessed chip — chrome_bg is *darker* than the card,
        // so the button reads as "pressed into" the surface. Lifts to
        // hover_bg on mouse-over.
        let cancel_bg = if cancel_hovered {
            th.selection_bg
        } else {
            th.chrome_bg
        };
        f.render_widget(
            Line::from(Span::styled(
                format!("  {}  ", modal.cancel_label),
                Style::default()
                    .fg(th.fg_primary)
                    .bg(cancel_bg)
                    .add_modifier(if cancel_hovered {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )),
            Rect::new(cancel_x, buttons_y, cancel_btn_w, 1),
        );

        // Primary: tone-coloured chip — danger uses error_bg, default uses
        // accent. Hover adds UNDERLINED rather than REVERSED so the chip
        // stays legible on light themes (REVERSED would swap fg/bg and
        // give us dark-text-on-cyan, which fights the high-contrast
        // light preset).
        let (primary_bg, primary_fg) = match modal.tone {
            ModalTone::Danger => (th.error_bg, th.fg_primary),
            ModalTone::Default => (th.accent, th.chrome_bg),
        };
        let mut primary_style = Style::default()
            .fg(primary_fg)
            .bg(primary_bg)
            .add_modifier(Modifier::BOLD);
        if primary_hovered {
            primary_style = primary_style.add_modifier(Modifier::UNDERLINED);
        }
        f.render_widget(
            Line::from(Span::styled(
                format!("  {}  ", modal.primary_label),
                primary_style,
            )),
            Rect::new(primary_x, buttons_y, primary_btn_w, 1),
        );

        // Hit zones registered LAST so they shadow the fallthrough cancel.
        app.hit_registry.register_row(
            cancel_x,
            buttons_y,
            cancel_btn_w,
            ClickAction::ConfirmModalCancel,
        );
        app.hit_registry.register_row(
            primary_x,
            buttons_y,
            primary_btn_w,
            ClickAction::ConfirmModalPrimary,
        );
    }

    // Keyboard hint centered under the buttons. Secondary fg so it reads
    // as chrome, not as part of the prompt itself.
    if hint_y < area.y + area.height {
        let hint_x = inner_x + inner_w.saturating_sub(hint_w) / 2;
        f.render_widget(
            Line::from(Span::styled(
                hint,
                Style::default().fg(th.fg_secondary).bg(card_bg),
            )),
            Rect::new(hint_x, hint_y, hint_w, 1),
        );
    }
}
