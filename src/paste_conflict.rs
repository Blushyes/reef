//! Conflict resolution prompt + auto-rename helper for the Files-tab
//! paste / drop / duplicate flows.
//!
//! Three pieces:
//!
//! 1. `next_copy_name` — pure function. Given a target filename and the
//!    set of names already present in the destination, picks a free
//!    `name copy.ext` / `name copy 2.ext` / … candidate. Used directly
//!    when the destination is the *same* directory as the source
//!    (Duplicate, in-folder paste) — VS Code never prompts in that
//!    case.
//!
//! 2. `classify_paste` — pure function. Given the op, destination,
//!    sources, and a snapshot of the destination's existing names,
//!    returns a `PasteClassification` carrying auto-resolved
//!    decisions (no conflict, or same-dir copy auto-rename), pending
//!    items needing a user prompt, and counts for skipped paths
//!    (self-descent, same-dir Cut no-op). Lifts the smart
//!    classification out of `App` so it can be unit-tested without
//!    instantiating an `App`.
//!
//! 3. `PasteConflictPrompt` — the modal status-bar prompt that drives
//!    Replace / Skip / Keep Both / Cancel decisions when paste lands
//!    items in a *different* directory and a same-named entry exists.
//!    Items without conflicts are pre-recorded as `Resolution::Replace`
//!    (which the worker treats as "land at the chosen name without
//!    needing to clobber anything") so the final decision list is one
//!    flat `Vec<(PathBuf, Resolution)>` per source.
//!
//! Neither classification nor the prompt touches IO. The caller
//! dispatches the worker when the prompt drains.

use crate::file_clipboard::ClipMode;
use std::collections::{HashSet, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Per-item decision recorded while the user steps through conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Overwrite the existing entry at the destination. Worker will
    /// trash the existing path before placing the new one — keeps
    /// undo possible via the OS Trash.
    Replace,
    /// Drop this source from the batch entirely. No FS change.
    Skip,
    /// Land the source under a different basename (provided here so
    /// the worker doesn't need to redo the conflict scan).
    KeepBoth(String),
    /// Bail — caller should drop the entire batch, including any
    /// already-auto-resolved items further down the queue. Status-bar
    /// flag, not a per-item decision in practice.
    Cancel,
}

/// One conflict awaiting user input. Source + the existing destination
/// path that's blocking the placement.
#[derive(Debug, Clone)]
pub struct ConflictItem {
    /// Workdir-relative path of the item the user is moving / copying.
    pub source: PathBuf,
    /// Workdir-relative destination path that already exists.
    pub existing_at_dest: PathBuf,
}

/// Outcome of `classify_paste`. The caller dispatches the worker
/// directly with `auto_decisions` when `pending` is empty; otherwise
/// it opens a `PasteConflictPrompt` carrying both lists.
///
/// `self_descent_blocked` and `same_dir_cut_skipped` are advisory
/// counts so the caller can surface a single toast covering all
/// rejected items rather than one toast per row.
#[derive(Debug, Default)]
pub struct PasteClassification {
    pub auto_decisions: Vec<(PathBuf, Resolution)>,
    pub pending: Vec<ConflictItem>,
    pub self_descent_blocked: usize,
    pub same_dir_cut_skipped: usize,
}

/// `dest_rel` would absorb `from_rel` itself or one of its descendants
/// — block the paste before the worker even spins up. Pure path
/// arithmetic; `Path::starts_with` is component-aware so a sibling
/// with a shared prefix (`src` vs `srcother`) doesn't trigger.
pub fn would_be_self_descent(from_rel: &Path, dest_rel: &Path) -> bool {
    if from_rel == dest_rel {
        return true;
    }
    dest_rel.starts_with(from_rel)
}

/// Classify a paste batch. Each source is one of:
///   - **Skipped — self-descent**: source folder being placed into
///     itself or a descendant. Counted, not in any output list.
///   - **Skipped — same-dir Cut**: Cut into source's own parent is a
///     no-op. Counted, not in any output list.
///   - **Auto-decision (`KeepBoth`)**: Copy into source's own parent.
///     Auto-renamed via `next_copy_name`, never prompts.
///   - **Auto-decision (`Replace`)**: cross-directory paste with no
///     name collision. The worker convention treats `Replace` here
///     as "land at the chosen name" since nothing to clobber.
///   - **Pending**: cross-directory paste with a name collision —
///     user has to decide via `PasteConflictPrompt`.
///
/// `existing` is a snapshot of the destination directory's basenames
/// at the call site — for same-dir auto-rename, the function tracks
/// names introduced earlier in the same batch so two siblings with
/// the same basename don't collide on rename.
pub fn classify_paste(
    op: ClipMode,
    dest_rel: &Path,
    sources: &[PathBuf],
    existing: &HashSet<String>,
) -> PasteClassification {
    let mut working: HashSet<String> = existing.clone();
    let mut out = PasteClassification::default();

    for source in sources {
        let basename = match source.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if would_be_self_descent(source, dest_rel) {
            out.self_descent_blocked += 1;
            continue;
        }
        let source_parent = source.parent().map(PathBuf::from).unwrap_or_default();
        let same_dir = source_parent == dest_rel;
        if same_dir && matches!(op, ClipMode::Cut) {
            // Cut into own parent = no-op. Counter is so the caller
            // can choose to surface a single "nothing to do" toast
            // when the entire batch consists of these.
            out.same_dir_cut_skipped += 1;
            continue;
        }
        if same_dir {
            // Same-dir Copy — auto-rename. VS Code never prompts.
            // Seed against `working` (existing + names already added
            // earlier in this batch) so two siblings with the same
            // basename don't collide on rename. `next_copy_name`
            // takes `&HashSet<String>` so we pass `working` directly,
            // no per-item rebuild.
            let new_name = next_copy_name(&basename, &working);
            working.insert(new_name.clone());
            out.auto_decisions
                .push((source.clone(), Resolution::KeepBoth(new_name)));
        } else if existing.contains(&basename) {
            out.pending.push(ConflictItem {
                source: source.clone(),
                existing_at_dest: dest_rel.join(&basename),
            });
        } else {
            // Free landing — track in `working` too so a later same-
            // batch sibling with the same basename doesn't also
            // claim "no conflict" and silently overwrite it.
            working.insert(basename);
            out.auto_decisions
                .push((source.clone(), Resolution::Replace));
        }
    }
    out
}

#[derive(Debug)]
pub struct PasteConflictPrompt {
    op: ClipMode,
    /// Workdir-relative directory every source is heading into.
    dest_dir: PathBuf,
    /// Conflicts queued for the user. Front is the displayed item.
    pending: VecDeque<ConflictItem>,
    /// Decisions accumulated, in source order. Items without conflicts
    /// arrive here pre-set by the caller; resolved conflicts append.
    decisions: Vec<(PathBuf, Resolution)>,
    /// Set when the user picks Cancel — caller drops the whole batch.
    cancelled: bool,
}

impl PasteConflictPrompt {
    /// Construct a prompt with `auto_decisions` (items that didn't need
    /// user input — no conflict at the destination) and `pending`
    /// (items that did). The order of `auto_decisions` is preserved so
    /// the post-paste cursor lands at the first source.
    pub fn new(
        op: ClipMode,
        dest_dir: PathBuf,
        auto_decisions: Vec<(PathBuf, Resolution)>,
        pending: Vec<ConflictItem>,
    ) -> Self {
        Self {
            op,
            dest_dir,
            pending: pending.into(),
            decisions: auto_decisions,
            cancelled: false,
        }
    }

    pub fn op(&self) -> ClipMode {
        self.op
    }

    pub fn dest_dir(&self) -> &Path {
        &self.dest_dir
    }

    /// Conflict currently displayed to the user (front of queue).
    pub fn current(&self) -> Option<&ConflictItem> {
        self.pending.front()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_done(&self) -> bool {
        self.cancelled || self.pending.is_empty()
    }

    pub fn was_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Resolve the current item and advance the queue.
    pub fn resolve_one(&mut self, r: Resolution) {
        if let Some(item) = self.pending.pop_front() {
            if matches!(r, Resolution::Cancel) {
                self.cancelled = true;
                self.pending.clear();
                return;
            }
            self.decisions.push((item.source, r));
        }
    }

    /// Resolve the current and every remaining queued item with `r`.
    /// `KeepBoth` and `Cancel` are not legal here (the new-basename
    /// must be computed per-item, and Cancel is independent) — callers
    /// should restrict the UI to Replace / Skip for "apply to all".
    pub fn resolve_all_with(&mut self, r: Resolution) {
        debug_assert!(matches!(r, Resolution::Replace | Resolution::Skip));
        let drained: Vec<_> = self.pending.drain(..).collect();
        for item in drained {
            self.decisions.push((item.source, r.clone()));
        }
    }

    /// Consume the prompt into the final decision list (one entry per
    /// source the caller fed in). When `was_cancelled` is true the
    /// caller should ignore this and skip the worker dispatch.
    pub fn into_decisions(self) -> Vec<(PathBuf, Resolution)> {
        self.decisions
    }
}

/// Split a basename into (stem, extension-without-dot).
///
/// - `foo.txt`         → `("foo", "txt")`
/// - `foo.tar.gz`      → `("foo.tar", "gz")` (last dot wins — explicit
///   "compound extension" knowledge isn't worth the bytes)
/// - `Makefile`        → `("Makefile", "")`
/// - `.gitignore`      → `(".gitignore", "")` (leading dot is part of
///   the stem — dotfiles have no extension)
/// - `.env.local`      → `(".env", "local")`
fn split_stem_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(idx) if idx > 0 => (&name[..idx], &name[idx + 1..]),
        _ => (name, ""),
    }
}

fn join_stem_ext(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// Find a free basename of the form `stem copy[ N].ext` for `orig`.
/// `existing` should be the set of basenames already present in the
/// destination directory.
///
/// Tries `stem copy.ext`, then `stem copy 2.ext` … through 9999. If
/// every slot is taken (~impossibly rare), falls back to a hash-derived
/// suffix from `orig` + `existing.len()` for determinism.
///
/// Takes `&HashSet<String>` so callers (`classify_paste`,
/// `App::keep_both_name_for_current_conflict`) can pass their owned
/// "names so far" set directly without rebuilding it as
/// `HashSet<&str>` per call. `HashSet<String>::contains(&str)` works
/// via the `Borrow<str>` impl on `String`, so the body is unchanged
/// from the borrowed-ref version.
pub fn next_copy_name(orig: &str, existing: &HashSet<String>) -> String {
    let (stem, ext) = split_stem_ext(orig);
    let first = join_stem_ext(&format!("{stem} copy"), ext);
    if !existing.contains(first.as_str()) {
        return first;
    }
    for n in 2..=9999 {
        let candidate = join_stem_ext(&format!("{stem} copy {n}"), ext);
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
    }
    let mut h = DefaultHasher::new();
    orig.hash(&mut h);
    existing.len().hash(&mut h);
    let suffix = format!("{:x}", h.finish() & 0xFFFF_FFFF);
    join_stem_ext(&format!("{stem} copy {suffix}"), ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> HashSet<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn split_basic() {
        assert_eq!(split_stem_ext("foo.txt"), ("foo", "txt"));
        assert_eq!(split_stem_ext("foo.tar.gz"), ("foo.tar", "gz"));
        assert_eq!(split_stem_ext("Makefile"), ("Makefile", ""));
    }

    #[test]
    fn split_dotfiles_have_no_extension() {
        assert_eq!(split_stem_ext(".gitignore"), (".gitignore", ""));
        assert_eq!(split_stem_ext(".env.local"), (".env", "local"));
    }

    #[test]
    fn next_copy_name_first_slot() {
        assert_eq!(next_copy_name("foo.txt", &s(&[])), "foo copy.txt");
        assert_eq!(next_copy_name("Makefile", &s(&[])), "Makefile copy");
        assert_eq!(next_copy_name(".gitignore", &s(&[])), ".gitignore copy");
    }

    #[test]
    fn next_copy_name_iterates_n() {
        let names = s(&["foo copy.txt", "foo copy 2.txt"]);
        assert_eq!(next_copy_name("foo.txt", &names), "foo copy 3.txt");
    }

    #[test]
    fn next_copy_name_handles_compound_extensions() {
        assert_eq!(
            next_copy_name("archive.tar.gz", &s(&[])),
            "archive.tar copy.gz"
        );
    }

    #[test]
    fn next_copy_name_skips_only_taken_slots() {
        // Slot 1 free → returns first form even when later slots taken.
        let names = s(&["foo copy 2.txt", "foo copy 3.txt"]);
        assert_eq!(next_copy_name("foo.txt", &names), "foo copy.txt");
    }

    #[test]
    fn next_copy_name_fallback_when_9999_full() {
        // Hostile but tractable: occupy slots 1..=9999.
        let mut names: HashSet<String> = HashSet::with_capacity(9999);
        names.insert("foo copy.txt".to_string());
        for n in 2..=9999 {
            names.insert(format!("foo copy {n}.txt"));
        }
        let result = next_copy_name("foo.txt", &names);
        // Fallback must not collide with the saturated set.
        assert!(!names.contains(result.as_str()));
        // Fallback must keep the extension.
        assert!(result.ends_with(".txt"));
        // Fallback must remain deterministic for the same input.
        let result2 = next_copy_name("foo.txt", &names);
        assert_eq!(result, result2);
    }

    fn confl(src: &str, dest: &str) -> ConflictItem {
        ConflictItem {
            source: PathBuf::from(src),
            existing_at_dest: PathBuf::from(dest),
        }
    }

    #[test]
    fn prompt_with_no_conflicts_is_done_immediately() {
        let p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![(PathBuf::from("a"), Resolution::Replace)],
            vec![],
        );
        assert!(p.is_done());
        assert_eq!(p.into_decisions().len(), 1);
    }

    #[test]
    fn prompt_walks_pending_one_at_a_time() {
        let mut p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![],
            vec![confl("src/a", "dst/a"), confl("src/b", "dst/b")],
        );
        assert!(!p.is_done());
        assert_eq!(p.current().unwrap().source, PathBuf::from("src/a"));
        p.resolve_one(Resolution::Replace);
        assert_eq!(p.current().unwrap().source, PathBuf::from("src/b"));
        p.resolve_one(Resolution::Skip);
        assert!(p.is_done());
        let dec = p.into_decisions();
        assert_eq!(
            dec,
            vec![
                (PathBuf::from("src/a"), Resolution::Replace),
                (PathBuf::from("src/b"), Resolution::Skip),
            ]
        );
    }

    #[test]
    fn prompt_apply_to_all_drains_remainder() {
        let mut p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![],
            vec![
                confl("src/a", "dst/a"),
                confl("src/b", "dst/b"),
                confl("src/c", "dst/c"),
            ],
        );
        p.resolve_all_with(Resolution::Skip);
        assert!(p.is_done());
        let dec = p.into_decisions();
        assert_eq!(dec.len(), 3);
        assert!(dec.iter().all(|(_, r)| *r == Resolution::Skip));
    }

    #[test]
    fn prompt_cancel_clears_pending_and_flags_cancelled() {
        let mut p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![(PathBuf::from("seed"), Resolution::Replace)],
            vec![confl("src/a", "dst/a"), confl("src/b", "dst/b")],
        );
        p.resolve_one(Resolution::Cancel);
        assert!(p.is_done());
        assert!(p.was_cancelled());
        // Cancel preserves any auto-decisions but does not append the
        // current; caller is expected to discard the entire batch.
        let dec = p.into_decisions();
        assert_eq!(dec.len(), 1);
    }

    #[test]
    fn prompt_keep_both_records_new_name() {
        let mut p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![],
            vec![confl("src/a", "dst/a")],
        );
        p.resolve_one(Resolution::KeepBoth("a copy.txt".to_string()));
        let dec = p.into_decisions();
        assert_eq!(
            dec[0],
            (
                PathBuf::from("src/a"),
                Resolution::KeepBoth("a copy.txt".to_string())
            )
        );
    }

    #[test]
    fn auto_decisions_appear_before_user_decisions() {
        let mut p = PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            vec![
                (PathBuf::from("src/x"), Resolution::Replace),
                (PathBuf::from("src/y"), Resolution::Replace),
            ],
            vec![confl("src/z", "dst/z")],
        );
        p.resolve_one(Resolution::Skip);
        let dec = p.into_decisions();
        assert_eq!(dec.len(), 3);
        assert_eq!(dec[0].0, PathBuf::from("src/x"));
        assert_eq!(dec[1].0, PathBuf::from("src/y"));
        assert_eq!(dec[2], (PathBuf::from("src/z"), Resolution::Skip));
    }

    // ── would_be_self_descent ────────────────────────────────────

    #[test]
    fn self_descent_same_path() {
        assert!(would_be_self_descent(Path::new("src"), Path::new("src")));
    }

    #[test]
    fn self_descent_into_descendant() {
        assert!(would_be_self_descent(
            Path::new("src"),
            Path::new("src/sub")
        ));
        assert!(would_be_self_descent(
            Path::new("src"),
            Path::new("src/sub/deep")
        ));
    }

    #[test]
    fn self_descent_rejects_prefix_sibling() {
        // `Path::starts_with` is component-aware — `srcother` does
        // not start with `src` because `src` and `srcother` are
        // distinct components, not byte-prefix overlap.
        assert!(!would_be_self_descent(
            Path::new("src"),
            Path::new("srcother")
        ));
    }

    #[test]
    fn self_descent_unrelated_paths() {
        assert!(!would_be_self_descent(Path::new("src"), Path::new("dst")));
        assert!(!would_be_self_descent(
            Path::new("src/a.txt"),
            Path::new("dst")
        ));
    }

    #[test]
    fn self_descent_root_dest_accepts_anything() {
        // Pasting a nested file into the workspace root (empty path)
        // is fine — root isn't a descendant of any source.
        assert!(!would_be_self_descent(
            Path::new("src/a.txt"),
            Path::new("")
        ));
    }

    // ── classify_paste ───────────────────────────────────────────

    fn names(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_same_dir_copy_auto_renames() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let existing = names(&["a.txt"]);
        let cls = classify_paste(ClipMode::Copy, Path::new("src"), &sources, &existing);
        assert!(cls.pending.is_empty());
        assert_eq!(cls.auto_decisions.len(), 1);
        let (_, r) = &cls.auto_decisions[0];
        match r {
            Resolution::KeepBoth(name) => assert_eq!(name, "a copy.txt"),
            other => panic!("expected KeepBoth, got {other:?}"),
        }
    }

    #[test]
    fn classify_same_dir_cut_is_noop() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let existing = names(&["a.txt"]);
        let cls = classify_paste(ClipMode::Cut, Path::new("src"), &sources, &existing);
        assert!(cls.auto_decisions.is_empty());
        assert!(cls.pending.is_empty());
        assert_eq!(cls.same_dir_cut_skipped, 1);
    }

    #[test]
    fn classify_cross_dir_conflict_goes_to_pending() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let existing = names(&["a.txt"]);
        let cls = classify_paste(ClipMode::Copy, Path::new("dst"), &sources, &existing);
        assert!(cls.auto_decisions.is_empty());
        assert_eq!(cls.pending.len(), 1);
        assert_eq!(cls.pending[0].source, PathBuf::from("src/a.txt"));
        assert_eq!(cls.pending[0].existing_at_dest, PathBuf::from("dst/a.txt"));
    }

    #[test]
    fn classify_cross_dir_no_conflict_auto_replaces() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let existing = names(&[]);
        let cls = classify_paste(ClipMode::Copy, Path::new("dst"), &sources, &existing);
        assert_eq!(cls.auto_decisions.len(), 1);
        assert!(cls.pending.is_empty());
        let (path, r) = &cls.auto_decisions[0];
        assert_eq!(path, &PathBuf::from("src/a.txt"));
        assert!(matches!(r, Resolution::Replace));
    }

    #[test]
    fn classify_blocks_self_descent() {
        let sources = vec![PathBuf::from("src")];
        let existing = names(&[]);
        let cls = classify_paste(ClipMode::Copy, Path::new("src/sub"), &sources, &existing);
        assert!(cls.auto_decisions.is_empty());
        assert!(cls.pending.is_empty());
        assert_eq!(cls.self_descent_blocked, 1);
    }

    #[test]
    fn classify_paste_into_root_uses_empty_path() {
        // Lifting a nested file to the workspace root: `dest_rel`
        // is the empty path, source's parent is `src` ≠ root, so
        // this is a cross-dir Copy with no conflict → auto Replace.
        let sources = vec![PathBuf::from("src/a.txt")];
        let existing = names(&[]);
        let cls = classify_paste(ClipMode::Copy, Path::new(""), &sources, &existing);
        assert_eq!(cls.auto_decisions.len(), 1);
    }

    #[test]
    fn classify_same_dir_copy_avoids_intra_batch_collision() {
        // Two sources with the same basename pasted into the parent
        // dir — second auto-rename must avoid the first's chosen
        // name. Without `working` tracking, both would land at
        // `a copy.txt` and the second would clobber the first.
        let sources = vec![PathBuf::from("src/a.txt"), PathBuf::from("src/a.txt")];
        let existing = names(&["a.txt"]);
        let cls = classify_paste(ClipMode::Copy, Path::new("src"), &sources, &existing);
        assert_eq!(cls.auto_decisions.len(), 2);
        let kept_names: Vec<String> = cls
            .auto_decisions
            .iter()
            .filter_map(|(_, r)| match r {
                Resolution::KeepBoth(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(kept_names, vec!["a copy.txt", "a copy 2.txt"]);
    }

    #[test]
    fn classify_cross_dir_intra_batch_basenames_collide_one_pending() {
        // Two sources with the same basename pasted into a dest
        // that *doesn't* yet have that name. The first lands
        // free; the second sees the first's name reserved in
        // `working`, so the *user-visible* effect is "free
        // landing for one, no auto-resolution for the other".
        // We don't add an intra-batch ConflictItem here — that
        // would require tracking "auto-claimed" names as if they
        // already existed. Currently both go to auto Replace,
        // and the worker writes them in order, second overwriting
        // the first. This is acceptable v1 behaviour: the user
        // selected two different sources expecting both to land,
        // so prompting on intra-batch collision would be more
        // surprising than overwriting. Document the choice.
        let sources = vec![PathBuf::from("a/x.txt"), PathBuf::from("b/x.txt")];
        let existing = names(&[]);
        let cls = classify_paste(ClipMode::Copy, Path::new("dst"), &sources, &existing);
        assert_eq!(cls.auto_decisions.len(), 2);
        assert!(cls.pending.is_empty());
    }

    #[test]
    fn classify_mixed_batch() {
        // Multi-source mix: one cross-dir with conflict, one cross-
        // dir without, one same-dir Copy auto-rename, one self-
        // descent block.
        let sources = vec![
            PathBuf::from("a.txt"),     // existing in dst → pending
            PathBuf::from("src/b.txt"), // no conflict → auto Replace
            PathBuf::from("dst/c.txt"), // same-dir Copy → auto KeepBoth
            PathBuf::from("dst"),       // self-descent into dst → blocked
        ];
        let existing = names(&["a.txt", "c.txt"]);
        let cls = classify_paste(ClipMode::Copy, Path::new("dst"), &sources, &existing);
        assert_eq!(cls.pending.len(), 1);
        assert_eq!(cls.auto_decisions.len(), 2);
        assert_eq!(cls.self_descent_blocked, 1);
        assert_eq!(cls.same_dir_cut_skipped, 0);
    }
}
