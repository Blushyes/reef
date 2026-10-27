//! Delegate upgrades to the package manager that owns this executable.

use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Deserialize)]
struct Receipt {
    source: ReceiptSource,
}

#[derive(Deserialize)]
struct ReceiptSource {
    tap: String,
}

struct HomebrewInstallation<'a> {
    prefix: &'a Path,
    keg: &'a Path,
}

pub fn run() -> io::Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let brew = homebrew_command(&executable)?;
    run_brew(&brew, &["update"])?;
    run_brew(&brew, &["upgrade", "blushyes/tap/reef"])?;
    println!("Reef update completed. Run reef to start the installed version.");
    Ok(())
}

fn homebrew_command(executable: &Path) -> io::Result<PathBuf> {
    let installation = homebrew_installation(executable).ok_or_else(|| {
        io::Error::other(
            "reef update supports Homebrew and global npm installations. For npm, run the npm-installed reef launcher. For source builds or manual binaries, rebuild or replace using your original installation method.",
        )
    })?;
    let receipt_path = installation.keg.join("INSTALL_RECEIPT.json");
    let receipt: Receipt = serde_json::from_slice(&fs::read(&receipt_path).map_err(|error| {
        io::Error::other(format!("Cannot read {}: {error}", receipt_path.display()))
    })?)
    .map_err(|error| io::Error::other(format!("Invalid Homebrew installation receipt: {error}")))?;
    if receipt.source.tap != "blushyes/tap" {
        return Err(io::Error::other(
            "This Reef installation is not from blushyes/tap. Upgrade it using its original tap.",
        ));
    }
    Ok(installation.prefix.join("bin/brew"))
}

fn homebrew_installation(executable: &Path) -> Option<HomebrewInstallation<'_>> {
    let bin = executable.parent()?;
    let keg = bin.parent()?;
    let formula = keg.parent()?;
    let cellar = formula.parent()?;
    if executable.file_name()? != "reef"
        || bin.file_name()? != "bin"
        || formula.file_name()? != "reef"
        || cellar.file_name()? != "Cellar"
    {
        return None;
    }
    Some(HomebrewInstallation {
        prefix: cellar.parent()?,
        keg,
    })
}

fn run_brew(brew: &Path, args: &[&str]) -> io::Result<()> {
    println!("Running: {} {}", brew.display(), args.join(" "));
    let status = Command::new(brew)
        .args(args)
        .env("HOMEBREW_NO_AUTO_UPDATE", "1")
        .status()
        .map_err(|error| io::Error::other(format!("Cannot run {}: {error}", brew.display())))?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "brew {} failed ({status}); resolve the package-manager error above and retry reef update",
            args.join(" ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_uses_the_owning_prefix() {
        for prefix in [
            "/opt/homebrew",
            "/usr/local",
            "/home/linuxbrew/.linuxbrew",
            "/custom brew",
        ] {
            let executable = Path::new(prefix).join("Cellar/reef/0.44.1/bin/reef");
            assert_eq!(
                homebrew_installation(&executable).unwrap().prefix,
                Path::new(prefix)
            );
        }
    }

    #[test]
    fn unrelated_executables_are_not_homebrew_installations() {
        for path in [
            "/usr/local/bin/reef",
            "/repo/target/release/reef",
            "/opt/homebrew/Cellar/other/1/bin/reef",
        ] {
            assert!(homebrew_installation(Path::new(path)).is_none());
        }
    }

    #[test]
    fn another_taps_receipt_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let keg = temp.path().join("Cellar/reef/1");
        fs::create_dir_all(keg.join("bin")).unwrap();
        fs::write(
            keg.join("INSTALL_RECEIPT.json"),
            r#"{"source":{"tap":"other/tap"}}"#,
        )
        .unwrap();
        assert!(
            homebrew_command(&keg.join("bin/reef"))
                .unwrap_err()
                .to_string()
                .contains("original tap")
        );
    }
}
