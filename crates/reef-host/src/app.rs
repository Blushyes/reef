use crate::git::{DiffContent, FileEntry, GitRepo};
use crate::mouse::{ClickAction, HitTestRegistry};
use crate::plugin::manager::PluginManager;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Files,
    Diff,
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
    pub repo: GitRepo,

    // File state
    pub staged_files: Vec<FileEntry>,
    pub unstaged_files: Vec<FileEntry>,

    // UI state
    pub selected_file: Option<SelectedFile>,
    pub active_panel: Panel,
    pub diff_content: Option<DiffContent>,
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,

    // Sections
    pub staged_collapsed: bool,
    pub unstaged_collapsed: bool,

    // Scroll
    pub file_scroll: usize,
    pub diff_scroll: usize,

    // Layout
    pub split_percent: u16, // 0-100, left panel width percentage
    pub dragging_split: bool,

    // Mouse
    pub hit_registry: HitTestRegistry,
    pub hover_row: Option<u16>,

    // Plugin system
    pub plugin_manager: PluginManager,
    pub active_sidebar_panel: Option<String>, // panel id

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
    pub fn new() -> Result<Self, git2::Error> {
        let repo = GitRepo::open()?;
        let (saved_layout, saved_mode) = load_prefs();
        let mut app = Self {
            repo,
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            selected_file: None,
            active_panel: Panel::Files,
            diff_content: None,
            diff_layout: saved_layout,
            diff_mode: saved_mode,
            staged_collapsed: false,
            unstaged_collapsed: false,
            file_scroll: 0,
            diff_scroll: 0,
            split_percent: 30,
            dragging_split: false,
            hit_registry: HitTestRegistry::new(),
            hover_row: None,
            plugin_manager: PluginManager::new(),
            active_sidebar_panel: None,
            should_quit: false,
            select_mode: false,
            show_help: false,
        };
        app.refresh_status();
        app.load_plugins();
        Ok(app)
    }

    pub fn refresh_status(&mut self) {
        let (staged, unstaged) = self.repo.get_status();
        self.staged_files = staged;
        self.unstaged_files = unstaged;

        // If selected file no longer exists in either list, clear selection
        if let Some(ref sel) = self.selected_file {
            let still_exists = if sel.is_staged {
                self.staged_files.iter().any(|f| f.path == sel.path)
            } else {
                self.unstaged_files.iter().any(|f| f.path == sel.path)
            };
            if !still_exists {
                self.selected_file = None;
                self.diff_content = None;
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
        if let Some(ref sel) = self.selected_file {
            let context = match self.diff_mode {
                DiffMode::FullFile => 9999,
                DiffMode::Compact => 3,
            };
            self.diff_content = self.repo.get_diff(&sel.path, sel.is_staged, context);
        }
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
        if self.repo.stage_file(path).is_ok() {
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
        if self.repo.unstage_file(path).is_ok() {
            if let Some(ref mut sel) = self.selected_file {
                if sel.path == path && sel.is_staged {
                    sel.is_staged = false;
                }
            }
            self.refresh_status();
            self.load_diff();
        }
    }

    pub fn handle_action(&mut self, action: ClickAction) {
        match action {
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
            ClickAction::PluginCommand { command, args } => {
                self.plugin_manager.execute_command(&command, args);
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

        let (path, staged) = &items[new_idx];
        self.select_file(path, *staged);
    }

    /// Returns the plugin panel_id for the currently focused panel, if any.
    pub fn focused_plugin_panel(&self) -> Option<String> {
        match self.active_panel {
            Panel::Files => self.active_sidebar_panel.clone(),
            Panel::Diff => self
                .plugin_manager
                .panels
                .iter()
                .find(|p| p.decl.slot == reef_protocol::PanelSlot::Editor)
                .map(|p| p.decl.id.clone()),
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
            let builtin = exe.parent().unwrap_or(std::path::Path::new(".")).join("plugins");
            self.plugin_manager.load_from_dir(&builtin);
        }

        // 2. Dev mode: look for plugins/ next to the workspace root
        //    (covers `cargo run` from the project directory)
        if self.plugin_manager.panels.is_empty() {
            let dev_paths = [
                // workspace root / plugins/
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent().unwrap_or(std::path::Path::new("."))
                    .parent().unwrap_or(std::path::Path::new("."))
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
            let user = PathBuf::from(home).join(".config").join("reef").join("plugins");
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

        // Handle plugin→host requests
        let requests: Vec<_> = self.plugin_manager.pending_host_requests.drain(..).collect();
        for req in requests {
            match req.method.as_str() {
                "reef/openFile" => {
                    if let Ok(p) = serde_json::from_value::<reef_protocol::OpenFileParams>(req.params.clone()) {
                        // TODO: open file in editor panel
                        eprintln!("[reef] openFile: {}", p.path);
                    }
                    let _ = self.plugin_manager.respond_to_plugin(
                        &req,
                        serde_json::json!({ "success": true }),
                    );
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
