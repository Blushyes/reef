//! Client-side remote agent deployment.
//!
//! Two-stage rollout modelled on VSCode Remote SSH:
//!   1. Generate a bash install script (see `script.rs`) and feed it to
//!      `ssh <host> 'bash -c "<script>"'`. The script does platform
//!      detection, idempotency check, and attempts a GitHub Release
//!      download.
//!   2. If the script reports `installState=download_failed` (no
//!      internet on the remote, wrong release, etc.) fall back to
//!      streaming the embedded binary over the same ssh session via
//!      `cat > ... && mv`.
//!
//! The public entry point is [`ensure_agent`]; everything else is
//! implementation detail.

use std::process::{Command, Stdio};

pub mod embedded;
pub mod script;
pub mod ssh;
pub mod upload;

pub use script::{InstallState, RemoteOs, ScriptReport};
pub use ssh::SshSession;

/// Where the agent lives on the remote host, plus the detected
/// platform/arch tuple.
#[derive(Debug, Clone)]
pub struct AgentLocation {
    pub host: String,
    pub remote_path: String,
    pub platform: String,
    pub arch: String,
    pub via: InstallPath,
    /// Remote OS family, used by `RemoteBackend::editor_launch_spec` /
    /// `upload::upload_agent` / other call sites that need to pick
    /// between POSIX and Windows shell code paths.
    pub remote_os: RemoteOs,
}

/// Which code path produced the final agent binary on the remote.
/// Useful for toast messaging + diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPath {
    /// Binary was already present before we connected.
    AlreadyInstalled,
    /// Install script downloaded it from the release URL.
    Downloaded,
    /// Primary download failed; we uploaded the embedded bytes.
    Uploaded,
}

/// Default URL template for the GitHub Releases download. Substitutes
/// `{version}`, `{platform}`, `{arch}` at script-execution time on the
/// remote host.
pub const DEFAULT_DOWNLOAD_URL_TEMPLATE: &str = "https://github.com/reef-tui/reef/releases/download/v{version}/reef-agent-{platform}-{arch}.tar.gz";

/// Convenience wrapper: connect to `host`, ensure `reef-agent` of the
/// current `reef` version is installed, return where it lives.
pub fn ensure_agent(host: &str) -> Result<AgentLocation, DeployError> {
    let session = SshSession::for_host(host).map_err(DeployError::SessionInit)?;
    ensure_agent_with_session(
        &session,
        env!("CARGO_PKG_VERSION"),
        DEFAULT_DOWNLOAD_URL_TEMPLATE,
    )
}

/// Lower-level variant exposed for tests and for callers that want to
/// override the version / download URL (e.g. pointing at a staging
/// mirror).
pub fn ensure_agent_with_session(
    session: &SshSession,
    version: &str,
    download_url_template: &str,
) -> Result<AgentLocation, DeployError> {
    // Probe the remote OS before committing to a script flavour. `uname
    // -s` works on Linux/Darwin/*BSD; Windows' native `cmd.exe` doesn't
    // know `uname` and falls through to `ver` which emits something like
    // `Microsoft Windows [Version 10.0.xxxxx]`. If both fail we default
    // to POSIX — the bash script then reports `platform=unknown` and
    // bubbles up as a clean error.
    let remote_os = probe_remote_os(session).unwrap_or(RemoteOs::Posix);

    let script_id = script::new_script_id();
    // `PROTOCOL_VERSION` tracks the reef-proto wire contract; bumped in
    // lockstep with DTO changes. The install script uses this to evict
    // stale agents (e.g. an agent from the previous minor that spoke
    // v2 when the client now speaks v3).
    let expected_proto = reef_proto::PROTOCOL_VERSION.to_string();

    let script_body = match remote_os {
        RemoteOs::Posix => script::generate_install_script(
            version,
            "$HOME/.reef",
            &script_id,
            download_url_template,
            &expected_proto,
        ),
        RemoteOs::Windows => script::generate_install_script_powershell(
            version,
            r"$env:USERPROFILE\.reef",
            &script_id,
            download_url_template,
            &expected_proto,
        ),
    };

    let report = run_install_script(session, &script_body, &script_id, remote_os)?;

    // Fatal: platform/arch couldn't be detected.
    if matches!(report.install_state, Some(InstallState::Unsupported)) {
        return Err(DeployError::UnsupportedPlatform {
            platform: report.platform.unwrap_or_else(|| "unknown".into()),
            arch: report.arch.unwrap_or_else(|| "unknown".into()),
        });
    }

    // Fatal: script ran, but we didn't get the mandatory fields back.
    let agent_path = report
        .agent_path
        .clone()
        .ok_or(DeployError::MissingField("agentPath"))?;
    let platform = report
        .platform
        .clone()
        .ok_or(DeployError::MissingField("platform"))?;
    let arch = report
        .arch
        .clone()
        .ok_or(DeployError::MissingField("arch"))?;

    match report.install_state {
        Some(InstallState::Existed) => Ok(AgentLocation {
            host: session.host().to_string(),
            remote_path: agent_path,
            platform,
            arch,
            via: InstallPath::AlreadyInstalled,
            remote_os,
        }),
        Some(InstallState::Downloaded) => Ok(AgentLocation {
            host: session.host().to_string(),
            remote_path: agent_path,
            platform,
            arch,
            via: InstallPath::Downloaded,
            remote_os,
        }),
        Some(InstallState::DownloadFailed) | Some(InstallState::ExtractFailed) => {
            // Fall back to upload. Re-use the same ssh session so
            // ControlMaster makes this second command cheap.
            upload::upload_agent(session, &agent_path, &platform, &arch, remote_os)
                .map_err(DeployError::Upload)?;
            Ok(AgentLocation {
                host: session.host().to_string(),
                remote_path: agent_path,
                platform,
                arch,
                via: InstallPath::Uploaded,
                remote_os,
            })
        }
        Some(InstallState::Unsupported) | None => Err(DeployError::ScriptFailed {
            exit_code: report.exit_code,
            raw: report.raw,
        }),
    }
}

/// Probe the remote OS family. Runs a tiny `uname -s || ver` command
/// over ssh; anything starting with `Microsoft` / `Windows` in the
/// output is interpreted as Windows. Failures (ssh down, odd shell)
/// return `None` so the caller can pick a default — usually POSIX,
/// which lets the bash script emit `platform=unknown` and bubble a
/// clean error.
fn probe_remote_os(session: &SshSession) -> Option<RemoteOs> {
    let output = Command::new("ssh")
        .args(session.ssh_args())
        .arg(session.host())
        // `uname -s` wins on POSIX (returns "Linux" / "Darwin" / …); on
        // Windows `cmd.exe` treats it as an unknown command and we fall
        // through to `ver` which prints "Microsoft Windows [Version …]".
        // The `2>&1` collapses stderr into stdout so the pattern match
        // below sees everything.
        .arg("uname -s 2>/dev/null || ver")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("Windows") || stdout.contains("Microsoft") {
        Some(RemoteOs::Windows)
    } else if !stdout.trim().is_empty() {
        Some(RemoteOs::Posix)
    } else {
        None
    }
}

/// Run the install script via `ssh <host> '<shell> -c "<script>"'`.
/// Captures stdout and delegates the parse to `script::parse_script_output`.
fn run_install_script(
    session: &SshSession,
    script_body: &str,
    script_id: &str,
    remote_os: RemoteOs,
) -> Result<ScriptReport, DeployError> {
    // We pass the script as a single argument to the remote shell.
    // POSIX: `bash -c '…'`. Windows: `powershell -NoProfile
    // -NonInteractive -Command -` and the script is fed on stdin so we
    // sidestep cmd.exe's interactive quoting rules.
    let (remote_cmd, stdin_mode) = match remote_os {
        RemoteOs::Posix => {
            let escaped = ssh::shell_escape(script_body);
            (format!("bash -c {escaped}"), Stdio::null())
        }
        RemoteOs::Windows => (
            "powershell -NoProfile -NonInteractive -Command -".to_string(),
            Stdio::piped(),
        ),
    };

    let mut child = Command::new("ssh")
        .args(session.ssh_args())
        .arg(session.host())
        .arg(&remote_cmd)
        .stdin(stdin_mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(DeployError::Spawn)?;

    if let RemoteOs::Windows = remote_os {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            // Feed the PowerShell body verbatim. PS reads until EOF on
            // `-Command -` and then executes.
            let _ = stdin.write_all(script_body.as_bytes());
        }
    }
    let output = child.wait_with_output().map_err(DeployError::Spawn)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && stdout.is_empty() {
        // ssh itself failed (bad host, auth refused, etc.). Surface
        // stderr so the user sees the underlying error rather than a
        // confusing "missing delimiter" from the parser.
        return Err(DeployError::SshFailed {
            status: output.status.code().unwrap_or(-1),
            stderr: stderr.trim().to_string(),
        });
    }

    script::parse_script_output(script_id, &stdout).map_err(|e| DeployError::ScriptParse {
        message: e.to_string(),
        stdout,
        stderr,
    })
}

#[derive(Debug)]
pub enum DeployError {
    SessionInit(std::io::Error),
    Spawn(std::io::Error),
    SshFailed {
        status: i32,
        stderr: String,
    },
    ScriptParse {
        message: String,
        stdout: String,
        stderr: String,
    },
    ScriptFailed {
        exit_code: i32,
        raw: std::collections::HashMap<String, String>,
    },
    MissingField(&'static str),
    UnsupportedPlatform {
        platform: String,
        arch: String,
    },
    Upload(upload::UploadError),
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionInit(e) => write!(f, "ssh session init: {e}"),
            Self::Spawn(e) => write!(f, "failed to spawn ssh: {e}"),
            Self::SshFailed { status, stderr } => {
                write!(f, "ssh failed (status {status}): {stderr}")
            }
            Self::ScriptParse {
                message,
                stdout,
                stderr,
            } => write!(
                f,
                "install script output unparseable: {message}\n\
                 stdout:\n{stdout}\nstderr:\n{stderr}"
            ),
            Self::ScriptFailed { exit_code, raw } => {
                write!(
                    f,
                    "install script failed (exit {exit_code}); report: {raw:?}"
                )
            }
            Self::MissingField(name) => write!(f, "install script did not report `{name}`"),
            Self::UnsupportedPlatform { platform, arch } => {
                write!(f, "unsupported remote platform: {platform}-{arch}")
            }
            Self::Upload(e) => write!(f, "upload fallback failed: {e}"),
        }
    }
}

impl std::error::Error for DeployError {}
