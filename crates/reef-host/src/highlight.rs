use ratatui::style::{Color, Modifier, Style};
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedThemeName;

pub type StyledToken = (Style, String);

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

static THEME: LazyLock<Theme> = LazyLock::new(|| {
    two_face::theme::extra()
        .get(EmbeddedThemeName::OneHalfDark)
        .clone()
});

fn resolve_syntax(path: &str, lines: &[String]) -> Option<&'static SyntaxReference> {
    let p = Path::new(path);

    // 1. By extension (with aliases for common variants two-face doesn't cover)
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        let mapped = match ext {
            "jsx" | "cjs" | "mjs" => "js",
            "mts" | "cts" => "ts",
            "jsonc" | "json5" => "json",
            "ejs" | "erb" => "html",
            "zshrc" | "bashrc" | "profile" => "sh",
            _ => ext,
        };
        if let Some(s) = SYNTAX_SET.find_syntax_by_extension(mapped) {
            return Some(s);
        }
    }

    // 2. By full filename (Makefile, Dockerfile, Gemfile, CMakeLists.txt, .gitignore, ...)
    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
        if let Some(s) = SYNTAX_SET.find_syntax_by_extension(name) {
            return Some(s);
        }
        if let Some(s) = SYNTAX_SET.find_syntax_by_token(name) {
            return Some(s);
        }
    }

    // 3. By shebang / first-line match (scripts without extensions)
    if let Some(first) = lines.first() {
        if let Some(s) = SYNTAX_SET.find_syntax_by_first_line(first) {
            return Some(s);
        }
    }

    None
}

pub fn highlight_file(path: &str, lines: &[String]) -> Option<Vec<Vec<StyledToken>>> {
    let syntax = resolve_syntax(path, lines)?;
    let mut h = HighlightLines::new(syntax, &THEME);

    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let tokens = match h.highlight_line(line, &SYNTAX_SET) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(s, text)| (convert_style(s), text.to_string()))
                .collect(),
            Err(_) => vec![(Style::default(), line.clone())],
        };
        out.push(tokens);
    }
    Some(out)
}

fn convert_style(s: syntect::highlighting::Style) -> Style {
    let mut style = Style::default().fg(Color::Rgb(
        s.foreground.r,
        s.foreground.g,
        s.foreground.b,
    ));
    if s.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_rust_line() {
        let lines = vec!["fn main() { let x = \"hi\"; }".to_string()];
        let out = highlight_file("foo.rs", &lines).expect("rust must highlight");
        println!("tokens: {:#?}", out);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() > 1, "expected multiple tokens, got {:?}", out[0]);
    }

    #[test]
    fn highlight_extensions_exist() {
        let exts = [
            "rs", "py", "js", "ts", "tsx", "jsx", "go", "md", "json", "jsonc",
            "yml", "yaml", "toml", "sh", "bash", "zsh", "html", "css", "scss",
            "c", "cpp", "h", "hpp", "java", "kt", "swift", "rb", "php", "lua",
            "vue", "svelte", "dart", "zig", "nix", "hcl", "tf", "proto",
            "sql", "xml", "ini", "cjs", "mjs", "mts",
        ];
        let mut missing = vec![];
        for ext in exts {
            let path = format!("foo.{}", ext);
            if highlight_file(&path, &["hello".to_string()]).is_none() {
                missing.push(ext);
            }
        }
        assert!(missing.is_empty(), "missing extensions: {:?}", missing);
    }

    #[test]
    fn highlight_by_filename() {
        for name in ["Makefile", "Dockerfile", "Gemfile", "Rakefile", "CMakeLists.txt"] {
            let result = highlight_file(name, &["x = y".to_string()]);
            assert!(result.is_some(), "{} should resolve a syntax", name);
        }
    }

    #[test]
    fn highlight_by_shebang() {
        let result = highlight_file(
            "no_extension_file",
            &["#!/usr/bin/env python".to_string(), "print(1)".to_string()],
        );
        assert!(result.is_some(), "shebang should resolve python");
    }
}

