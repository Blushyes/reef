use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

const MRU_SEP: char = '\t';

pub const MRU_MAX: usize = 50;
pub const MRU_PREF_KEY: &str = "quickopen.mru";

pub struct QuickOpenCandidate {
    pub rel_path: PathBuf,
    pub display: String,
    utf32: Utf32String,
}

impl std::fmt::Debug for QuickOpenCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickOpenCandidate")
            .field("rel_path", &self.rel_path)
            .field("display", &self.display)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickOpenMatch {
    pub idx: usize,
    pub score: u32,
    pub indices: Vec<u32>,
}

pub fn build_candidates(paths: impl IntoIterator<Item = String>) -> Vec<QuickOpenCandidate> {
    let mut out = Vec::new();
    for display in paths {
        if display.is_empty() {
            continue;
        }
        let utf32 = Utf32String::from(display.as_str());
        out.push(QuickOpenCandidate {
            rel_path: PathBuf::from(&display),
            display,
            utf32,
        });
    }
    out
}

pub fn filter_candidates(
    candidates: &[QuickOpenCandidate],
    query: &str,
    mru: &VecDeque<PathBuf>,
) -> Vec<QuickOpenMatch> {
    if query.is_empty() {
        let mut matches = Vec::with_capacity(candidates.len());
        let mut seen: HashSet<usize> = HashSet::new();
        for path in mru {
            if let Some(idx) = candidates.iter().position(|c| &c.rel_path == path) {
                matches.push(QuickOpenMatch {
                    idx,
                    score: 0,
                    indices: Vec::new(),
                });
                seen.insert(idx);
            }
        }
        for idx in 0..candidates.len() {
            if !seen.contains(&idx) {
                matches.push(QuickOpenMatch {
                    idx,
                    score: 0,
                    indices: Vec::new(),
                });
            }
        }
        return matches;
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = Vec::new();
    for (idx, cand) in candidates.iter().enumerate() {
        let mut indices = Vec::new();
        if let Some(score) = pattern.indices(cand.utf32.slice(..), &mut matcher, &mut indices) {
            matches.push(QuickOpenMatch {
                idx,
                score,
                indices,
            });
        }
    }
    matches.sort_by(|a, b| {
        b.score.cmp(&a.score).then_with(|| {
            let la = candidates[a.idx].display.len();
            let lb = candidates[b.idx].display.len();
            la.cmp(&lb)
                .then_with(|| candidates[a.idx].display.cmp(&candidates[b.idx].display))
        })
    });
    matches
}

pub fn bump_mru(mru: &mut VecDeque<PathBuf>, selected: PathBuf, cap: usize) {
    mru.retain(|p| p != &selected);
    mru.push_front(selected);
    while mru.len() > cap {
        mru.pop_back();
    }
}

pub fn decode_mru(raw: &str) -> VecDeque<PathBuf> {
    raw.split(MRU_SEP)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn encode_mru(mru: &VecDeque<PathBuf>) -> String {
    mru.iter()
        .map(|p| p.to_string_lossy().replace(['\t', '\n'], " "))
        .collect::<Vec<_>>()
        .join(&MRU_SEP.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(paths: &[&str]) -> Vec<QuickOpenCandidate> {
        build_candidates(paths.iter().map(|s| s.to_string()))
    }

    #[test]
    fn empty_query_returns_mru_first_then_rest() {
        let c = candidates(&["a.rs", "b.rs", "c.rs"]);
        let mru = VecDeque::from([PathBuf::from("c.rs"), PathBuf::from("missing.rs")]);
        let out = filter_candidates(&c, "", &mru);
        assert_eq!(out.iter().map(|m| m.idx).collect::<Vec<_>>(), vec![2, 0, 1]);
    }

    #[test]
    fn fuzzy_filter_sorts_by_score_then_shorter_path() {
        let c = candidates(&["deep/path/foo.rs", "foo.rs"]);
        let out = filter_candidates(&c, "foo", &VecDeque::new());
        assert_eq!(c[out[0].idx].display, "foo.rs");
    }

    #[test]
    fn bump_mru_moves_selected_to_front_and_caps() {
        let mut mru = VecDeque::from([PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")]);
        bump_mru(&mut mru, PathBuf::from("b"), 2);
        assert_eq!(
            mru.into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("b"), PathBuf::from("a")]
        );
    }

    #[test]
    fn mru_encode_sanitizes_and_decodes() {
        let mru = VecDeque::from([PathBuf::from("a\tb"), PathBuf::from("c\nd")]);
        let encoded = encode_mru(&mru);
        assert_eq!(encoded, "a b\tc d");
        assert_eq!(
            decode_mru(&encoded).into_iter().collect::<Vec<_>>(),
            vec![PathBuf::from("a b"), PathBuf::from("c d")]
        );
    }
}
