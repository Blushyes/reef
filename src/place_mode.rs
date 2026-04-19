//! VSCode-style drag-and-drop destination picker.
//!
//! The OS-level drag session ends at the terminal boundary — crossterm only
//! sees the dropped paths as a bracketed-paste payload, with no way to
//! observe drag-over or hover position during the drag itself. We sidestep
//! that by splitting the interaction in two:
//!
//! 1. Detect the drop (`input::handle_paste` parses the paste, and if every
//!    segment resolves to an existing path we call `App::enter_place_mode`).
//! 2. Hand control to this modal picker: the file tree sprouts a dashed
//!    root drop-zone border, hovered folder rows highlight, a banner pins
//!    to the top reminding the user what's about to land, and a click on a
//!    folder row / the root kicks off an async copy.
//!
//! `PlaceModeState` is intentionally tiny — just enough cheap UI state to
//! drive rendering and input gating. Everything expensive (the recursive
//! copy itself, name-conflict resolution) runs on the files worker.

use crate::file_tree::TreeEntry;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long the cursor must rest on a collapsed folder before we auto-expand
/// it. VSCode's Explorer uses ~500ms; 600ms feels slightly less twitchy on
/// a terminal where mouse movement is coarser. Tuned by feel.
pub const HOVER_EXPAND_DELAY: Duration = Duration::from_millis(600);

#[derive(Debug, Default)]
pub struct PlaceModeState {
    /// `true` while the modal picker owns input. When set, `input::handle_key`
    /// and `input::handle_mouse` short-circuit: Esc / right-click cancel,
    /// clicks on folder rows or the root drop-zone confirm, everything else
    /// is either scroll (to reach deep folders) or ignored.
    pub active: bool,

    /// Absolute paths of the items the user is placing. All go to the same
    /// destination — no per-item picking.
    pub sources: Vec<PathBuf>,

    /// Index (into `file_tree.entries`) of the folder currently resolved as
    /// the hover target (either the hovered folder itself, or the parent
    /// folder of the hovered nested file). Tracks the auto-expand timer
    /// and the render-time block highlight. `None` when the hover target
    /// is the root drop zone.
    pub hover_folder_idx: Option<usize>,

    /// When `hover_folder_idx` was first set to its current value. Cleared
    /// after we fire the auto-expand so a single long hover expands once,
    /// not repeatedly.
    pub hover_since: Option<Instant>,
}

impl PlaceModeState {
    /// Primary filename shown in the banner. Picks the first source and
    /// falls back to `"?"` if the list is somehow empty so the UI never
    /// renders a naked `Placing 0 file(s)`.
    ///
    /// Filenames are sanitised for display: macOS allows control
    /// characters (`\n`, `\t`, bell, …) in file names and an embedded
    /// newline would break `Line::from` rendering, splitting the banner
    /// mid-row and misaligning the accent background. We replace any
    /// control char with `?` — paths stay recoverable from the sources
    /// list, the UI stays tidy.
    pub fn primary_name(&self) -> String {
        self.sources
            .first()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(sanitize_display)
            .unwrap_or_else(|| "?".to_string())
    }

    pub fn count(&self) -> usize {
        self.sources.len()
    }

    /// Update the auto-expand tracker. Called on every mouse move in place
    /// mode. Changing the hovered folder (or moving off any folder) resets
    /// the timer — otherwise dragging the cursor across a row of folders
    /// would inherit a timer from the previous one and misfire.
    pub fn update_hover(&mut self, folder_idx: Option<usize>) {
        if self.hover_folder_idx != folder_idx {
            self.hover_folder_idx = folder_idx;
            self.hover_since = folder_idx.map(|_| Instant::now());
        }
    }

    /// Whether enough time has elapsed on the current hover to justify
    /// firing an auto-expand. Caller is responsible for then clearing
    /// `hover_since` so we don't re-fire on the next tick.
    pub fn auto_expand_due(&self, now: Instant) -> Option<usize> {
        match (self.hover_folder_idx, self.hover_since) {
            (Some(idx), Some(t)) if now.duration_since(t) >= HOVER_EXPAND_DELAY => Some(idx),
            _ => None,
        }
    }
}

/// Replace ASCII / Unicode control characters (except space) in a
/// filename for safe single-line display. Embedded newlines and tabs
/// are the common offenders on macOS, where the filesystem will happily
/// accept them and terminals then mis-render the `Line`.
fn sanitize_display(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Resolves what a click at `hovered_idx` should drop INTO, given the
/// current flattened tree. VSCode semantics: folders drop into themselves;
/// files drop into their parent folder; top-level files drop into the
/// project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoverTarget {
    /// No folder target — click goes to the project root. Either the
    /// cursor is on an empty area, or it's on a depth-0 file (whose
    /// "parent" is the root).
    Root,
    /// Click drops INTO `folder_idx`. `block_start .. block_end` is the
    /// range of visible rows that make up the folder's "block"
    /// (folder row + all its expanded descendants). Used by render to
    /// highlight the whole block on hover.
    Folder {
        folder_idx: usize,
        block_start: usize,
        block_end: usize,
    },
}

impl HoverTarget {
    /// True if `row` falls inside the folder block (for per-row highlight
    /// during render). Root hovers highlight the whole panel instead of
    /// any specific block, so this always returns false for `Root`.
    pub fn contains_row(&self, row: usize) -> bool {
        matches!(
            self,
            HoverTarget::Folder { block_start, block_end, .. }
                if row >= *block_start && row < *block_end
        )
    }
}

/// Given a flattened tree and a hovered row, compute what the click would
/// actually target — and, for folder targets, the range of rows that
/// should light up together as a "block".
///
/// Invariant assumed from the file-tree builder: if an entry at depth D
/// appears at index I, there exists a directory entry at depth D-1
/// somewhere in `entries[0..I]`. (A nested entry must have a parent.)
/// If that invariant breaks we fail soft into `Root` rather than panic.
pub fn resolve_hover_target(entries: &[TreeEntry], hovered_idx: usize) -> HoverTarget {
    let Some(entry) = entries.get(hovered_idx) else {
        return HoverTarget::Root;
    };

    // Pick the folder whose block this row belongs to.
    let (folder_idx, folder_depth) = if entry.is_dir {
        (hovered_idx, entry.depth)
    } else {
        // Files at depth 0 have no parent folder in the tree — they belong
        // to the root drop zone.
        let Some(parent_depth) = entry.depth.checked_sub(1) else {
            return HoverTarget::Root;
        };
        // Walk backwards to the nearest directory entry at `parent_depth`.
        // The loop can't miss thanks to the invariant above, but we guard
        // against `idx == 0` so a broken tree degrades instead of panicking.
        let mut idx = hovered_idx;
        loop {
            if idx == 0 {
                return HoverTarget::Root;
            }
            idx -= 1;
            let e = &entries[idx];
            if e.is_dir && e.depth == parent_depth {
                break (idx, parent_depth);
            }
        }
    };

    // Block ends at the first subsequent row with depth <= folder_depth —
    // that's either a sibling, an uncle, or the end of the tree.
    let mut block_end = entries.len();
    for (j, e) in entries.iter().enumerate().skip(folder_idx + 1) {
        if e.depth <= folder_depth {
            block_end = j;
            break;
        }
    }

    HoverTarget::Folder {
        folder_idx,
        block_start: folder_idx,
        block_end,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(path: &str, depth: usize) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            depth,
            is_dir: true,
            is_expanded: true,
            git_status: None,
        }
    }

    fn file(path: &str, depth: usize) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            depth,
            is_dir: false,
            is_expanded: false,
            git_status: None,
        }
    }

    #[test]
    fn default_is_inactive_and_empty() {
        let s = PlaceModeState::default();
        assert!(!s.active);
        assert_eq!(s.count(), 0);
        assert_eq!(s.primary_name(), "?");
    }

    #[test]
    fn primary_name_reads_basename() {
        let s = PlaceModeState {
            active: true,
            sources: vec![PathBuf::from("/tmp/alpha/beta.txt")],
            ..PlaceModeState::default()
        };
        assert_eq!(s.primary_name(), "beta.txt");
        assert_eq!(s.count(), 1);
    }

    #[test]
    fn primary_name_strips_control_chars() {
        // Embedded newline (legal on macOS) must not bleed into the
        // banner — the Line widget would otherwise split mid-row and
        // misalign the accent background.
        let s = PlaceModeState {
            active: true,
            sources: vec![PathBuf::from("/tmp/na\nme.txt")],
            ..PlaceModeState::default()
        };
        assert_eq!(s.primary_name(), "na?me.txt");
    }

    #[test]
    fn sanitize_display_replaces_control_chars() {
        assert_eq!(sanitize_display("plain.txt"), "plain.txt");
        assert_eq!(sanitize_display("a\tb"), "a?b");
        assert_eq!(sanitize_display("a\nb\rc"), "a?b?c");
        assert_eq!(sanitize_display("中文.rs"), "中文.rs");
    }

    #[test]
    fn depth_zero_file_resolves_to_root() {
        let entries = vec![file("README.md", 0), dir("src", 0)];
        assert_eq!(resolve_hover_target(&entries, 0), HoverTarget::Root);
    }

    #[test]
    fn folder_resolves_to_self_with_full_block() {
        //  0: ▾ src        (depth 0, folder)
        //  1:   ▸ ui       (depth 1, folder, collapsed)
        //  2:   main.rs    (depth 1, file)
        //  3: README.md    (depth 0, file)
        let entries = vec![
            dir("src", 0),
            TreeEntry {
                path: "src/ui".into(),
                name: "ui".into(),
                depth: 1,
                is_dir: true,
                is_expanded: false,
                git_status: None,
            },
            file("src/main.rs", 1),
            file("README.md", 0),
        ];
        let t = resolve_hover_target(&entries, 0); // hovering src
        assert_eq!(
            t,
            HoverTarget::Folder {
                folder_idx: 0,
                block_start: 0,
                block_end: 3, // stops at README.md (depth 0)
            }
        );
    }

    #[test]
    fn nested_file_resolves_to_parent_folder_block() {
        //  0: ▾ src
        //  1:   ▾ ui
        //  2:     main.rs    ← hover here
        //  3:     helper.rs
        //  4:   mod.rs
        //  5: README.md
        let entries = vec![
            dir("src", 0),
            dir("src/ui", 1),
            file("src/ui/main.rs", 2),
            file("src/ui/helper.rs", 2),
            file("src/mod.rs", 1),
            file("README.md", 0),
        ];
        let t = resolve_hover_target(&entries, 2);
        assert_eq!(
            t,
            HoverTarget::Folder {
                folder_idx: 1, // ui
                block_start: 1,
                block_end: 4, // stops at mod.rs (depth 1)
            }
        );
    }

    #[test]
    fn contains_row_uses_half_open_range() {
        let block = HoverTarget::Folder {
            folder_idx: 1,
            block_start: 1,
            block_end: 4,
        };
        assert!(block.contains_row(1));
        assert!(block.contains_row(3));
        assert!(!block.contains_row(4));
        assert!(!block.contains_row(0));
        assert!(!HoverTarget::Root.contains_row(0));
    }

    #[test]
    fn update_hover_resets_timer_only_on_change() {
        let mut s = PlaceModeState::default();
        s.update_hover(Some(3));
        let first = s.hover_since;
        assert!(first.is_some());
        std::thread::sleep(Duration::from_millis(2));
        s.update_hover(Some(3)); // same → unchanged
        assert_eq!(s.hover_since, first);
        s.update_hover(Some(5)); // different → reset
        assert!(s.hover_since > first);
        s.update_hover(None); // clear
        assert_eq!(s.hover_folder_idx, None);
        assert_eq!(s.hover_since, None);
    }

    #[test]
    fn auto_expand_fires_after_delay() {
        let mut s = PlaceModeState::default();
        s.update_hover(Some(2));
        let start = s.hover_since.unwrap();
        assert_eq!(s.auto_expand_due(start), None);
        assert_eq!(s.auto_expand_due(start + HOVER_EXPAND_DELAY), Some(2));
    }
}
