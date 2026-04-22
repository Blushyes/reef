pub mod graph;
pub mod tree;

use git2::{DiffOptions, Repository, Sort, StatusOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub status: FileStatus,
    /// Lines added in this file for the relevant diff (HEAD→index for staged,
    /// index→workdir for unstaged; whole-file line count for untracked).
    /// Populated by [`GitRepo::get_status`]; `commit_files` leaves this at 0.
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

impl FileStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FileStatus, count_workdir_lines};
    use std::io::Write;

    #[test]
    fn file_status_label_all_variants() {
        assert_eq!(FileStatus::Modified.label(), "M");
        assert_eq!(FileStatus::Added.label(), "A");
        assert_eq!(FileStatus::Deleted.label(), "D");
        assert_eq!(FileStatus::Renamed.label(), "R");
        assert_eq!(FileStatus::Untracked.label(), "U");
    }

    fn write_tmp(bytes: &[u8]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = "f.txt";
        let mut f = std::fs::File::create(dir.path().join(name)).expect("create");
        f.write_all(bytes).expect("write");
        (dir, name.to_string())
    }

    #[test]
    fn count_workdir_lines_empty_file() {
        let (dir, name) = write_tmp(b"");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 0);
    }

    #[test]
    fn count_workdir_lines_single_line_no_trailing_newline() {
        let (dir, name) = write_tmp(b"hello");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 1);
    }

    #[test]
    fn count_workdir_lines_single_line_with_trailing_newline() {
        let (dir, name) = write_tmp(b"hello\n");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 1);
    }

    #[test]
    fn count_workdir_lines_multi_line() {
        let (dir, name) = write_tmp(b"a\nb\nc\n");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 3);
    }

    #[test]
    fn count_workdir_lines_just_newline() {
        // "\n" is a single blank line — matches `str::lines()` ("" has 0, "\n" has 1).
        let (dir, name) = write_tmp(b"\n");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 1);
    }

    #[test]
    fn count_workdir_lines_binary_nul_short_circuits() {
        let (dir, name) = write_tmp(b"abc\x00def\n");
        assert_eq!(count_workdir_lines(Some(dir.path()), &name), 0);
    }

    #[test]
    fn count_workdir_lines_missing_file_is_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(count_workdir_lines(Some(dir.path()), "nope.txt"), 0);
    }

    #[test]
    fn count_workdir_lines_no_workdir_is_zero() {
        assert_eq!(count_workdir_lines(None, "whatever.txt"), 0);
    }
}

#[derive(Debug, Clone)]
pub struct DiffContent {
    pub file_path: String,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: LineTag,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTag {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub oid: String,
    pub short_oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub time: i64,
    pub subject: String,
}

#[derive(Debug, Clone)]
pub struct CommitDetail {
    pub info: CommitInfo,
    pub message: String,
    pub committer_name: String,
    pub committer_time: i64,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefLabel {
    Head,
    Branch(String),
    RemoteBranch(String),
    Tag(String),
}

pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    pub fn open() -> Result<Self, git2::Error> {
        let repo = Repository::discover(".")?;
        Ok(Self { repo })
    }

    pub fn open_at(workdir: &Path) -> Result<Self, git2::Error> {
        let repo = Repository::discover(workdir)?;
        Ok(Self { repo })
    }

    pub fn workdir_path(&self) -> Option<PathBuf> {
        self.repo.workdir().map(|p| p.to_path_buf())
    }

    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }

    pub fn gitdir(&self) -> &Path {
        self.repo.path()
    }

    pub fn branch_name(&self) -> String {
        self.repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(String::from))
            .unwrap_or_else(|| "(detached)".into())
    }

    pub fn workdir_name(&self) -> String {
        self.repo
            .workdir()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| "repo".into())
    }

    pub fn get_status(&self) -> (Vec<FileEntry>, Vec<FileEntry>) {
        let mut staged = Vec::new();
        let mut unstaged = Vec::new();

        // Force-reload index from disk so we always see the latest staged changes.
        // Without this, git2's in-memory index cache can lag behind what was just written.
        if let Ok(mut idx) = self.repo.index() {
            let _ = idx.read(true);
        }

        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .renames_head_to_index(true);

        let statuses = match self.repo.statuses(Some(&mut opts)) {
            Ok(s) => s,
            Err(_) => return (staged, unstaged),
        };

        // Per-file line add/remove counts. Two diffs cover staged and
        // unstaged separately so each section can show its own numbers
        // (e.g. a file modified in index then re-edited in workdir has a
        // distinct +/- on each side).
        let staged_stats = self.diff_line_counts_staged();
        let unstaged_stats = self.diff_line_counts_unstaged();

        for entry in statuses.iter() {
            let status = entry.status();
            let fallback_path = entry.path().map(str::to_string);

            // Staged changes (index vs HEAD). Check RENAMED before MODIFIED
            // because libgit2 surfaces renamed+edited files with both flags
            // set and `entry.path()` keyed on the *old* path — only the
            // `head_to_index` delta carries the new (post-rename) path,
            // which is what the index and our diffstat key on.
            if status.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED,
            ) {
                let (file_status, path) = if status.contains(git2::Status::INDEX_RENAMED) {
                    let new_path = entry
                        .head_to_index()
                        .and_then(|d| {
                            d.new_file()
                                .path()
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                        .or_else(|| fallback_path.clone());
                    (FileStatus::Renamed, new_path)
                } else if status.contains(git2::Status::INDEX_NEW) {
                    (FileStatus::Added, fallback_path.clone())
                } else if status.contains(git2::Status::INDEX_MODIFIED) {
                    (FileStatus::Modified, fallback_path.clone())
                } else {
                    (FileStatus::Deleted, fallback_path.clone())
                };
                if let Some(path) = path {
                    let (additions, deletions) = staged_stats.get(&path).copied().unwrap_or((0, 0));
                    staged.push(FileEntry {
                        path,
                        status: file_status,
                        additions,
                        deletions,
                    });
                }
            }

            // Unstaged changes (workdir vs index). Same ordering rationale
            // as above — renamed files in the workdir show WT_RENAMED
            // alongside WT_MODIFIED.
            if status.intersects(
                git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_NEW
                    | git2::Status::WT_RENAMED,
            ) {
                let (file_status, path) = if status.contains(git2::Status::WT_RENAMED) {
                    let new_path = entry
                        .index_to_workdir()
                        .and_then(|d| {
                            d.new_file()
                                .path()
                                .map(|p| p.to_string_lossy().into_owned())
                        })
                        .or_else(|| fallback_path.clone());
                    (FileStatus::Renamed, new_path)
                } else if status.contains(git2::Status::WT_NEW) {
                    (FileStatus::Untracked, fallback_path.clone())
                } else if status.contains(git2::Status::WT_MODIFIED) {
                    (FileStatus::Modified, fallback_path.clone())
                } else {
                    (FileStatus::Deleted, fallback_path.clone())
                };
                if let Some(path) = path {
                    // Untracked paths don't appear in the index→workdir diff;
                    // count their lines directly so the +N column is still useful.
                    let (additions, deletions) = if matches!(file_status, FileStatus::Untracked) {
                        (count_workdir_lines(self.repo.workdir(), &path), 0)
                    } else {
                        unstaged_stats.get(&path).copied().unwrap_or((0, 0))
                    };
                    unstaged.push(FileEntry {
                        path,
                        status: file_status,
                        additions,
                        deletions,
                    });
                }
            }
        }

        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));

        (staged, unstaged)
    }

    fn diff_line_counts_staged(&self) -> HashMap<String, (u32, u32)> {
        let head_tree = self.repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        let mut diff = match self.repo.diff_tree_to_index(head_tree.as_ref(), None, None) {
            Ok(d) => d,
            Err(_) => return HashMap::new(),
        };
        merge_renames(&mut diff);
        collect_diff_line_counts(&diff)
    }

    fn diff_line_counts_unstaged(&self) -> HashMap<String, (u32, u32)> {
        let mut diff = match self.repo.diff_index_to_workdir(None, None) {
            Ok(d) => d,
            Err(_) => return HashMap::new(),
        };
        merge_renames(&mut diff);
        collect_diff_line_counts(&diff)
    }

    pub fn get_diff(&self, path: &str, staged: bool, context_lines: u32) -> Option<DiffContent> {
        if staged {
            self.get_staged_diff(path, context_lines)
        } else {
            self.get_unstaged_diff(path, context_lines)
        }
    }

    fn get_staged_diff(&self, path: &str, context_lines: u32) -> Option<DiffContent> {
        // Force-reload index so we see writes from a concurrent external
        // `git add`, and so our own index.write() from stage_file is picked
        // up without needing to reopen the repo.
        if let Ok(mut idx) = self.repo.index() {
            let _ = idx.read(true);
        }

        // Staged: compare HEAD to index
        let head_tree = self.repo.head().ok()?.peel_to_tree().ok();
        let mut opts = DiffOptions::new();
        opts.pathspec(path).context_lines(context_lines);

        let diff = self
            .repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
            .ok()?;

        self.parse_git2_diff(&diff, path)
    }

    fn get_unstaged_diff(&self, path: &str, context_lines: u32) -> Option<DiffContent> {
        // Force-reload index so we see writes from a concurrent external
        // `git add`, and so our own index.write() from stage_file is picked
        // up without needing to reopen the repo.
        if let Ok(mut idx) = self.repo.index() {
            let _ = idx.read(true);
        }

        // Check if file is untracked — use similar for full-file diff
        let statuses = self.repo.statuses(None).ok()?;
        for entry in statuses.iter() {
            if entry.path() == Some(path) && entry.status().contains(git2::Status::WT_NEW) {
                return self.get_untracked_diff(path);
            }
        }

        // Unstaged: compare index to workdir
        let mut opts = DiffOptions::new();
        opts.pathspec(path).context_lines(context_lines);

        let diff = self
            .repo
            .diff_index_to_workdir(None, Some(&mut opts))
            .ok()?;

        self.parse_git2_diff(&diff, path)
    }

    fn get_untracked_diff(&self, path: &str) -> Option<DiffContent> {
        let workdir = self.repo.workdir()?;
        let full_path = workdir.join(path);
        let content = std::fs::read_to_string(&full_path).ok()?;

        let lines: Vec<DiffLine> = content
            .lines()
            .enumerate()
            .map(|(i, line)| DiffLine {
                tag: LineTag::Added,
                content: line.to_string(),
                old_lineno: None,
                new_lineno: Some(i as u32 + 1),
            })
            .collect();

        Some(DiffContent {
            file_path: path.to_string(),
            hunks: vec![DiffHunk {
                header: format!("@@ -0,0 +1,{} @@ (new file)", lines.len()),
                lines,
            }],
        })
    }

    fn parse_git2_diff(&self, diff: &git2::Diff, path: &str) -> Option<DiffContent> {
        let mut hunks = Vec::new();

        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            let delta_path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("");

            if delta_path != path {
                return true;
            }

            match line.origin() {
                '+' | '-' | ' ' => {
                    let tag = match line.origin() {
                        '+' => LineTag::Added,
                        '-' => LineTag::Removed,
                        _ => LineTag::Context,
                    };
                    let content = String::from_utf8_lossy(line.content()).to_string();
                    // Trim trailing newline
                    let content = content.trim_end_matches('\n').to_string();

                    let diff_line = DiffLine {
                        tag,
                        content,
                        old_lineno: line.old_lineno(),
                        new_lineno: line.new_lineno(),
                    };

                    if let Some(last_hunk) = hunks.last_mut() {
                        let hunk_ref: &mut DiffHunk = last_hunk;
                        hunk_ref.lines.push(diff_line);
                    }
                }
                'H' => {
                    // Hunk header
                    let header = String::from_utf8_lossy(line.content()).trim().to_string();
                    hunks.push(DiffHunk {
                        header,
                        lines: Vec::new(),
                    });
                }
                _ => {}
            }
            true
        })
        .ok()?;

        if hunks.is_empty() {
            return None;
        }

        Some(DiffContent {
            file_path: path.to_string(),
            hunks,
        })
    }

    pub fn stage_file(&self, path: &str) -> Result<(), git2::Error> {
        let mut index = self.repo.index()?;

        // Check if the file exists in workdir
        let workdir = self.repo.workdir().unwrap();
        let full_path = workdir.join(path);

        if full_path.exists() {
            index.add_path(Path::new(path))?;
        } else {
            // File was deleted
            index.remove_path(Path::new(path))?;
        }
        index.write()?;
        Ok(())
    }

    pub fn unstage_file(&self, path: &str) -> Result<(), git2::Error> {
        let head = self.repo.head();

        match head {
            Ok(head_ref) => {
                let head_commit = head_ref.peel_to_commit()?;
                self.repo
                    .reset_default(Some(head_commit.as_object()), [path])?;
            }
            Err(_) => {
                // No HEAD (initial commit) — remove from index
                let mut index = self.repo.index()?;
                index.remove_path(Path::new(path))?;
                index.write()?;
            }
        }
        Ok(())
    }

    /// Restore a working-tree file to its HEAD state (like `git restore <file>`).
    /// For untracked files that have no HEAD counterpart, the file is deleted.
    /// Does not touch the index.
    pub fn restore_file(&self, path: &str) -> Result<(), git2::Error> {
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| git2::Error::from_str("no workdir"))?;

        let in_head = self
            .repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .and_then(|c| c.tree().ok())
            .and_then(|t| t.get_path(Path::new(path)).ok())
            .is_some();

        if in_head {
            let head_tree = self.repo.head()?.peel_to_commit()?.tree()?;
            let mut opts = git2::build::CheckoutBuilder::new();
            opts.force().update_index(false).path(path);
            self.repo
                .checkout_tree(head_tree.as_object(), Some(&mut opts))
        } else {
            std::fs::remove_file(workdir.join(path))
                .map_err(|e| git2::Error::from_str(&e.to_string()))
        }
    }

    // ── remote sync state / push ───────────────────────────────────────────

    /// Returns `(ahead, behind)` commit counts of the current branch vs. its
    /// upstream. Returns `None` when HEAD is detached, the branch has no
    /// upstream configured, or any lookup fails. Does NOT perform a network
    /// fetch — the `behind` count reflects the last-fetched state of the
    /// upstream ref.
    pub fn ahead_behind(&self) -> Option<(usize, usize)> {
        let head = self.repo.head().ok()?;
        let head_oid = head.target()?;
        let shorthand = head.shorthand()?;
        let branch = self
            .repo
            .find_branch(shorthand, git2::BranchType::Local)
            .ok()?;
        let upstream = branch.upstream().ok()?;
        let upstream_oid = upstream.get().target()?;
        self.repo.graph_ahead_behind(head_oid, upstream_oid).ok()
    }

    /// Push the current branch to its upstream. Thin wrapper around
    /// [`push_at`] — the free function is what the App's background-push
    /// thread actually calls, because `GitRepo` holds a non-Send libgit2
    /// handle and can't cross thread boundaries.
    pub fn push(&self, force: bool) -> Result<(), String> {
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| "no workdir (bare repo?)".to_string())?;
        push_at(workdir, force)
    }
}

/// Ask libgit2 to collapse `(delete old, add new)` pairs whose contents are
/// similar into a single `Renamed` delta. Matches `StatusOptions.renames_*`
/// so the line counts keyed by `new_file().path()` line up with the
/// `FileStatus::Renamed` entry the sidebar shows. Copy detection is
/// intentionally off — it's O(n²) over unchanged files and `git status`
/// itself doesn't enable it. Failure (e.g. the diff holds no renameable
/// pairs) is ignored; the unmerged diff still yields correct `+N -M` for
/// non-rename files.
fn merge_renames(diff: &mut git2::Diff) {
    let mut opts = git2::DiffFindOptions::new();
    opts.renames(true);
    let _ = diff.find_similar(Some(&mut opts));
}

/// Walk the patch text of `diff` and tally `+` / `-` lines per file path.
/// Used by [`GitRepo::get_status`] to show `+N -M` next to each file row.
/// Binary files produce zero counts here (libgit2 emits no line callbacks
/// for them) — the same behavior you'd see from `git diff --numstat`.
fn collect_diff_line_counts(diff: &git2::Diff) -> HashMap<String, (u32, u32)> {
    let mut out: HashMap<String, (u32, u32)> = HashMap::new();
    let _ = diff.foreach(
        &mut |_, _| true,
        None,
        None,
        Some(&mut |delta, _hunk, line| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .unwrap_or("");
            if path.is_empty() {
                return true;
            }
            let entry = out.entry(path.to_string()).or_insert((0, 0));
            match line.origin() {
                '+' => entry.0 += 1,
                '-' => entry.1 += 1,
                _ => {}
            }
            true
        }),
    );
    out
}

/// Count newline-delimited lines in an untracked workdir file. Streams via
/// `BufReader::read_until` so a large untracked log doesn't get slurped into
/// memory. Bails early with 0 on a NUL byte (binary file — mirrors how
/// `git diff` suppresses numstat for binaries). Unreadable / missing files
/// also return 0 instead of propagating. Counting semantics follow
/// `str::lines()`: an unterminated trailing line still counts once.
fn count_workdir_lines(workdir: Option<&Path>, path: &str) -> u32 {
    use std::io::{BufRead, BufReader};
    let Some(root) = workdir else {
        return 0;
    };
    let Ok(file) = std::fs::File::open(root.join(path)) else {
        return 0;
    };
    let mut reader = BufReader::new(file);
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut count: u32 = 0;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return count,
            Ok(_) => {
                if buf.contains(&0) {
                    return 0;
                }
                count = count.saturating_add(1);
            }
            Err(_) => return 0,
        }
    }
}

/// Push the branch at `workdir` to its upstream. When `force` is true, uses
/// `--force-with-lease` (safer than `--force`: rejects the push if the
/// remote advanced since our last fetch, preventing accidental overwrites
/// of work pushed by collaborators we didn't know about).
///
/// Shells out to the `git` binary because libgit2's push requires
/// credential handling (SSH agent, keychain, credential helpers) that
/// would otherwise need reimplementing. `git push` respects the user's
/// existing git config and works identically to running it manually.
///
/// This is a free function (not a `GitRepo` method) because it's invoked
/// from a background thread after `run_push` spawns one — `GitRepo` holds
/// a `git2::Repository`, which isn't `Send`.
pub fn push_at(workdir: &Path, force: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(workdir).arg("push");
    if force {
        cmd.arg("--force-with-lease");
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "push failed".to_string()
        } else {
            err
        });
    }
    Ok(())
}

impl GitRepo {
    // ── commit history / refs ──────────────────────────────────────────────

    pub fn head_oid(&self) -> Option<String> {
        self.repo
            .head()
            .ok()?
            .peel_to_commit()
            .ok()
            .map(|c| c.id().to_string())
    }

    /// Walk up to `limit` commits reachable from all branches/tags/HEAD, in
    /// topological + time order (child before parent, newest first).
    pub fn list_commits(&self, limit: usize) -> Vec<CommitInfo> {
        let Ok(mut walk) = self.repo.revwalk() else {
            return Vec::new();
        };
        let _ = walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME);

        // Seed from local branches, remote branches, tags (peeled), and HEAD.
        // push_glob dereferences annotated tags; non-commit targets are skipped.
        let _ = walk.push_glob("refs/heads/*");
        let _ = walk.push_glob("refs/remotes/*");
        let _ = walk.push_glob("refs/tags/*");
        let _ = walk.push_head();

        let mut out = Vec::with_capacity(limit.min(256));
        for oid in walk.flatten().take(limit) {
            let Ok(commit) = self.repo.find_commit(oid) else {
                continue;
            };
            out.push(commit_to_info(&commit));
        }
        out
    }

    /// Map oid (hex) → list of ref labels pointing at that commit. HEAD is
    /// inserted separately using the current HEAD commit oid.
    pub fn list_refs(&self) -> HashMap<String, Vec<RefLabel>> {
        let mut map: HashMap<String, Vec<RefLabel>> = HashMap::new();

        if let Ok(refs) = self.repo.references() {
            for r in refs.flatten() {
                let Some(oid) = r
                    .peel(git2::ObjectType::Commit)
                    .ok()
                    .map(|o| o.id().to_string())
                else {
                    continue;
                };
                let Some(name) = r.name() else { continue };

                let label = if let Some(rest) = name.strip_prefix("refs/heads/") {
                    RefLabel::Branch(rest.to_string())
                } else if let Some(rest) = name.strip_prefix("refs/remotes/") {
                    // Skip HEAD symlinks like `origin/HEAD` — they duplicate another branch.
                    if rest.ends_with("/HEAD") {
                        continue;
                    }
                    RefLabel::RemoteBranch(rest.to_string())
                } else if let Some(rest) = name.strip_prefix("refs/tags/") {
                    RefLabel::Tag(rest.to_string())
                } else {
                    continue;
                };

                map.entry(oid).or_default().push(label);
            }
        }

        if let Some(head_oid) = self.head_oid() {
            map.entry(head_oid).or_default().insert(0, RefLabel::Head);
        }

        map
    }

    pub fn get_commit(&self, oid_str: &str) -> Option<CommitDetail> {
        let oid = git2::Oid::from_str(oid_str).ok()?;
        let commit = self.repo.find_commit(oid).ok()?;
        let info = commit_to_info(&commit);
        let message = commit.message().unwrap_or("").to_string();
        let committer = commit.committer();
        let committer_name = committer.name().unwrap_or("").to_string();
        let committer_time = committer.when().seconds();

        let files = self.commit_files(&commit);

        Some(CommitDetail {
            info,
            message,
            committer_name,
            committer_time,
            files,
        })
    }

    pub fn get_commit_file_diff(
        &self,
        oid_str: &str,
        path: &str,
        context_lines: u32,
    ) -> Option<DiffContent> {
        let oid = git2::Oid::from_str(oid_str).ok()?;
        let commit = self.repo.find_commit(oid).ok()?;
        let new_tree = commit.tree().ok()?;
        // Merge commits: diff against first parent (simplest reasonable choice).
        let parent_tree = commit.parents().next().and_then(|p| p.tree().ok());

        let mut opts = DiffOptions::new();
        opts.pathspec(path).context_lines(context_lines);

        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(&mut opts))
            .ok()?;

        self.parse_git2_diff(&diff, path)
    }

    /// Compute the list of files changed by this commit (vs first parent, or
    /// vs empty tree for the initial commit).
    fn commit_files(&self, commit: &git2::Commit) -> Vec<FileEntry> {
        let Ok(new_tree) = commit.tree() else {
            return Vec::new();
        };
        let parent_tree = commit.parents().next().and_then(|p| p.tree().ok());

        let diff = match self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)
        {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        collect_files_from_diff(&diff)
    }

    /// Files changed by the range [oldest, newest] (inclusive) — computed as
    /// the net tree delta `parent(oldest).tree → newest.tree`. Merge commits
    /// (either endpoint or anywhere inside the range) follow first-parent, the
    /// same convention as single-commit diffs.
    pub fn get_range_files(&self, oldest_oid: &str, newest_oid: &str) -> Vec<FileEntry> {
        let Some(oldest) = git2::Oid::from_str(oldest_oid)
            .ok()
            .and_then(|o| self.repo.find_commit(o).ok())
        else {
            return Vec::new();
        };
        let Some(newest) = git2::Oid::from_str(newest_oid)
            .ok()
            .and_then(|o| self.repo.find_commit(o).ok())
        else {
            return Vec::new();
        };
        let Ok(new_tree) = newest.tree() else {
            return Vec::new();
        };
        // None = oldest is a root commit; diff against empty tree.
        let parent_tree = oldest.parents().next().and_then(|p| p.tree().ok());

        let diff = match self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)
        {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        collect_files_from_diff(&diff)
    }

    /// Single-file diff for a commit range: same semantics as
    /// `get_range_files` — `parent(oldest).tree → newest.tree`, first-parent
    /// on merges, empty-tree baseline if `oldest` has no parent.
    pub fn get_range_file_diff(
        &self,
        oldest_oid: &str,
        newest_oid: &str,
        path: &str,
        context_lines: u32,
    ) -> Option<DiffContent> {
        let oldest = self
            .repo
            .find_commit(git2::Oid::from_str(oldest_oid).ok()?)
            .ok()?;
        let newest = self
            .repo
            .find_commit(git2::Oid::from_str(newest_oid).ok()?)
            .ok()?;
        let new_tree = newest.tree().ok()?;
        let parent_tree = oldest.parents().next().and_then(|p| p.tree().ok());

        let mut opts = DiffOptions::new();
        opts.pathspec(path).context_lines(context_lines);

        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(&mut opts))
            .ok()?;

        self.parse_git2_diff(&diff, path)
    }
}

/// Shared `git2::Diff → Vec<FileEntry>` reducer. Keeps the delta→status map
/// and the trailing sort in one place so `commit_files` and `get_range_files`
/// stay structurally identical.
fn collect_files_from_diff(diff: &git2::Diff) -> Vec<FileEntry> {
    let mut files = Vec::new();
    diff.foreach(
        &mut |delta, _progress| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .and_then(|p| p.to_str())
                .map(String::from);
            if let Some(path) = path {
                let status = match delta.status() {
                    git2::Delta::Added => FileStatus::Added,
                    git2::Delta::Deleted => FileStatus::Deleted,
                    git2::Delta::Renamed | git2::Delta::Copied => FileStatus::Renamed,
                    git2::Delta::Untracked => FileStatus::Untracked,
                    _ => FileStatus::Modified,
                };
                files.push(FileEntry {
                    path,
                    status,
                    additions: 0,
                    deletions: 0,
                });
            }
            true
        },
        None,
        None,
        None,
    )
    .ok();

    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

fn commit_to_info(commit: &git2::Commit) -> CommitInfo {
    let oid = commit.id().to_string();
    let short_oid = oid.chars().take(7).collect::<String>();
    let parents = commit.parent_ids().map(|p| p.to_string()).collect();
    let author = commit.author();
    let author_name = author.name().unwrap_or("").to_string();
    let author_email = author.email().unwrap_or("").to_string();
    let time = author.when().seconds();
    let subject = commit.summary().unwrap_or("").to_string();
    CommitInfo {
        oid,
        short_oid,
        parents,
        author_name,
        author_email,
        time,
        subject,
    }
}
