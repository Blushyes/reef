//! OSC 52 剪贴板写入。终端托管在 iTerm2 / Kitty / WezTerm / Alacritty /
//! Ghostty / foot / 新版 tmux(需 `set -g set-clipboard on`)等里时,
//! OSC 52 转义序列能让应用直接写系统剪贴板,不需要 arboard/copypasta 这种
//! 跨平台依赖。macOS Terminal.app 不支持——选中仍然会高亮但剪贴板无变化。
//!
//! 格式: `ESC ] 52 ; c ; <base64(payload)> BEL`
//!
//! base64 编码手写(20 行),避免拉新依赖。

use std::io::{self, Write};

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(BASE64_ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

/// 向 stdout 写 OSC 52 序列把 `text` 放进系统剪贴板。写失败的情况(stdout
/// 异常)极罕见,此时返回 IoError 供上层做降级提示;OSC 52 被目标终端忽略
/// 时本函数仍然会成功返回——终端那头没有回复的协议。
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    let payload = base64_encode(text.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{payload}\x07")?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_one_byte_two_pad() {
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_two_bytes_one_pad() {
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn base64_three_bytes_no_pad() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_utf8_cjk() {
        // "你" = E4 BD A0 — 3 字节,正好对齐无填充。
        assert_eq!(base64_encode("你".as_bytes()), "5L2g");
    }

    #[test]
    fn base64_binary_full_range() {
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = base64_encode(&data);
        // 长度 = ceil(256/3)*4 = 344
        assert_eq!(encoded.len(), 344);
        // 末尾应只有 '=' 填充(256 mod 3 == 1 → "=="),倒数两个是 '='。
        assert!(encoded.ends_with("=="));
    }
}
