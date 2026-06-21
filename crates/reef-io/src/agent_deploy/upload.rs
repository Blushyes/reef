//! Upload fallback — when the in-place install script can't reach the
//! GitHub Release download URL, we stream the bytes of the locally
//! embedded agent up the ssh pipe.
//!
//! Design mirrors VSCode Remote's "copy server via scp" fallback, but
//! cheaper: `cat > tmp && chmod +x && mv` atomically, and the payload is
//! `include_bytes!`'d at reef's compile time so the user's machine
//! already has it. See `build.rs` for the embedding side.
//!
//! This path is only viable when the reef host arch matches the target
//! arch. Cross-arch deploy requires GitHub Release (the primary path).

use std::io::Write;
use std::process::{Command, Stdio};

use super::embedded::{EMBEDDED_AGENT_ARCH, EMBEDDED_AGENT_BYTES, EMBEDDED_AGENT_PLATFORM};
use super::script::RemoteOs;
use super::ssh::{SshSession, shell_escape};

/// Stream `EMBEDDED_AGENT_BYTES` to `remote_path` on `host`, via the
/// given SSH session (shares the session's ControlMaster so no extra
/// auth prompt). POSIX remotes get `cat > tmp && chmod +x && mv`;
/// Windows remotes get a PowerShell one-liner that decodes a
/// base64-stdin blob into `WriteAllBytes`.
///
/// `target_platform` / `target_arch` come from the install script's
/// `platform` / `arch` report; if they don't match the embedded binary
/// we fail fast with `UploadError::ArchMismatch`.
pub fn upload_agent(
    session: &SshSession,
    remote_path: &str,
    target_platform: &str,
    target_arch: &str,
    remote_os: RemoteOs,
) -> Result<(), UploadError> {
    if EMBEDDED_AGENT_BYTES.is_empty() {
        return Err(UploadError::NoEmbeddedBinary);
    }
    if EMBEDDED_AGENT_PLATFORM != target_platform || EMBEDDED_AGENT_ARCH != target_arch {
        return Err(UploadError::ArchMismatch {
            embedded: format!("{EMBEDDED_AGENT_PLATFORM}-{EMBEDDED_AGENT_ARCH}"),
            target: format!("{target_platform}-{target_arch}"),
        });
    }

    match remote_os {
        RemoteOs::Posix => upload_agent_posix(session, remote_path),
        RemoteOs::Windows => upload_agent_windows(session, remote_path),
    }
}

fn upload_agent_posix(session: &SshSession, remote_path: &str) -> Result<(), UploadError> {
    let tmp_path = format!("{remote_path}.uploading");
    let parent = parent_dir(remote_path);
    let tmp_escaped = shell_escape(&tmp_path);
    let final_escaped = shell_escape(remote_path);
    let parent_escaped = shell_escape(&parent);

    // One shell pipeline: mkdir + cat into tmp, chmod, mv. `set -e` so a
    // failure anywhere in the chain propagates as a non-zero exit.
    let remote_cmd = format!(
        "set -e; mkdir -p {parent_escaped}; \
         cat > {tmp_escaped}; \
         chmod +x {tmp_escaped}; \
         mv -f {tmp_escaped} {final_escaped}"
    );

    let mut cmd = Command::new("ssh");
    cmd.args(session.ssh_args())
        .arg(session.host())
        .arg(&remote_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(UploadError::Spawn)?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| UploadError::Other("ssh stdin closed".into()))?;
        stdin
            .write_all(EMBEDDED_AGENT_BYTES)
            .map_err(UploadError::Spawn)?;
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().map_err(UploadError::Spawn)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(UploadError::RemoteFailed {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }
    Ok(())
}

/// Windows upload: embed the agent bytes as a base64 literal in a short
/// PowerShell body — `cat > file` doesn't exist on Windows and piping raw
/// binary through ssh stdin runs into CRLF conversion inside cmd.exe.
/// Base64 is ~33% larger but dodges both pitfalls and keeps the
/// transport path identical (ssh + session args) to the POSIX case.
fn upload_agent_windows(session: &SshSession, remote_path: &str) -> Result<(), UploadError> {
    // PowerShell's `[Convert]::FromBase64String` is tolerant of embedded
    // whitespace, so we can wrap the long line for shell reliability.
    let encoded = base64_encode(EMBEDDED_AGENT_BYTES);
    let parent = windows_parent_dir(remote_path);
    // We cannot reliably shell-escape arbitrary Windows paths through
    // ssh + cmd + powershell; the conservative thing is to pass the
    // whole PS program on stdin via `powershell -Command -` just like
    // the install script does, with the path values interpolated into
    // PS string literals (so the agent_path from the script report
    // round-trips verbatim).
    let ps = format!(
        r###"$ErrorActionPreference = 'Stop'
$target = '{remote_path}'
$parent = '{parent}'
if ($parent -and -not (Test-Path $parent)) {{
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
}}
$tmp = "$target.uploading"
$bytes = [Convert]::FromBase64String('{encoded}')
[IO.File]::WriteAllBytes($tmp, $bytes)
Move-Item -Force $tmp $target
"###
    );

    let mut cmd = Command::new("ssh");
    cmd.args(session.ssh_args())
        .arg(session.host())
        .arg("powershell -NoProfile -NonInteractive -Command -")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(UploadError::Spawn)?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| UploadError::Other("ssh stdin closed".into()))?;
        stdin.write_all(ps.as_bytes()).map_err(UploadError::Spawn)?;
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().map_err(UploadError::Spawn)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(UploadError::RemoteFailed {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }
    Ok(())
}

fn windows_parent_dir(path: &str) -> String {
    // Windows paths in our context come from the install script's
    // `agentPath` output, which uses `\` (PowerShell's `Join-Path`
    // default). Handle `/` too defensively.
    path.rsplit_once(['\\', '/'])
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

/// Tiny standard-alphabet base64 encoder — sidesteps adding a whole
/// dep for the one place we need to serialise the embedded binary.
/// The reef-proto crate has a near-identical one for bytes DTOs; we
/// don't share because that's a different crate and the function is
/// ~40 lines.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHA[(n & 0x3F) as usize] as char);
        i += 3;
    }
    match input.len() - i {
        2 => {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        1 => {
            let n = (input[i] as u32) << 16;
            out.push(ALPHA[((n >> 18) & 0x3F) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        _ => {}
    }
    out
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.to_string(),
        _ => ".".to_string(),
    }
}

#[derive(Debug)]
pub enum UploadError {
    NoEmbeddedBinary,
    ArchMismatch { embedded: String, target: String },
    Spawn(std::io::Error),
    RemoteFailed { status: i32, stderr: String },
    Other(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEmbeddedBinary => f.write_str(
                "no embedded reef-agent in this build; \
                 rebuild with `cargo build -p reef-agent` first",
            ),
            Self::ArchMismatch { embedded, target } => write!(
                f,
                "embedded agent is {embedded} but remote needs {target} — \
                 cross-arch upload not supported"
            ),
            Self::Spawn(e) => write!(f, "failed to spawn ssh: {e}"),
            Self::RemoteFailed { status, stderr } => {
                write!(f, "remote cat/mv failed (status {status}): {stderr}")
            }
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for UploadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_handles_absolute_path() {
        assert_eq!(parent_dir("/a/b/c"), "/a/b");
        assert_eq!(parent_dir("/etc/hosts"), "/etc");
    }

    #[test]
    fn parent_dir_handles_relative_single_component() {
        assert_eq!(parent_dir("foo"), ".");
    }

    #[test]
    fn parent_dir_handles_root() {
        // `/foo` → parent is `/` but we want `.` or `/` ? Shell's mkdir -p
        // treats empty prefix as cwd, so we collapse to `.`. This is
        // intentional.
        assert_eq!(parent_dir("/foo"), ".");
    }

    // ── Windows upload path (Track E) ────────────────────────────────────

    #[test]
    fn windows_parent_dir_handles_backslash() {
        assert_eq!(
            windows_parent_dir(r"C:\Users\me\.reef\agent\0.14.0\reef-agent.exe"),
            r"C:\Users\me\.reef\agent\0.14.0"
        );
    }

    #[test]
    fn windows_parent_dir_handles_forward_slash() {
        // Agent-emitted paths may use either; both should round-trip.
        assert_eq!(
            windows_parent_dir("C:/Users/me/.reef/agent/0.14.0/reef-agent.exe"),
            "C:/Users/me/.reef/agent/0.14.0"
        );
    }

    #[test]
    fn windows_parent_dir_handles_drive_root() {
        assert_eq!(windows_parent_dir(r"C:\foo.exe"), "C:");
    }

    #[test]
    fn base64_encode_empty_input() {
        assert_eq!(base64_encode(&[]), "");
    }

    #[test]
    fn base64_encode_standard_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_binary_roundtrip_via_ps_decoder_contract() {
        // We don't run PowerShell here, but assert the alphabet/padding
        // match what `[Convert]::FromBase64String` accepts: standard
        // alphabet (A-Za-z0-9+/) plus `=` padding.
        let bytes: Vec<u8> = (0..128u8).collect();
        let enc = base64_encode(&bytes);
        assert!(
            enc.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "base64 alphabet violated: {enc}"
        );
        // Padded length is 4 * ceil(len/3).
        assert_eq!(enc.len(), bytes.len().div_ceil(3) * 4);
    }
}
