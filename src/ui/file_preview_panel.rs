use crate::app::App;
use crate::file_tree::{BinaryInfo, BinaryReason, ImagePreview, PreviewBody, PreviewContent};
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::text::{clip_spans, overlay_match_highlight, overlay_selection_highlight};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use ratatui_image::StatefulImage;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Cache the preview-panel rect so the mouse handler can hit-test
    // drag-to-select events. Reset to None at the top of `ui::render` on
    // tabs that don't render this panel.
    app.last_preview_rect = Some(inner);

    let preview = match app.preview_content.take() {
        None => {
            render_empty(f, app, inner);
            return;
        }
        Some(preview) => preview,
    };

    match &preview.body {
        PreviewBody::Text { .. } => render_text(f, app, inner, &preview),
        PreviewBody::Image(img) => render_image(f, app, inner, &preview.file_path, img),
        PreviewBody::Binary(info) => render_binary_info(f, app, inner, &preview.file_path, info),
    }

    app.preview_content = Some(preview);
}

fn render_empty(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 1 {
        return;
    }
    let msg = Line::from(Span::styled(
        t(Msg::PreviewEmpty),
        Style::default().fg(app.theme.fg_secondary),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(20) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

/// Image preview. Header + separator + metadata line + StatefulImage.
/// `StatefulProtocol` lives on `App` (not on `PreviewContent`) because it
/// holds non-`Send` state and is constructed on the main thread when the
/// worker's `DynamicImage` lands. See `App::apply_worker_result`.
fn render_image(f: &mut Frame, app: &mut App, area: Rect, path: &str, img: &ImagePreview) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;

    // Header.
    let mut y = area.y;
    let max_y = area.y + area.height;
    f.render_widget(
        Line::from(Span::styled(
            path,
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, y, area.width, 1),
    );
    y += 1;

    // Separator.
    if y < max_y {
        f.render_widget(
            Line::from(Span::styled(
                "─".repeat(area.width as usize),
                Style::default().fg(th.fg_secondary),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }

    // Metadata line. Skipped when the panel is too short — in that case
    // we'd rather reclaim the row for actual pixels than spend it on text.
    let wants_meta = area.height >= 5;
    if wants_meta && y < max_y {
        let meta = image_meta_text(img);
        f.render_widget(
            Line::from(Span::styled(meta, Style::default().fg(th.fg_secondary))),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
        // Blank spacer row for visual breathing room.
        if y < max_y {
            y += 1;
        }
    }

    // Image body. ratatui-image handles the encoding to whichever protocol
    // the Picker detected. If the picker wasn't detected at all (None),
    // fall back to a text card — nothing is renderable without it.
    if y >= max_y {
        return;
    }
    let image_area = Rect::new(area.x, y, area.width, max_y - y);
    if image_area.height < 1 || image_area.width < 1 {
        return;
    }
    match app.preview_image_protocol.as_mut() {
        Some(proto) => {
            let widget = StatefulImage::default();
            f.render_stateful_widget(widget, image_area, proto);
        }
        None => {
            let msg = Line::from(Span::styled(
                t(Msg::PreviewImageUnavailable),
                Style::default().fg(th.fg_secondary),
            ));
            let cy = image_area.y + image_area.height / 2;
            let cx = image_area.x
                + image_area
                    .width
                    .saturating_sub(UnicodeWidthStr::width(t(Msg::PreviewImageUnavailable)) as u16)
                    / 2;
            f.render_widget(msg, Rect::new(cx, cy, image_area.width, 1));
        }
    }
}

fn image_meta_text(img: &ImagePreview) -> String {
    let fmt = format_name(img.format);
    let size = format_size(img.bytes_on_disk);
    match img.frame_count {
        Some(n) if n > 1 => format!(
            "{}×{} · {} · {} · {} frames (first frame shown)",
            img.width_px, img.height_px, fmt, size, n
        ),
        _ => format!("{}×{} · {} · {}", img.width_px, img.height_px, fmt, size),
    }
}

fn format_name(f: image::ImageFormat) -> &'static str {
    match f {
        image::ImageFormat::Png => "PNG",
        image::ImageFormat::Jpeg => "JPEG",
        image::ImageFormat::Gif => "GIF",
        image::ImageFormat::WebP => "WebP",
        image::ImageFormat::Bmp => "BMP",
        image::ImageFormat::Tiff => "TIFF",
        image::ImageFormat::Ico => "ICO",
        _ => "image",
    }
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Friendly metadata card for anything we can't render as pixels —
/// non-image binaries (PDF, zip, video…), oversized images, unsupported
/// formats (SVG/AVIF/HEIC), corrupt files, and the 0-byte case. The
/// `reason` decides the one-line message; the header carries the filename
/// and the metadata line carries MIME + size.
fn render_binary_info(f: &mut Frame, app: &App, area: Rect, path: &str, info: &BinaryInfo) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let mut y = area.y;
    let max_y = area.y + area.height;

    f.render_widget(
        Line::from(Span::styled(
            path,
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x, y, area.width, 1),
    );
    y += 1;

    if y < max_y {
        f.render_widget(
            Line::from(Span::styled(
                "─".repeat(area.width as usize),
                Style::default().fg(th.fg_secondary),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }

    // MIME + size line (e.g. "application/pdf · 2.4 MB"). Skipped if we
    // have no MIME (NullBytes / Empty reasons).
    if y < max_y {
        let parts: Vec<String> = match info.mime {
            Some(m) if info.bytes_on_disk > 0 => {
                vec![m.to_string(), format_size(info.bytes_on_disk)]
            }
            Some(m) => vec![m.to_string()],
            None if info.bytes_on_disk > 0 => vec![format_size(info.bytes_on_disk)],
            None => Vec::new(),
        };
        if !parts.is_empty() {
            let meta = parts.join(" · ");
            f.render_widget(
                Line::from(Span::styled(meta, Style::default().fg(th.fg_secondary))),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
        }
    }

    // Reason line, centred vertically in the remaining space.
    let reason = binary_reason_text(info);
    if y < max_y {
        let cy = y + (max_y - y) / 2;
        let reason_w = UnicodeWidthStr::width(reason.as_str()) as u16;
        let cx = area.x + area.width.saturating_sub(reason_w) / 2;
        f.render_widget(
            Line::from(Span::styled(reason, Style::default().fg(th.fg_secondary))),
            Rect::new(cx, cy, area.width.saturating_sub(cx - area.x), 1),
        );
    }
}

fn binary_reason_text(info: &BinaryInfo) -> String {
    match &info.reason {
        BinaryReason::NonImage => t(Msg::PreviewBinaryNonImage).to_string(),
        BinaryReason::UnsupportedImage => t(Msg::PreviewBinaryUnsupportedImage).to_string(),
        BinaryReason::TooLarge => t(Msg::PreviewBinaryTooLarge).to_string(),
        BinaryReason::DecodeError(msg) => {
            format!("{}: {}", t(Msg::PreviewBinaryDecodeError), msg)
        }
        BinaryReason::NullBytes => t(Msg::PreviewBinaryGeneric).to_string(),
        BinaryReason::Empty => t(Msg::PreviewBinaryEmpty).to_string(),
    }
}

fn render_text(f: &mut Frame, app: &mut App, area: Rect, preview: &PreviewContent) {
    let (lines, highlighted) = match &preview.body {
        PreviewBody::Text { lines, highlighted } => (lines, highlighted),
        _ => return,
    };
    let th = app.theme;
    let mut y = area.y;
    let max_y = area.y + area.height;

    // File header
    if y < max_y {
        let header = Line::from(Span::styled(
            preview.file_path.as_str(),
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(header, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    // Separator
    if y < max_y {
        let sep = Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(sep, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    let content_height = (max_y - y) as usize;
    // Cache the content viewport height so search-jump can center matches.
    app.last_preview_view_h = content_height as u16;
    let max_scroll = lines.len().saturating_sub(content_height);
    app.preview_scroll = app.preview_scroll.min(max_scroll);

    let gutter_w = 6usize; // " NNNNN "
    let content_w = (area.width as usize).saturating_sub(gutter_w);

    // Clamp horizontal scroll against the widest line currently in view.
    let max_visible_w: usize = lines
        .iter()
        .skip(app.preview_scroll)
        .take(content_height)
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    app.preview_h_scroll = app.preview_h_scroll.min(max_h);
    let h = app.preview_h_scroll;

    // Cache the content-area origin so the mouse handler can translate a
    // `(column, row)` hit into `(file_line, byte_offset)`.
    app.last_preview_content_origin = Some((area.x + gutter_w as u16, y, gutter_w as u16));
    let selection = app.preview_selection;

    for (i, line) in lines.iter().skip(app.preview_scroll).enumerate() {
        let cy = y + i as u16;
        if cy >= max_y {
            break;
        }
        let real_idx = app.preview_scroll + i;
        let lineno = real_idx + 1;

        let gutter = Span::styled(
            format!("{:>5} ", lineno),
            Style::default().fg(th.fg_secondary),
        );

        // Unified path for both the syntect-tokenized case and the plain-text
        // fallback: build a token vec, overlay any search matches for this row,
        // then clip horizontally. Keeps horizontal-scroll and search highlight
        // independent of whether syntax tokens were produced.
        let base_tokens: Vec<(Style, String)> =
            match highlighted.as_ref().and_then(|hh| hh.get(real_idx)) {
                Some(tokens) => tokens.clone(),
                None => vec![(Style::default().fg(th.fg_primary), line.clone())],
            };
        let (mut ranges, mut cur) = app
            .search
            .ranges_on_row(SearchTarget::FilePreview, real_idx);
        // `global_search::accept` stashes a single-row highlight at the
        // matching line so we can light it up once the async preview lands.
        // Applied alongside the `/` search ranges using the same overlay
        // helper — the existing "current match" slot is natural, since there
        // is only ever one global-search highlight per preview.
        if let Some(hl) = app.preview_highlight.as_ref() {
            if preview.file_path == hl.path.to_string_lossy() && hl.row == real_idx {
                ranges.push(hl.byte_range.clone());
                if cur.is_none() {
                    cur = Some(hl.byte_range.clone());
                }
            }
        }
        let tokens = if ranges.is_empty() {
            base_tokens
        } else {
            overlay_match_highlight(
                base_tokens,
                &ranges,
                cur,
                th.search_match,
                th.search_current,
            )
        };
        // Drag-selection highlight layered on top — `Modifier::REVERSED`
        // so it composes cleanly with any theme / search background.
        let tokens = match selection
            .as_ref()
            .and_then(|s| s.line_byte_range(real_idx, line))
        {
            Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
            _ => tokens,
        };

        let mut spans = vec![gutter];
        spans.extend(clip_spans(&tokens, h, content_w));

        let rendered = Line::from(spans);
        f.render_widget(rendered, Rect::new(area.x, cy, area.width, 1));
    }
}
