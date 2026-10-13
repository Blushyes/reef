use std::cmp::Ordering;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One direct child of a directory. Missing timestamps remain last in either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_dir: bool,
    pub has_children: bool,
    pub modified_millis: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileTreeSortField {
    #[default]
    Name,
    Kind,
    Modified,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileTreeSortDirection {
    #[default]
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTreeSort {
    pub field: FileTreeSortField,
    pub direction: FileTreeSortDirection,
    pub folders_first: bool,
}

impl Default for FileTreeSort {
    fn default() -> Self {
        Self {
            field: FileTreeSortField::Name,
            direction: FileTreeSortDirection::Ascending,
            folders_first: true,
        }
    }
}

impl FileTreeSort {
    pub fn sort_entries(self, entries: &mut [DirectoryEntry]) {
        entries.sort_by(|left, right| self.compare(left, right));
    }

    pub fn compare(self, left: &DirectoryEntry, right: &DirectoryEntry) -> Ordering {
        if self.folders_first && left.is_dir != right.is_dir {
            return right.is_dir.cmp(&left.is_dir);
        }
        let order = match self.field {
            FileTreeSortField::Name => Ordering::Equal,
            FileTreeSortField::Kind => kind(left).cmp(&kind(right)),
            FileTreeSortField::Modified => match (left.modified_millis, right.modified_millis) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
        }
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name));
        match self.direction {
            FileTreeSortDirection::Ascending => order,
            FileTreeSortDirection::Descending => order.reverse(),
        }
    }
}

fn kind(entry: &DirectoryEntry) -> (bool, String) {
    (
        !entry.is_dir,
        if entry.is_dir {
            String::new()
        } else {
            Path::new(&entry.name)
                .extension()
                .map(|extension| extension.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<DirectoryEntry> {
        [
            ("z-dir", true, Some(10)),
            ("a-dir", true, Some(30)),
            ("b.rs", false, Some(20)),
            ("a.txt", false, Some(40)),
            ("unknown", false, None),
        ]
        .into_iter()
        .map(|(name, is_dir, modified_millis)| DirectoryEntry {
            name: name.into(),
            is_dir,
            has_children: is_dir,
            modified_millis,
        })
        .collect()
    }

    #[test]
    fn name_sort_interleaves_directories_and_files() {
        let mut entries = entries();
        FileTreeSort {
            direction: FileTreeSortDirection::Descending,
            folders_first: false,
            ..Default::default()
        }
        .sort_entries(&mut entries);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["z-dir", "unknown", "b.rs", "a.txt", "a-dir"]
        );
    }

    #[test]
    fn modified_sort_orders_directories_and_files_with_unknown_times_last() {
        for (direction, folders_first, expected) in [
            (
                FileTreeSortDirection::Ascending,
                false,
                ["z-dir", "b.rs", "a-dir", "a.txt", "unknown"],
            ),
            (
                FileTreeSortDirection::Descending,
                false,
                ["a.txt", "a-dir", "b.rs", "z-dir", "unknown"],
            ),
            (
                FileTreeSortDirection::Ascending,
                true,
                ["z-dir", "a-dir", "b.rs", "a.txt", "unknown"],
            ),
            (
                FileTreeSortDirection::Descending,
                true,
                ["a-dir", "z-dir", "a.txt", "b.rs", "unknown"],
            ),
        ] {
            let mut entries = entries();
            FileTreeSort {
                field: FileTreeSortField::Modified,
                direction,
                folders_first,
            }
            .sort_entries(&mut entries);
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn kind_sort_groups_extensions_and_uses_names_for_directories() {
        let mut entries = entries();
        FileTreeSort {
            field: FileTreeSortField::Kind,
            ..Default::default()
        }
        .sort_entries(&mut entries);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["a-dir", "z-dir", "unknown", "b.rs", "a.txt"]
        );
    }
}
