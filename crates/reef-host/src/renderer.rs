use reef_protocol::{Color, StyledLine};
use ratatui::style::{Color as TColor, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert a reef StyledLine into a ratatui Line.
pub fn to_ratatui_line(sl: &StyledLine) -> Line<'static> {
    let spans: Vec<Span<'static>> = sl.spans.iter().map(|s| {
        let mut style = Style::default();
        if let Some(ref fg) = s.fg { style = style.fg(to_tcolor(fg)); }
        if let Some(ref bg) = s.bg { style = style.bg(to_tcolor(bg)); }
        if s.bold == Some(true) { style = style.add_modifier(Modifier::BOLD); }
        if s.dim  == Some(true) { style = style.add_modifier(Modifier::DIM); }
        Span::styled(s.text.clone(), style)
    }).collect();
    Line::from(spans)
}

fn to_tcolor(c: &Color) -> TColor {
    match c {
        Color::Named(name) => match name.as_str() {
            "black"    => TColor::Black,
            "red"      => TColor::Red,
            "green"    => TColor::Green,
            "yellow"   => TColor::Yellow,
            "blue"     => TColor::Blue,
            "magenta"  => TColor::Magenta,
            "cyan"     => TColor::Cyan,
            "white"    => TColor::White,
            "gray"     => TColor::Gray,
            "darkGray" => TColor::DarkGray,
            "reset"    => TColor::Reset,
            _          => TColor::Reset,
        },
        Color::Rgb([r, g, b]) => TColor::Rgb(*r, *g, *b),
    }
}
