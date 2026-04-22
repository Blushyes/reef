//! Minimal `~/.ssh/config` parser for the Hosts picker (Ctrl+O).
//!
//! We deliberately keep this small — a 50-line hand-rolled parser is
//! enough to surface the host aliases a user has typed into their config.
//! Unsupported directives (`Match`, `Include`, wildcards like `Host *`)
//! are skipped silently; users who want picker entries can declare a
//! specific `Host alias` block.
//!
//! Ordering of returned entries matches declaration order in the file so
//! the UI can show the user's own layout. Duplicate aliases aren't
//! de-duped (unusual in practice; first writer wins in ssh's own
//! resolution anyway, which the picker doesn't try to simulate).

use std::path::{Path, PathBuf};

/// One host block from `~/.ssh/config`. Only the three fields the picker
/// actually renders are carried — we don't translate the full ssh
/// config grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// The alias from the `Host` line, e.g. `prod-db`. Never a wildcard.
    pub alias: String,
    /// `HostName` if set in the block; `None` means ssh will fall back
    /// to using `alias` as the hostname.
    pub hostname: Option<String>,
    /// `User` if set in the block.
    pub user: Option<String>,
}

/// Parse `~/.ssh/config` (or the fallback provided by
/// `SSH_CONFIG_PATH_OVERRIDE` in tests). Missing file → `Ok(vec![])` so
/// callers can unconditionally call this without branching on existence.
pub fn parse_ssh_config() -> std::io::Result<Vec<HostEntry>> {
    let path = default_config_path();
    parse_file(&path)
}

/// Explicit-path variant — used by the test suite to feed a fixture
/// without mucking with `$HOME`.
pub fn parse_file(path: &Path) -> std::io::Result<Vec<HostEntry>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(path)?;
    Ok(parse_str(&body))
}

fn default_config_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("SSH_CONFIG_PATH_OVERRIDE") {
        return PathBuf::from(override_path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".ssh").join("config")
}

/// Core parse logic separated for testability. `Match` and `Include`
/// blocks reset the current alias so following `HostName`/`User` lines
/// don't accidentally attach to whatever `Host` came before them.
pub fn parse_str(body: &str) -> Vec<HostEntry> {
    let mut out: Vec<HostEntry> = Vec::new();
    let mut current: Option<HostEntry> = None;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Key / value split: ssh_config accepts both whitespace and
        // a single `=` as separator. We only look at the first split
        // so values keep their inner spaces.
        let (key, value) = match split_kv(line) {
            Some(kv) => kv,
            None => continue,
        };

        match key.to_ascii_lowercase().as_str() {
            "host" => {
                // A `Host a b c` line declares multiple aliases in one
                // block — rare but legal. We keep only the first
                // non-wildcard alias; the rest get ignored the same way
                // `Host *` is ignored.
                if let Some(prev) = current.take() {
                    out.push(prev);
                }
                let first_concrete = value.split_whitespace().find(|a| !is_wildcard_alias(a));
                current = first_concrete.map(|a| HostEntry {
                    alias: a.to_string(),
                    hostname: None,
                    user: None,
                });
            }
            // `Match` / `Include` terminate any in-flight Host block
            // without declaring a new one. This matches how ssh itself
            // scopes subsequent settings.
            "match" | "include" => {
                if let Some(prev) = current.take() {
                    out.push(prev);
                }
            }
            "hostname" => {
                if let Some(h) = current.as_mut() {
                    h.hostname = Some(value.trim().to_string());
                }
            }
            "user" => {
                if let Some(h) = current.as_mut() {
                    h.user = Some(value.trim().to_string());
                }
            }
            // Anything else (Port, IdentityFile, ProxyCommand, …) doesn't
            // affect picker presentation; silently skip.
            _ => {}
        }
    }
    if let Some(prev) = current.take() {
        out.push(prev);
    }
    out
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    // ssh_config syntax: `Key Value` OR `Key=Value`. The whitespace
    // form allows arbitrary amounts, so we find the first stretch of
    // whitespace/eq and split there.
    let bytes = line.as_bytes();
    let sep_pos = bytes
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'=')?;
    let (k, rest) = line.split_at(sep_pos);
    let v = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '=');
    Some((k, v))
}

fn is_wildcard_alias(a: &str) -> bool {
    // `*`, `?`, `!` patterns are ssh config wildcards. A bare
    // `Host *` defaults block is the most common case; we also skip
    // negations. Exact-match aliases never contain these characters.
    a.contains('*') || a.contains('?') || a.starts_with('!')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_block() {
        let cfg = "Host prod\n  HostName db.example.com\n  User root\n";
        let hosts = parse_str(cfg);
        assert_eq!(
            hosts,
            vec![HostEntry {
                alias: "prod".to_string(),
                hostname: Some("db.example.com".to_string()),
                user: Some("root".to_string()),
            }]
        );
    }

    #[test]
    fn skips_wildcard_host_block() {
        let cfg = "Host *\n  ForwardAgent yes\n\nHost realone\n  HostName one\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "realone");
    }

    #[test]
    fn match_terminates_previous_block() {
        // `Match` mid-file closes the current Host block; settings after
        // it don't attach. We can't render `Match` blocks so the alias
        // "hidden" is dropped from the picker.
        let cfg = "Host aliased\n  HostName x\nMatch user root\n  HostName y\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "aliased");
        assert_eq!(hosts[0].hostname.as_deref(), Some("x"));
    }

    #[test]
    fn include_directive_does_not_produce_entry() {
        let cfg = "Include ~/.ssh/config.d/*\nHost real\n  HostName r\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real");
    }

    #[test]
    fn equals_separator_accepted() {
        let cfg = "Host eqsep\nHostName=x.example\nUser=eric\n";
        let hosts = parse_str(cfg);
        assert_eq!(
            hosts,
            vec![HostEntry {
                alias: "eqsep".to_string(),
                hostname: Some("x.example".to_string()),
                user: Some("eric".to_string()),
            }]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let cfg = "# top comment\n\nHost a\n  # inline\n  HostName h\n\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
    }

    #[test]
    fn multi_alias_host_line_keeps_first_concrete() {
        // `Host alpha beta *` is legal ssh syntax. The picker only ever
        // connects via one alias per row, so we keep the first non-
        // wildcard.
        let cfg = "Host alpha beta *\n  HostName h\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "alpha");
    }

    #[test]
    fn file_missing_returns_empty() {
        let missing = PathBuf::from("/nonexistent/nope/ssh/config");
        assert!(parse_file(&missing).unwrap().is_empty());
    }
}
