use ratatui::style::Style;
use ratatui::text::Span;
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

/// 从字符串头部跳过 `skip_cols` 个显示列，返回剩余切片。
///
/// 跨越宽字符（CJK / emoji）边界时，整个宽字符会被跳过 —— 宁可多跳一列也
/// 不切半个字符。这与 [`truncate_to_width`] 的保守策略一致。
pub fn skip_n_columns(s: &str, skip_cols: usize) -> &str {
    if skip_cols == 0 {
        return s;
    }
    let mut skipped = 0usize;
    for (i, c) in s.char_indices() {
        if skipped >= skip_cols {
            return &s[i..];
        }
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        skipped += cw;
    }
    ""
}

/// 对 styled token 流先跳 `skip_cols` 显示列、再保留至多 `max_width` 显示
/// 列。在跨越 skip 边界的宽字符会被整体丢弃（保持与 [`skip_n_columns`] 行为
/// 一致）；在右端超出 `max_width` 的字符直接截断。
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

    // ── skip_n_columns ───────────────────────────────────────────────────────

    #[test]
    fn skip_n_columns_zero_returns_original() {
        assert_eq!(skip_n_columns("hello", 0), "hello");
    }

    #[test]
    fn skip_n_columns_ascii_within() {
        assert_eq!(skip_n_columns("hello world", 6), "world");
    }

    #[test]
    fn skip_n_columns_ascii_exact_end() {
        assert_eq!(skip_n_columns("hello", 5), "");
    }

    #[test]
    fn skip_n_columns_ascii_over_end() {
        assert_eq!(skip_n_columns("hello", 100), "");
    }

    #[test]
    fn skip_n_columns_cjk_exact_boundary() {
        // "你" 占 2 列，skip=2 正好跨过它
        assert_eq!(skip_n_columns("你好", 2), "好");
    }

    #[test]
    fn skip_n_columns_cjk_inside_wide_char_skips_entire_char() {
        // skip=1 落在"你"中间 → 整体跳过"你"
        assert_eq!(skip_n_columns("你好", 1), "好");
    }

    #[test]
    fn skip_n_columns_cjk_inside_second_wide_char() {
        // skip=3 落在"好"中间 → 整体跳过"好"，返回空
        assert_eq!(skip_n_columns("你好", 3), "");
    }

    #[test]
    fn skip_n_columns_mixed_ascii_cjk() {
        // "a你bc" — a=1, 你=2, b=1, c=1（总 5 列）
        assert_eq!(skip_n_columns("a你bc", 3), "bc");
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
}
