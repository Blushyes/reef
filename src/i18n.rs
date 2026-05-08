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
    /// Placeholder shown in the second input row of the Search tab when
    /// replace mode is active and the user hasn't typed anything.
    ReplaceWithPlaceholder,
    /// Footer label for the per-match counter when replace mode is on,
    /// e.g. "12 to replace".
    ReplaceCountSuffix,
    /// Footer button label that commits the replace batch.
    ApplyReplace,
    /// Replace-in-progress hint while a batch is in flight.
    ReplacingHint,
    /// Toast header for a successful replace summary, e.g.
    /// "Replaced 12 lines in 3 files".
    ReplaceSummaryToast,
    /// Toast suffix when some lines drifted under the search snapshot.
    ReplaceSkippedStaleSuffix,
    /// Toast suffix when files exceeded `MAX_REPLACE_FILE_SIZE`.
    ReplaceTooLargeSuffix,
    /// Title used by the Search tab when replace mode is open.
    SearchReplaceTitle,

    // Toasts
    PushSuccess,
    ForcePushSuccess,
    PullSuccess,
    PullThreadCrashed,
    PushThreadCrashed,
    CommitSuccess,
    CommitThreadCrashed,
    ClipboardCopied,
    ClipboardCopyFailed,

    // Git status panel
    Repository,
    Branch,
    RepoScanning,
    RepoDiscoverFailed,
    NoReposFound,
    RepoSelectPrompt,
    PushingHint,
    PullingHint,
    PullFailedPrefix,
    PushFailedPrefix,
    DismissClose,
    ForcePushPrompt,
    ForcePushWarning,
    PushToRemote,
    ConfirmForcePush,
    ConfirmPush,
    Cancel,
    YEscHint,
    ViewModeTree,
    ViewModeList,
    StagedChanges,
    Changes,
    StageAll,
    UnstageAll,
    DiscardAll,
    NoFiles,

    // Commit box
    CommitMessagePlaceholder,
    CommitButton,
    CommittingHint,
    CommitFailedPrefix,
    CommitHint,
    CommitNothingStaged,

    // Diff panel
    DiffEmpty,
    DiffLoading,
    PreviewEmpty,
    PreviewImageUnavailable,
    PreviewLoading,
    PreviewBinaryNonImage,
    PreviewBinaryUnsupportedImage,
    PreviewBinaryTooLarge,
    PreviewBinaryDecodeError,
    PreviewBinaryEmpty,
    /// SQLite preview: header for the left-side tables list column.
    DbTablesHeader,
    /// SQLite preview: shown when the database has zero user tables.
    DbEmpty,
    /// SQLite preview: shown for the currently-selected table when it
    /// has zero rows (vs an empty database, which uses [`DbEmpty`]).
    DbNoRows,
    /// SQLite preview: short label inserted into the page footer
    /// before the page-index pair, e.g. "page 2 / 5 · row …".
    DbPageLabel,
    /// SQLite preview: short label inserted into the page footer
    /// before the row range pair, e.g. "… · row 11-20 / 100".
    DbRowsLabel,
    /// Page-jump input prompt label, shown in the footer while the
    /// `g`-prefix goto-page input is active.
    DbGotoPagePrompt,
    /// Hint text shown alongside the goto-page prompt.
    DbGotoPageHint,
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
    HelpKeyRightClick,
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
    HelpGraphRangeExtend,
    HelpGraphRangeExtendFast,
    HelpGraphRangeClear,
    HelpGraphShiftExtend,
    HelpGraphShiftClick,
    HelpGraphVisualMode,
    HelpGraphVisualClick,
    RangeHint,
    StatusBarRangeHint,
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
    HelpRenameEntry,
    HelpDeleteEntry,
    HelpHardDeleteEntry,
    HelpRightClickMenu,
    HelpToggleSidebar,
    HelpOpenSettings,
    /// `Esc` row in the help popup — describes the general two-step
    /// back-out behaviour (clear dormant search, then return panel
    /// focus to the list/tree column). The Graph-visual-mode row
    /// keeps its own description because that Esc fires earlier.
    HelpEscBackOut,
    SidebarHiddenHint,
    HelpAnyKey,

    // Status bar panel-focus chip — short labels for the right-aligned
    // indicator showing which panel currently owns focus.
    PanelFiles,
    PanelPreview,
    PanelCommit,
    PanelDiff,
    PanelSearch,
    PanelGraph,

    // Settings page
    SettingsTitle,
    SettingsFooterHint,
    SettingsEditorEditHint,
    SettingsSectionGeneral,
    SettingsSectionEditor,
    SettingsSectionGit,
    SettingsSectionGraph,
    SettingsItemTheme,
    SettingsItemEditor,
    SettingsItemDiffLayout,
    SettingsItemDiffMode,
    SettingsItemStatusTreeMode,
    SettingsItemCommitDiffLayout,
    SettingsItemCommitDiffMode,
    SettingsItemCommitFilesTreeMode,
    SettingsDescTheme,
    SettingsDescEditor,
    SettingsDescDiffLayout,
    SettingsDescDiffMode,
    SettingsDescStatusTreeMode,
    SettingsDescCommitDiffLayout,
    SettingsDescCommitDiffMode,
    SettingsDescCommitFilesTreeMode,
    SettingsValueThemeAuto,
    SettingsValueThemeDark,
    SettingsValueThemeLight,
    SettingsValueOn,
    SettingsValueOff,
    SettingsEditorPlaceholder,
    /// Toast for cycling `ui.theme` to `auto` — the OSC 11 probe only
    /// runs at startup before raw mode, so the live theme keeps its
    /// current preset until next launch.
    SettingsAutoThemeOnNextLaunch,
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
        ReplaceWithPlaceholder => "替换为…",
        ReplaceCountSuffix => "处待替换",
        ApplyReplace => "应用",
        ReplacingHint => "正在替换…",
        ReplaceSummaryToast => "已替换",
        ReplaceSkippedStaleSuffix => "处过期跳过",
        ReplaceTooLargeSuffix => "个文件过大已跳过",
        SearchReplaceTitle => " 🔎 查找与替换 ",
        PushSuccess => "推送成功",
        ForcePushSuccess => "强制推送成功",
        PullSuccess => "拉取成功",
        PullThreadCrashed => "拉取线程异常退出，请重试",
        PushThreadCrashed => "推送线程异常退出，请重试",
        CommitSuccess => "提交成功",
        CommitThreadCrashed => "提交线程异常退出，请重试",
        ClipboardCopied => "已复制到剪贴板",
        ClipboardCopyFailed => "复制到剪贴板失败",
        Repository => "仓库",
        Branch => "分支",
        RepoScanning => "  正在扫描仓库…",
        RepoDiscoverFailed => "仓库发现失败",
        NoReposFound => "  未发现仓库",
        RepoSelectPrompt => "  选择一个仓库",
        PushingHint => "  ⋯ 推送中…",
        PullingHint => "  ⋯ 拉取中…",
        PullFailedPrefix => "  ✖ 拉取失败: ",
        PushFailedPrefix => "  ✖ 推送失败: ",
        DismissClose => "  [关闭]",
        ForcePushPrompt => "  ⚠ 强制推送？",
        ForcePushWarning => "（会覆盖远端，使用 --force-with-lease）",
        PushToRemote => "  推送到远端？",
        ConfirmForcePush => " 确认强制推送 ",
        ConfirmPush => " 确认推送 ",
        Cancel => " 取消 ",
        YEscHint => "(y / Esc)",
        ViewModeTree => "视图: 树形",
        ViewModeList => "视图: 列表",
        StagedChanges => "暂存的更改",
        Changes => "更改",
        StageAll => "暂存全部",
        UnstageAll => "取消全部",
        DiscardAll => "全部撤回",
        NoFiles => "  无文件",
        CommitMessagePlaceholder => "输入提交消息…",
        CommitButton => " ✓ 提交 ",
        CommittingHint => "  ⋯ 提交中…",
        CommitFailedPrefix => "  ✖ 提交失败: ",
        CommitHint => "(Ctrl+Enter 提交 · Esc 取消)",
        CommitNothingStaged => "没有可提交的已暂存文件",
        DiffEmpty => "选择一个文件查看 diff",
        DiffLoading => "diff 载入中…",
        PreviewEmpty => "选择一个文件预览内容",
        PreviewImageUnavailable => "当前终端不支持图片预览",
        PreviewLoading => "加载中…",
        PreviewBinaryNonImage => "二进制文件",
        PreviewBinaryUnsupportedImage => "不支持的图片格式",
        PreviewBinaryTooLarge => "文件过大，跳过解码",
        PreviewBinaryDecodeError => "解码失败",
        PreviewBinaryEmpty => "空文件",
        DbTablesHeader => "表",
        DbEmpty => "(空数据库)",
        DbNoRows => "(无数据)",
        DbPageLabel => "页",
        DbRowsLabel => "行",
        DbGotoPagePrompt => "跳到页",
        DbGotoPageHint => "(Enter 确认 · Esc 取消)",
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
        HelpKeyRightClick => "右键文件树行",
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
        HelpGraphRangeExtend => "扩选一行提交（Graph 标签页）",
        HelpGraphRangeExtendFast => "扩选 10 行提交（Graph 标签页）",
        HelpGraphRangeClear => "退出可视模式 / 清除范围选择",
        HelpGraphShiftExtend => "扩选（在支持 Shift 透传的终端，否则按 V 进入可视模式）",
        HelpGraphShiftClick => "Shift+点击：扩选到该提交（同上，否则用可视模式）",
        HelpGraphVisualMode => "进入/退出可视模式（Graph 标签页）",
        HelpGraphVisualClick => "可视模式下点击提交 = 改变终点",
        RangeHint => "点击下方任一提交即可折叠范围回到单选",
        StatusBarRangeHint => "↑↓/点击 扩选 · V/Esc 退出",
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
        HelpRenameEntry => "重命名选中项",
        HelpDeleteEntry => "移动到废纸篓（带确认）",
        HelpHardDeleteEntry => "永久删除（不可撤销）",
        HelpRightClickMenu => "打开文件树右键菜单",
        HelpToggleSidebar => "切换侧边栏显示",
        HelpOpenSettings => {
            "打开设置页（部分终端不转发 Ctrl+, ，可在 Settings 内手动通过 Esc 退出）"
        }
        HelpEscBackOut => "退出焦点 / 清除搜索",
        SidebarHiddenHint => "侧边栏已隐藏 — Ctrl+B 可恢复",
        HelpAnyKey => "关闭帮助",
        PanelFiles => "文件",
        PanelPreview => "预览",
        PanelCommit => "提交",
        PanelDiff => "Diff",
        PanelSearch => "搜索",
        PanelGraph => "图表",
        SettingsTitle => " ⚙ 设置 ",
        SettingsFooterHint => "  ↑↓ 选择 · Enter 切换/编辑 · Esc 返回",
        SettingsEditorEditHint => "  Enter 保存 · Esc 取消",
        SettingsSectionGeneral => "通用",
        SettingsSectionEditor => "外部编辑器",
        SettingsSectionGit => "Git Diff",
        SettingsSectionGraph => "提交详情",
        SettingsItemTheme => "主题",
        SettingsItemEditor => "编辑器命令",
        SettingsItemDiffLayout => "Diff 布局",
        SettingsItemDiffMode => "Diff 模式",
        SettingsItemStatusTreeMode => "状态侧栏树形视图",
        SettingsItemCommitDiffLayout => "提交 Diff 布局",
        SettingsItemCommitDiffMode => "提交 Diff 模式",
        SettingsItemCommitFilesTreeMode => "提交文件列表树形视图",
        SettingsDescTheme => "auto 自动检测终端背景；显式指定可避免误判（重启后生效一次）",
        SettingsDescEditor => {
            "Enter 打开文件时调用的命令；留空则按 $VISUAL → $EDITOR → vi 顺序回退"
        }
        SettingsDescDiffLayout => "Git tab 右侧 diff 显示方式：上下统一 / 左右对比",
        SettingsDescDiffMode => "Git tab diff 显示范围：仅变更块 / 整个文件",
        SettingsDescStatusTreeMode => "Git tab 文件列表用树形或列表呈现",
        SettingsDescCommitDiffLayout => "图表 tab commit 详情中的 diff 显示方式",
        SettingsDescCommitDiffMode => "图表 tab commit 详情中的 diff 显示范围",
        SettingsDescCommitFilesTreeMode => "图表 tab commit 变更文件用树形或列表呈现",
        SettingsValueThemeAuto => "自动",
        SettingsValueThemeDark => "深色",
        SettingsValueThemeLight => "浅色",
        SettingsValueOn => "开",
        SettingsValueOff => "关",
        SettingsEditorPlaceholder => "(未设置 — 使用 $VISUAL / $EDITOR / vi)",
        SettingsAutoThemeOnNextLaunch => "已切换到 auto 主题，下次启动生效",
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
        ReplaceWithPlaceholder => "Replace with…",
        ReplaceCountSuffix => "to replace",
        ApplyReplace => "Apply",
        ReplacingHint => "Replacing…",
        ReplaceSummaryToast => "Replaced",
        ReplaceSkippedStaleSuffix => "skipped (stale)",
        ReplaceTooLargeSuffix => "file(s) too large to replace",
        SearchReplaceTitle => " 🔎 Find & Replace ",
        PushSuccess => "Push succeeded",
        ForcePushSuccess => "Force push succeeded",
        PullSuccess => "Pull succeeded",
        PullThreadCrashed => "Pull worker crashed, please retry",
        PushThreadCrashed => "Push worker crashed, please retry",
        CommitSuccess => "Commit succeeded",
        CommitThreadCrashed => "Commit worker crashed, please retry",
        ClipboardCopied => "Copied to clipboard",
        ClipboardCopyFailed => "Clipboard copy failed",
        Repository => "Repository",
        Branch => "Branch",
        RepoScanning => "  scanning repositories…",
        RepoDiscoverFailed => "Repository discovery failed",
        NoReposFound => "  no repositories found",
        RepoSelectPrompt => "  select a repository",
        PushingHint => "  ⋯ Pushing…",
        PullingHint => "  ⋯ Pulling…",
        PullFailedPrefix => "  ✖ Pull failed: ",
        PushFailedPrefix => "  ✖ Push failed: ",
        DismissClose => "  [dismiss]",
        ForcePushPrompt => "  ⚠ Force push?",
        ForcePushWarning => "(overwrites remote, uses --force-with-lease)",
        PushToRemote => "  Push to remote?",
        ConfirmForcePush => " Confirm force push ",
        ConfirmPush => " Confirm push ",
        Cancel => " Cancel ",
        YEscHint => "(y / Esc)",
        ViewModeTree => "View: tree",
        ViewModeList => "View: list",
        StagedChanges => "Staged changes",
        Changes => "Changes",
        StageAll => "Stage all",
        UnstageAll => "Unstage all",
        DiscardAll => "Discard all",
        NoFiles => "  (no files)",
        CommitMessagePlaceholder => "Message (commit staged)…",
        CommitButton => " ✓ Commit ",
        CommittingHint => "  ⋯ Committing…",
        CommitFailedPrefix => "  ✖ Commit failed: ",
        CommitHint => "(Ctrl+Enter to commit · Esc to cancel)",
        CommitNothingStaged => "Nothing staged to commit",
        DiffEmpty => "Select a file to view diff",
        DiffLoading => "Loading diff…",
        PreviewEmpty => "Select a file to preview",
        PreviewImageUnavailable => "image preview unavailable in this terminal",
        PreviewLoading => "loading…",
        PreviewBinaryNonImage => "binary file",
        PreviewBinaryUnsupportedImage => "unsupported image format",
        PreviewBinaryTooLarge => "file too large to decode",
        PreviewBinaryDecodeError => "decode failed",
        PreviewBinaryEmpty => "empty file",
        DbTablesHeader => "tables",
        DbEmpty => "(empty database)",
        DbNoRows => "(no rows)",
        DbPageLabel => "page",
        DbRowsLabel => "row",
        DbGotoPagePrompt => "go to page",
        DbGotoPageHint => "(Enter to jump · Esc to cancel)",
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
        HelpKeyRightClick => "Right-click a tree row",
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
        HelpGraphRangeExtend => "Extend commit range by 1 (Graph tab)",
        HelpGraphRangeExtendFast => "Extend commit range by 10 (Graph tab)",
        HelpGraphRangeClear => "Exit visual mode / clear range",
        HelpGraphShiftExtend => "Extend range (terminals that forward Shift; else use V)",
        HelpGraphShiftClick => "Shift+Click: extend range (same; else use visual mode)",
        HelpGraphVisualMode => "Enter/exit visual mode (Graph tab)",
        HelpGraphVisualClick => "In visual mode: click = move endpoint",
        RangeHint => "Click any commit below to collapse back to single-select",
        StatusBarRangeHint => "↑↓/click extend · V/Esc exit",
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
        HelpRenameEntry => "Rename the selected entry",
        HelpDeleteEntry => "Move to Trash (with confirm)",
        HelpHardDeleteEntry => "Delete permanently (cannot be undone)",
        HelpRightClickMenu => "Open file-tree context menu",
        HelpToggleSidebar => "Toggle sidebar",
        HelpOpenSettings => "Open settings page (some terminals don't forward Ctrl+,)",
        HelpEscBackOut => "Exit focus / clear search",
        SidebarHiddenHint => "Sidebar hidden — Ctrl+B to restore",
        HelpAnyKey => "Close help",
        PanelFiles => "Files",
        PanelPreview => "Preview",
        PanelCommit => "Commit",
        PanelDiff => "Diff",
        PanelSearch => "Search",
        PanelGraph => "Graph",
        SettingsTitle => " ⚙ Settings ",
        SettingsFooterHint => "  ↑↓ select · Enter toggle/edit · Esc back",
        SettingsEditorEditHint => "  Enter save · Esc cancel",
        SettingsSectionGeneral => "General",
        SettingsSectionEditor => "External editor",
        SettingsSectionGit => "Git diff",
        SettingsSectionGraph => "Commit detail",
        SettingsItemTheme => "Theme",
        SettingsItemEditor => "Editor command",
        SettingsItemDiffLayout => "Diff layout",
        SettingsItemDiffMode => "Diff mode",
        SettingsItemStatusTreeMode => "Status sidebar — tree view",
        SettingsItemCommitDiffLayout => "Commit diff layout",
        SettingsItemCommitDiffMode => "Commit diff mode",
        SettingsItemCommitFilesTreeMode => "Commit files — tree view",
        SettingsDescTheme => {
            "auto detects terminal background; pick dark / light to override (takes effect on next launch)"
        }
        SettingsDescEditor => {
            "Command launched when you press Enter on a file; empty falls back to $VISUAL → $EDITOR → vi"
        }
        SettingsDescDiffLayout => "Git tab right-side diff layout — unified / side-by-side",
        SettingsDescDiffMode => "Git tab diff body — compact (changed hunks) / full file",
        SettingsDescStatusTreeMode => "Git tab file list — tree or flat list",
        SettingsDescCommitDiffLayout => "Graph tab commit-detail diff layout",
        SettingsDescCommitDiffMode => "Graph tab commit-detail diff body",
        SettingsDescCommitFilesTreeMode => "Graph tab commit changed files — tree or flat list",
        SettingsValueThemeAuto => "auto",
        SettingsValueThemeDark => "dark",
        SettingsValueThemeLight => "light",
        SettingsValueOn => "on",
        SettingsValueOff => "off",
        SettingsEditorPlaceholder => "(unset — uses $VISUAL / $EDITOR / vi)",
        SettingsAutoThemeOnNextLaunch => "Theme set to auto — takes effect on next launch",
    }
}

// ─── Parameterised strings ────────────────────────────────────────────────────

pub fn edit_open_failed(e: &str) -> String {
    match lang() {
        Lang::Zh => format!("打开编辑器失败: {e}"),
        Lang::En => format!("Failed to open editor: {e}"),
    }
}

/// Toast surfaced when `--ssh` / hosts-picker session swap fails to
/// connect. Goes through the toast queue so the user actually sees it
/// — `eprintln!` while the alt-screen is up is silently swallowed.
pub fn ssh_connect_failed(target: &str, e: &str) -> String {
    match lang() {
        Lang::Zh => format!("连接 {target} 失败: {e}"),
        Lang::En => format!("Failed to connect to {target}: {e}"),
    }
}

pub fn push_failed_toast(e: &str) -> String {
    match lang() {
        Lang::Zh => format!("推送失败: {e}"),
        Lang::En => format!("Push failed: {e}"),
    }
}

/// Toast variant for commit failure. Commit errors are often multi-line
/// (hook output, rejected commit-msg template, etc.); the toast picks
/// the first non-empty line so it stays readable next to other status
/// bar items. The full text still surfaces in the in-panel
/// `commit_error` banner.
pub fn commit_failed_toast(e: &str) -> String {
    let first = e
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(e);
    match lang() {
        Lang::Zh => format!("提交失败: {first}"),
        Lang::En => format!("Commit failed: {first}"),
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

pub fn pull_button(behind: usize) -> String {
    match lang() {
        Lang::Zh => format!(" ↓ 拉取 ({behind}) "),
        Lang::En => format!(" ↓ Pull ({behind}) "),
    }
}

pub fn pull_failed_toast(e: &str) -> String {
    let first = e
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or(e);
    match lang() {
        Lang::Zh => format!("拉取失败: {first}"),
        Lang::En => format!("Pull failed: {first}"),
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

pub fn range_header_count(n: usize) -> String {
    match lang() {
        Lang::Zh => format!("范围 · {} 个提交", n),
        Lang::En => format!("Range · {} commits", n),
    }
}

pub fn range_badge(n: usize) -> String {
    // "RANGE" is a fixed UI token (matches SELECT/PLACE/TRASH badge style),
    // not a translatable word; keep it language-agnostic.
    format!(" RANGE {} ", n)
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

/// Toast text after a successful file-tree mutation (Create / Rename /
/// Trash / HardDelete). The `kind` is carried on the `WorkerResult::FsMutation`
/// so each branch can pick an appropriate verb without the merge site
/// having to re-derive it from paths.
pub fn fs_mutation_success_toast(kind: &crate::tasks::FsMutationKind) -> String {
    use crate::tasks::FsMutationKind as K;
    match (lang(), kind) {
        (Lang::Zh, K::CreatedFile { name }) => format!("已创建文件 {name}"),
        (Lang::En, K::CreatedFile { name }) => format!("Created {name}"),
        (Lang::Zh, K::CreatedFolder { name }) => format!("已创建文件夹 {name}"),
        (Lang::En, K::CreatedFolder { name }) => format!("Created folder {name}"),
        (Lang::Zh, K::Renamed { old_name, new_name }) => {
            format!("已重命名 {old_name} → {new_name}")
        }
        (Lang::En, K::Renamed { old_name, new_name }) => {
            format!("Renamed {old_name} → {new_name}")
        }
        (Lang::Zh, K::Trashed { name }) => format!("已移动到废纸篓: {name}"),
        (Lang::En, K::Trashed { name }) => format!("Moved to Trash: {name}"),
        (Lang::Zh, K::HardDeleted { name }) => format!("已永久删除: {name}"),
        (Lang::En, K::HardDeleted { name }) => format!("Deleted permanently: {name}"),
        (Lang::Zh, K::Moved { old_name, new_name }) => {
            format!("已移动 {old_name} → {new_name}")
        }
        (Lang::En, K::Moved { old_name, new_name }) => {
            format!("Moved {old_name} → {new_name}")
        }
        (Lang::Zh, K::CopiedTo { name }) => format!("已复制到 {name}"),
        (Lang::En, K::CopiedTo { name }) => format!("Copied to {name}"),
        (Lang::Zh, K::MovedMulti { count }) => format!("已移动 {count} 项"),
        (Lang::En, K::MovedMulti { count }) => format!("Moved {count} items"),
        (Lang::Zh, K::CopiedMulti { count }) => format!("已复制 {count} 项"),
        (Lang::En, K::CopiedMulti { count }) => format!("Copied {count} items"),
    }
}

/// Toast text after a file-tree mutation fails. Carries the raw error
/// string from the worker so permission / EEXIST / cross-device-link
/// failures aren't silently swallowed.
pub fn fs_mutation_error_toast(kind: &crate::tasks::FsMutationKind, error: &str) -> String {
    use crate::tasks::FsMutationKind as K;
    let verb = match (lang(), kind) {
        (Lang::Zh, K::CreatedFile { .. }) => "创建文件失败",
        (Lang::En, K::CreatedFile { .. }) => "Create file failed",
        (Lang::Zh, K::CreatedFolder { .. }) => "创建文件夹失败",
        (Lang::En, K::CreatedFolder { .. }) => "Create folder failed",
        (Lang::Zh, K::Renamed { .. }) => "重命名失败",
        (Lang::En, K::Renamed { .. }) => "Rename failed",
        (Lang::Zh, K::Trashed { .. }) => "移动到废纸篓失败",
        (Lang::En, K::Trashed { .. }) => "Move to Trash failed",
        (Lang::Zh, K::HardDeleted { .. }) => "删除失败",
        (Lang::En, K::HardDeleted { .. }) => "Delete failed",
        (Lang::Zh, K::Moved { .. }) | (Lang::Zh, K::MovedMulti { .. }) => "移动失败",
        (Lang::En, K::Moved { .. }) | (Lang::En, K::MovedMulti { .. }) => "Move failed",
        (Lang::Zh, K::CopiedTo { .. }) | (Lang::Zh, K::CopiedMulti { .. }) => "复制失败",
        (Lang::En, K::CopiedTo { .. }) | (Lang::En, K::CopiedMulti { .. }) => "Copy failed",
    };
    format!("{verb}: {error}")
}

// ─── File-tree edit row / delete confirm / toolbar ──────────────────────────

/// Placeholder shown in an empty editable row when the user hasn't
/// typed yet. Differs by mode so the hint tells them what they're
/// about to create.
pub fn tree_edit_placeholder(mode: crate::tree_edit::TreeEditMode) -> &'static str {
    use crate::tree_edit::TreeEditMode as M;
    match (lang(), mode) {
        (Lang::Zh, M::NewFile) => "新文件名…",
        (Lang::En, M::NewFile) => "New file name…",
        (Lang::Zh, M::NewFolder) => "新文件夹名…",
        (Lang::En, M::NewFolder) => "New folder name…",
        (Lang::Zh, M::Rename) => "新名字…",
        (Lang::En, M::Rename) => "New name…",
    }
}

/// Error line rendered directly under the editable row when commit is
/// rejected. Uses the specific variant so the user knows whether to
/// change the name or delete the conflicting file first.
pub fn tree_edit_error(err: &crate::tree_edit::TreeEditError) -> String {
    use crate::tree_edit::TreeEditError as E;
    match (lang(), err) {
        (Lang::Zh, E::InvalidName) => "名字不能为空或是 `.` / `..`".to_string(),
        (Lang::En, E::InvalidName) => "Name cannot be empty or `.` / `..`".to_string(),
        (Lang::Zh, E::IllegalChars) => "名字不能包含 `/`、`\\` 或控制字符".to_string(),
        (Lang::En, E::IllegalChars) => {
            "Name cannot contain `/`, `\\`, or control chars".to_string()
        }
        (Lang::Zh, E::NameAlreadyExists(name)) => format!("`{name}` 已存在"),
        (Lang::En, E::NameAlreadyExists(name)) => format!("`{name}` already exists"),
    }
}

/// Status-bar takeover while a delete is pending confirmation. `hard`
/// switches the wording so the user sees that Shift+Delete is
/// permanent, not just trash.
pub fn tree_delete_confirm_prompt(name: &str, is_dir: bool, hard: bool) -> String {
    let kind_zh = if is_dir { "文件夹" } else { "文件" };
    let kind_en = if is_dir { "folder" } else { "file" };
    match (lang(), hard) {
        (Lang::Zh, true) => format!("  ⚠ 永久删除{kind_zh} `{name}`？(不可恢复) (y / Esc)  "),
        (Lang::En, true) => {
            format!("  ⚠ Permanently delete {kind_en} `{name}`? (cannot be undone) (y / Esc)  ")
        }
        (Lang::Zh, false) => format!("  ⚠ 把{kind_zh} `{name}` 移到废纸篓？(y / Esc)  "),
        (Lang::En, false) => format!("  ⚠ Move {kind_en} `{name}` to Trash? (y / Esc)  "),
    }
}

/// Warning toast when the user tries to start / confirm another
/// tree mutation while one is still in flight. Prevents the
/// generation-bump race where an earlier worker result would be
/// silently dropped.
pub fn tree_op_blocked_by_in_flight() -> String {
    match lang() {
        Lang::Zh => "上次文件操作还在进行中，稍等再试".to_string(),
        Lang::En => "A file operation is still running — please wait".to_string(),
    }
}

/// Toast when the user invokes Paste with an empty file_clipboard.
pub fn paste_clipboard_empty() -> String {
    match lang() {
        Lang::Zh => "剪贴板为空".to_string(),
        Lang::En => "Clipboard is empty".to_string(),
    }
}

/// Toast when paste resolves to zero actionable items (everything
/// got Skipped).
pub fn paste_nothing_to_do() -> String {
    match lang() {
        Lang::Zh => "没有需要粘贴的项".to_string(),
        Lang::En => "Nothing to paste".to_string(),
    }
}

/// Toast when the user picks Cancel in the conflict prompt.
pub fn paste_cancelled() -> String {
    match lang() {
        Lang::Zh => "已取消粘贴".to_string(),
        Lang::En => "Paste cancelled".to_string(),
    }
}

/// Toast when the user attempts to paste a folder into itself or a
/// descendant of itself.
pub fn paste_self_into_descendant() -> String {
    match lang() {
        Lang::Zh => "不能把目录粘贴到它自身内部".to_string(),
        Lang::En => "Cannot paste a folder into itself".to_string(),
    }
}

/// Toast after `Copy Path` succeeds.
pub fn copy_path_done(count: usize) -> String {
    match (lang(), count) {
        (Lang::Zh, 1) => "已复制路径".to_string(),
        (Lang::En, 1) => "Path copied".to_string(),
        (Lang::Zh, n) => format!("已复制 {n} 条路径"),
        (Lang::En, n) => format!("Copied {n} paths"),
    }
}

/// Toast after `Copy Relative Path` succeeds.
pub fn copy_relative_path_done(count: usize) -> String {
    match (lang(), count) {
        (Lang::Zh, 1) => "已复制相对路径".to_string(),
        (Lang::En, 1) => "Relative path copied".to_string(),
        (Lang::Zh, n) => format!("已复制 {n} 条相对路径"),
        (Lang::En, n) => format!("Copied {n} relative paths"),
    }
}

/// Warn the user that one or more paths contained non-UTF-8 bytes
/// and were lossy-decoded for clipboard write.
pub fn copy_path_lossy_utf8() -> String {
    match lang() {
        Lang::Zh => "路径含非 UTF-8 字符，已替换为 �".to_string(),
        Lang::En => "Path contained non-UTF-8 bytes (replaced with �)".to_string(),
    }
}

/// Status-bar text for the modal paste-conflict prompt. `name` is the
/// existing destination's basename. Keys: R=Replace, S=Skip,
/// K=Keep both, A=apply current (Replace/Skip) to all, C/Esc=Cancel.
pub fn paste_conflict_prompt(name: &str, remaining: usize) -> String {
    match lang() {
        Lang::Zh => format!(
            "  ⚠ `{name}` 已存在 ({remaining} 项待处理) [R]替换 [S]跳过 [K]两者保留 [A]全部应用 [C]取消  "
        ),
        Lang::En => format!(
            "  ⚠ `{name}` already exists ({remaining} pending) [R]eplace [S]kip [K]eep both [A]pply-to-all [C]ancel  "
        ),
    }
}

/// Toast when "Reveal in Finder" fires on a platform we haven't wired
/// up yet.
pub fn tree_reveal_unsupported_platform() -> String {
    match lang() {
        Lang::Zh => "Reveal in Finder 目前只支持 macOS / Windows".to_string(),
        Lang::En => "Reveal in Finder is only supported on macOS / Windows yet".to_string(),
    }
}

/// Right-click context-menu item labels. Kept parallel with
/// `ContextMenuItem` so the render loop can map 1:1.
pub fn tree_context_menu_label(item: &crate::tree_context_menu::ContextMenuItem) -> &'static str {
    use crate::tree_context_menu::ContextMenuItem as I;
    match (lang(), item) {
        (Lang::Zh, I::Cut) => "剪切",
        (Lang::En, I::Cut) => "Cut",
        (Lang::Zh, I::Copy) => "复制",
        (Lang::En, I::Copy) => "Copy",
        (Lang::Zh, I::Paste) => "粘贴",
        (Lang::En, I::Paste) => "Paste",
        (Lang::Zh, I::Duplicate) => "复制副本",
        (Lang::En, I::Duplicate) => "Duplicate",
        (Lang::Zh, I::NewFile) => "新建文件",
        (Lang::En, I::NewFile) => "New File",
        (Lang::Zh, I::NewFolder) => "新建文件夹",
        (Lang::En, I::NewFolder) => "New Folder",
        (Lang::Zh, I::Rename) => "重命名",
        (Lang::En, I::Rename) => "Rename",
        (Lang::Zh, I::Delete) => "删除",
        (Lang::En, I::Delete) => "Delete",
        (Lang::Zh, I::CopyPath) => "复制路径",
        (Lang::En, I::CopyPath) => "Copy Path",
        (Lang::Zh, I::CopyRelativePath) => "复制相对路径",
        (Lang::En, I::CopyRelativePath) => "Copy Relative Path",
        (Lang::Zh, I::RevealInFinder) => "在 Finder 中显示",
        (Lang::En, I::RevealInFinder) => "Reveal in Finder",
    }
}

/// Toolbar button tooltips — rendered inline next to the icon when the
/// panel is wide enough. Narrow panels render icons only.
pub fn tree_toolbar_new_file() -> &'static str {
    match lang() {
        Lang::Zh => "新建文件",
        Lang::En => "New File",
    }
}
pub fn tree_toolbar_new_folder() -> &'static str {
    match lang() {
        Lang::Zh => "新建文件夹",
        Lang::En => "New Folder",
    }
}
pub fn tree_toolbar_refresh() -> &'static str {
    match lang() {
        Lang::Zh => "刷新",
        Lang::En => "Refresh",
    }
}
pub fn tree_toolbar_collapse_all() -> &'static str {
    match lang() {
        Lang::Zh => "全部折叠",
        Lang::En => "Collapse All",
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
