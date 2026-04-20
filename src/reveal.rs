//! Platform-specific "Reveal in Finder / File Explorer".
//!
//! Called from the file-tree right-click menu's `RevealInFinder` item.
//! Spawns the platform file manager with the target path pre-selected
//! (on the platforms that support selection; others fall back to
//! opening the parent directory).
//!
//! This is strictly best-effort: we spawn and don't wait. Any failure
//! surfaces as a toast at the call site; we don't block the UI on
//! `open`'s exit code because `open -R` on macOS returns before
//! Finder finishes animating the selection.

use std::path::Path;
use std::process::Command;

/// Open the platform file manager with `path` selected. Returns
/// `Ok(())` on spawn, `Err(message)` if spawn itself failed or the
/// platform isn't supported.
///
/// Cross-platform behaviour:
/// - **macOS**: `open -R <path>` — opens Finder and selects the
///   target entry.
/// - **Windows**: `explorer /select,<path>` — same selection
///   semantics, using the path's backslashes verbatim.
/// - **Linux / BSD / anything else**: no standard selection
///   primitive. We return `Err(unsupported)` and the caller toasts
///   a follow-up hint; we don't silently fall back to `xdg-open`
///   on the parent dir because that surprises users who expected
///   the target entry to be visible.
#[allow(unused_variables)]
pub fn reveal_in_finder(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("open").arg("-R").arg(path).spawn();
        match status {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("open -R failed: {e}")),
        }
    }
    #[cfg(target_os = "windows")]
    {
        // `/select,` requires no space between comma and path, and the
        // path gets escaped automatically by Command::arg.
        let arg = format!("/select,{}", path.display());
        let status = Command::new("explorer").arg(arg).spawn();
        match status {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("explorer failed: {e}")),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err("reveal-in-finder not supported on this platform yet".to_string())
    }
}
