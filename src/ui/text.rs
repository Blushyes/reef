use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

/// 从字符串头部按显示列截取，直到累计宽度超过 `max_width` 为止。返回
/// 的切片以显示列为单位，保证不会切在宽字符中间。
pub fn truncate_to_width(s: &str, max_width: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_width {
            return &s[..i];
        }
        width += cw;
    }
    s
}

/// 对 styled token 流先跳 `skip_cols` 显示列、再保留至多 `max_width` 显示
/// 列。在跨越 skip 边界的宽字符会被整体丢弃（宁可多跳一列也不切半个字符，
/// 与 [`truncate_to_width`] 的保守策略一致）；在右端超出 `max_width` 的
/// 字符直接截断。
pub fn clip_spans<'a>(
    tokens: &'a [(Style, String)],
    skip_cols: usize,
    max_width: usize,
) -> Vec<Span<'a>> {
    let mut out: Vec<Span<'a>> = Vec::new();
    if max_width == 0 {
        return out;
    }
    let mut skipped = 0usize;
    let mut kept_cols = 0usize;

    'outer: for (style, text) in tokens {
        if kept_cols >= max_width {
            break;
        }

        let mut start: Option<usize> = None;
        let mut end: usize = 0;
        let mut local_kept = 0usize;

        for (i, c) in text.char_indices() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);

            if start.is_none() && skipped < skip_cols {
                if skipped + cw > skip_cols {
                    // 跨越 skip 边界的宽字符：整体丢弃，下一个字符起才开始保留。
                    skipped = skip_cols;
                    continue;
                }
                skipped += cw;
                continue;
            }

            if start.is_none() {
                start = Some(i);
                end = i;
            }

            if kept_cols + local_kept + cw > max_width {
                break;
            }
            local_kept += cw;
            end = i + c.len_utf8();
        }

        if let Some(s) = start {
            if end > s {
                out.push(Span::styled(&text[s..end], *style));
                kept_cols += local_kept;
            }
        }

        if kept_cols >= max_width {
            break 'outer;
        }
    }
    out
}

/// Overlay search-match backgrounds onto already-styled tokens. Byte ranges
/// are absolute offsets into the concatenated text of `tokens` (that is, the
/// plain text of the row). Follows the `hover::apply` composability pattern:
/// when a token already carries a background (e.g. diff added/removed rows),
/// the match is surfaced via `Modifier::REVERSED` instead of clobbering the
/// color — keeps the diff context visible under the highlight.
///
/// Returns a new token vec with (potentially) more tokens because each match
/// range that straddles a token boundary forces a split. Rows with no matches
/// round-trip unchanged.
pub fn overlay_match_highlight(
    tokens: Vec<(Style, String)>,
    ranges: &[Range<usize>],
    current_range: Option<Range<usize>>,
    match_bg: Color,
    current_bg: Color,
) -> Vec<(Style, String)> {
    if ranges.is_empty() {
        return tokens;
    }
    let mut out: Vec<(Style, String)> = Vec::with_capacity(tokens.len());
    let mut abs = 0usize;
    for (style, text) in tokens {
        if text.is_empty() {
            out.push((style, text));
            continue;
        }
        let base_abs = abs;
        let len = text.len();
        abs += len;

        // Walk char starts, flush a styled segment whenever the match-kind
        // changes. Runs of the same kind stay as a single token.
        let mut seg_start = 0usize;
        let mut run_kind = kind_at(base_abs, ranges, current_range.as_ref());
        for (i, _) in text.char_indices() {
            if i == 0 {
                continue;
            }
            let next_kind = kind_at(base_abs + i, ranges, current_range.as_ref());
            if next_kind != run_kind {
                out.push(styled_segment(
                    style,
                    &text[seg_start..i],
                    run_kind,
                    match_bg,
                    current_bg,
                ));
                seg_start = i;
                run_kind = next_kind;
            }
        }
        if seg_start < len {
            out.push(styled_segment(
                style,
                &text[seg_start..len],
                run_kind,
                match_bg,
                current_bg,
            ));
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    None,
    Match,
    Current,
}

fn kind_at(abs: usize, ranges: &[Range<usize>], current: Option<&Range<usize>>) -> MatchKind {
    if let Some(c) = current {
        if c.start <= abs && abs < c.end {
            return MatchKind::Current;
        }
    }
    for r in ranges {
        if r.start <= abs && abs < r.end {
            return MatchKind::Match;
        }
    }
    MatchKind::None
}

fn styled_segment(
    base: Style,
    text: &str,
    kind: MatchKind,
    match_bg: Color,
    current_bg: Color,
) -> (Style, String) {
    let style = match kind {
        MatchKind::None => base,
        MatchKind::Match => apply_search_bg(base, match_bg),
        MatchKind::Current => apply_search_bg(base, current_bg),
    };
    (style, text.to_string())
}

fn apply_search_bg(base: Style, bg: Color) -> Style {
    if base.bg.is_none() {
        base.bg(bg)
    } else {
        // Diff rows already carry a bg — flip fg/bg so the match stays visible.
        base.add_modifier(Modifier::REVERSED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── truncate_to_width ────────────────────────────────────────────────────

    #[test]
    fn truncate_to_width_ascii_within_limit() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_ascii_exact() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_width_ascii_over_limit() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_width_cjk_fits_pair() {
        assert_eq!(truncate_to_width("你好世界", 4), "你好");
    }

    #[test]
    fn truncate_to_width_cjk_cuts_before_wide_char() {
        // 3 列放不下第二个"好"，保留"你"
        assert_eq!(truncate_to_width("你好", 3), "你");
    }

    // ── clip_spans ───────────────────────────────────────────────────────────

    fn sty() -> Style {
        Style::default()
    }

    #[test]
    fn clip_spans_skip_zero_equivalent_to_truncate() {
        let tokens = vec![(sty(), "hello ".to_string()), (sty(), "world".to_string())];
        let out = clip_spans(&tokens, 0, 8);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "hello wo");
    }

    #[test]
    fn clip_spans_skip_past_first_token() {
        let tokens = vec![(sty(), "hello ".to_string()), (sty(), "world".to_string())];
        let out = clip_spans(&tokens, 6, 5);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "world");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn clip_spans_skip_splits_first_token() {
        let tokens = vec![(sty(), "abcdef".to_string())];
        let out = clip_spans(&tokens, 2, 10);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "cdef");
    }

    #[test]
    fn clip_spans_max_width_zero_returns_empty() {
        let tokens = vec![(sty(), "abc".to_string())];
        assert!(clip_spans(&tokens, 0, 0).is_empty());
    }

    #[test]
    fn clip_spans_skip_crosses_cjk_boundary_drops_wide_char() {
        // 跳 1 列落在"你"中间 → 整个"你"被丢弃
        let tokens = vec![(sty(), "你好".to_string())];
        let out = clip_spans(&tokens, 1, 10);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "好");
    }

    #[test]
    fn clip_spans_skip_beyond_total_width() {
        let tokens = vec![(sty(), "hi".to_string())];
        assert!(clip_spans(&tokens, 10, 10).is_empty());
    }

    #[test]
    fn clip_spans_preserves_distinct_styles_across_tokens() {
        let a = Style::default();
        let b = Style::default().fg(ratatui::style::Color::Red);
        let tokens = vec![(a, "hello".to_string()), (b, " world".to_string())];
        let out = clip_spans(&tokens, 0, 20);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content.as_ref(), "hello");
        assert_eq!(out[0].style, a);
        assert_eq!(out[1].content.as_ref(), " world");
        assert_eq!(out[1].style, b);
    }

    #[test]
    fn clip_spans_truncates_across_token_boundary() {
        let tokens = vec![(sty(), "abc".to_string()), (sty(), "defgh".to_string())];
        let out = clip_spans(&tokens, 0, 5);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "abcde");
    }

    #[test]
    fn clip_spans_empty_token_tolerated() {
        let tokens = vec![
            (sty(), "".to_string()),
            (sty(), "abc".to_string()),
            (sty(), "".to_string()),
        ];
        let out = clip_spans(&tokens, 1, 10);
        let joined: String = out.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "bc");
    }

    // ── overlay_match_highlight ──────────────────────────────────────────────

    fn collect_text(v: &[(Style, String)]) -> String {
        v.iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn overlay_no_ranges_is_identity() {
        let tokens = vec![(Style::default(), "hello".to_string())];
        let out = overlay_match_highlight(
            tokens.clone(),
            &[],
            None,
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(out, tokens);
    }

    #[test]
    fn overlay_single_match_splits_and_colors() {
        let tokens = vec![(Style::default(), "abcdef".to_string())];
        let r = 2..4;
        let out = overlay_match_highlight(
            tokens,
            std::slice::from_ref(&r),
            None,
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(collect_text(&out), "abcdef");
        // 3 segments: before, match, after.
        assert_eq!(out.len(), 3);
        assert!(out[0].0.bg.is_none());
        assert_eq!(out[1].0.bg, Some(ratatui::style::Color::Yellow));
        assert!(out[2].0.bg.is_none());
        assert_eq!(out[1].1, "cd");
    }

    #[test]
    fn overlay_match_spans_token_boundary() {
        let tokens = vec![
            (Style::default(), "abc".to_string()),
            (
                Style::default().fg(ratatui::style::Color::Red),
                "def".to_string(),
            ),
        ];
        // Range "bcde" — crosses boundary.
        let r = 1..5;
        let out = overlay_match_highlight(
            tokens,
            std::slice::from_ref(&r),
            None,
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(collect_text(&out), "abcdef");
        // Boundaries: a | bc | de | f  (4 segments, preserving fg styling)
        assert_eq!(out.len(), 4);
        assert!(out[0].0.bg.is_none());
        assert_eq!(out[0].1, "a");
        assert_eq!(out[1].0.bg, Some(ratatui::style::Color::Yellow));
        assert_eq!(out[1].1, "bc");
        assert_eq!(out[2].0.bg, Some(ratatui::style::Color::Yellow));
        assert_eq!(out[2].1, "de");
        assert_eq!(out[2].0.fg, Some(ratatui::style::Color::Red));
        assert!(out[3].0.bg.is_none());
    }

    #[test]
    fn overlay_current_overrides_match() {
        let tokens = vec![(Style::default(), "foofoo".to_string())];
        let out = overlay_match_highlight(
            tokens,
            &[0..3, 3..6],
            Some(3..6),
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(collect_text(&out), "foofoo");
        assert_eq!(out[0].0.bg, Some(ratatui::style::Color::Yellow));
        assert_eq!(out[1].0.bg, Some(ratatui::style::Color::Red));
    }

    #[test]
    fn overlay_reverses_when_bg_already_set() {
        let base = Style::default().bg(ratatui::style::Color::Green);
        let tokens = vec![(base, "abcdef".to_string())];
        let r = 2..4;
        let out = overlay_match_highlight(
            tokens,
            std::slice::from_ref(&r),
            None,
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(out[1].0.bg, Some(ratatui::style::Color::Green));
        assert!(out[1].0.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn overlay_preserves_text_across_segments() {
        let tokens = vec![(Style::default(), "hello world".to_string())];
        let out = overlay_match_highlight(
            tokens,
            &[0..5, 6..11],
            None,
            ratatui::style::Color::Yellow,
            ratatui::style::Color::Red,
        );
        assert_eq!(collect_text(&out), "hello world");
    }
}
