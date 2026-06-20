//! Minimal `~/.ssh/config` parser for the Hosts picker.
//!
//! This intentionally parses only the fields Reef renders. Unsupported
//! directives (`Match`, `Include`, wildcards like `Host *`) are skipped.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
}

pub fn parse_ssh_config() -> std::io::Result<Vec<HostEntry>> {
    let path = default_config_path();
    parse_file(&path)
}

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

pub fn parse_str(body: &str) -> Vec<HostEntry> {
    let mut out: Vec<HostEntry> = Vec::new();
    let mut current: Option<HostEntry> = None;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match split_kv(line) {
            Some(kv) => kv,
            None => continue,
        };

        match key.to_ascii_lowercase().as_str() {
            "host" => {
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
            _ => {}
        }
    }
    if let Some(prev) = current.take() {
        out.push(prev);
    }
    out
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let sep_pos = bytes
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'=')?;
    let (k, rest) = line.split_at(sep_pos);
    let v = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '=');
    Some((k, v))
}

fn is_wildcard_alias(a: &str) -> bool {
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
        let cfg = "Host alpha beta *\n  HostName h\n";
        let hosts = parse_str(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "alpha");
    }

    #[test]
    fn file_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = parse_file(&dir.path().join("missing")).unwrap();
        assert!(hosts.is_empty());
    }
}
