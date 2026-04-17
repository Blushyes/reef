use reef_protocol::{Color, StyledLine};
use ratatui::style::{Color as TColor, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert a reef StyledLine into a ratatui Line.
/// `hover` applies a subtle bg tint to spans that don't already have a bg color.
pub fn to_ratatui_line(sl: &StyledLine, hover: bool) -> Line<'static> {
    let hover_bg = TColor::Rgb(40, 40, 50);
    let spans: Vec<Span<'static>> = sl.spans.iter().map(|s| {
        let mut style = Style::default();
        if let Some(ref fg) = s.fg { style = style.fg(to_tcolor(fg)); }
        if let Some(ref bg) = s.bg {
            style = style.bg(to_tcolor(bg));
        } else if hover {
            style = style.bg(hover_bg);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_tcolor_named_basic_colors() {
        assert_eq!(to_tcolor(&Color::named("black")),   TColor::Black);
        assert_eq!(to_tcolor(&Color::named("red")),     TColor::Red);
        assert_eq!(to_tcolor(&Color::named("green")),   TColor::Green);
        assert_eq!(to_tcolor(&Color::named("yellow")),  TColor::Yellow);
        assert_eq!(to_tcolor(&Color::named("blue")),    TColor::Blue);
        assert_eq!(to_tcolor(&Color::named("magenta")), TColor::Magenta);
        assert_eq!(to_tcolor(&Color::named("cyan")),    TColor::Cyan);
        assert_eq!(to_tcolor(&Color::named("white")),   TColor::White);
    }

    #[test]
    fn to_tcolor_gray_variants() {
        assert_eq!(to_tcolor(&Color::named("gray")),     TColor::Gray);
        assert_eq!(to_tcolor(&Color::named("darkGray")), TColor::DarkGray);
    }

    #[test]
    fn to_tcolor_reset() {
        assert_eq!(to_tcolor(&Color::named("reset")), TColor::Reset);
    }

    #[test]
    fn to_tcolor_unknown_name_falls_back_to_reset() {
        assert_eq!(to_tcolor(&Color::named("chartreuse")), TColor::Reset);
        assert_eq!(to_tcolor(&Color::named("")), TColor::Reset);
    }

    #[test]
    fn to_tcolor_rgb_preserves_channels() {
        assert_eq!(to_tcolor(&Color::rgb(10, 20, 30)), TColor::Rgb(10, 20, 30));
        assert_eq!(to_tcolor(&Color::rgb(0, 0, 0)), TColor::Rgb(0, 0, 0));
        assert_eq!(to_tcolor(&Color::rgb(255, 255, 255)), TColor::Rgb(255, 255, 255));
    }
}
