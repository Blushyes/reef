use crate::file_tree::{self, FileTree, PreviewContent};
use crate::git::{DiffContent, FileEntry, GitRepo};
use crate::mouse::{ClickAction, HitTestRegistry};
use crate::plugin::manager::PluginManager;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Git,
    Files,
    Graph,
}

impl Tab {
    /// Canonical ordering shared by the tab bar renderer and the digit shortcut.
    pub const ALL: &'static [Tab] = &[Tab::Files, Tab::Git, Tab::Graph];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Files => " 📁 Files ",
            Tab::Git => " ⎇ Git ",
            Tab::Graph => " ⑂ Graph ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Files, // left
    Diff,  // right
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLayout {
    Unified,    // 上下统一视图
    SideBySide, // 左右对比视图
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Compact,  // 只显示变更区域 ± context
    FullFile, // 显示整个文件
}

pub struct App {
    pub repo: Option<GitRepo>,

    // Tab
    pub active_tab: Tab,
    pub active_panel: Panel,

    // ── Git tab state ──
    pub staged_files: Vec<FileEntry>,
    pub unstaged_files: Vec<FileEntry>,
    pub selected_file: Option<SelectedFile>,
    pub diff_content: Option<DiffContent>,
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub staged_collapsed: bool,
    pub unstaged_collapsed: bool,
    pub file_scroll: usize,
    pub diff_scroll: usize,

    // ── Files tab state ──
    pub file_tree: FileTree,
    pub preview_content: Option<PreviewContent>,
    pub tree_scroll: usize,
    pub preview_scroll: usize,

    // Layout
    pub split_percent: u16,
    pub dragging_split: bool,

    // Mouse
    pub hit_registry: HitTestRegistry,
    pub hover_row: Option<u16>,
    pub hover_col: Option<u16>,
    /// (timestamp, column, row) of the last mouse-down — used to detect double-clicks.
    pub last_click: Option<(Instant, u16, u16)>,

    // Plugin system
    pub plugin_manager: PluginManager,
    pub active_sidebar_panel: Option<String>,
    /// Per-plugin-panel scroll offsets, keyed by panel_id. Host-native panels
    /// (file_tree, file_preview, legacy diff/file) keep their own scalars.
    pub panel_scroll: HashMap<String, usize>,

    // Control
    pub should_quit: bool,
    pub select_mode: bool,
    pub show_help: bool,
}

#[derive(Debug, Clone)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

impl App {
    pub fn new() -> Self {
        let repo = GitRepo::open().ok();
        let workdir = repo
            .as_ref()
            .and_then(|r| r.workdir_path())
            .unwrap_or_else(|| PathBuf::from("."));
        let file_tree = FileTree::new(&workdir);
        let (saved_layout, saved_mode) = load_prefs();
        let mut app = Self {
            repo,
            active_tab: Tab::Files,
            active_panel: Panel::Files,
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            selected_file: None,
            diff_content: None,
            diff_layout: saved_layout,
            diff_mode: saved_mode,
            staged_collapsed: false,
            unstaged_collapsed: false,
            file_scroll: 0,
            diff_scroll: 0,
            file_tree,
            preview_content: None,
            tree_scroll: 0,
            preview_scroll: 0,
            split_percent: 30,
            dragging_split: false,
            hit_registry: HitTestRegistry::new(),
            hover_row: None,
            hover_col: None,
            last_click: None,
            plugin_manager: PluginManager::new(),
            active_sidebar_panel: None,
            panel_scroll: HashMap::new(),
            should_quit: false,
            select_mode: false,
            show_help: false,
        };
        app.refresh_status();
        app.load_plugins();
        app
    }

    pub fn refresh_status(&mut self) {
        let Some(ref repo) = self.repo else {
            return;
        };
        let (staged, unstaged) = repo.get_status();
        self.staged_files = staged;
        self.unstaged_files = unstaged;

        self.file_tree
            .refresh_git_statuses(&self.staged_files, &self.unstaged_files);

        // Reconcile selection: check both lists so is_staged stays correct
        // even if the plugin staged/unstaged via a key event that the host
        // didn't directly observe.
        if let Some(ref mut sel) = self.selected_file {
            let in_staged = self.staged_files.iter().any(|f| f.path == sel.path);
            let in_unstaged = self.unstaged_files.iter().any(|f| f.path == sel.path);
            if in_staged {
                sel.is_staged = true;
            } else if in_unstaged {
                sel.is_staged = false;
            } else {
                self.selected_file = None;
                self.diff_content = None;
            }
        }
    }

    pub fn load_preview(&mut self) {
        if let Some(entry) = self.file_tree.selected_entry() {
            if !entry.is_dir {
                self.preview_content = file_tree::load_preview(&self.file_tree.root, &entry.path);
                self.preview_scroll = 0;
            }
        }
    }

    pub fn select_file(&mut self, path: &str, is_staged: bool) {
        self.selected_file = Some(SelectedFile {
            path: path.to_string(),
            is_staged,
        });
        self.diff_scroll = 0;
        self.load_diff();
    }

    pub fn load_diff(&mut self) {
        let diff = if let (Some(repo), Some(sel)) = (&self.repo, &self.selected_file) {
            let context = match self.diff_mode {
                DiffMode::FullFile => 9999,
                DiffMode::Compact => 3,
            };
            repo.get_diff(&sel.path, sel.is_staged, context)
        } else {
            None
        };
        self.diff_content = diff;
    }

    pub fn toggle_diff_layout(&mut self) {
        self.diff_layout = match self.diff_layout {
            DiffLayout::Unified => DiffLayout::SideBySide,
            DiffLayout::SideBySide => DiffLayout::Unified,
        };
        self.diff_scroll = 0;
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn toggle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
        self.diff_scroll = 0;
        self.load_diff();
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn stage_file(&mut self, path: &str) {
        let ok = self
            .repo
            .as_ref()
            .map(|r| r.stage_file(path).is_ok())
            .unwrap_or(false);
        if ok {
            // If we were viewing this file, update selection
            if let Some(ref mut sel) = self.selected_file {
                if sel.path == path && !sel.is_staged {
                    sel.is_staged = true;
                }
            }
            self.refresh_status();
            self.load_diff();
        }
    }

    pub fn unstage_file(&mut self, path: &str) {
        let ok = self
            .repo
            .as_ref()
            .map(|r| r.unstage_file(path).is_ok())
            .unwrap_or(false);
        if ok {
            if let Some(ref mut sel) = self.selected_file {
                if sel.path == path && sel.is_staged {
                    sel.is_staged = false;
                }
            }
            self.refresh_status();
            self.load_diff();
        }
    }

    pub fn stage_all(&mut self) {
        let paths: Vec<String> = self.unstaged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            if let Some(ref repo) = self.repo {
                let _ = repo.stage_file(p);
            }
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = true;
            }
        }
        self.refresh_status();
        self.load_diff();
        // fs watcher will re-invalidate plugin panels shortly, but invalidate
        // now so the sidebar updates without waiting on the debounce.
        self.plugin_manager.invalidate_panels();
    }

    pub fn unstage_all(&mut self) {
        let paths: Vec<String> = self.staged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            if let Some(ref repo) = self.repo {
                let _ = repo.unstage_file(p);
            }
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = false;
            }
        }
        self.refresh_status();
        self.load_diff();
        self.plugin_manager.invalidate_panels();
    }

    pub fn handle_action(&mut self, action: ClickAction) {
        match action {
            ClickAction::SwitchTab(tab) => {
                self.active_tab = tab;
            }
            ClickAction::TreeClick(index) => {
                self.file_tree.selected = index;
                if let Some(entry) = self.file_tree.entries.get(index) {
                    if entry.is_dir {
                        self.file_tree.toggle_expand(index);
                    } else {
                        self.load_preview();
                    }
                }
            }
            ClickAction::SelectFile { path, staged } => {
                self.select_file(&path, staged);
            }
            ClickAction::StageFile(path) => {
                self.stage_file(&path);
            }
            ClickAction::UnstageFile(path) => {
                self.unstage_file(&path);
            }
            ClickAction::ToggleStaged => {
                self.staged_collapsed = !self.staged_collapsed;
            }
            ClickAction::ToggleUnstaged => {
                self.unstaged_collapsed = !self.unstaged_collapsed;
            }
            ClickAction::StartDragSplit => {
                self.dragging_split = true;
            }
            ClickAction::PluginCommand { command, args, .. } => {
                // Reset commit-detail scroll so a new commit starts at the top.
                if command == "git.selectCommit" {
                    self.panel_scroll.insert("git.commitDetail".into(), 0);
                }
                // Keep host state in sync for known selection commands
                if command == "git.selectFile" {
                    if let (Some(path), Some(staged)) = (
                        args.get("path")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        args.get("staged").and_then(|v| v.as_bool()),
                    ) {
                        self.selected_file = Some(SelectedFile {
                            path,
                            is_staged: staged,
                        });
                        self.diff_scroll = 0;
                        self.load_diff();
                    }
                }
                // Sync collapsed state so navigate_files skips collapsed sections correctly
                if command == "git.toggleStaged" {
                    self.staged_collapsed = !self.staged_collapsed;
                }
                if command == "git.toggleUnstaged" {
                    self.unstaged_collapsed = !self.unstaged_collapsed;
                }
                // Stage/unstage: host executes directly so its state is
                // immediately consistent; also forward to plugin so its
                // sidebar refreshes.
                if command == "git.stage" {
                    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                        self.stage_file(path);
                    }
                    self.plugin_manager.execute_command(&command, args);
                } else if command == "git.unstage" {
                    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                        self.unstage_file(path);
                    }
                    self.plugin_manager.execute_command(&command, args);
                } else if command == "git.stageAll" {
                    self.stage_all();
                } else if command == "git.unstageAll" {
                    self.unstage_all();
                } else {
                    self.plugin_manager.execute_command(&command, args);
                }
            }
        }
    }

    /// Total visible file rows (for keyboard navigation)
    pub fn visible_file_count(&self) -> usize {
        let mut count = 0;
        if !self.staged_files.is_empty() {
            count += 1; // header
            if !self.staged_collapsed {
                count += self.staged_files.len();
            }
        }
        count += 1; // unstaged header
        if !self.unstaged_collapsed {
            count += self.unstaged_files.len();
        }
        count
    }

    pub fn navigate_files(&mut self, delta: i32) {
        // Build a flat list of selectable items
        let mut items: Vec<(String, bool)> = Vec::new();

        if !self.staged_files.is_empty() && !self.staged_collapsed {
            for f in &self.staged_files {
                items.push((f.path.clone(), true));
            }
        }
        if !self.unstaged_collapsed {
            for f in &self.unstaged_files {
                items.push((f.path.clone(), false));
            }
        }

        if items.is_empty() {
            return;
        }

        let current_idx = self
            .selected_file
            .as_ref()
            .and_then(|sel| {
                items
                    .iter()
                    .position(|(p, s)| p == &sel.path && *s == sel.is_staged)
            })
            .unwrap_or(0);

        let new_idx = if delta > 0 {
            (current_idx + delta as usize).min(items.len() - 1)
        } else {
            current_idx.saturating_sub((-delta) as usize)
        };

        let (path, staged) = items[new_idx].clone();
        // Only update host selection state; caller is responsible for syncing to plugin
        // so rapid key repeats can be coalesced into a single command.
        self.selected_file = Some(SelectedFile {
            path,
            is_staged: staged,
        });
        self.diff_scroll = 0;
    }

    /// Returns the plugin panel_id for the currently focused panel, if any.
    pub fn focused_plugin_panel(&self) -> Option<String> {
        match (self.active_tab, self.active_panel) {
            (Tab::Graph, Panel::Files) => Some("git.graph".into()),
            (Tab::Graph, Panel::Diff) => Some("git.commitDetail".into()),
            (_, Panel::Files) => self.active_sidebar_panel.clone(),
            (_, Panel::Diff) => None, // diff is host-native, no plugin panel
        }
    }

    /// Route a key event to the plugin that owns the currently focused panel.
    /// Returns true if the event was forwarded to a plugin.
    pub fn route_key_to_plugin(&mut self, key: &str) -> bool {
        let Some(panel_id) = self.focused_plugin_panel() else {
            return false;
        };
        self.plugin_manager.send_key_event(&panel_id, key, vec![])
    }

    /// Discover and start plugins from known locations.
    pub fn load_plugins(&mut self) {
        // 1. Built-in plugins shipped alongside the binary
        if let Ok(exe) = std::env::current_exe() {
            let builtin = exe
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("plugins");
            self.plugin_manager.load_from_dir(&builtin);
        }

        // 2. Dev mode: look for plugins/ next to the workspace root
        //    (covers `cargo run` from the project directory)
        if self.plugin_manager.panels.is_empty() {
            let dev_paths = [
                // workspace root / plugins/
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .join("plugins"),
            ];
            for path in &dev_paths {
                if path.exists() {
                    self.plugin_manager.load_from_dir(path);
                }
            }
        }

        // 3. User plugins in ~/.config/reef/plugins/
        if let Ok(home) = std::env::var("HOME") {
            let user = PathBuf::from(home)
                .join(".config")
                .join("reef")
                .join("plugins");
            self.plugin_manager.load_from_dir(&user);
        }

        // Set default active sidebar panel to first sidebar panel
        if self.active_sidebar_panel.is_none() {
            if let Some(p) = self.plugin_manager.sidebar_panels().first() {
                self.active_sidebar_panel = Some(p.decl.id.clone());
            }
        }
    }

    /// Called every frame: let plugin manager process incoming messages.
    pub fn tick_plugins(&mut self) {
        self.plugin_manager.tick();

        // If the plugin refreshed its git state (after staging/unstaging), sync host file list
        if self.plugin_manager.status_refresh_needed {
            // statusChanged only fires on stage/unstage (not file selection),
            // so a synchronous refresh here is acceptable — it's user-initiated.
            self.refresh_status();
            self.load_diff();
        }

        // Handle plugin→host requests
        let requests: Vec<_> = self
            .plugin_manager
            .pending_host_requests
            .drain(..)
            .collect();
        for req in requests {
            match req.method.as_str() {
                "reef/openFile" => {
                    if let Ok(p) =
                        serde_json::from_value::<reef_protocol::OpenFileParams>(req.params.clone())
                    {
                        // TODO: open file in editor panel
                        eprintln!("[reef] openFile: {}", p.path);
                    }
                    let _ = self
                        .plugin_manager
                        .respond_to_plugin(&req, serde_json::json!({ "success": true }));
                }
                _ => {}
            }
        }
    }
}

// ─── Prefs persistence ────────────────────────────────────────────────────────

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config").join("reef");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("prefs"))
}

fn load_prefs() -> (DiffLayout, DiffMode) {
    let default = (DiffLayout::Unified, DiffMode::Compact);
    let path = match prefs_path() {
        Some(p) => p,
        None => return default,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return default,
    };
    let mut layout = DiffLayout::Unified;
    let mut mode = DiffMode::Compact;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("layout=") {
            layout = match val.trim() {
                "side_by_side" => DiffLayout::SideBySide,
                _ => DiffLayout::Unified,
            };
        } else if let Some(val) = line.strip_prefix("mode=") {
            mode = match val.trim() {
                "full_file" => DiffMode::FullFile,
                _ => DiffMode::Compact,
            };
        }
    }
    (layout, mode)
}

fn save_prefs(layout: DiffLayout, mode: DiffMode) {
    if let Some(path) = prefs_path() {
        let layout_str = match layout {
            DiffLayout::Unified => "unified",
            DiffLayout::SideBySide => "side_by_side",
        };
        let mode_str = match mode {
            DiffMode::Compact => "compact",
            DiffMode::FullFile => "full_file",
        };
        let content = format!("layout={}\nmode={}\n", layout_str, mode_str);
        let _ = std::fs::write(path, content);
    }
}
