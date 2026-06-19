use pulldown_cmark::{
    Alignment as MdAlignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownPreview {
    pub rows: Vec<Vec<MarkdownSpan>>,
    pub text_rows: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownSpan {
    pub text: String,
    pub style: MarkdownStyle,
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownStyle {
    pub role: MarkdownRole,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownRole {
    Normal,
    Heading,
    Quote,
    Code,
    CodeBlockHeader,
    CodeBlockText,
    Link,
    TableHeader,
    Border,
}

const CODE_BLOCK_LABEL_FG: Color = Color::Rgb(150, 180, 205);

impl MarkdownPreview {
    pub fn spans_for_row(&self, row: usize) -> Option<&[MarkdownSpan]> {
        self.rows.get(row).map(Vec::as_slice)
    }

    pub fn text_for_row(&self, row: usize) -> Option<&str> {
        self.text_rows.get(row).map(String::as_str)
    }
}

impl MarkdownStyle {
    fn normal() -> Self {
        Self {
            role: MarkdownRole::Normal,
            bold: false,
            italic: false,
        }
    }

    fn role(role: MarkdownRole) -> Self {
        Self {
            role,
            bold: false,
            italic: false,
        }
    }

    fn apply(self, base: Style, theme: &crate::ui::theme::Theme) -> Style {
        let mut out = match self.role {
            MarkdownRole::Normal => base,
            MarkdownRole::Heading => base.fg(theme.accent).add_modifier(Modifier::BOLD),
            MarkdownRole::Quote => base.fg(theme.fg_secondary),
            MarkdownRole::Code => base.fg(theme.accent),
            MarkdownRole::CodeBlockHeader => base
                .fg(code_block_label_fg(theme))
                .bg(code_block_bg(theme))
                .add_modifier(Modifier::BOLD),
            MarkdownRole::CodeBlockText => base.fg(code_block_fg(theme)).bg(code_block_bg(theme)),
            MarkdownRole::Link => base.fg(theme.accent).add_modifier(Modifier::UNDERLINED),
            MarkdownRole::Border => base.fg(theme.fg_secondary),
            MarkdownRole::TableHeader => base.add_modifier(Modifier::BOLD),
        };
        if self.bold {
            out = out.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            out = out.add_modifier(Modifier::ITALIC);
        }
        out
    }
}

pub fn code_block_bg(theme: &crate::ui::theme::Theme) -> Color {
    if theme.is_dark {
        Color::Rgb(36, 38, 46)
    } else {
        Color::Rgb(246, 248, 250)
    }
}

fn code_block_fg(theme: &crate::ui::theme::Theme) -> Color {
    if theme.is_dark {
        Color::Rgb(230, 232, 238)
    } else {
        theme.fg_primary
    }
}

fn code_block_label_fg(theme: &crate::ui::theme::Theme) -> Color {
    if theme.is_dark {
        CODE_BLOCK_LABEL_FG
    } else {
        theme.fg_secondary
    }
}

impl MarkdownSpan {
    pub fn styled_text<'a>(&'a self, theme: &crate::ui::theme::Theme) -> (Style, Cow<'a, str>) {
        let mut style = self
            .style
            .apply(Style::default().fg(theme.fg_primary), theme);
        if self.link.is_some() {
            style = style.fg(theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        (style, Cow::Borrowed(self.text.as_str()))
    }
}

#[derive(Debug)]
struct InlineState {
    spans: Vec<MarkdownSpan>,
    style: MarkdownStyle,
    link_dest: Option<String>,
}

impl InlineState {
    fn new() -> Self {
        Self {
            spans: Vec::new(),
            style: MarkdownStyle::normal(),
            link_dest: None,
        }
    }

    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == self.style
            && last.link == self.link_dest
        {
            last.text.push_str(text);
            return;
        }
        self.spans.push(MarkdownSpan {
            text: text.to_string(),
            style: self.style,
            link: self.link_dest.clone(),
        });
    }

    fn finish(self) -> Vec<MarkdownSpan> {
        self.spans
    }
}

impl Default for InlineState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct TableBuild {
    alignments: Vec<MdAlignment>,
    rows: Vec<Vec<Vec<MarkdownSpan>>>,
    current_row: Vec<Vec<MarkdownSpan>>,
    current_cell: InlineState,
}

pub fn build_markdown_preview(path: &str, source: &str) -> Option<MarkdownPreview> {
    if !is_markdown_path(path) {
        return None;
    }

    let mut rows = Vec::new();
    let mut inline = InlineState::new();
    let mut table: Option<TableBuild> = None;
    let mut quote_depth = 0usize;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut in_code_block = false;

    let parser = Parser::new_ext(
        source,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    );
    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    flush_inline(&mut rows, &mut inline);
                    inline.style.role = MarkdownRole::Heading;
                    inline.style.bold = true;
                }
                Tag::BlockQuote(_) => {
                    flush_inline(&mut rows, &mut inline);
                    quote_depth += 1;
                    inline.style.role = MarkdownRole::Quote;
                    inline.push(&format!("{} ", "│".repeat(quote_depth)));
                }
                Tag::CodeBlock(kind) => {
                    flush_inline(&mut rows, &mut inline);
                    in_code_block = true;
                    let label = match kind {
                        CodeBlockKind::Fenced(lang) => code_block_label(lang.as_ref()),
                        CodeBlockKind::Indented => None,
                    };
                    if let Some(label) = label {
                        rows.push(vec![MarkdownSpan {
                            text: format!(" {label} "),
                            style: MarkdownStyle::role(MarkdownRole::CodeBlockHeader),
                            link: None,
                        }]);
                    }
                    rows.push(code_block_padding_row());
                }
                Tag::List(start) => {
                    flush_inline(&mut rows, &mut inline);
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush_inline(&mut rows, &mut inline);
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    let marker = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}. ");
                            *n += 1;
                            marker
                        }
                        _ => "• ".to_string(),
                    };
                    inline.push(&indent);
                    inline.push(&marker);
                }
                Tag::Emphasis => active_inline(&mut inline, &mut table).style.italic = true,
                Tag::Strong => active_inline(&mut inline, &mut table).style.bold = true,
                Tag::Link { dest_url, .. } => {
                    let active = active_inline(&mut inline, &mut table);
                    active.style.role = MarkdownRole::Link;
                    active.link_dest = Some(dest_url.to_string());
                }
                Tag::Image { dest_url, .. } => {
                    let active = active_inline(&mut inline, &mut table);
                    active.push("image: ");
                    active.link_dest = Some(dest_url.to_string());
                }
                Tag::Table(alignments) => {
                    flush_inline(&mut rows, &mut inline);
                    table = Some(TableBuild {
                        alignments,
                        ..TableBuild::default()
                    });
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = table.as_mut() {
                        t.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = table.as_mut() {
                        t.current_cell = InlineState::new();
                    }
                }
                Tag::Paragraph => {}
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Heading(_) => {
                    flush_inline(&mut rows, &mut inline);
                    push_blank(&mut rows);
                    inline.style = MarkdownStyle::normal();
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline(&mut rows, &mut inline);
                    quote_depth = quote_depth.saturating_sub(1);
                    inline.style = if quote_depth > 0 {
                        MarkdownStyle::role(MarkdownRole::Quote)
                    } else {
                        MarkdownStyle::normal()
                    };
                    if quote_depth == 0 {
                        push_blank(&mut rows);
                    }
                }
                TagEnd::CodeBlock => {
                    rows.push(code_block_padding_row());
                    push_blank(&mut rows);
                    in_code_block = false;
                }
                TagEnd::List(_) => {
                    flush_inline(&mut rows, &mut inline);
                    list_stack.pop();
                    if list_stack.is_empty() {
                        push_blank(&mut rows);
                    }
                }
                TagEnd::Item => flush_inline(&mut rows, &mut inline),
                TagEnd::Emphasis => active_inline(&mut inline, &mut table).style.italic = false,
                TagEnd::Strong => active_inline(&mut inline, &mut table).style.bold = false,
                TagEnd::Link => {
                    let active = active_inline(&mut inline, &mut table);
                    active.link_dest = None;
                    active.style.role = MarkdownRole::Normal;
                }
                TagEnd::Image => {
                    let active = active_inline(&mut inline, &mut table);
                    if let Some(dest) = active.link_dest.clone() {
                        active.push(&format!(" ({dest})"));
                    }
                    active.link_dest = None;
                    active.style.role = MarkdownRole::Normal;
                }
                TagEnd::TableCell => {
                    if let Some(t) = table.as_mut() {
                        let cell = std::mem::take(&mut t.current_cell);
                        t.current_row.push(cell.finish());
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(t) = table.as_mut()
                        && !t.current_row.is_empty()
                    {
                        t.rows.push(std::mem::take(&mut t.current_row));
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = table.take() {
                        rows.extend(render_table(t));
                        push_blank(&mut rows);
                    }
                }
                TagEnd::Paragraph => {
                    flush_inline(&mut rows, &mut inline);
                    if table.is_none() && list_stack.is_empty() && quote_depth == 0 {
                        push_blank(&mut rows);
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    for part in text.split_terminator('\n') {
                        rows.push(vec![
                            MarkdownSpan {
                                text: "  ".to_string(),
                                style: MarkdownStyle::role(MarkdownRole::CodeBlockText),
                                link: None,
                            },
                            MarkdownSpan {
                                text: part.to_string(),
                                style: MarkdownStyle::role(MarkdownRole::CodeBlockText),
                                link: None,
                            },
                        ]);
                    }
                    continue;
                }
                for (idx, part) in text.split('\n').enumerate() {
                    if idx > 0 {
                        flush_inline(&mut rows, &mut inline);
                    }
                    active_inline(&mut inline, &mut table).push(part);
                }
            }
            Event::Code(text) => {
                let active = active_inline(&mut inline, &mut table);
                let old = active.style;
                active.style.role = MarkdownRole::Code;
                active.push(text.as_ref());
                active.style = old;
            }
            Event::SoftBreak | Event::HardBreak => flush_inline(&mut rows, &mut inline),
            Event::Rule => {
                flush_inline(&mut rows, &mut inline);
                rows.push(vec![MarkdownSpan {
                    text: "─".repeat(32),
                    style: MarkdownStyle::role(MarkdownRole::Border),
                    link: None,
                }]);
                push_blank(&mut rows);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                active_inline(&mut inline, &mut table).push(html.as_ref())
            }
            Event::FootnoteReference(name) => {
                active_inline(&mut inline, &mut table).push(&format!("[^{name}]"))
            }
            Event::TaskListMarker(checked) => {
                active_inline(&mut inline, &mut table).push(if checked { "☑ " } else { "☐ " })
            }
            _ => {}
        }
    }
    flush_inline(&mut rows, &mut inline);
    trim_trailing_blanks(&mut rows);

    let text_rows = rows.iter().map(|row| row_text(row)).collect();
    Some(MarkdownPreview { rows, text_rows })
}

pub fn is_markdown_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase),
        Some(ext) if matches!(ext.as_str(), "md" | "markdown" | "mdown" | "mkd")
    )
}

fn active_inline<'a>(
    inline: &'a mut InlineState,
    table: &'a mut Option<TableBuild>,
) -> &'a mut InlineState {
    if let Some(t) = table.as_mut() {
        return &mut t.current_cell;
    }
    inline
}

fn flush_inline(rows: &mut Vec<Vec<MarkdownSpan>>, inline: &mut InlineState) {
    if inline.spans.is_empty() {
        return;
    }
    rows.push(std::mem::take(&mut inline.spans));
}

fn push_blank(rows: &mut Vec<Vec<MarkdownSpan>>) {
    if rows.last().is_some_and(Vec::is_empty) {
        return;
    }
    rows.push(Vec::new());
}

fn trim_trailing_blanks(rows: &mut Vec<Vec<MarkdownSpan>>) {
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
}

fn code_block_label(info: &str) -> Option<String> {
    info.split_whitespace().next().map(str::to_string)
}

fn code_block_padding_row() -> Vec<MarkdownSpan> {
    vec![MarkdownSpan {
        text: String::new(),
        style: MarkdownStyle::role(MarkdownRole::CodeBlockText),
        link: None,
    }]
}

fn render_table(table: TableBuild) -> Vec<Vec<MarkdownSpan>> {
    if table.rows.is_empty() {
        return Vec::new();
    }
    let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }

    let mut widths = vec![3usize; cols];
    for row in &table.rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(spans_width(cell));
        }
    }

    let mut out = Vec::new();
    out.push(table_rule("┏", "┳", "┓", &widths));
    for (row_idx, row) in table.rows.iter().enumerate() {
        let header = row_idx == 0;
        out.push(render_table_row(row, &widths, &table.alignments, header));
        if header {
            out.push(table_rule("┣", "╋", "┫", &widths));
        }
    }
    out.push(table_rule("┗", "┻", "┛", &widths));
    out
}

fn table_rule(left: &str, join: &str, right: &str, widths: &[usize]) -> Vec<MarkdownSpan> {
    let mut text = String::from(left);
    for (idx, width) in widths.iter().enumerate() {
        text.push_str(&"━".repeat(width + 2));
        text.push_str(if idx + 1 == widths.len() { right } else { join });
    }
    vec![MarkdownSpan {
        text,
        style: MarkdownStyle::role(MarkdownRole::Border),
        link: None,
    }]
}

fn render_table_row(
    row: &[Vec<MarkdownSpan>],
    widths: &[usize],
    alignments: &[MdAlignment],
    header: bool,
) -> Vec<MarkdownSpan> {
    let mut out = Vec::new();
    out.push(table_border("┃ "));
    for (idx, width) in widths.iter().enumerate() {
        let cell = row.get(idx).cloned().unwrap_or_default();
        push_aligned_cell(
            &mut out,
            cell,
            *width,
            *alignments.get(idx).unwrap_or(&MdAlignment::None),
            header,
        );
        out.push(table_border(" ┃"));
        if idx + 1 < widths.len() {
            out.push(table_border(" "));
        }
    }
    out
}

fn push_aligned_cell(
    out: &mut Vec<MarkdownSpan>,
    mut cell: Vec<MarkdownSpan>,
    width: usize,
    alignment: MdAlignment,
    header: bool,
) {
    let cell_w = spans_width(&cell);
    let pad = width.saturating_sub(cell_w);
    let (left, right) = match alignment {
        MdAlignment::Right => (pad, 0),
        MdAlignment::Center => (pad / 2, pad - pad / 2),
        MdAlignment::Left | MdAlignment::None => (0, pad),
    };
    if left > 0 {
        out.push(normal_spaces(left));
    }
    if header {
        for span in &mut cell {
            span.style.role = MarkdownRole::TableHeader;
            span.style.bold = true;
        }
    }
    out.extend(cell);
    if right > 0 {
        out.push(normal_spaces(right));
    }
}

fn spans_width(spans: &[MarkdownSpan]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.text.as_str()))
        .sum()
}

fn row_text(row: &[MarkdownSpan]) -> String {
    row.iter().map(|s| s.text.as_str()).collect()
}

fn table_border(s: &str) -> MarkdownSpan {
    MarkdownSpan {
        text: s.to_string(),
        style: MarkdownStyle::role(MarkdownRole::Border),
        link: None,
    }
}

fn normal_spaces(width: usize) -> MarkdownSpan {
    MarkdownSpan {
        text: " ".repeat(width),
        style: MarkdownStyle::normal(),
        link: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(md: &MarkdownPreview) -> Vec<String> {
        md.rows
            .iter()
            .map(|r| r.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn markdown_path_detection_is_extension_based() {
        assert!(is_markdown_path("README.md"));
        assert!(is_markdown_path("notes.Markdown"));
        assert!(!is_markdown_path("notes.txt"));
    }

    #[test]
    fn builds_headings_lists_quotes_links_and_code() {
        let source =
            "# Title\n\n- **bold** [site](https://x.test)\n> quote\n\n```rs\nfn main() {}\n```\n";
        let md = build_markdown_preview("README.md", source).unwrap();
        let rendered = texts(&md);
        assert!(rendered.iter().any(|l| l == "Title"));
        assert!(rendered.iter().any(|l| l.contains("• bold site")));
        let link = md
            .rows
            .iter()
            .flatten()
            .find(|span| span.text == "site")
            .expect("link span");
        assert_eq!(link.link.as_deref(), Some("https://x.test"));
        assert!(
            link.styled_text(&crate::ui::theme::Theme::dark())
                .0
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(rendered.iter().any(|l| l == "│ quote"));
        assert!(rendered.iter().any(|l| l == " rs "));
        assert!(rendered.iter().any(|l| l == "  fn main() {}"));
        let code_row = md
            .rows
            .iter()
            .find(|row| row_text(row) == "  fn main() {}")
            .expect("code row");
        assert!(
            code_row
                .iter()
                .all(|span| span.style.role == MarkdownRole::CodeBlockText)
        );
    }

    #[test]
    fn unlabeled_code_block_has_no_header() {
        let md = build_markdown_preview("README.md", "```\nplain\n```\n").unwrap();
        let rendered = texts(&md);

        assert_eq!(rendered, vec!["", "  plain", ""]);
    }

    #[test]
    fn builds_integrated_table_with_cjk_width() {
        let source = "| 名称 | Count |\n|:---|---:|\n| 鲨鱼 | 12 |\n| ray | 3 |\n";
        let md = build_markdown_preview("README.md", source).unwrap();
        let rendered = texts(&md);
        assert_eq!(md.text_rows, rendered);
        assert_eq!(rendered[0], "┏━━━━━━┳━━━━━━━┓");
        assert_eq!(rendered[1], "┃ 名称 ┃ Count ┃");
        assert_eq!(rendered[2], "┣━━━━━━╋━━━━━━━┫");
        assert_eq!(rendered[3], "┃ 鲨鱼 ┃    12 ┃");
        assert_eq!(rendered[4], "┃ ray  ┃     3 ┃");
        assert_eq!(rendered[5], "┗━━━━━━┻━━━━━━━┛");
    }
}
