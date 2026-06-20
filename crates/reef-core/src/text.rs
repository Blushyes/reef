#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub fg: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledToken {
    pub style: TextStyle,
    pub text: String,
}

impl StyledToken {
    pub fn new(style: TextStyle, text: impl Into<String>) -> Self {
        Self {
            style,
            text: text.into(),
        }
    }
}
