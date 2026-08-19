use std::collections::HashMap;
use std::ops::Range;

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub row: usize,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatchRange {
    pub byte_range: Range<usize>,
    pub utf16_range: Range<usize>,
    pub display_column_range: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct LiteralSearchMatcher {
    regex: regex::Regex,
}

impl LiteralSearchMatcher {
    pub fn new(query: &str) -> Result<Self, regex::Error> {
        let regex = regex::RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()?;
        Ok(Self { regex })
    }

    pub fn ranges(&self, text: &str) -> Vec<TextMatchRange> {
        let mut ranges = Vec::new();
        self.visit_ranges(text, |range| {
            ranges.push(range);
            true
        });
        ranges
    }

    pub fn visit_ranges(&self, text: &str, mut visitor: impl FnMut(TextMatchRange) -> bool) {
        let mut byte_cursor: usize = 0;
        let mut utf16_cursor: usize = 0;
        let mut display_column_cursor: usize = 0;
        for matched in self.regex.find_iter(text) {
            if matched.start() >= matched.end() {
                continue;
            }
            let byte_range = matched.start()..matched.end();
            let prefix = &text[byte_cursor..byte_range.start];
            let matched_text = &text[byte_range.clone()];
            utf16_cursor = utf16_cursor.saturating_add(prefix.encode_utf16().count());
            display_column_cursor =
                display_column_cursor.saturating_add(UnicodeWidthStr::width(prefix));
            let matched_utf16_length = matched_text.encode_utf16().count();
            let matched_display_width = UnicodeWidthStr::width(matched_text);
            let range = TextMatchRange {
                byte_range,
                utf16_range: utf16_cursor..utf16_cursor.saturating_add(matched_utf16_length),
                display_column_range: display_column_cursor
                    ..display_column_cursor.saturating_add(matched_display_width),
            };
            if !visitor(range) {
                break;
            }
            byte_cursor = matched.end();
            utf16_cursor = utf16_cursor.saturating_add(matched_utf16_length);
            display_column_cursor = display_column_cursor.saturating_add(matched_display_width);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

pub fn smart_case_insensitive(query: &str) -> bool {
    query.chars().all(|c| !c.is_uppercase())
}

pub fn find_literal_all(haystack: &str, needle: &str, case_insensitive: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    if case_insensitive {
        let h = haystack.to_ascii_lowercase();
        let n = needle.to_ascii_lowercase();
        let mut start = 0usize;
        while let Some(pos) = h[start..].find(&n) {
            let abs = start + pos;
            results.push(abs..abs + needle.len());
            start = abs + needle.len().max(1);
        }
    } else {
        let mut start = 0usize;
        while let Some(pos) = haystack[start..].find(needle) {
            let abs = start + pos;
            results.push(abs..abs + needle.len());
            start = abs + needle.len().max(1);
        }
    }
    results
}

pub fn find_all_with_options(
    haystack: &str,
    needle: &str,
    opts: &SearchOptions,
) -> Result<Vec<Range<usize>>, regex::Error> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    if opts.regex {
        let pattern = if opts.whole_word {
            format!(r"\b(?:{}){}", needle, r"\b")
        } else {
            needle.to_string()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!opts.case_sensitive)
            .multi_line(false)
            .build()?;
        let mut out = Vec::new();
        for m in re.find_iter(haystack) {
            if m.start() < m.end() {
                out.push(m.start()..m.end());
            }
        }
        return Ok(out);
    }

    let raw = find_literal_all(haystack, needle, !opts.case_sensitive);
    if !opts.whole_word {
        return Ok(raw);
    }
    Ok(raw
        .into_iter()
        .filter(|r| is_word_boundary(haystack, r))
        .collect())
}

pub fn build_row_index(matches: &[SearchMatch]) -> HashMap<usize, Vec<usize>> {
    let mut index = HashMap::new();
    for (i, m) in matches.iter().enumerate() {
        index.entry(m.row).or_insert_with(Vec::new).push(i);
    }
    index
}

pub fn ranges_on_row(
    matches: &[SearchMatch],
    row_index: &HashMap<usize, Vec<usize>>,
    current: Option<usize>,
    row: usize,
) -> (Vec<Range<usize>>, Option<Range<usize>>) {
    let Some(idxs) = row_index.get(&row) else {
        return (Vec::new(), None);
    };
    let mut all = Vec::with_capacity(idxs.len());
    let mut cur = None;
    for &i in idxs {
        let Some(m) = matches.get(i) else {
            continue;
        };
        if Some(i) == current {
            cur = Some(m.byte_range.clone());
        }
        all.push(m.byte_range.clone());
    }
    (all, cur)
}

pub fn truncate_line(text: &str, max_chars: usize) -> String {
    let mut chars_seen = 0usize;
    let mut byte_end = 0usize;
    for (bi, c) in text.char_indices() {
        if chars_seen >= max_chars {
            break;
        }
        byte_end = bi + c.len_utf8();
        chars_seen += 1;
    }
    if byte_end >= text.len() {
        text.to_string()
    } else {
        text[..byte_end].to_string()
    }
}

pub fn clip_range(range: Range<usize>, max_end: usize) -> Option<Range<usize>> {
    if range.start >= max_end {
        return None;
    }
    let end = range.end.min(max_end);
    Some(range.start..end)
}

fn is_word_boundary(text: &str, range: &Range<usize>) -> bool {
    let before_ok = range.start == 0
        || !text[..range.start]
            .chars()
            .next_back()
            .is_some_and(is_word_char);
    let after_ok =
        range.end >= text.len() || !text[range.end..].chars().next().is_some_and(is_word_char);
    before_ok && after_ok
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(case_sensitive: bool, whole_word: bool, regex: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            whole_word,
            regex,
        }
    }

    #[test]
    fn literal_case_insensitive_finds_all() {
        assert_eq!(
            find_literal_all("Foo FOO foo", "foo", true),
            vec![0..3, 4..7, 8..11]
        );
    }

    #[test]
    fn literal_case_sensitive_finds_exact_case() {
        assert_eq!(find_literal_all("Foo FOO foo", "Foo", false), vec![0..3]);
    }

    #[test]
    fn literal_empty_needle_returns_empty() {
        assert!(find_literal_all("abc", "", true).is_empty());
    }

    #[test]
    fn literal_overlapping_advances_by_needle_len() {
        assert_eq!(find_literal_all("aaaa", "aa", true), vec![0..2, 2..4]);
    }

    #[test]
    fn smart_case_lowercase_is_insensitive() {
        assert!(smart_case_insensitive("foo"));
        assert!(!smart_case_insensitive("Foo"));
    }

    #[test]
    fn whole_word_rejects_substring_match() {
        let r = find_all_with_options("food foo foo!", "foo", &opts(false, true, false)).unwrap();
        assert_eq!(r, vec![5..8, 9..12]);
    }

    #[test]
    fn regex_basic() {
        let r = find_all_with_options("a1 b22 c333", r"\d+", &opts(false, false, true)).unwrap();
        assert_eq!(r, vec![1..2, 4..6, 8..11]);
    }

    #[test]
    fn regex_invalid_returns_err() {
        assert!(find_all_with_options("anything", "(unclosed", &opts(false, false, true)).is_err());
    }

    #[test]
    fn regex_zero_width_matches_are_skipped() {
        let r = find_all_with_options("abc", r".*?", &opts(false, false, true)).unwrap();
        assert!(r.iter().all(|m| m.start < m.end));
    }

    #[test]
    fn row_index_returns_ranges_for_row() {
        let matches = vec![
            SearchMatch {
                row: 1,
                byte_range: 0..3,
            },
            SearchMatch {
                row: 1,
                byte_range: 5..8,
            },
            SearchMatch {
                row: 2,
                byte_range: 0..3,
            },
        ];
        let index = build_row_index(&matches);
        let (all, cur) = ranges_on_row(&matches, &index, Some(1), 1);
        assert_eq!(all, vec![0..3, 5..8]);
        assert_eq!(cur, Some(5..8));
    }

    #[test]
    fn truncate_line_respects_utf8_boundary() {
        let mut s = "a".repeat(200);
        for _ in 0..60 {
            s.push('你');
        }
        let out = truncate_line(&s, 250);
        assert_eq!(out.chars().count(), 250);
    }

    #[test]
    fn clip_range_clips_end_and_drops_out_of_bounds() {
        assert_eq!(clip_range(3..8, 5), Some(3..5));
        assert_eq!(clip_range(8..10, 5), None);
    }

    #[test]
    fn literal_matcher_reports_utf16_and_display_coordinates() {
        let matcher = LiteralSearchMatcher::new("配角").unwrap();
        let ranges = matcher.ranges("a🪸配角 z 配角");

        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].utf16_range, 3..5);
        assert_eq!(ranges[0].display_column_range, 3..7);
        assert_eq!(ranges[1].utf16_range, 8..10);
        assert_eq!(ranges[1].display_column_range, 10..14);
    }

    #[test]
    fn literal_matcher_is_unicode_case_insensitive() {
        let matcher = LiteralSearchMatcher::new("reef").unwrap();
        let ranges = matcher.ranges("Reef REEF reef");

        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].byte_range, 0..4);
        assert_eq!(ranges[2].byte_range, 10..14);
    }
}
