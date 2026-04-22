//! End-to-end smoke test for `agent_deploy` against a real `ssh localhost`.
//!
//! Self-skipping: if the host isn't running sshd, doesn't allow password-
//! less localhost auth, or `ssh` isn't on PATH, the test prints "skip" and
//! returns without failing. This matches how we exercise this path in CI:
//! the Linux runner has sshd enabled and a throwaway key; a developer's
//! laptop usually doesn't.
//!
//! Coverage:
//!   - `existed`: pre-populate `$HOME/.reef/agent/<ver>/reef-agent`, run
//!     ensure_agent, expect `InstallPath::AlreadyInstalled` and the same
//!     remote_path.
//!   - `download_failed → upload fallback`: point the template at a
//!     guaranteed-404 URL, ensure the upload path takes over and the
//!     binary ends up executable on the remote.
//!
//! Both cases set `$HOME` to a tempdir so the test doesn't scribble in
//! the developer's `~/.reef`.

use std::process::Command;
use std::sync::Mutex;

use reef::agent_deploy::{
    self, AgentLocation, DeployError, InstallPath, SshSession,
    script::{self, InstallState},
};

static SSH_LOCK: Mutex<()> = Mutex::new(());

/// Check whether `ssh -o BatchMode=yes localhost true` works. This tells us
/// there's sshd listening AND the current user can auth without a prompt
/// (key-based or already-unlocked agent). The test would hang without
/// BatchMode=yes when prompting for a password.
fn ssh_localhost_reachable() -> bool {
    Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "localhost",
            "true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Lookup the debug-built `reef-agent`. Same approach as
/// `backend_loopback.rs`: `CARGO_BIN_EXE_*` doesn't cross crates in a
/// workspace, so we walk `target/{debug,release}`.
fn agent_bin() -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let root = std::path::PathBuf::from(manifest_dir);
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join("reef-agent");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "reef-agent binary not found under target/{{debug,release}}/ — \
         run `cargo build -p reef-agent` first"
    );
}

struct HomeGuard {
    prev: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn enter(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("HOME");
        // SAFETY: callers serialise via SSH_LOCK; this test binary does
        // not touch HOME anywhere else.
        unsafe {
            std::env::set_var("HOME", path);
        }
        Self { prev }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(v) = self.prev.take() {
                std::env::set_var("HOME", v);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

#[test]
fn ensure_agent_existed_path() {
    let _lock = SSH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ssh_localhost_reachable() {
        eprintln!("skip: ssh localhost not reachable without a password");
        return;
    }

    // Keep HOME inside a tempdir so we don't pollute the developer's
    // `~/.reef` and so the script's `$HOME/.reef/agent/...` path lines
    // up with somewhere we control.
    let tmp_home = tempfile::TempDir::new().expect("tempdir");
    let _home = HomeGuard::enter(tmp_home.path());

    // Prime the expected path with the compiled debug agent.
    let version = "test-fakever";
    let target_dir = tmp_home.path().join(".reef").join("agent").join(version);
    std::fs::create_dir_all(&target_dir).unwrap();
    let target_bin = target_dir.join("reef-agent");
    std::fs::copy(agent_bin(), &target_bin).unwrap();
    // Make sure it's executable (TempDir defaults should, but be explicit
    // — some CI Linux hosts set umask 0022 which strips group +x.).
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&target_bin, perms).unwrap();

    let session = SshSession::for_host("localhost").expect("session");
    let location = agent_deploy::ensure_agent_with_session(
        &session,
        version,
        agent_deploy::DEFAULT_DOWNLOAD_URL_TEMPLATE,
    )
    .expect("ensure_agent");
    assert_eq!(location.via, InstallPath::AlreadyInstalled);
    assert!(
        location.remote_path.ends_with("reef-agent"),
        "remote_path should end in reef-agent, got {}",
        location.remote_path
    );
    assert!(!location.platform.is_empty());
    assert!(!location.arch.is_empty());
}

#[test]
fn ensure_agent_download_failed_upload_fallback() {
    let _lock = SSH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !ssh_localhost_reachable() {
        eprintln!("skip: ssh localhost not reachable without a password");
        return;
    }
    if !reef::agent_deploy::embedded::EMBEDDED_AGENT_PRESENT {
        eprintln!("skip: no embedded agent bytes in this build (pre-build reef-agent and rerun)");
        return;
    }

    let tmp_home = tempfile::TempDir::new().expect("tempdir");
    let _home = HomeGuard::enter(tmp_home.path());

    let version = "test-fakever-download";
    let session = SshSession::for_host("localhost").expect("session");

    // Guaranteed-unreachable endpoint. `localhost:1` is always
    // RST/refused — even stricter than a 404 so the download branch bails
    // inside the retry window.
    let bogus_url = "http://127.0.0.1:1/reef-{version}-{platform}-{arch}.tar.gz";
    let location: AgentLocation =
        agent_deploy::ensure_agent_with_session(&session, version, bogus_url).expect("fallback");
    assert_eq!(location.via, InstallPath::Uploaded);

    // Sanity: the agent now sits where ensure_agent said it does. We
    // resolve $HOME manually because the install script evaluates it on
    // the *remote* (which in localhost tests is the same machine, but
    // we don't want to assume).
    let home = std::env::var_os("HOME").expect("HOME");
    let expected_prefix = std::path::PathBuf::from(&home)
        .join(".reef")
        .join("agent")
        .join(version);
    assert!(
        location
            .remote_path
            .starts_with(&*expected_prefix.to_string_lossy()),
        "remote_path {} should start with {}",
        location.remote_path,
        expected_prefix.display(),
    );
    assert!(std::path::Path::new(&location.remote_path).is_file());

    // Clean up so the next run of this test starts from a fresh state —
    // though the HomeGuard + tempdir already accomplishes this.
    drop(location);
}

#[test]
fn ensure_agent_surfaces_ssh_failure() {
    // Bogus host that Clippy-clean, zero-network, and guaranteed to fail
    // before auth. Proves the error type surfaces rather than panicking.
    let session = SshSession::for_host("reef-nonexistent-host.invalid").expect("session");
    let result = agent_deploy::ensure_agent_with_session(
        &session,
        "0.0.0",
        agent_deploy::DEFAULT_DOWNLOAD_URL_TEMPLATE,
    );
    match result {
        Err(DeployError::SshFailed { .. })
        | Err(DeployError::ScriptParse { .. })
        | Err(DeployError::Spawn(_)) => { /* expected */ }
        Err(other) => panic!("expected ssh failure, got {other}"),
        Ok(loc) => panic!("expected failure, got success: {loc:?}"),
    }
}

#[test]
fn script_state_enum_covers_all_variants() {
    for s in [
        "existed",
        "downloaded",
        "download_failed",
        "extract_failed",
        "unsupported",
    ] {
        assert!(script::InstallState::parse(s).is_some(), "missing: {s}");
    }
    assert_eq!(script::InstallState::parse("bogus"), None);
    assert_eq!(
        script::InstallState::parse("existed"),
        Some(InstallState::Existed),
    );
}
