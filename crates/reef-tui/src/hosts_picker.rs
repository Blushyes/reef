//! TUI prefs helpers for the shared hosts picker state.

use reef_app::{MAX_RECENT, RECENT_PREF_KEY, SshTarget};

/// Load the recent-targets list from prefs, cropped to `MAX_RECENT`.
pub fn load_recent() -> Vec<SshTarget> {
    let Some(raw) = crate::prefs::get(RECENT_PREF_KEY) else {
        return Vec::new();
    };
    raw.split('\t')
        .filter(|s| !s.is_empty())
        .take(MAX_RECENT)
        .map(SshTarget::parse)
        .collect()
}

/// Persist the recent list. Drops duplicates (most-recent wins) and
/// crops to `MAX_RECENT`.
pub fn save_recent(targets: &[SshTarget]) {
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<&SshTarget> = Vec::new();
    for t in targets {
        let key = t.to_arg();
        if seen.insert(key) {
            deduped.push(t);
        }
        if deduped.len() >= MAX_RECENT {
            break;
        }
    }
    let joined = deduped
        .iter()
        .map(|t| t.to_arg())
        .collect::<Vec<_>>()
        .join("\t");
    crate::prefs::set(RECENT_PREF_KEY, &joined);
}
