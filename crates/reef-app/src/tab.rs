use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppTab {
    Files,
    Search,
    Git,
    Graph,
}

impl AppTab {
    pub const ALL: [Self; 4] = [Self::Files, Self::Search, Self::Git, Self::Graph];
}
