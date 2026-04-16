use git2::{DiffOptions, Repository, StatusOptions};
use std::path::Path;

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

pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    pub fn open() -> Result<Self, git2::Error> {
        let repo = Repository::discover(".")?;
        Ok(Self { repo })
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
        // Force-reload index — the plugin process may have written a new index.
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
        // Force-reload index — the plugin process may have written a new index.
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

    fn parse_git2_diff(
        &self,
        diff: &git2::Diff,
        path: &str,
    ) -> Option<DiffContent> {
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
                    let content =
                        String::from_utf8_lossy(line.content()).to_string();
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
                    let header = String::from_utf8_lossy(line.content())
                        .trim()
                        .to_string();
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
                self.repo.reset_default(
                    Some(head_commit.as_object()),
                    [path],
                )?;
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
}
