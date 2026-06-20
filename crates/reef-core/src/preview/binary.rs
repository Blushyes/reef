#[derive(Debug, Clone)]
pub struct BinaryInfo {
    pub bytes_on_disk: u64,
    pub mime: Option<&'static str>,
    pub reason: BinaryReason,
    pub meta_line: String,
}

#[derive(Debug, Clone)]
pub enum BinaryReason {
    NonImage,
    UnsupportedImage,
    TooLarge,
    DecodeError(String),
    NullBytes,
    Empty,
}

impl BinaryInfo {
    pub fn new(bytes_on_disk: u64, mime: Option<&'static str>, reason: BinaryReason) -> Self {
        Self {
            bytes_on_disk,
            mime,
            reason,
            meta_line: binary_meta_line(mime, bytes_on_disk),
        }
    }
}

pub(crate) fn decode_error(msg: impl Into<String>) -> BinaryReason {
    const MAX_DECODE_ERROR_LEN: usize = 100;
    let mut s: String = msg.into();
    if s.len() > MAX_DECODE_ERROR_LEN {
        s.truncate(MAX_DECODE_ERROR_LEN);
        s.push('…');
    }
    BinaryReason::DecodeError(s)
}

pub fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn binary_meta_line(mime: Option<&'static str>, bytes_on_disk: u64) -> String {
    match mime {
        Some(m) if bytes_on_disk > 0 => format!("{m} · {}", human_bytes(bytes_on_disk)),
        Some(m) => m.to_string(),
        None if bytes_on_disk > 0 => human_bytes(bytes_on_disk),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_error_truncates_long_messages() {
        let long = "x".repeat(600);

        let BinaryReason::DecodeError(message) = decode_error(long) else {
            panic!("expected DecodeError");
        };

        assert!(message.chars().count() <= 101);
        assert!(message.ends_with('…'));
    }
}
