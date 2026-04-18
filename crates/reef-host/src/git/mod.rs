pub mod graph;
pub mod tree;

use git2::{DiffOptions, Repository, Sort, StatusOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub status: FileStatus,
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
    use super::FileStatus;

    #[test]
    fn file_status_label_all_variants() {
        assert_eq!(FileStatus::Modified.label(), "M");
        assert_eq!(FileStatus::Added.label(), "A");
        assert_eq!(FileStatus::Deleted.label(), "D");
        assert_eq!(FileStatus::Renamed.label(), "R");
        assert_eq!(FileStatus::Untracked.label(), "U");
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

        for entry in statuses.iter() {
            let path = match entry.path() {
                Some(p) => p.to_string(),
                None => continue,
            };
            let status = entry.status();

            // Staged changes (index vs HEAD)
            if status.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED,
            ) {
                let file_status = if status.contains(git2::Status::INDEX_NEW) {
                    FileStatus::Added
                } else if status.contains(git2::Status::INDEX_MODIFIED) {
                    FileStatus::Modified
                } else if status.contains(git2::Status::INDEX_DELETED) {
                    FileStatus::Deleted
                } else {
                    FileStatus::Renamed
                };
                staged.push(FileEntry {
                    path: path.clone(),
                    status: file_status,
                });
            }

            // Unstaged changes (workdir vs index)
            if status.intersects(
                git2::Status::WT_MODIFIED
                    | git2::Status::WT_DELETED
                    | git2::Status::WT_NEW
                    | git2::Status::WT_RENAMED,
            ) {
                let file_status = if status.contains(git2::Status::WT_NEW) {
                    FileStatus::Untracked
                } else if status.contains(git2::Status::WT_MODIFIED) {
                    FileStatus::Modified
                } else if status.contains(git2::Status::WT_DELETED) {
                    FileStatus::Deleted
                } else {
                    FileStatus::Renamed
                };
                unstaged.push(FileEntry {
                    path,
                    status: file_status,
                });
            }
        }

        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));

        (staged, unstaged)
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

    /// Push the current branch to its upstream. When `force` is true, uses
    /// `--force-with-lease` (safer than `--force`: rejects the push if the
    /// remote advanced since our last fetch, preventing accidental overwrites
    /// of work pushed by collaborators we didn't know about).
    ///
    /// Shells out to the `git` binary because libgit2's push requires
    /// credential handling (SSH agent, keychain, credential helpers) that
    /// would otherwise need reimplementing. `git push` respects the user's
    /// existing git config and works identically to running it manually.
    pub fn push(&self, force: bool) -> Result<(), String> {
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| "no workdir (bare repo?)".to_string())?;
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
                    files.push(FileEntry { path, status });
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
