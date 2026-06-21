use ratatui::style::{Color, Modifier, Style};

pub type StyledToken = (Style, String);

pub fn highlight_file(path: &str, lines: &[String], dark: bool) -> Option<Vec<Vec<StyledToken>>> {
    reef_core::highlight::highlight_file(path, lines, dark).map(to_ratatui_tokens)
}

pub fn highlight_code_block(
    language: &str,
    lines: &[String],
    dark: bool,
) -> Option<Vec<Vec<StyledToken>>> {
    reef_core::highlight::highlight_code_block(language, lines, dark).map(to_ratatui_tokens)
}

pub fn to_ratatui_style(style: reef_core::text::TextStyle) -> Style {
    let mut out = Style::default();
    if let Some(fg) = style.fg {
        out = out.fg(Color::Rgb(fg.r, fg.g, fg.b));
    }
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.underlined {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn to_ratatui_tokens(rows: Vec<Vec<reef_core::text::StyledToken>>) -> Vec<Vec<StyledToken>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|token| (to_ratatui_style(token.style), token.text))
                .collect()
        })
        .collect()
}
