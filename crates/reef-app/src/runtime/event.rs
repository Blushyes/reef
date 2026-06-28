use crate::app::{JumpToLocationOutcome, LspRefineOutcome, TabChangeOutcome, ToggleSidebarOutcome};
use crate::features::hosts_picker::SshTarget;
use crate::tasks::{FsMutationKind, ReplaceSummary};
use reef_core::preview::PreviewDocument;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileActionNotice {
    PlaceCopyInFlight,
    TreeOpInFlight,
    PasteClipboardEmpty,
    PasteSelfIntoDescendant,
    PasteNothingToDo,
    PasteCancelled,
}

#[derive(Debug)]
pub enum AppRuntimeEvent {
    PreviewResultForAdapter {
        generation: u64,
        result: Result<Option<PreviewDocument>, String>,
    },
    TabChanged(TabChangeOutcome),
    SidebarToggled(ToggleSidebarOutcome),
    LoadPreviewSelected,
    LoadDiffRequested,
    SyncSearchPreviewIfStale,
    RecomputeVimSearch,
    RecomputeFindWidget,
    AcceptQuickOpenSelection,
    AcceptGlobalSearchSelection,
    FileActionNotice(FileActionNotice),
    FileCopyDone {
        result: Result<usize, String>,
    },
    FsMutationDone {
        kind: FsMutationKind,
        result: Result<(), String>,
    },
    CommitDone {
        result: Result<(), String>,
    },
    PushDone {
        force: bool,
        result: Result<(), String>,
    },
    DismissConfirm,
    ReplaceDone {
        result: Result<ReplaceSummary, String>,
    },
    ClearCommitGraphSearch,
    LocationJumped(JumpToLocationOutcome),
    ClearPreviewSelection,
    LspRefineJump(LspRefineOutcome),
    ResolvePendingHighlight,
    ClearCommitDetailSelection,
    ClearDiffSelection,
    PersistQuickOpenMru(String),
    PersistHostsRecent(Vec<SshTarget>),
    PersistGraphScope,
    GraphScopeFallback {
        short_ref: String,
    },
    GraphBranchPickerNotReady,
    GraphBranchPickerStaleBranch {
        short_ref: String,
    },
}
