//! 应用级文本选中模型:记录 preview 面板里拖拽选中的范围,供渲染反色、
//! 复制到剪贴板时使用。
//!
//! 坐标系:`(line_index, byte_offset_in_line)`,其中 `line_index` 是 preview
//! 文件中的行号(0 起),`byte_offset_in_line` 是该行 UTF-8 字节偏移(不是
//! 显示列)。这样选中与 `preview_scroll` / `preview_h_scroll` 解耦——
//! 滚动后选中范围仍然指向文件里原来的同一段文本。

use std::ops::Range;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy)]
pub struct PreviewSelection {
    pub anchor: (usize, usize),
    pub active: (usize, usize),
    /// 鼠标当前是否仍按住左键在拖。Up(Left) 时置为 false。选中仍可渲染,
    /// 只是新的 Down 会开启一个新的锚点。
    pub dragging: bool,
}

impl PreviewSelection {
    pub fn new(anchor: (usize, usize)) -> Self {
        Self {
            anchor,
            active: anchor,
            dragging: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.active
    }

    /// 按文档顺序归一化成 `(start, end)`,`start <= end`。
    pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.active {
            (self.anchor, self.active)
        } else {
            (self.active, self.anchor)
        }
    }

    pub fn contains_line(&self, row: usize) -> bool {
        let (start, end) = self.normalized();
        row >= start.0 && row <= end.0
    }

    /// 给定某一行的原始文本,返回该行内被选中的字节区间(开闭区间,end
    /// 可能等于 `line.len()`,即"选中到行尾包括换行")。
    ///
    /// 不在选中范围内的行返回 `None`。
    pub fn line_byte_range(&self, row: usize, line: &str) -> Option<Range<usize>> {
        let (start, end) = self.normalized();
        if row < start.0 || row > end.0 {
            return None;
        }
        let line_len = line.len();
        let s = if row == start.0 {
            start.1.min(line_len)
        } else {
            0
        };
        let e = if row == end.0 {
            end.1.min(line_len)
        } else {
            line_len
        };
        if s >= e {
            // 单行同位置 / 反转后为空,仍返回一个空区间,让渲染端自己判断。
            return Some(s..s);
        }
        Some(s..e)
    }
}

/// 字符分类:用于双击选词时确定扩展边界。
#[derive(PartialEq, Eq)]
enum CharClass {
    AlphaNum, // [a-zA-Z0-9_] — 标识符字符
    Space,    // 空白
    Other,    // 标点 / 运算符等
}

fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::AlphaNum
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// 双击选词:以 `byte_offset`(必须是 UTF-8 字符起点或 `line.len()`)为基础,
/// 向两端扩展到同一字符分类的边界,返回字节区间。
///
/// - 标识符字符(`[a-zA-Z0-9_]`)向两边扩展整个单词。
/// - 空白向两边扩展连续空白。
/// - 其他字符(标点/运算符)同样向两边扩展相同分类的连续片段(如 `->`, `::`)。
/// - `byte_offset >= line.len()` 时返回空区间 `line.len()..line.len()`。
pub fn word_at_byte(line: &str, byte_offset: usize) -> Range<usize> {
    if line.is_empty() || byte_offset >= line.len() {
        return line.len()..line.len();
    }
    let anchor_char = line[byte_offset..].chars().next().unwrap();
    let class = char_class(anchor_char);

    // 向左扩展
    let mut start = byte_offset;
    let before: Vec<(usize, char)> = line[..byte_offset].char_indices().collect();
    for &(i, c) in before.iter().rev() {
        if char_class(c) == class {
            start = i;
        } else {
            break;
        }
    }

    // 向右扩展
    let mut end = byte_offset + anchor_char.len_utf8();
    for (off, c) in line[byte_offset..].char_indices().skip(1) {
        if char_class(c) == class {
            end = byte_offset + off + c.len_utf8();
        } else {
            break;
        }
    }

    start..end
}

/// 把可见列坐标(`UnicodeWidthChar` 计的显示列,0 起)翻译成该行的字节
/// 偏移。落在宽字符后半格时返回那个字符起点(选到它);超过行宽时返回
/// 行长度——这让"拖到行尾空白"的体验自然:选中一直延伸到换行符。
pub fn col_to_byte_offset(line: &str, col: usize) -> usize {
    let mut acc = 0usize;
    for (i, c) in line.char_indices() {
        if acc >= col {
            return i;
        }
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if acc + cw > col {
            // col 落在宽字符内部,取字符起点(含这个字符)。
            return i;
        }
        acc += cw;
    }
    line.len()
}

/// 把选中范围从 `lines` 提取成一段纯文本。多行之间用 `\n` 连接。
pub fn collect_selected_text(lines: &[String], sel: &PreviewSelection) -> String {
    if sel.is_empty() {
        return String::new();
    }
    let (start, end) = sel.normalized();
    let last = end.0.min(lines.len().saturating_sub(1));
    let mut out = String::new();
    for row in start.0..=last {
        let line = &lines[row];
        let range = match sel.line_byte_range(row, line) {
            Some(r) => r,
            None => continue,
        };
        if range.start < line.len() {
            let end_clamped = range.end.min(line.len());
            out.push_str(&line[range.start..end_clamped]);
        }
        if row < last {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── col_to_byte_offset ──────────────────────────────────────────────

    #[test]
    fn col_to_byte_offset_ascii_mid() {
        assert_eq!(col_to_byte_offset("hello", 3), 3);
    }

    #[test]
    fn col_to_byte_offset_ascii_zero() {
        assert_eq!(col_to_byte_offset("hello", 0), 0);
    }

    #[test]
    fn col_to_byte_offset_ascii_past_end_returns_len() {
        assert_eq!(col_to_byte_offset("hello", 99), 5);
    }

    #[test]
    fn col_to_byte_offset_cjk_boundary() {
        // "你好" — 每个字符 2 列、3 字节。col=2 正好落在第二个字符起点。
        assert_eq!(col_to_byte_offset("你好", 2), 3);
    }

    #[test]
    fn col_to_byte_offset_cjk_inside_wide_char() {
        // col=1 落在"你"内部,取"你"的起点(0)。
        assert_eq!(col_to_byte_offset("你好", 1), 0);
    }

    #[test]
    fn col_to_byte_offset_mixed() {
        // "a你b" — a(1 列) 你(2 列) b(1 列),总 4 列。col=3 → b 起点。
        assert_eq!(col_to_byte_offset("a你b", 3), 4);
    }

    // ── PreviewSelection ────────────────────────────────────────────────

    #[test]
    fn normalized_orders_anchor_before_active() {
        let s = PreviewSelection {
            anchor: (5, 3),
            active: (2, 0),
            dragging: false,
        };
        let (a, b) = s.normalized();
        assert_eq!(a, (2, 0));
        assert_eq!(b, (5, 3));
    }

    #[test]
    fn is_empty_when_anchor_equals_active() {
        let s = PreviewSelection::new((1, 1));
        assert!(s.is_empty());
    }

    #[test]
    fn line_byte_range_single_line() {
        let s = PreviewSelection {
            anchor: (0, 2),
            active: (0, 5),
            dragging: false,
        };
        assert_eq!(s.line_byte_range(0, "abcdef"), Some(2..5));
    }

    #[test]
    fn line_byte_range_multi_line_middle_full() {
        let s = PreviewSelection {
            anchor: (0, 2),
            active: (2, 3),
            dragging: false,
        };
        assert_eq!(s.line_byte_range(0, "abcdef"), Some(2..6));
        assert_eq!(s.line_byte_range(1, "middle"), Some(0..6));
        assert_eq!(s.line_byte_range(2, "tailxyz"), Some(0..3));
    }

    #[test]
    fn line_byte_range_out_of_range_returns_none() {
        let s = PreviewSelection {
            anchor: (0, 0),
            active: (1, 0),
            dragging: false,
        };
        assert_eq!(s.line_byte_range(5, "anything"), None);
    }

    #[test]
    fn line_byte_range_clamps_past_end() {
        let s = PreviewSelection {
            anchor: (0, 0),
            active: (0, 999),
            dragging: false,
        };
        // 请求字节 0..999,实际行长度 3,钳到 0..3。
        assert_eq!(s.line_byte_range(0, "abc"), Some(0..3));
    }

    // ── collect_selected_text ───────────────────────────────────────────

    #[test]
    fn collect_empty_returns_empty_string() {
        let lines = vec!["hello".to_string()];
        let s = PreviewSelection::new((0, 0));
        assert_eq!(collect_selected_text(&lines, &s), "");
    }

    #[test]
    fn collect_single_line_partial() {
        let lines = vec!["hello world".to_string()];
        let s = PreviewSelection {
            anchor: (0, 6),
            active: (0, 11),
            dragging: false,
        };
        assert_eq!(collect_selected_text(&lines, &s), "world");
    }

    #[test]
    fn collect_multi_line_joins_with_newlines() {
        let lines = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        let s = PreviewSelection {
            anchor: (0, 2),
            active: (2, 3),
            dragging: false,
        };
        assert_eq!(collect_selected_text(&lines, &s), "rst\nsecond\nthi");
    }

    #[test]
    fn collect_reverse_direction_still_in_document_order() {
        let lines = vec!["abc".to_string(), "def".to_string()];
        let s = PreviewSelection {
            anchor: (1, 2),
            active: (0, 1),
            dragging: false,
        };
        assert_eq!(collect_selected_text(&lines, &s), "bc\nde");
    }

    #[test]
    fn collect_cjk_preserves_full_chars() {
        let lines = vec!["你好世界".to_string()];
        // 字节偏移 3..9 = "好世"
        let s = PreviewSelection {
            anchor: (0, 3),
            active: (0, 9),
            dragging: false,
        };
        assert_eq!(collect_selected_text(&lines, &s), "好世");
    }

    // ── word_at_byte ────────────────────────────────────────────────────

    #[test]
    fn word_empty_line_returns_empty() {
        assert_eq!(word_at_byte("", 0), 0..0);
    }

    #[test]
    fn word_past_end_returns_end() {
        assert_eq!(word_at_byte("abc", 99), 3..3);
    }

    #[test]
    fn word_ascii_identifier_mid() {
        // "fn hello_world()" — click on 'l' at byte 5
        let line = "fn hello_world()";
        assert_eq!(word_at_byte(line, 5), 3..14); // "hello_world"
    }

    #[test]
    fn word_ascii_identifier_start() {
        let line = "fn hello()";
        assert_eq!(word_at_byte(line, 3), 3..8); // "hello"
    }

    #[test]
    fn word_ascii_identifier_end() {
        let line = "fn hello()";
        // click on 'o' = last char of "hello", byte 7
        assert_eq!(word_at_byte(line, 7), 3..8); // "hello"
    }

    #[test]
    fn word_space_expands_whitespace_run() {
        let line = "foo   bar";
        // click on middle space (byte 4)
        assert_eq!(word_at_byte(line, 4), 3..6); // "   "
    }

    #[test]
    fn word_operator_expands_to_run() {
        let line = "a -> b";
        // click on '-' at byte 2
        assert_eq!(word_at_byte(line, 2), 2..4); // "->"
    }

    #[test]
    fn word_single_punct() {
        let line = "a(b)";
        // click on '(' at byte 1
        assert_eq!(word_at_byte(line, 1), 1..2); // "("
    }

    #[test]
    fn word_cjk_identifier_class() {
        // CJK chars are alphanumeric in Rust (is_alphanumeric)
        let line = "let 变量 = 1";
        // "变量" — "变" at byte 4, "量" at byte 7
        assert_eq!(word_at_byte(line, 4), 4..10); // "变量"
    }

    #[test]
    fn word_underscore_is_alnum() {
        let line = "_foo_bar";
        assert_eq!(word_at_byte(line, 0), 0..8); // whole "_foo_bar"
    }
}
