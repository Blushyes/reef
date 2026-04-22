//! SSH session plumbing — builds the minimal option list every `ssh`
//! invocation shares.
//!
//! We rely on the system `ssh` binary rather than the `openssh` /
//! `russh` Rust crates. Reasons:
//!   1. `~/.ssh/config`, `known_hosts`, ssh-agent, IdentityAgent,
//!      1Password/YubiKey plugins all just work — replacing them is a
//!      week of effort and another week of bug reports.
//!   2. ControlMaster is an ssh-binary feature; reimplementing it in
//!      process isn't meaningful.
//!   3. reef is a developer tool; every user already has `ssh` installed.
//!
//! Deliberate non-goal: overriding the user's ControlMaster / ControlPath.
//! Users very commonly already configure ControlMaster in `~/.ssh/config`
//! with a shared ControlPath (e.g. `/tmp/control-%h-%p-%r`). Forcing reef
//! onto its own path broke authentication in the common case where the
//! existing socket was the only thing that got past a non-default
//! IdentityFile — see issue history for `Host hongxuan` + IdentityAgent.

use std::path::PathBuf;

/// An ssh session context — carries the control path and the host
/// identifier so we can reuse both the ControlMaster socket and the
/// same argument vector across install / upload / agent-run stages.
#[derive(Debug, Clone)]
pub struct SshSession {
    host: String,
    control_dir: PathBuf,
    /// Resolved `ssh` arg list (without `<host>` and without the remote
    /// command). Precomputed so each stage can call `.ssh_args()` in
    /// a hot loop without re-touching the filesystem.
    args: Vec<String>,
}

impl SshSession {
    /// Build a session targeting `host`. Strategy for ControlMaster /
    /// ControlPath:
    ///
    /// 1. Ask ssh itself (`ssh -G <host>`) what ControlPath and
    ///    ControlMaster it would resolve from the user's config.
    /// 2. If the user already declared `ControlPath` (e.g.
    ///    `/tmp/control-%h-%p-%r` inside `Host *`), reuse it and pass
    ///    `ControlMaster=auto` explicitly — this guarantees reef's ssh
    ///    subprocess attaches any existing master socket from the user's
    ///    own `ssh hongxuan` etc. session.
    /// 3. If the user hasn't set one, fall back to reef's private
    ///    `<control_dir>/cm-%h-%p-%r` so at least our own ssh invocations
    ///    (install probe + agent spawn) share one master.
    ///
    /// Why this matters in practice: users very commonly configure a
    /// Host alias with a non-default `IdentityFile` or `IdentityAgent`.
    /// Their `ssh root@1.2.3.4` succeeds only because it attaches an
    /// already-authenticated master socket opened by `ssh hongxuan`. If
    /// reef forces its own ControlPath, ssh opens a fresh TCP connection
    /// that (a) must re-authenticate from scratch (which fails when the
    /// alias-only IdentityFile isn't in the default search path) and
    /// (b) can get intercepted by transparent proxies (Clash TUN, etc.)
    /// that the established socket already bypassed.
    pub fn for_host(host: &str) -> std::io::Result<Self> {
        let control_dir = resolve_control_dir()?;
        std::fs::create_dir_all(&control_dir)?;

        let resolved = probe_user_control_path(host);

        let control_path = match resolved {
            Some(ref cp) => cp.clone(),
            None => control_dir
                .join("cm-%h-%p-%r")
                .to_string_lossy()
                .into_owned(),
        };

        let args = vec![
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            format!("ControlPath={control_path}"),
            "-o".to_string(),
            "ControlPersist=10m".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=30".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            "-o".to_string(),
            "BatchMode=no".to_string(),
        ];

        Ok(Self {
            host: host.to_string(),
            control_dir,
            args,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn control_dir(&self) -> &std::path::Path {
        &self.control_dir
    }

    /// Shared `-o …` argument block. Does not include the host or
    /// remote command; those are appended per invocation.
    pub fn ssh_args(&self) -> &[String] {
        &self.args
    }
}

/// Minimal POSIX-shell single-quote escape. The only character we need
/// to worry about is `'` itself — everything else is literal inside
/// single quotes. The `'\''` trick ends the quote, inserts an escaped
/// quote, and reopens. Works in bash, dash, ash, and zsh.
pub fn shell_escape(raw: &str) -> String {
    if !raw.is_empty()
        && raw.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | ':' | '@' | '+')
        })
    {
        // Safe-looking argv; skip the quoting for readability in logs.
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('\'');
    for c in raw.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// PowerShell single-quote escape. Inside single-quoted PS strings the
/// only special sequence is `''` (escaped `'`). Used when composing
/// `Set-Location 'path'` and `& $editor 'path'` on Windows remotes.
pub fn powershell_escape(raw: &str) -> String {
    raw.replace('\'', "''")
}

/// Run `ssh -G <host>` and pull out the `controlpath` line. `ssh -G`
/// prints the fully-resolved config (applying `Host` matches and
/// defaults) without making a network connection, so it's safe even
/// when transparent proxies would interfere with a real connect.
///
/// Returns `None` when the user hasn't configured a ControlPath, when
/// ssh reports the magic value `none`, or when anything goes wrong
/// (missing `ssh` binary, parse failure). The caller falls back to
/// reef's own control directory.
fn probe_user_control_path(host: &str) -> Option<String> {
    let output = std::process::Command::new("ssh")
        .arg("-G")
        .arg(host)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("controlpath ") {
            let trimmed = rest.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
                return None;
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

fn resolve_control_dir() -> std::io::Result<PathBuf> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(rt).join("reef"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home).join(".reef").join("cm"));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "neither XDG_RUNTIME_DIR nor HOME is set; cannot place ssh ControlMaster socket",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_passthrough_for_simple() {
        assert_eq!(shell_escape("alpha"), "alpha");
        assert_eq!(shell_escape("/var/tmp/reef-agent"), "/var/tmp/reef-agent");
        assert_eq!(shell_escape("0.6.0"), "0.6.0");
    }

    #[test]
    fn shell_escape_quotes_whitespace() {
        assert_eq!(shell_escape("has space"), "'has space'");
    }

    #[test]
    fn shell_escape_escapes_embedded_single_quote() {
        assert_eq!(shell_escape("o'brien"), "'o'\\''brien'");
    }

    #[test]
    fn shell_escape_handles_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn ssh_args_include_controlmaster_with_resolved_path() {
        // Point HOME at a tempdir so we don't pollute the dev's $HOME.
        // We use a host with no config match, so `ssh -G` returns the
        // defaults — controlpath=none — and reef falls back to its own
        // path rooted at `control_dir()`.
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: tests are serial in this crate; we don't parallelise
        // env mutations elsewhere in this file.
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::set_var("HOME", tmp.path());
        }
        let session = SshSession::for_host("reef-test-host-that-should-not-match.invalid").unwrap();
        let args = session.ssh_args().join(" ");
        assert!(args.contains("ControlMaster=auto"), "{args}");
        assert!(args.contains("ControlPath="), "{args}");
        assert!(args.contains("ControlPersist=10m"), "{args}");
        assert!(args.contains("ServerAliveInterval=30"), "{args}");
        assert!(args.contains("BatchMode=no"), "{args}");
        assert!(session.control_dir().exists());
    }
}
