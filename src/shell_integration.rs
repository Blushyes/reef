//! Auto-SSH on terminal split.
//!
//! When reef connects via SSH, it emits OSC 7 pointing at an anchor
//! directory under `~/.reef/sessions/<pid>/`. Terminals that honour OSC 7
//! (Ghostty, iTerm2, Terminal.app, Alacritty, WezTerm, kitty, …) pass
//! that path as the CWD to every new split / tab spawned from the reef
//! pane. A one-time snippet in the user's shell rc detects the anchor,
//! verifies reef is still alive, and execs `ssh` into the same host —
//! reusing the ControlMaster socket for zero re-authentication.
//!
//! reef's side is: create the anchor dir + `ssh-info` file on connect,
//! emit OSC 7, restore OSC 7 + delete the dir on drop, and sweep stale
//! dirs on startup (for `kill -9` crash recovery).
//!
//! ## Files on disk
//!
//! ```text
//! ~/.reef/sessions/<pid>/ssh-info      # KEY=VALUE, sourced by shell snippet
//! ~/.reef/snippet-installed            # touched after `reef shell-integration` runs
//! ```
//!
//! The `ssh-info` file is intentionally minimal — just the three fields
//! the shell snippet needs. ssh resolves the rest of its config (
//! IdentityFile, Host alias, etc.) from the user's `~/.ssh/config` the
//! same way it would for a bare `ssh <host>`.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Shells we ship a split-auto-SSH snippet for. `Shell::rc_basename` +
/// `Shell::snippet` close the match-arms that used to live in main.rs
/// as raw string comparisons — a typo-free source of truth for
/// `reef shell-integration <zsh|bash|fish>` and the `$SHELL`
/// auto-detect fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub const ALL: &'static [Shell] = &[Shell::Zsh, Shell::Bash, Shell::Fish];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "zsh" => Some(Shell::Zsh),
            "bash" => Some(Shell::Bash),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
        }
    }

    /// Conventional rc path under `$HOME` — used in the first-connect
    /// toast so users see `~/.zshrc` / `~/.bashrc` / `~/.config/fish/config.fish`.
    pub fn rc_path_suffix(self) -> &'static str {
        match self {
            Shell::Zsh => ".zshrc",
            Shell::Bash => ".bashrc",
            Shell::Fish => ".config/fish/config.fish",
        }
    }

    /// The snippet body embedded in the reef binary. `reef shell-integration
    /// <shell>` prints this to stdout so the user can redirect it into
    /// their rc file.
    pub fn snippet(self) -> &'static str {
        match self {
            Shell::Zsh => ZSH_SNIPPET,
            Shell::Bash => BASH_SNIPPET,
            Shell::Fish => FISH_SNIPPET,
        }
    }
}

const ZSH_SNIPPET: &str = include_str!("../assets/shell-integration/zsh.sh");
const BASH_SNIPPET: &str = include_str!("../assets/shell-integration/bash.sh");
const FISH_SNIPPET: &str = include_str!("../assets/shell-integration/fish.fish");

/// `~/.reef/` — root for everything reef keeps on the client. Returns
/// `None` when `$HOME` isn't set (Windows native without the env var is
/// the typical case); callers silently skip the feature in that
/// situation. Single source of truth for `.reef`-relative paths so
/// `sessions_root()` and `snippet_installed_marker()` don't drift.
fn reef_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".reef"))
}

/// `~/.reef/sessions/` — parent of per-pid anchor dirs.
pub fn sessions_root() -> Option<PathBuf> {
    Some(reef_home()?.join("sessions"))
}

/// `~/.reef/snippet-installed` — touched by `reef shell-integration …`
/// so the first-connect toast stops nagging users who already installed.
pub fn snippet_installed_marker() -> Option<PathBuf> {
    Some(reef_home()?.join("snippet-installed"))
}

/// Scan `ControlPath=<value>` out of the `-o Key=Value` arg block built
/// by `SshSession::for_host`. `%h`/`%p`/`%r` tokens in the value are
/// left intact — ssh expands them against the target host at connect
/// time, so the shell snippet's ssh invocation will hit the same
/// master socket reef is already using.
pub fn extract_control_path(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|a| a.strip_prefix("ControlPath=").map(str::to_string))
}

/// Percent-encode a path for the OSC 7 `file://` URL. RFC 3986 unreserved
/// set plus `/` for path segments; everything else gets `%XX`. Keeping
/// this dependency-free (we already avoid `url`/`percent-encoding` crates
/// elsewhere).
fn url_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// The OSC 7 byte sequence for a given cwd. Used by `emit_osc7` at
/// runtime and by unit tests for a byte-for-byte assertion.
pub fn osc7_sequence(path: &Path) -> String {
    format!("\x1b]7;file://localhost{}\x07", url_encode_path(path))
}

/// Write the OSC 7 sequence to a writer, flushing afterwards. Kept
/// generic over `Write` for tests; `emit_osc7_stdout` is the
/// real-process wrapper.
pub fn write_osc7<W: Write>(w: &mut W, path: &Path) -> io::Result<()> {
    w.write_all(osc7_sequence(path).as_bytes())?;
    w.flush()
}

/// Emit OSC 7 to real stdout. Called twice per SSH session: once on
/// connect (anchor dir) and once on disconnect (restore original cwd).
///
/// No-op when stdout isn't a TTY — unit tests, pipes, and redirects
/// don't want these bytes in their capture buffer, and real terminals
/// that don't honour OSC 7 just ignore unknown escape sequences
/// harmlessly, so the `is_terminal` check costs us nothing in the
/// production path. Other errors swallowed: losing this signal is not
/// worth crashing over.
pub fn emit_osc7_stdout(path: &Path) {
    let mut out = io::stdout();
    if !out.is_terminal() {
        return;
    }
    let _ = write_osc7(&mut out, path);
}

/// The three fields a shell snippet needs to exec ssh back into the
/// same remote reef is talking to. Grouped so `Session::install` takes
/// one borrowed struct instead of three positional strings — readable
/// at the call site and easier to extend (ssh port, forwarded env …)
/// without touching every caller.
pub struct SshInfo<'a> {
    pub host: &'a str,
    pub workdir: &'a str,
    pub control_path: &'a str,
}

/// Serialize the ssh-info file body as plain `KEY=VALUE\n` lines — no
/// quoting, no escaping. Each shell snippet reads the file with a
/// "split on first `=`" loop and assigns via its builtin (`typeset`
/// for bash/zsh, `set -gx` for fish), not via `source`. This sidesteps
/// per-shell escape rules entirely: values may contain spaces,
/// apostrophes, dollar signs, backslashes — all treated as literal
/// bytes since no shell expansion runs on the value.
///
/// The only inputs we reject are values containing `\n` (would split
/// a line) or `\0` (POSIX paths can't contain it anyway); in practice
/// host names, remote workdirs, and ControlPath strings never contain
/// either. An illegal char would return `Err` rather than silently
/// corrupting the file.
pub fn ssh_info_body(info: &SshInfo<'_>) -> io::Result<String> {
    for (label, v) in [
        ("REEF_HOST", info.host),
        ("REEF_WORKDIR", info.workdir),
        ("REEF_CONTROL_PATH", info.control_path),
    ] {
        if v.contains('\n') || v.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} contains NUL or newline; refusing to write ssh-info"),
            ));
        }
    }
    Ok(format!(
        "REEF_HOST={}\nREEF_WORKDIR={}\nREEF_CONTROL_PATH={}\n",
        info.host, info.workdir, info.control_path
    ))
}

/// RAII wrapper: creates `~/.reef/sessions/<pid>/` + ssh-info + emits
/// OSC 7 pointing there. Drop removes the dir and emits OSC 7 back to
/// the original cwd (so the user's terminal pane remembers where it
/// came from once reef exits).
///
/// If HOME isn't set or any fs op fails at `install` time, returns an
/// error — the caller (RemoteBackend) currently swallows it and
/// logs so the SSH connect still succeeds.
pub struct Session {
    dir: PathBuf,
    original_cwd: Option<PathBuf>,
}

impl Session {
    /// Install under a caller-provided sessions root — used by tests so
    /// they can point at a tempdir without mutating `$HOME` globally.
    pub fn install_in(root: &Path, info: &SshInfo<'_>) -> io::Result<Self> {
        let dir = root.join(std::process::id().to_string());
        // `create_dir_all` creates intermediates, so this single call
        // also materialises `root` itself if it didn't exist — no need
        // for a second `create_dir_all(&root)` up the call stack.
        fs::create_dir_all(&dir)?;
        let info_path = dir.join("ssh-info");
        fs::write(&info_path, ssh_info_body(info)?)?;
        // Tighten perms to 0o600 on Unix: the file reveals the target
        // host + remote workdir + ControlPath to anyone who can read
        // $HOME. The ControlPath socket is itself 0o600 by ssh so
        // there's no session-hijack risk, but the info-disclosure is
        // still worth closing. No-op on Windows (no notion of POSIX
        // perms from std).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = fs::set_permissions(&info_path, perms);
        }

        let original_cwd = std::env::current_dir().ok();
        emit_osc7_stdout(&dir);
        Ok(Self { dir, original_cwd })
    }

    /// Production entry point. `None` when HOME isn't available — caller
    /// treats that as "feature disabled for this session".
    pub fn install(info: &SshInfo<'_>) -> Option<io::Result<Self>> {
        Some(Self::install_in(&sessions_root()?, info))
    }

    /// Test-only accessor for the anchor dir path.
    #[doc(hidden)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cwd) = &self.original_cwd {
            emit_osc7_stdout(cwd);
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Scan `~/.reef/sessions/` and remove any `<pid>/` dir whose pid is no
/// longer alive. Called from main.rs startup to clean up after `kill -9`
/// or a panic-through-drop. Best-effort: errors ignored, silent.
pub fn sweep_stale_sessions() {
    let Some(root) = sessions_root() else {
        return;
    };
    sweep_stale_sessions_in(&root);
}

pub fn sweep_stale_sessions_in(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if !is_pid_alive(pid) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Cheap cross-process liveness check. We shell out to `ps -p <pid>`
/// rather than link `libc`: this only runs during the startup sweep (a
/// few stale dirs at most), so the subprocess cost is fine and we keep
/// the dep surface small.
///
/// `ps -p` is preferred over `kill -0` because the latter returns
/// non-zero for *both* "no such process" (good) and "operation not
/// permitted" (bad — a neighboring user's live reef would get its dir
/// swept). `ps` reports existence regardless of ownership. On
/// unexpected errors (missing `ps`, weird fs state) we conservatively
/// assume the pid is alive and leave the dir intact.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn osc7_sequence_is_correct_bytes() {
        let path = Path::new("/Users/pan/.reef/sessions/12345");
        let s = osc7_sequence(path);
        assert_eq!(
            s,
            "\x1b]7;file://localhost/Users/pan/.reef/sessions/12345\x07"
        );
    }

    #[test]
    fn osc7_encodes_special_chars() {
        let path = Path::new("/tmp/a b");
        let s = osc7_sequence(path);
        assert_eq!(s, "\x1b]7;file://localhost/tmp/a%20b\x07");
    }

    fn sample_info<'a>(host: &'a str, workdir: &'a str, cp: &'a str) -> SshInfo<'a> {
        SshInfo {
            host,
            workdir,
            control_path: cp,
        }
    }

    #[test]
    fn ssh_info_is_plain_key_value() {
        let body =
            ssh_info_body(&sample_info("root@host", "/srv/app", "/tmp/cm-%h-%p-%r")).unwrap();
        assert_eq!(
            body,
            "REEF_HOST=root@host\n\
             REEF_WORKDIR=/srv/app\n\
             REEF_CONTROL_PATH=/tmp/cm-%h-%p-%r\n"
        );
    }

    #[test]
    fn ssh_info_preserves_special_chars_literally() {
        // Apostrophe, space, dollar sign — no escaping applied; shells
        // don't run expansion on the split-on-= value, so these land
        // as-is in the exported variable.
        let body = ssh_info_body(&sample_info("u@h", "/w/o'n with $space", "cp")).unwrap();
        assert!(body.contains("REEF_WORKDIR=/w/o'n with $space\n"));
    }

    #[test]
    fn ssh_info_rejects_newline_in_value() {
        let err = ssh_info_body(&sample_info("u@h", "/w/o\nsneaky", "cp")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn shell_enum_roundtrip() {
        for s in Shell::ALL {
            assert_eq!(Shell::parse(s.name()), Some(*s));
            assert!(!s.snippet().is_empty());
        }
        assert_eq!(Shell::parse("nushell"), None);
    }

    #[test]
    fn control_path_extracted_from_ssh_args() {
        let args: Vec<String> = [
            "-o",
            "ControlMaster=auto",
            "-o",
            "ControlPath=/tmp/cm-foo",
            "-o",
            "ControlPersist=10m",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(extract_control_path(&args).as_deref(), Some("/tmp/cm-foo"));
    }

    #[test]
    fn control_path_missing_returns_none() {
        let args: Vec<String> = ["-o", "ControlMaster=auto"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(extract_control_path(&args), None);
    }

    #[test]
    fn session_install_creates_dir_with_ssh_info() {
        let tmp = tempfile::tempdir().unwrap();
        let session =
            Session::install_in(tmp.path(), &sample_info("root@host", "/srv/app", "/tmp/cm"))
                .unwrap();
        let info = session.dir().join("ssh-info");
        assert!(info.exists(), "ssh-info should exist at {info:?}");
        let content = std::fs::read_to_string(&info).unwrap();
        assert!(content.contains("REEF_HOST=root@host"));
        assert!(content.contains("REEF_WORKDIR=/srv/app"));
        assert!(content.contains("REEF_CONTROL_PATH=/tmp/cm"));
    }

    #[cfg(unix)]
    #[test]
    fn session_install_tightens_ssh_info_perms_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::install_in(tmp.path(), &sample_info("u@h", "/w", "cp")).unwrap();
        let info = session.dir().join("ssh-info");
        let mode = std::fs::metadata(&info).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "ssh-info should be 0o600, got 0o{mode:o}");
    }

    #[test]
    fn session_drop_removes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_path;
        {
            let session = Session::install_in(tmp.path(), &sample_info("u@h", "/w", "cp")).unwrap();
            dir_path = session.dir().to_path_buf();
            assert!(dir_path.exists());
        }
        // Dropped — anchor dir should be gone.
        assert!(
            !dir_path.exists(),
            "dir {dir_path:?} should be removed on drop"
        );
    }

    #[test]
    fn sweep_removes_stale_pid_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Pid 1 is always alive (init). Pick a very high pid that almost
        // certainly isn't — we'll assert it gets removed but pid 1's
        // dir stays.
        let live = root.join("1");
        let stale = root.join("4294967295"); // u32::MAX, definitely not alive
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&stale).unwrap();
        sweep_stale_sessions_in(root);
        assert!(live.exists(), "pid 1 dir should survive sweep");
        assert!(!stale.exists(), "stale pid dir should be removed");
    }

    #[test]
    fn sweep_ignores_non_numeric_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let unrelated = root.join("not-a-pid");
        std::fs::create_dir_all(&unrelated).unwrap();
        sweep_stale_sessions_in(root);
        assert!(unrelated.exists(), "non-numeric dir should be left alone");
    }
}
