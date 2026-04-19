use ratatui::style::{Color, Modifier, Style};
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedThemeName;

pub type StyledToken = (Style, String);

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

static THEME_DARK: LazyLock<Theme> = LazyLock::new(|| {
    two_face::theme::extra()
        .get(EmbeddedThemeName::OneHalfDark)
        .clone()
});

static THEME_LIGHT: LazyLock<Theme> = LazyLock::new(|| {
    two_face::theme::extra()
        .get(EmbeddedThemeName::OneHalfLight)
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

pub fn highlight_file(path: &str, lines: &[String], dark: bool) -> Option<Vec<Vec<StyledToken>>> {
    let syntax = resolve_syntax(path, lines)?;
    let theme: &Theme = if dark { &THEME_DARK } else { &THEME_LIGHT };
    let mut h = HighlightLines::new(syntax, theme);

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
    let mut style = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
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
        let out = highlight_file("foo.rs", &lines, true).expect("rust must highlight");
        println!("tokens: {:#?}", out);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].len() > 1,
            "expected multiple tokens, got {:?}",
            out[0]
        );
    }

    #[test]
    fn highlight_extensions_exist() {
        let exts = [
            "rs", "py", "js", "ts", "tsx", "jsx", "go", "md", "json", "jsonc", "yml", "yaml",
            "toml", "sh", "bash", "zsh", "html", "css", "scss", "c", "cpp", "h", "hpp", "java",
            "kt", "swift", "rb", "php", "lua", "vue", "svelte", "dart", "zig", "nix", "hcl", "tf",
            "proto", "sql", "xml", "ini", "cjs", "mjs", "mts",
        ];
        let mut missing = vec![];
        for ext in exts {
            let path = format!("foo.{}", ext);
            if highlight_file(&path, &["hello".to_string()], true).is_none() {
                missing.push(ext);
            }
        }
        assert!(missing.is_empty(), "missing extensions: {:?}", missing);
    }

    #[test]
    fn highlight_by_filename() {
        for name in [
            "Makefile",
            "Dockerfile",
            "Gemfile",
            "Rakefile",
            "CMakeLists.txt",
        ] {
            let result = highlight_file(name, &["x = y".to_string()], true);
            assert!(result.is_some(), "{} should resolve a syntax", name);
        }
    }

    #[test]
    fn highlight_by_shebang() {
        let result = highlight_file(
            "no_extension_file",
            &["#!/usr/bin/env python".to_string(), "print(1)".to_string()],
            true,
        );
        assert!(result.is_some(), "shebang should resolve python");
    }

    #[test]
    fn highlight_empty_lines_returns_empty_vec() {
        let result = highlight_file("foo.rs", &[], true);
        // A known syntax with zero lines should return Some([])
        assert!(
            matches!(result, Some(ref v) if v.is_empty()),
            "empty input → Some([])"
        );
    }

    #[test]
    fn highlight_unknown_extension_returns_none() {
        assert!(highlight_file("foo.xyz", &["hello".to_string()], true).is_none());
    }

    #[test]
    fn highlight_returns_same_line_count() {
        let lines: Vec<String> = (0..10).map(|i| format!("let x{} = {};", i, i)).collect();
        let result = highlight_file("foo.rs", &lines, true).expect("rust must highlight");
        assert_eq!(result.len(), lines.len());
    }

    #[test]
    fn highlight_many_lines_all_returned() {
        // highlight_file has no line-count limit — all lines are processed
        let lines: Vec<String> = (0..100).map(|i| format!("let x{i} = {i};")).collect();
        let result = highlight_file("foo.rs", &lines, true).expect("rust must highlight");
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn highlight_toml_file_recognized() {
        let lines = vec!["[package]".to_string(), "name = \"reef\"".to_string()];
        assert!(
            highlight_file("Cargo.toml", &lines, true).is_some(),
            "toml should be recognized"
        );
    }

    #[test]
    fn highlight_light_theme_smoke() {
        // Light theme uses OneHalfLight; token colors come from a different
        // palette but structure must match (same line count, multiple tokens
        // for a non-trivial rust line).
        let lines = vec!["fn main() { let x = \"hi\"; }".to_string()];
        let out = highlight_file("foo.rs", &lines, false).expect("light theme must highlight");
        assert_eq!(out.len(), 1);
        assert!(out[0].len() > 1);
    }
}
