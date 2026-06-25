//! Pure file-operation helpers shared by renderers.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipMode {
    Cut,
    Copy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Replace,
    Skip,
    KeepBoth(String),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct ConflictItem {
    pub source: PathBuf,
    pub existing_at_dest: PathBuf,
}

#[derive(Debug, Default)]
pub struct PasteClassification {
    pub auto_decisions: Vec<(PathBuf, Resolution)>,
    pub pending: Vec<ConflictItem>,
    pub self_descent_blocked: usize,
    pub same_dir_cut_skipped: usize,
}

pub fn would_be_self_descent(from_rel: &Path, dest_rel: &Path) -> bool {
    if from_rel == dest_rel {
        return true;
    }
    dest_rel.starts_with(from_rel)
}

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
            out.same_dir_cut_skipped += 1;
            continue;
        }
        if same_dir {
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
    dest_dir: PathBuf,
    pending: VecDeque<ConflictItem>,
    decisions: Vec<(PathBuf, Resolution)>,
    used_names: HashSet<String>,
    cancelled: bool,
}

impl PasteConflictPrompt {
    pub fn new(
        op: ClipMode,
        dest_dir: PathBuf,
        auto_decisions: Vec<(PathBuf, Resolution)>,
        pending: Vec<ConflictItem>,
        used_names: HashSet<String>,
    ) -> Self {
        Self {
            op,
            dest_dir,
            pending: pending.into(),
            decisions: auto_decisions,
            used_names,
            cancelled: false,
        }
    }

    pub fn op(&self) -> ClipMode {
        self.op
    }

    pub fn dest_dir(&self) -> &Path {
        &self.dest_dir
    }

    pub fn current(&self) -> Option<&ConflictItem> {
        self.pending.front()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn is_done(&self) -> bool {
        self.cancelled || self.pending.is_empty()
    }

    pub fn keep_both_name_for_current(&self) -> Option<String> {
        let item = self.current()?;
        let basename = item.source.file_name().and_then(|s| s.to_str())?;
        Some(next_copy_name(basename, &self.used_names))
    }

    pub fn was_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn resolve_one(&mut self, r: Resolution) {
        if let Some(item) = self.pending.pop_front() {
            if matches!(r, Resolution::Cancel) {
                self.cancelled = true;
                self.pending.clear();
                return;
            }
            if let Resolution::KeepBoth(name) = &r {
                self.used_names.insert(name.clone());
            }
            self.decisions.push((item.source, r));
        }
    }

    pub fn resolve_all_with(&mut self, r: Resolution) {
        debug_assert!(matches!(r, Resolution::Replace | Resolution::Skip));
        let drained: Vec<_> = self.pending.drain(..).collect();
        for item in drained {
            self.decisions.push((item.source, r.clone()));
        }
    }

    pub fn into_decisions(self) -> Vec<(PathBuf, Resolution)> {
        self.decisions
    }
}

pub fn used_names_after_auto_decisions(
    existing: &HashSet<String>,
    auto_decisions: &[(PathBuf, Resolution)],
) -> HashSet<String> {
    let mut used = existing.clone();
    for (source, resolution) in auto_decisions {
        match resolution {
            Resolution::KeepBoth(name) => {
                used.insert(name.clone());
            }
            Resolution::Replace => {
                if let Some(name) = source.file_name().and_then(|name| name.to_str()) {
                    used.insert(name.to_string());
                }
            }
            Resolution::Skip | Resolution::Cancel => {}
        }
    }
    used
}

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

pub fn next_copy_name(orig: &str, existing: &HashSet<String>) -> String {
    let (stem, ext) = split_stem_ext(orig);
    let first = join_stem_ext(&format!("{stem} copy"), ext);
    if !existing.contains(first.as_str()) {
        return first;
    }
    let mut n = 2;
    loop {
        let candidate = join_stem_ext(&format!("{stem} copy {n}"), ext);
        if !existing.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNameError {
    InvalidName,
    IllegalChars,
    NameAlreadyExists(String),
}

pub fn sanitize_filename(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

pub fn validate_basename(raw: &str) -> Result<String, FileNameError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(FileNameError::InvalidName);
    }
    if trimmed == "." || trimmed == ".." {
        return Err(FileNameError::InvalidName);
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(FileNameError::IllegalChars);
    }
    if trimmed.contains('\0') {
        return Err(FileNameError::IllegalChars);
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(FileNameError::IllegalChars);
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> HashSet<String> {
        items.iter().map(|x| x.to_string()).collect()
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

    fn confl(src: &str, dest: &str) -> ConflictItem {
        ConflictItem {
            source: PathBuf::from(src),
            existing_at_dest: PathBuf::from(dest),
        }
    }

    fn prompt(
        auto_decisions: Vec<(PathBuf, Resolution)>,
        pending: Vec<ConflictItem>,
    ) -> PasteConflictPrompt {
        PasteConflictPrompt::new(
            ClipMode::Copy,
            PathBuf::from("dst"),
            auto_decisions,
            pending,
            s(&[]),
        )
    }

    #[test]
    fn prompt_with_no_conflicts_is_done_immediately() {
        let p = prompt(vec![(PathBuf::from("a"), Resolution::Replace)], vec![]);
        assert!(p.is_done());
        assert_eq!(p.into_decisions().len(), 1);
    }

    #[test]
    fn prompt_walks_pending_one_at_a_time() {
        let mut p = prompt(
            vec![],
            vec![confl("src/a", "dst/a"), confl("src/b", "dst/b")],
        );
        assert_eq!(p.current().unwrap().source, PathBuf::from("src/a"));
        p.resolve_one(Resolution::Replace);
        assert_eq!(p.current().unwrap().source, PathBuf::from("src/b"));
        p.resolve_one(Resolution::Skip);
        assert!(p.is_done());
        assert_eq!(
            p.into_decisions(),
            vec![
                (PathBuf::from("src/a"), Resolution::Replace),
                (PathBuf::from("src/b"), Resolution::Skip),
            ]
        );
    }

    #[test]
    fn prompt_apply_to_all_drains_remainder() {
        let mut p = prompt(
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
        let mut p = prompt(
            vec![(PathBuf::from("seed"), Resolution::Replace)],
            vec![confl("src/a", "dst/a"), confl("src/b", "dst/b")],
        );
        p.resolve_one(Resolution::Cancel);
        assert!(p.is_done());
        assert!(p.was_cancelled());
        assert_eq!(p.into_decisions().len(), 1);
    }

    #[test]
    fn self_descent_rejects_prefix_sibling() {
        assert!(!would_be_self_descent(
            Path::new("src"),
            Path::new("srcother")
        ));
    }

    #[test]
    fn classify_same_dir_copy_auto_renames() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let cls = classify_paste(ClipMode::Copy, Path::new("src"), &sources, &s(&["a.txt"]));
        assert!(cls.pending.is_empty());
        assert_eq!(cls.auto_decisions.len(), 1);
        assert_eq!(
            cls.auto_decisions[0].1,
            Resolution::KeepBoth("a copy.txt".to_string())
        );
    }

    #[test]
    fn classify_same_dir_cut_is_noop() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let cls = classify_paste(ClipMode::Cut, Path::new("src"), &sources, &s(&["a.txt"]));
        assert!(cls.auto_decisions.is_empty());
        assert!(cls.pending.is_empty());
        assert_eq!(cls.same_dir_cut_skipped, 1);
    }

    #[test]
    fn classify_cross_dir_conflict_goes_to_pending() {
        let sources = vec![PathBuf::from("src/a.txt")];
        let cls = classify_paste(ClipMode::Copy, Path::new("dst"), &sources, &s(&["a.txt"]));
        assert!(cls.auto_decisions.is_empty());
        assert_eq!(cls.pending.len(), 1);
        assert_eq!(cls.pending[0].existing_at_dest, PathBuf::from("dst/a.txt"));
    }

    #[test]
    fn classify_blocks_self_descent() {
        let sources = vec![PathBuf::from("src")];
        let cls = classify_paste(ClipMode::Copy, Path::new("src/sub"), &sources, &s(&[]));
        assert!(cls.auto_decisions.is_empty());
        assert!(cls.pending.is_empty());
        assert_eq!(cls.self_descent_blocked, 1);
    }

    #[test]
    fn sanitize_strips_control_chars_and_trims() {
        assert_eq!(sanitize_filename("  foo.rs  "), "foo.rs");
        assert_eq!(sanitize_filename("a\tb\nc"), "a?b?c");
        assert_eq!(sanitize_filename("中文.rs"), "中文.rs");
    }

    #[test]
    fn validate_rejects_empty_and_dot_names() {
        assert_eq!(validate_basename(""), Err(FileNameError::InvalidName));
        assert_eq!(validate_basename("   "), Err(FileNameError::InvalidName));
        assert_eq!(validate_basename("."), Err(FileNameError::InvalidName));
        assert_eq!(validate_basename(".."), Err(FileNameError::InvalidName));
    }

    #[test]
    fn validate_rejects_separators_nul_and_controls() {
        assert_eq!(
            validate_basename("foo/bar"),
            Err(FileNameError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\\bar"),
            Err(FileNameError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\0bar"),
            Err(FileNameError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\tbar"),
            Err(FileNameError::IllegalChars)
        );
    }

    #[test]
    fn validate_accepts_reasonable_names() {
        assert_eq!(validate_basename("foo.rs"), Ok("foo.rs".into()));
        assert_eq!(validate_basename("  foo.rs  "), Ok("foo.rs".into()));
        assert_eq!(validate_basename(".gitignore"), Ok(".gitignore".into()));
        assert_eq!(validate_basename("中文.rs"), Ok("中文.rs".into()));
    }
}
