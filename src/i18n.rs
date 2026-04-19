//! Minimal two-language (en / zh) i18n for the TUI.
//!
//! Detection order: `ui.lang` pref → `REEF_LANG` env → `LC_ALL` →
//! `LC_MESSAGES` → `LANG`. A locale starting with `zh` selects Chinese;
//! anything else (empty, POSIX, en_*, fr_*, …) falls back to English.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn lang() -> Lang {
    *LANG.get_or_init(detect)
}

fn detect() -> Lang {
    if let Some(v) = crate::prefs::get("ui.lang") {
        match v.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh_cn" | "zh-hans" => return Lang::Zh,
            "en" | "en-us" | "en_us" => return Lang::En,
            _ => {}
        }
    }
    for var in ["REEF_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
        let Ok(raw) = std::env::var(var) else {
            continue;
        };
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if s.to_ascii_lowercase().starts_with("zh") {
            return Lang::Zh;
        }
        return Lang::En;
    }
    Lang::En
}

#[derive(Debug, Clone, Copy)]
pub enum Msg {
    // Tab labels + tab bar hint
    TabFiles,
    TabSearch,
    TabGit,
    TabGraph,
    TabBarHint,

    // Chrome
    StatusBarHint,
    SelectModeHint,
    HelpTitle,

    // No-repo panel
    NoRepoTitle,
    NoRepoHint,

    // Search
    SearchNoMatch,

    // Toasts
    PushSuccess,
    ForcePushSuccess,
    PushThreadCrashed,

    // Git status panel
    PushingHint,
    PushFailedPrefix,
    DismissClose,
    ForcePushPrompt,
    ForcePushWarning,
    PushToRemote,
    ConfirmForcePush,
    ConfirmPush,
    Cancel,
    YEscHint,
    DiscardPromptPrefix,
    DiscardPromptSuffix,
    ConfirmDiscard,
    ViewModeTree,
    ViewModeList,
    StagedChanges,
    Changes,
    StageAll,
    UnstageAll,
    NoFiles,

    // Diff panel
    DiffEmpty,
    PreviewEmpty,
    LayoutUnified,
    LayoutSideBySide,
    ModeCompact,
    ModeFullFile,

    // Commit detail panel
    CommitDetailEmpty,
    AuthorLabel,
    DateLabel,
    RefsLabel,
    CommitLabel,
    ChangedFiles,
    ViewTree,
    ViewList,

    // Graph panel
    NoCommits,

    // File tree
    EmptyDir,

    // Help popup — key column
    HelpKeyAnyKey,
    HelpKeyMouseHScroll,
    HelpKeyDragDrop,
    // Help popup — descriptions
    HelpQuit,
    HelpSwitchTab,
    HelpSwitchPanel,
    HelpJumpTab,
    HelpNavUp,
    HelpNavDown,
    HelpPageUp,
    HelpPageDown,
    HelpHScroll,
    HelpHScrollFast,
    HelpHomeEnd,
    HelpMouseHScroll,
    HelpStageUnstage,
    HelpDiscard,
    HelpDiffLayout,
    HelpDiffMode,
    HelpToggleView,
    HelpRefresh,
    HelpSelectMode,
    HelpShowHelp,
    HelpQuickOpen,
    HelpGlobalSearch,
    HelpDragDrop,
    HelpAnyKey,
}

pub fn t(m: Msg) -> &'static str {
    match lang() {
        Lang::Zh => t_zh(m),
        Lang::En => t_en(m),
    }
}

fn t_zh(m: Msg) -> &'static str {
    use Msg::*;
    match m {
        TabFiles => " 📁 文件 ",
        TabSearch => " 🔎 搜索 ",
        TabGit => " ⎇ Git ",
        TabGraph => " ⑂ 图表 ",
        TabBarHint => " 1:文件 2:搜索 3:Git 4:图表",
        StatusBarHint => " q:退出 Tab:切换 s:暂存 u:取消 r:刷新 h:帮助 ",
        SelectModeHint => "  拖拽鼠标选择文字，按 v 退出选择模式",
        HelpTitle => " 快捷键帮助 ",
        NoRepoTitle => "不在 git 仓库中",
        NoRepoHint => "运行 `git init` 初始化，或在 git 仓库里打开 reef。",
        SearchNoMatch => "无匹配 ",
        PushSuccess => "推送成功",
        ForcePushSuccess => "强制推送成功",
        PushThreadCrashed => "推送线程异常退出，请重试",
        PushingHint => "  ⋯ 推送中…",
        PushFailedPrefix => "  ✖ 推送失败: ",
        DismissClose => "  [关闭]",
        ForcePushPrompt => "  ⚠ 强制推送？",
        ForcePushWarning => "（会覆盖远端，使用 --force-with-lease）",
        PushToRemote => "  推送到远端？",
        ConfirmForcePush => " 确认强制推送 ",
        ConfirmPush => " 确认推送 ",
        Cancel => " 取消 ",
        YEscHint => "(y / Esc)",
        DiscardPromptPrefix => "  ⚠ 还原 ",
        DiscardPromptSuffix => "？（不可撤销）",
        ConfirmDiscard => " 确认还原 ",
        ViewModeTree => "视图: 树形",
        ViewModeList => "视图: 列表",
        StagedChanges => "暂存的更改",
        Changes => "更改",
        StageAll => "暂存全部",
        UnstageAll => "取消全部",
        NoFiles => "  无文件",
        DiffEmpty => "选择一个文件查看 diff",
        PreviewEmpty => "选择一个文件预览内容",
        LayoutUnified => "上下",
        LayoutSideBySide => "左右",
        ModeCompact => "局部",
        ModeFullFile => "全量",
        CommitDetailEmpty => "  选择一个 commit 查看详情",
        AuthorLabel => "作者: ",
        DateLabel => "日期: ",
        RefsLabel => "引用: ",
        CommitLabel => "commit ",
        ChangedFiles => "变更文件",
        ViewTree => "树形",
        ViewList => "列表",
        NoCommits => "  无 commit",
        EmptyDir => "(空)",
        HelpKeyAnyKey => "任意键",
        HelpKeyMouseHScroll => "Shift+滚轮 / 触控板横划",
        HelpKeyDragDrop => "拖文件进终端",
        HelpQuit => "退出",
        HelpSwitchTab => "切换顶部标签页（文件 ↔ Git ↔ 图表）",
        HelpSwitchPanel => "切换焦点面板（侧边栏 ↔ 编辑区）",
        HelpJumpTab => "跳转到第 N 个标签页",
        HelpNavUp => "向上导航 / 向上滚动",
        HelpNavDown => "向下导航 / 向下滚动",
        HelpPageUp => "快速向上翻页",
        HelpPageDown => "快速向下翻页",
        HelpHScroll => "横向滚动（Diff/预览 面板聚焦时）",
        HelpHScrollFast => "横向快速滚动（10 列）",
        HelpHomeEnd => "回到行首 / 跳到行尾",
        HelpMouseHScroll => "鼠标横向滚动",
        HelpStageUnstage => "暂存 / 取消暂存（Git tab）",
        HelpDiscard => "还原工作树文件（Git tab）",
        HelpDiffLayout => "切换 Diff 布局（上下 ↔ 左右）",
        HelpDiffMode => "切换 Diff 模式（局部 ↔ 全量）",
        HelpToggleView => "切换列表 / 树形视图",
        HelpRefresh => "刷新",
        HelpSelectMode => "文字选择模式",
        HelpShowHelp => "显示 / 关闭此帮助",
        HelpQuickOpen => "打开 / 关闭快速打开浮层（全局模糊搜索）",
        HelpGlobalSearch => "打开全局内容搜索浮层",
        HelpDragDrop => "进入放置模式：点击文件夹复制到那里，Esc / 右键 取消",
        HelpAnyKey => "关闭帮助",
    }
}

fn t_en(m: Msg) -> &'static str {
    use Msg::*;
    match m {
        TabFiles => " 📁 Files ",
        TabSearch => " 🔎 Search ",
        TabGit => " ⎇ Git ",
        TabGraph => " ⑂ Graph ",
        TabBarHint => " 1:Files 2:Search 3:Git 4:Graph",
        StatusBarHint => " q:quit Tab:switch s:stage u:unstage r:refresh h:help ",
        SelectModeHint => "  Drag to select text, press v to exit select mode",
        HelpTitle => " Keybindings ",
        NoRepoTitle => "Not a git repository",
        NoRepoHint => "Run `git init` to initialise one, or open reef inside a git repo.",
        SearchNoMatch => "no match ",
        PushSuccess => "Push succeeded",
        ForcePushSuccess => "Force push succeeded",
        PushThreadCrashed => "Push worker crashed, please retry",
        PushingHint => "  ⋯ Pushing…",
        PushFailedPrefix => "  ✖ Push failed: ",
        DismissClose => "  [dismiss]",
        ForcePushPrompt => "  ⚠ Force push?",
        ForcePushWarning => "(overwrites remote, uses --force-with-lease)",
        PushToRemote => "  Push to remote?",
        ConfirmForcePush => " Confirm force push ",
        ConfirmPush => " Confirm push ",
        Cancel => " Cancel ",
        YEscHint => "(y / Esc)",
        DiscardPromptPrefix => "  ⚠ Discard ",
        DiscardPromptSuffix => "? (irreversible)",
        ConfirmDiscard => " Confirm discard ",
        ViewModeTree => "View: tree",
        ViewModeList => "View: list",
        StagedChanges => "Staged changes",
        Changes => "Changes",
        StageAll => "Stage all",
        UnstageAll => "Unstage all",
        NoFiles => "  (no files)",
        DiffEmpty => "Select a file to view diff",
        PreviewEmpty => "Select a file to preview",
        LayoutUnified => "unified",
        LayoutSideBySide => "split",
        ModeCompact => "compact",
        ModeFullFile => "full",
        CommitDetailEmpty => "  Select a commit to view details",
        AuthorLabel => "Author: ",
        DateLabel => "Date:   ",
        RefsLabel => "Refs:   ",
        CommitLabel => "commit ",
        ChangedFiles => "Changed files",
        ViewTree => "tree",
        ViewList => "list",
        NoCommits => "  (no commits)",
        EmptyDir => "(empty)",
        HelpKeyAnyKey => "any key",
        HelpKeyMouseHScroll => "Shift+Wheel / trackpad",
        HelpKeyDragDrop => "Drag file into terminal",
        HelpQuit => "Quit",
        HelpSwitchTab => "Cycle top tabs (Files ↔ Git ↔ Graph)",
        HelpSwitchPanel => "Switch focused panel (sidebar ↔ editor)",
        HelpJumpTab => "Jump to the Nth tab",
        HelpNavUp => "Navigate up / scroll up",
        HelpNavDown => "Navigate down / scroll down",
        HelpPageUp => "Page up",
        HelpPageDown => "Page down",
        HelpHScroll => "Horizontal scroll (when Diff/Preview focused)",
        HelpHScrollFast => "Horizontal fast scroll (10 cols)",
        HelpHomeEnd => "Jump to line start / end",
        HelpMouseHScroll => "Mouse horizontal scroll",
        HelpStageUnstage => "Stage / unstage (Git tab)",
        HelpDiscard => "Discard workdir file (Git tab)",
        HelpDiffLayout => "Toggle diff layout (unified ↔ split)",
        HelpDiffMode => "Toggle diff mode (compact ↔ full)",
        HelpToggleView => "Toggle list / tree view",
        HelpRefresh => "Refresh",
        HelpSelectMode => "Text selection mode",
        HelpShowHelp => "Show / close this help",
        HelpQuickOpen => "Toggle quick-open palette (global fuzzy search)",
        HelpGlobalSearch => "Open global content-search palette",
        HelpDragDrop => {
            "Enter place mode: click a folder to copy there, Esc / right-click to cancel"
        }
        HelpAnyKey => "Close help",
    }
}

// ─── Parameterised strings ────────────────────────────────────────────────────

pub fn edit_open_failed(e: &str) -> String {
    match lang() {
        Lang::Zh => format!("打开编辑器失败: {e}"),
        Lang::En => format!("Failed to open editor: {e}"),
    }
}

pub fn push_failed_toast(e: &str) -> String {
    match lang() {
        Lang::Zh => format!("推送失败: {e}"),
        Lang::En => format!("Push failed: {e}"),
    }
}

pub fn push_n_commits_prompt(ahead: usize) -> String {
    match lang() {
        Lang::Zh => format!("  推送 {ahead} 个提交到远端？"),
        Lang::En => format!("  Push {ahead} commits to remote?"),
    }
}

pub fn push_button(ahead: usize) -> String {
    match lang() {
        Lang::Zh => format!(" ↑ 推送 ({ahead}) "),
        Lang::En => format!(" ↑ Push ({ahead}) "),
    }
}

pub fn behind_remote(behind: usize) -> String {
    match lang() {
        Lang::Zh => format!("  ↓ 落后远端 {behind} 次提交 — 请先 fetch/pull"),
        Lang::En => format!("  ↓ Behind remote by {behind} — fetch/pull first"),
    }
}

pub fn diverged_force_push(ahead: usize, behind: usize) -> String {
    match lang() {
        Lang::Zh => format!(" ⚠ 已分叉 ↑{ahead} ↓{behind} — 强制推送 "),
        Lang::En => format!(" ⚠ Diverged ↑{ahead} ↓{behind} — force push "),
    }
}

pub fn changed_files_header(n: usize) -> String {
    format!("{} ({})", t(Msg::ChangedFiles), n)
}

/// Trailing "  [label]  t 切换" / "  [label]  t toggle" hint on the commit
/// files header.
pub fn view_toggle_hint(view_label: &str) -> String {
    match lang() {
        Lang::Zh => format!("  [{view_label}]  t 切换"),
        Lang::En => format!("  [{view_label}]  t toggle"),
    }
}

/// Trailing "  [layout][mode]  m/f 切换" / "  [layout][mode]  m/f toggle" hint.
pub fn diff_mode_hint(layout_label: &str, mode_label: &str) -> String {
    match lang() {
        Lang::Zh => format!("  [{layout_label}][{mode_label}]  m/f 切换"),
        Lang::En => format!("  [{layout_label}][{mode_label}]  m/f toggle"),
    }
}

/// Counter + wrap message for active search. `wrap` is `Some(WrapMsg::Top)` /
/// `Some(WrapMsg::Bottom)` / `None`. Caller passes the raw state.
pub fn search_counter(i: usize, n: usize, wrap: Option<crate::search::WrapMsg>) -> String {
    use crate::search::WrapMsg;
    match (wrap, lang()) {
        (Some(WrapMsg::Bottom), Lang::Zh) => format!("{}/{}  ↻ 回到顶部 ", i + 1, n),
        (Some(WrapMsg::Bottom), Lang::En) => format!("{}/{}  ↻ top ", i + 1, n),
        (Some(WrapMsg::Top), Lang::Zh) => format!("{}/{}  ↻ 回到底部 ", i + 1, n),
        (Some(WrapMsg::Top), Lang::En) => format!("{}/{}  ↻ bottom ", i + 1, n),
        _ => format!("{}/{} ", i + 1, n),
    }
}

pub fn search_dormant_with_counter(prefix: char, query: &str, i: usize, n: usize) -> String {
    match lang() {
        Lang::Zh => format!(" {prefix}{query}  {}/{}  n/N 切换 ", i + 1, n),
        Lang::En => format!(" {prefix}{query}  {}/{}  n/N ", i + 1, n),
    }
}

pub fn place_mode_banner(primary_name: &str, count: usize) -> String {
    let extra = count.saturating_sub(1);
    match lang() {
        Lang::Zh => {
            if extra == 0 {
                format!(" 📋 放置 {primary_name} ")
            } else {
                format!(" 📋 放置 {primary_name} +{extra} 项 ")
            }
        }
        Lang::En => {
            if extra == 0 {
                format!(" 📋 Placing {primary_name} ")
            } else {
                format!(" 📋 Placing {primary_name} +{extra} more ")
            }
        }
    }
}

/// Status-bar hint shown while place mode is active. Mirrors how select-mode
/// commandeers the status bar with a high-contrast badge — the two
/// correspond to distinct modal states the user needs to exit explicitly.
pub fn place_mode_status_hint() -> &'static str {
    match lang() {
        Lang::Zh => "  点击文件夹放置 · 点击空白放到根目录 · Esc / 右键 取消",
        Lang::En => {
            "  Click a folder to drop · click empty to drop at root · Esc / right-click to cancel"
        }
    }
}

pub fn place_mode_copied(count: usize) -> String {
    match lang() {
        Lang::Zh => format!("已复制 {count} 项"),
        Lang::En => format!("Copied {count} item(s)"),
    }
}

pub fn place_mode_copy_failed(e: &str) -> String {
    match lang() {
        Lang::Zh => format!("复制失败: {e}"),
        Lang::En => format!("Copy failed: {e}"),
    }
}

/// Warning when a drop arrives while the user is in select-mode. Mouse
/// capture is off in that state, so entering place mode would leave
/// the user with no way to click a target.
pub fn place_mode_blocked_by_select_mode() -> String {
    match lang() {
        Lang::Zh => "拖拽被选择模式拦住了：按 v 退出选择模式后再试".to_string(),
        Lang::En => "Drop blocked by select mode — press v to exit, then retry".to_string(),
    }
}

/// Warning when a drop arrives while a previous copy is still running.
/// Entering place mode would overwrite the sources and invalidate the
/// in-flight generation, silently losing the earlier result toast.
pub fn place_mode_blocked_by_in_flight_copy() -> String {
    match lang() {
        Lang::Zh => "上次拷贝还没完成，先等一下".to_string(),
        Lang::En => "A copy is still in progress — please wait".to_string(),
    }
}

/// Status hint shown while a place-mode copy is running. Replaces the
/// normal "Placing X" banner so the user sees that work is actually
/// happening for large copies that take more than a few frames.
pub fn place_mode_copying_banner() -> String {
    match lang() {
        Lang::Zh => " ⋯ 正在复制… ".to_string(),
        Lang::En => " ⋯ Copying… ".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_mode_strings_are_nonempty() {
        assert!(!place_mode_copied(3).is_empty());
        assert!(!place_mode_copy_failed("boom").is_empty());
        assert!(!place_mode_banner("x.txt", 1).is_empty());
    }

    #[test]
    fn t_returns_translation_for_both_langs() {
        // Smoke test: every Msg variant should return non-empty for both
        // languages (catches accidentally-dropped match arms).
        for m in [
            Msg::TabFiles,
            Msg::PushSuccess,
            Msg::HelpQuit,
            Msg::DiffEmpty,
        ] {
            assert!(!t_zh(m).is_empty());
            assert!(!t_en(m).is_empty());
        }
    }
}
