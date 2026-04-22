//! Track A — `--protocol-version` gating in the install script.
//!
//! Runs the generated install script under `bash` on the host with two
//! seeded agents: one that prints the wrong protocol version (stale) and
//! one that prints the right one. The stale agent must be deleted and
//! the script should report `installState=download_failed` (because the
//! test URL is intentionally 404), leaving the client to fall through to
//! the upload path. The fresh agent must be kept (installState=existed)
//! and the parser must read the `protocolVersion` field back out.

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use reef::agent_deploy::script::{InstallState, generate_install_script, parse_script_output};
use tempfile::TempDir;

const EXPECTED: &str = "3";
/// Return `true` iff `bash` is on PATH — tests that depend on it skip on
/// the rare host where it isn't.
fn bash_available() -> bool {
    Command::new("bash")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write a shell-script "agent" that echoes `proto` when called with
/// `--protocol-version` and exits 0 for anything else. Marked +x.
fn seed_fake_agent(dir: &std::path::Path, proto: &str) {
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--protocol-version\" ]; then\n  printf '%s\\n' '{proto}'\n  exit 0\nfi\nexit 0\n"
    );
    std::fs::write(dir.join("reef-agent"), script).unwrap();
    let mut perms = std::fs::metadata(dir.join("reef-agent"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(dir.join("reef-agent"), perms).unwrap();
}

fn run_script(install_root: &str, id: &str) -> String {
    let script = generate_install_script(
        "0.0.0",
        install_root,
        id,
        // Point at a closed port so the download step fails deterministically;
        // we don't want the test's behaviour to depend on the network.
        "http://127.0.0.1:1/404/{version}/{platform}/{arch}.tar.gz",
        EXPECTED,
    );
    let mut child = Command::new("bash")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("bash wait");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn stale_agent_is_removed_and_reinstalled() {
    if !bash_available() {
        eprintln!("skip: bash not available");
        return;
    }
    let tmp = TempDir::new().unwrap();
    // Mirror the on-remote layout: `$install_root/agent/$version/reef-agent`.
    let agent_dir = tmp.path().join("agent").join("0.0.0");
    std::fs::create_dir_all(&agent_dir).unwrap();
    seed_fake_agent(&agent_dir, "1"); // stale — not EXPECTED (3)
    assert!(agent_dir.join("reef-agent").exists());

    let stdout = run_script(&tmp.path().display().to_string(), "stale-id");
    let report = parse_script_output("stale-id", &stdout).expect("parse");
    // Stale agent should have been rm'd; download will fail because the
    // URL is unreachable → installState=download_failed is the correct
    // terminal state for this test.
    assert_eq!(
        report.install_state,
        Some(InstallState::DownloadFailed),
        "stale agent wasn't evicted (installState={:?})",
        report.install_state
    );
    // First `protocolVersion==…==` emit is empty since we rm'd the
    // stale binary before re-probing.
    assert_eq!(report.protocol_version, None);
    // The script rm'd it, and the download failed, so nothing is there.
    assert!(
        !agent_dir.join("reef-agent").exists(),
        "stale binary should have been deleted"
    );
}

#[test]
fn fresh_agent_is_kept() {
    if !bash_available() {
        eprintln!("skip: bash not available");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let agent_dir = tmp.path().join("agent").join("0.0.0");
    std::fs::create_dir_all(&agent_dir).unwrap();
    seed_fake_agent(&agent_dir, EXPECTED);

    let stdout = run_script(&tmp.path().display().to_string(), "fresh-id");
    let report = parse_script_output("fresh-id", &stdout).expect("parse");
    assert_eq!(
        report.install_state,
        Some(InstallState::Existed),
        "fresh agent should be kept, got installState={:?}",
        report.install_state,
    );
    assert_eq!(report.protocol_version.as_deref(), Some(EXPECTED));
    assert!(
        agent_dir.join("reef-agent").exists(),
        "fresh binary should not have been deleted"
    );
}
