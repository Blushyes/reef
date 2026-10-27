//! CLI commands must finish before terminal initialization or package-manager writes.

use std::process::Command;

#[test]
fn update_help_exits_without_opening_the_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Homebrew"));
    assert!(!output.stdout.contains(&0x1b));
}

#[test]
fn update_rejects_extra_arguments_before_running() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .args(["update", "--ssh", "example.com"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reef update"));
}

#[test]
fn source_build_update_explains_supported_installations() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .arg("update")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("source"));
    assert!(!output.stdout.contains(&0x1b));
}

#[cfg(unix)]
mod homebrew {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;

    struct Installation {
        root: tempfile::TempDir,
    }

    impl Installation {
        fn new() -> Self {
            let binary = Path::new(env!("CARGO_BIN_EXE_reef"));
            let root = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
            let keg = root.path().join("Cellar/reef/0.44.1");
            fs::create_dir_all(keg.join("bin")).unwrap();
            fs::hard_link(binary, keg.join("bin/reef")).unwrap();
            fs::write(
                keg.join("INSTALL_RECEIPT.json"),
                r#"{"source":{"tap":"blushyes/tap"}}"#,
            )
            .unwrap();
            let bin = root.path().join("bin");
            fs::create_dir(&bin).unwrap();
            symlink(keg.join("bin/reef"), bin.join("reef")).unwrap();
            let brew = bin.join("brew");
            fs::write(&brew, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$REEF_TEST_LOG\"\nif [ \"$1\" = update ]; then exit \"$REEF_TEST_REFRESH_STATUS\"; fi\nexit \"$REEF_TEST_UPGRADE_STATUS\"\n").unwrap();
            fs::set_permissions(&brew, fs::Permissions::from_mode(0o755)).unwrap();
            Self { root }
        }

        fn run(&self, refresh_status: &str, upgrade_status: &str) -> std::process::Output {
            Command::new(self.root.path().join("bin/reef"))
                .arg("update")
                .env("REEF_TEST_LOG", self.root.path().join("brew.log"))
                .env("REEF_TEST_REFRESH_STATUS", refresh_status)
                .env("REEF_TEST_UPGRADE_STATUS", upgrade_status)
                .output()
                .unwrap()
        }

        fn calls(&self) -> String {
            fs::read_to_string(self.root.path().join("brew.log")).unwrap()
        }
    }

    #[test]
    fn symlinked_installation_refreshes_then_upgrades_with_its_own_brew() {
        let installation = Installation::new();
        let output = installation.run("0", "0");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(installation.calls(), "update\nupgrade blushyes/tap/reef\n");
    }

    #[test]
    fn failed_refresh_does_not_attempt_upgrade() {
        let installation = Installation::new();
        assert!(!installation.run("9", "0").status.success());
        assert_eq!(installation.calls(), "update\n");
    }

    #[test]
    fn failed_upgrade_does_not_report_success() {
        let installation = Installation::new();
        let output = installation.run("0", "9");
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("update completed"));
    }
}
