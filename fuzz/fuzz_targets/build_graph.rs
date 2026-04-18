#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use reef_host::git::CommitInfo;
use reef_host::git::graph::build_graph;

#[derive(Debug, Arbitrary)]
struct FuzzCommit {
    oid_bytes: Vec<u8>,
    parent_indices: Vec<u16>,
}

fuzz_target!(|commits: Vec<FuzzCommit>| {
    // Cap size so each iteration is fast enough to explore broadly.
    let n = commits.len().min(200);
    let mut infos: Vec<CommitInfo> = Vec::with_capacity(n);

    for (i, c) in commits.iter().take(n).enumerate() {
        let oid = format!(
            "c{}_{}",
            i,
            String::from_utf8_lossy(&c.oid_bytes).chars().take(4).collect::<String>()
        );
        // Parents must point to later commits (topological order) and exist in
        // this slice — mirror the assumption `build_graph` documents.
        let mut parents: Vec<String> = Vec::new();
        let remaining = n.saturating_sub(i + 1);
        if remaining > 0 {
            for pi in c.parent_indices.iter().take(3) {
                let off = (*pi as usize) % remaining;
                parents.push(format!("c{}_", i + 1 + off));
            }
        }
        parents.sort();
        parents.dedup();
        infos.push(CommitInfo {
            oid,
            short_oid: String::new(),
            parents,
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            subject: String::new(),
        });
    }

    let _ = build_graph(&infos);
});
