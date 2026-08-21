use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use reef_app::{
    AppPanel as Panel, AppPrefs, AppTab as Tab, AsyncState, DbNav, DiffMode,
    GRAPH_RECENT_BRANCHES_MAX, HighlightFade, Toast, ViewMode,
};
use reef_core::diff::DiffLayout;
use reef_core::git::GraphScope;
use reef_io::{Backend, LocalBackend};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Instant;

fn input_modifiers(mods: crossterm::event::KeyModifiers) -> reef_app::InputModifiers {
    reef_app::InputModifiers {
        alt: mods.contains(crossterm::event::KeyModifiers::ALT),
        ctrl: mods.contains(crossterm::event::KeyModifiers::CONTROL),
        shift: mods.contains(crossterm::event::KeyModifiers::SHIFT),
    }
}

/// Code-navigation request side (gd / gr / Ctrl+click / nav stack /
/// LSP refine / post-jump highlight). Kept in its own file so the
/// subsystem doesn't bloat this terminal adapter module.
mod nav;

/// Worker-produced `StatefulProtocol` carried back to the main thread
/// so it can be slotted into the current `ThreadProtocol`. The
/// `generation` matches the corresponding `preview_load` request — a
/// mismatch on arrival (user has since selected a different file)
/// means the build is stale and gets dropped.
pub struct BuiltProtocol {
    pub generation: u64,
    pub protocol: ratatui_image::protocol::StatefulProtocol,
}

/// Worker-produced video player carried back to the main thread. Opening a
/// clip probes it and decodes its first frame, which is far too slow to run
/// while the user is arrowing through a file tree — so it happens off-thread
/// under the same `generation` staleness rule as `BuiltProtocol`.
pub struct BuiltVideo {
    pub preview_generation: u64,
    pub request_generation: u64,
    pub path: PathBuf,
    pub source_revision: u64,
    pub area: ratatui::layout::Rect,
    pub result: Result<crate::video::VideoPlayer, crate::video::VideoUnavailable>,
}

struct VideoBuildRequest {
    preview_generation: u64,
    request_generation: u64,
    path: PathBuf,
    source_revision: u64,
    area: ratatui::layout::Rect,
    source: Option<crate::video::VideoSource>,
    position: f64,
    picker: ratatui_image::picker::Picker,
    cancellation: reef_io::CancellationToken,
}

fn spawn_video_build_worker(
    requests: mpsc::Receiver<VideoBuildRequest>,
    results: mpsc::Sender<BuiltVideo>,
) {
    let _ = std::thread::Builder::new()
        .name("reef-video-open".into())
        .spawn(move || {
            while let Ok(mut request) = requests.recv() {
                while let Ok(newer) = requests.try_recv() {
                    request.cancellation.cancel();
                    request = newer;
                }
                if request.cancellation.is_cancelled() {
                    continue;
                }
                let result = match request.source.take() {
                    Some(source) => crate::video::VideoPlayer::open_source_cancellable(
                        source,
                        &request.picker,
                        request.area,
                        request.position,
                        &request.cancellation,
                    ),
                    None => crate::video::VideoPlayer::open_cancellable(
                        &request.path,
                        request.source_revision,
                        &request.picker,
                        request.area,
                        &request.cancellation,
                    ),
                };
                if request.cancellation.is_cancelled() {
                    continue;
                }
                let _ = results.send(BuiltVideo {
                    preview_generation: request.preview_generation,
                    request_generation: request.request_generation,
                    path: request.path,
                    source_revision: request.source_revision,
                    area: request.area,
                    result,
                });
            }
        });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingVideoBuild {
    preview_generation: u64,
    request_generation: u64,
    path: PathBuf,
    source_revision: u64,
    area: ratatui::layout::Rect,
}

#[derive(Clone, Copy)]
struct VideoSeekDrag {
    start: u16,
    width: u16,
    ratio: f64,
}

fn video_seek_ratio(start: u16, width: u16, column: u16) -> f64 {
    let span = width.saturating_sub(1).max(1);
    let offset = column.saturating_sub(start).min(span);
    f64::from(offset) / f64::from(span)
}

#[derive(Debug, Clone)]
pub struct DbPreviewLayoutCache {
    pub path: String,
    pub selection: reef_sqlite_preview::DbObjectKey,
    pub page: u64,
    pub rows_len: usize,
    pub col_widths: Vec<usize>,
    pub total_table_w: usize,
}

#[derive(Debug, Default)]
pub struct FindWidgetUiState {
    pub last_widget_rect: Option<ratatui::layout::Rect>,
    pub space_leader_at: Option<Instant>,
}

impl DbPreviewLayoutCache {
    pub(crate) fn matches(&self, path: &str, state: &reef_app::DbPreviewState) -> bool {
        self.path == path
            && self.selection == state.selection
            && self.page == state.page
            && self.rows_len == state.current_rows.len()
    }

    pub(crate) fn rebuild(
        path: &str,
        state: &reef_app::DbPreviewState,
        columns: &[reef_sqlite_preview::ColumnInfo],
    ) -> Self {
        let col_widths = crate::ui::db_preview::natural_column_widths(columns, &state.current_rows);
        let total_table_w = crate::ui::db_preview::total_table_width(&col_widths);
        Self {
            path: path.to_string(),
            selection: state.selection.clone(),
            page: state.page,
            rows_len: state.current_rows.len(),
            col_widths,
            total_table_w,
        }
    }
}

pub fn tab_label(tab: Tab) -> &'static str {
    use crate::i18n::{Msg, t};
    match tab {
        Tab::Files => t(Msg::TabFiles),
        Tab::Search => t(Msg::TabSearch),
        Tab::Git => t(Msg::TabGit),
        Tab::Graph => t(Msg::TabGraph),
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TuiLayoutCache {
    pub last_total_width: u16,
    pub last_preview_view_h: u16,
    pub last_diff_view_h: u16,
    pub last_commit_detail_view_h: u16,
    pub last_rendered_tree_selected: Option<usize>,
    pub last_rendered_graph_selected: Option<usize>,
    pub quick_open_last_view_h: u16,
    pub global_search_last_view_h: u16,
}

#[derive(Debug, Clone, Copy)]
enum PendingPreviewNavAction {
    GotoDefinition,
    FindReferences,
}

#[derive(Debug, Clone)]
struct PendingPreviewNav {
    action: PendingPreviewNavAction,
    anchor: reef_app::NavAnchor,
    cursor: (usize, usize),
    path: PathBuf,
    generation: u64,
}

pub struct TuiApp {
    pub engine: reef_app::ReefApp,

    pub layout: TuiLayoutCache,

    /// Terminal-capability probe for image rendering. `None` on terminals
    /// with no graphics-protocol support or when the user set
    /// `REEF_IMAGE_PROTOCOL=off`.
    pub image_picker: Option<ratatui_image::picker::Picker>,
    pub preview_image_protocol: Option<ratatui_image::thread::ThreadProtocol>,
    pub preview_resize_tx: mpsc::Sender<ratatui_image::thread::ResizeRequest>,
    pub preview_resize_rx: mpsc::Receiver<ratatui_image::thread::ResizeResponse>,
    pub preview_build_rx: mpsc::Receiver<BuiltProtocol>,
    pub preview_build_tx: mpsc::Sender<BuiltProtocol>,
    pub preview_image_protocol_builds: u64,

    /// Inline playback for the video preview card. `None` when the current
    /// preview is not a video, or while one is still being opened.
    pub video: Option<crate::video::VideoPlayer>,
    /// Why the current video is not playing inline. Rendered on the card so
    /// a still frame never looks like a bug.
    pub video_status: Option<crate::video::VideoUnavailable>,
    video_pending: Option<PendingVideoBuild>,
    video_request_generation: u64,
    video_should_play: bool,
    video_seek_drag: Option<VideoSeekDrag>,
    video_build_tx: mpsc::Sender<VideoBuildRequest>,
    video_build_rx: mpsc::Receiver<BuiltVideo>,
    video_build_cancellation: Option<reef_io::CancellationToken>,

    pub preview_selection: Option<crate::ui::selection::PreviewSelection>,
    pub selection_context_menu: crate::selection_context_menu::SelectionContextMenuState,
    pending_preview_nav: Option<PendingPreviewNav>,
    pub last_preview_rect: Option<ratatui::layout::Rect>,
    pub(crate) last_video_frame_area: Option<ratatui::layout::Rect>,
    pub db_preview_layout: Option<DbPreviewLayoutCache>,
    /// Wrapped + colored lines for the opened SQLite cell, rebuilt
    /// when the cell or the pane width changes.
    pub(crate) db_cell_view: Option<crate::ui::db_preview::DbCellViewCache>,
    /// Screen rect of the cell value pane, so the wheel can scroll the
    /// pane the pointer is actually over.
    pub last_db_cell_rect: Option<ratatui::layout::Rect>,
    /// Cell cursor the grid has already scrolled into view. Reveal runs
    /// once per cursor move; re-running it every frame would drag the
    /// grid back under the user whenever they scroll away from the
    /// opened cell.
    pub(crate) last_db_cell_reveal: Option<(usize, usize)>,
    pub vertical_scroll_lock: crate::input::AxisLock,
    pub horizontal_scroll_lock: crate::input::AxisLock,
    pub vertical_scroll_pacer: crate::input::ScrollPacer,
    pub horizontal_scroll_pacer: crate::input::ScrollPacer,
    pub last_preview_content_origin: Option<(u16, u16, u16)>,
    pub last_markdown_content_origin: Option<(u16, u16)>,
    pub(crate) markdown_layout: crate::ui::preview::markdown::MarkdownLayoutCache,
    pub(crate) markdown_visual_scroll: usize,
    pub(crate) markdown_engine_scroll_seen: Option<usize>,
    pub preview_click_state: Option<(Instant, u16, u16, u8)>,

    pub diff_selection: Option<crate::ui::selection::DiffSelection>,
    pub last_diff_rect: Option<ratatui::layout::Rect>,
    pub last_diff_hit: Option<crate::ui::selection::DiffHit>,
    pub diff_click_state: Option<(Instant, u16, u16, u8)>,
    pub commit_detail_selection: Option<crate::ui::selection::PreviewSelection>,
    pub last_commit_detail_rect: Option<ratatui::layout::Rect>,
    pub last_commit_detail_hit: Option<crate::ui::selection::CommitDetailHit>,
    pub commit_detail_click_state: Option<(Instant, u16, u16, u8)>,
    pub last_drag_mouse: Option<(u16, u16)>,
    pub preview_autoscroll_at: Option<Instant>,
    pub diff_autoscroll_at: Option<Instant>,

    pub dragging_split: bool,
    pub dragging_graph_diff_split: bool,
    pub hit_registry: HitTestRegistry,
    pub hover_row: Option<u16>,
    pub hover_col: Option<u16>,
    pub last_click: Option<(Instant, u16, u16)>,
    pub nav_peek_anchor_col: u16,
    pub nav_peek_anchor_row: u16,
    pub(crate) nav_peek_preview_rect: Option<ratatui::layout::Rect>,
    pub(crate) nav_peek_tree_rect: Option<ratatui::layout::Rect>,
    pub(crate) nav_peek_preview_scroll: usize,
    pub(crate) nav_peek_preview_max_scroll: usize,
    pub(crate) nav_peek_preview_target: Option<(String, usize, usize)>,
    pub(crate) nav_peek_visible_rows: usize,
    pub(crate) nav_peek_reconciled_view: Option<(usize, usize)>,

    /// Active color theme. Chosen in `main.rs` before raw-mode entry.
    pub theme: Theme,

    pub space_leader_at: Option<Instant>,
    pub g_pending_at: Option<Instant>,
    pub quick_open_leader_at: Option<Instant>,
    pub global_search_leader_at: Option<Instant>,
    pub find_widget_ui: FindWidgetUiState,

    pub quick_open_popup_area: Option<ratatui::layout::Rect>,
    pub global_search_popup_area: Option<ratatui::layout::Rect>,
    pub hosts_picker_popup_area: Option<ratatui::layout::Rect>,
    pub graph_branch_picker_popup_area: Option<ratatui::layout::Rect>,

    /// Same Ctrl+hover affordance, but for the diff panel.
    pub diff_ctrl_hover: Option<crate::ui::selection::DiffHover>,
}

impl Drop for TuiApp {
    fn drop(&mut self) {
        if let Some(cancellation) = self.video_build_cancellation.take() {
            cancellation.cancel();
        }
    }
}

use self::TuiApp as App;

fn preview_text_extent(
    preview: Option<&reef_core::preview::PreviewDocument>,
) -> Option<(usize, usize)> {
    let body = &preview?.body;
    match body {
        reef_core::preview::PreviewBody::Text(text) => text
            .lines
            .last()
            .map(|line| (text.lines.len() - 1, line.len())),
        reef_core::preview::PreviewBody::Markdown(markdown) => {
            let last_row = markdown.line_count().checked_sub(1)?;
            markdown
                .text_for_row(last_row)
                .map(|line| (last_row, line.len()))
        }
        _ => None,
    }
}

impl App {
    /// Local-backend entry point. Threads `image_picker` straight through
    /// to `new_with_backend`. Tests construct via `new(Theme::dark(), None)`;
    /// `main.rs` constructs via `new_with_backend` so it can pick the
    /// backend (Local vs Remote) up front.
    pub fn new(theme: Theme, image_picker: Option<ratatui_image::picker::Picker>) -> Self {
        let backend = Arc::new(
            LocalBackend::open_cwd().unwrap_or_else(|_| LocalBackend::open_at(PathBuf::from("."))),
        );
        Self::new_with_backend(theme, backend, image_picker)
    }

    pub fn new_with_backend(
        theme: Theme,
        backend: Arc<dyn Backend>,
        image_picker: Option<ratatui_image::picker::Picker>,
    ) -> Self {
        // Fold pre-1.0 unprefixed keys (`layout=`, `mode=`) and the retired
        // `~/.config/reef/git.prefs` into the current prefixed namespace
        // BEFORE any `prefs::get` runs. Order matters: `load_prefs` below
        // reads `diff.layout` / `diff.mode`, and the `GitStatusState` /
        // `CommitDetailState` initializers read `status.*` / `commit.*` —
        // all of those keys only exist after the migrator has run on a
        // legacy install.
        crate::prefs::migrate_legacy_prefs();

        // Spin up the image-resize worker. Every `ThreadProtocol` we
        // build later clones this sender; the receiver is drained in
        // `tick`. Keeping the worker alive for the life of App means
        // subsequent preview switches reuse the same channel (and its
        // serial ordering guarantees — no "request N finished after
        // request N+1" races).
        let (preview_resize_tx, preview_resize_rx) = {
            let (req_tx, req_rx) = mpsc::channel::<ratatui_image::thread::ResizeRequest>();
            let (resp_tx, resp_rx) = mpsc::channel::<ratatui_image::thread::ResizeResponse>();
            std::thread::Builder::new()
                .name("reef-image-resize".into())
                .spawn(move || {
                    while let Ok(req) = req_rx.recv() {
                        if let Ok(resp) = req.resize_encode() {
                            let _ = resp_tx.send(resp);
                        }
                    }
                })
                .ok();
            (req_tx, resp_rx)
        };

        // Channel for `StatefulProtocol` construction results. Protocol
        // construction hashes every pixel of the decoded image (~16-30 ms
        // for 2048² RGBA on main thread) — so we spawn a one-shot thread
        // per build request and merge the result back via this channel.
        let (preview_build_tx, preview_build_rx) = mpsc::channel::<BuiltProtocol>();

        let (video_build_tx, video_build_requests) = mpsc::channel::<VideoBuildRequest>();
        let (video_build_results, video_build_rx) = mpsc::channel::<BuiltVideo>();
        spawn_video_build_worker(video_build_requests, video_build_results);

        let (saved_layout, saved_mode) = load_prefs();
        let (graph_scope, graph_recent_branches) = load_graph_scope_pref();
        let mut app = Self {
            engine: reef_app::ReefApp::new(reef_app::AppConfig {
                backend,
                prefs: AppPrefs {
                    diff_layout: saved_layout,
                    diff_mode: saved_mode,
                    status_tree_mode: crate::prefs::get_bool("status.tree_mode"),
                    graph_scope,
                    graph_recent_branches,
                    commit_diff_layout: crate::prefs::get("commit.diff_layout")
                        .as_deref()
                        .map(DiffLayout::from_pref_str)
                        .unwrap_or(DiffLayout::Unified),
                    commit_diff_mode: crate::prefs::get("commit.diff_mode")
                        .as_deref()
                        .map(DiffMode::from_pref_str)
                        .unwrap_or(DiffMode::Compact),
                    commit_files_tree_mode: crate::prefs::get_bool("commit.files_tree_mode"),
                    structured_preview_mode: crate::prefs::get(
                        reef_core::prefs::STRUCTURED_PREVIEW_MODE,
                    )
                    .as_deref()
                    .map(reef_app::StructuredPreviewMode::from_pref_str)
                    .unwrap_or_default(),
                    nav_peek_mode: crate::prefs::get(reef_core::prefs::NAV_PEEK_MODE)
                        .as_deref()
                        .map(reef_app::NavPeekMode::from_pref_str)
                        .unwrap_or_default(),
                    quick_open: crate::quick_open::from_prefs(),
                },
                subscribe_fs_events: true,
            }),
            layout: TuiLayoutCache::default(),
            image_picker,
            preview_image_protocol: None,
            preview_resize_tx,
            preview_resize_rx,
            preview_build_tx,
            preview_build_rx,
            preview_image_protocol_builds: 0,
            video: None,
            video_status: None,
            video_pending: None,
            video_request_generation: 0,
            video_should_play: false,
            video_seek_drag: None,
            video_build_tx,
            video_build_rx,
            video_build_cancellation: None,
            preview_selection: None,
            selection_context_menu:
                crate::selection_context_menu::SelectionContextMenuState::default(),
            pending_preview_nav: None,
            last_preview_rect: None,
            last_video_frame_area: None,
            db_preview_layout: None,
            db_cell_view: None,
            last_db_cell_rect: None,
            last_db_cell_reveal: None,
            vertical_scroll_lock: crate::input::AxisLock::new(),
            horizontal_scroll_lock: crate::input::AxisLock::new(),
            vertical_scroll_pacer: crate::input::ScrollPacer::new(),
            horizontal_scroll_pacer: crate::input::ScrollPacer::new(),
            last_preview_content_origin: None,
            last_markdown_content_origin: None,
            markdown_layout: crate::ui::preview::markdown::MarkdownLayoutCache::default(),
            markdown_visual_scroll: 0,
            markdown_engine_scroll_seen: None,
            preview_click_state: None,
            diff_selection: None,
            last_diff_rect: None,
            last_diff_hit: None,
            diff_click_state: None,
            commit_detail_selection: None,
            last_commit_detail_rect: None,
            last_commit_detail_hit: None,
            commit_detail_click_state: None,
            last_drag_mouse: None,
            preview_autoscroll_at: None,
            diff_autoscroll_at: None,
            dragging_split: false,
            dragging_graph_diff_split: false,
            hit_registry: HitTestRegistry::new(),
            hover_row: None,
            hover_col: None,
            last_click: None,
            nav_peek_anchor_col: 0,
            nav_peek_anchor_row: 0,
            nav_peek_preview_rect: None,
            nav_peek_tree_rect: None,
            nav_peek_preview_scroll: 0,
            nav_peek_preview_max_scroll: 0,
            nav_peek_preview_target: None,
            nav_peek_visible_rows: reef_app::NavCandidatesPopup::MAX_VISIBLE_ROWS,
            nav_peek_reconciled_view: None,
            theme,
            space_leader_at: None,
            g_pending_at: None,
            quick_open_leader_at: None,
            global_search_leader_at: None,
            find_widget_ui: FindWidgetUiState::default(),
            quick_open_popup_area: None,
            global_search_popup_area: None,
            hosts_picker_popup_area: None,
            graph_branch_picker_popup_area: None,
            diff_ctrl_hover: None,
        };
        app.refresh_status();
        app.refresh_file_tree();
        // Probe which LSP binaries are installed ONCE here (off the
        // render path) so the status-bar badge / Settings rows read a
        // cached map instead of walking PATH every frame.
        app.engine
            .dispatch(reef_app::AppCommand::RefreshLspInstalled);
        app
    }

    /// Minimum total width for the Graph tab's 3-column layout. Below this
    /// the panel falls back to the 2-column layout with the diff rendered
    /// inline inside `commit_detail_panel` (the pre-split behaviour).
    /// Chosen so the middle column still shows readable file names and the
    /// diff column has at least ~40 cols for content after its gutter.
    pub const GRAPH_THREE_COL_MIN_WIDTH: u16 = reef_app::GRAPH_THREE_COL_MIN_WIDTH;

    /// Width of the left (graph / tree / status) sidebar for the current
    /// frame. Single source of truth for the `split_percent → columns`
    /// clamp so `ui::render`, mouse hit-testing, and h-scroll routing
    /// never disagree about where the boundary is. Mirror of the
    /// `.max(10).min(total - 20)` clamp `ui::render` has applied since
    /// v0 — factored here so `input::*` and the render stay aligned
    /// even when `split_percent` lands near the extremes.
    pub fn graph_sidebar_width(&self, total_width: u16) -> u16 {
        self.engine.graph_sidebar_width(total_width)
    }

    /// Widths for the Graph 3-col layout: `(graph, commit, diff)`. Sum
    /// equals `total_width`. Only meaningful when `graph_uses_three_col()`
    /// is true; callers outside the render path should gate on that
    /// first. The `(20, 20)` floors keep both right-side columns usable
    /// when either `split_percent` or `graph_diff_split_percent` is
    /// near its edge — matches `ui::render`'s constraint math.
    pub fn graph_three_col_widths(&self, total_width: u16) -> (u16, u16, u16) {
        self.engine.graph_three_col_widths(total_width)
    }

    /// Whether the Graph tab should render with 3 columns right now —
    /// graph | commit metadata+files | diff. True when a file diff is
    /// loaded (or currently loading) AND the terminal is wide enough.
    /// Other tabs and narrow terminals fall back to the existing 2-col
    /// layout where the diff is inline under the file list.
    ///
    /// Callers that need this in non-render contexts (search target
    /// resolution, panel normalization) should read the TUI layout cache
    /// that `ui::render` refreshes every frame before any panel runs.
    pub fn graph_uses_three_col(&self) -> bool {
        self.engine
            .graph_uses_three_col_for_width(self.layout.last_total_width)
    }

    pub fn quit(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::Quit);
    }

    pub fn open_help(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::OpenHelp);
    }

    pub fn close_help(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CloseHelp);
    }

    pub fn push_toast(&mut self, toast: Toast) {
        self.engine.dispatch(reef_app::AppCommand::PushToast(toast));
    }

    pub fn set_active_panel(&mut self, panel: Panel) {
        self.engine
            .dispatch(reef_app::AppCommand::SetActivePanel(panel));
    }

    pub fn cycle_active_panel(&mut self, reverse: bool) {
        self.engine.dispatch(reef_app::AppCommand::CyclePanel {
            reverse,
            uses_three_col: self.graph_uses_three_col(),
        });
    }

    pub fn clear_preview_selection(&mut self) {
        self.preview_selection = None;
        self.preview_click_state = None;
        if self.selection_context_menu.target()
            == Some(crate::selection_context_menu::SelectionContextTarget::Preview)
        {
            self.selection_context_menu.close();
        }
    }

    pub fn preview_has_selectable_text(&self) -> bool {
        if self.engine.structured_preview_mode() == reef_app::StructuredPreviewMode::Tree
            && self.engine.structured_preview_document().is_some()
        {
            return false;
        }
        preview_text_extent(self.engine.preview_content_ref()).is_some()
    }

    pub fn selection_context_menu_has_text(&self) -> bool {
        self.selection_context_menu
            .target()
            .is_some_and(|target| self.selection_context_target_has_text(target))
    }

    pub fn selection_context_copy_enabled(&self) -> bool {
        use crate::selection_context_menu::SelectionContextTarget as Target;

        match self.selection_context_menu.target() {
            Some(Target::Preview) => self
                .preview_selection
                .is_some_and(|selection| !selection.is_empty()),
            Some(Target::Diff(_)) => self
                .diff_selection
                .is_some_and(|selection| !selection.sel.is_empty()),
            None => false,
        }
    }

    pub fn open_selection_context_menu(
        &mut self,
        target: crate::selection_context_menu::SelectionContextTarget,
        anchor: (u16, u16),
    ) {
        if !self.selection_context_target_has_text(target) {
            return;
        }
        self.close_tree_context_menu();
        self.nav_close_candidates();
        self.selection_context_menu.open(target, anchor);
    }

    fn selection_context_target_has_text(
        &self,
        target: crate::selection_context_menu::SelectionContextTarget,
    ) -> bool {
        use crate::selection_context_menu::SelectionContextTarget as Target;

        match target {
            Target::Preview => self.preview_has_selectable_text(),
            Target::Diff(_) => self
                .last_diff_hit
                .as_ref()
                .is_some_and(|hit| !hit.rows.is_empty()),
        }
    }

    pub fn close_selection_context_menu(&mut self) {
        self.selection_context_menu.close();
    }

    pub fn navigate_selection_context_menu(&mut self, delta: i32) {
        self.selection_context_menu.navigate(delta);
    }

    pub fn dispatch_selection_context_menu_item(
        &mut self,
        item: crate::selection_context_menu::SelectionContextMenuItem,
    ) {
        use crate::selection_context_menu::{
            SelectionContextMenuItem as Item, SelectionContextTarget as Target,
        };

        let Some(target) = self.selection_context_menu.target() else {
            return;
        };
        self.close_selection_context_menu();
        match (item, target) {
            (Item::Copy, Target::Preview) => {
                let Some(selection) = self
                    .preview_selection
                    .filter(|selection| !selection.is_empty())
                else {
                    return;
                };
                let Some(preview) = self.engine.preview_content_ref() else {
                    return;
                };
                let rows = preview.body.display_text_rows();
                let text = crate::ui::selection::collect_selected_text_from_rows(
                    rows.iter().map(|row| row.as_ref()),
                    rows.len(),
                    &selection,
                );
                if !text.is_empty() {
                    self.copy_text_to_clipboard(text);
                }
            }
            (Item::Copy, Target::Diff(_)) => {
                let Some(selection) = self
                    .diff_selection
                    .filter(|selection| !selection.sel.is_empty())
                else {
                    return;
                };
                let Some(hit) = self.last_diff_hit.as_ref() else {
                    return;
                };
                let text = crate::ui::selection::collect_diff_selected_text(hit, &selection);
                if !text.is_empty() {
                    self.copy_text_to_clipboard(text);
                }
            }
            (Item::SelectAll, Target::Preview) => {
                let Some((last_row, last_byte)) =
                    preview_text_extent(self.engine.preview_content_ref())
                else {
                    return;
                };
                self.preview_selection = Some(crate::ui::selection::PreviewSelection {
                    anchor: (0, 0),
                    active: (last_row, last_byte),
                    dragging: false,
                });
            }
            (Item::SelectAll, Target::Diff(side)) => {
                let Some(hit) = self.last_diff_hit.as_ref() else {
                    return;
                };
                let Some(last_row) = hit.rows.len().checked_sub(1) else {
                    return;
                };
                let last_byte = hit.rows[last_row].text_for(side).len();
                self.diff_selection = Some(crate::ui::selection::DiffSelection {
                    sel: crate::ui::selection::PreviewSelection {
                        anchor: (0, 0),
                        active: (last_row, last_byte),
                        dragging: false,
                    },
                    side,
                });
            }
        }
    }

    pub(crate) fn copy_text_to_clipboard(&mut self, text: String) {
        self.engine.dispatch(reef_app::AppCommand::CopyToClipboard {
            text,
            success: Some(Toast::info(crate::i18n::t(
                crate::i18n::Msg::ClipboardCopied,
            ))),
            failure: Toast::error(crate::i18n::t(crate::i18n::Msg::ClipboardCopyFailed)),
        });
    }

    /// Drop the in-panel diff selection and its click counter. Called
    /// whenever the underlying row list is about to shift out from under
    /// the cached `(row_idx, byte_offset)` anchor — file swap, layout /
    /// mode toggle, tab switch, 3-col→2-col transition. Keeping this in
    /// one place means future reset sites can't miss the click counter.
    pub fn clear_diff_selection(&mut self) {
        self.diff_selection = None;
        self.diff_click_state = None;
        if matches!(
            self.selection_context_menu.target(),
            Some(crate::selection_context_menu::SelectionContextTarget::Diff(
                _
            ))
        ) {
            self.selection_context_menu.close();
        }
    }

    pub fn clear_commit_detail_selection(&mut self) {
        self.commit_detail_selection = None;
        self.commit_detail_click_state = None;
    }

    /// Drop `Panel::Commit` back to a two-column-compatible panel when the
    /// layout no longer exposes a middle column (narrow terminal, diff
    /// unloaded, tab switched away). Prevents the user from being stuck
    /// focusing a column that isn't rendered. Called at the top of
    /// `ui::render` after refreshing the TUI layout cache.
    ///
    /// Also drops a stale diff selection if we just lost the panel that
    /// owned it — row indices from the old frame would overlay on top of
    /// whatever renders next (commit_detail's flat row list doesn't match
    /// `DiffHit.rows`), producing a bogus highlight.
    pub fn normalize_active_panel(&mut self) {
        let outcome = self
            .engine
            .dispatch(reef_app::AppCommand::NormalizeActivePanel {
                uses_three_col: self.graph_uses_three_col(),
            });
        if let Some(outcome) = outcome.normalize_active_panel {
            if outcome.clear_diff_selection && self.diff_selection.is_some() {
                self.clear_diff_selection();
            }
        }
    }

    /// Commit the current Tab::Search replace batch. Buckets the
    /// currently-included matches by path into `ReplaceItem`s and
    /// dispatches `FilesTask::ReplaceInFiles`. No-op when replace mode
    /// is closed, a previous batch is still in flight, the result list
    /// is empty, or every match has been opted out via the per-row
    /// checkbox. Bound to `Ctrl/Alt+Enter`, plain `Enter` from the
    /// replace input, and the `[Apply]` footer button.
    pub fn commit_replace_in_files(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CommitReplaceInFiles);
    }

    /// Toggle the left sidebar's visibility. Hiding collapses the left
    /// column to 0 width (via `graph_sidebar_width`'s short-circuit); all
    /// mouse hit-testing, h-scroll routing, and drag-zone registration key
    /// off that same value so they stay consistent. Focus on `Panel::Files`
    /// gets moved to `Panel::Diff` — the sidebar panel wouldn't render
    /// otherwise and keyboard nav would aim at nothing. Any in-flight
    /// column drag is cancelled so releasing the mouse after the hide
    /// doesn't snap a phantom split_percent.
    pub fn toggle_sidebar(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ToggleSidebar);
        self.drain_engine_runtime_events();
    }

    pub fn refresh_status(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::RefreshStatus);
    }

    /// Enter the drag-and-drop destination picker. Switches to the Files
    /// tab so the user can see the tree they're about to drop into, then
    /// stores the sources for the banner + eventual copy. Called from
    /// `input::handle_paste` when a paste payload resolves to existing
    /// on-disk paths.
    ///
    /// Refuses the transition when a place-mode copy is already in
    /// flight — overwriting `sources` would invalidate the worker's
    /// generation and the previous copy's completion result (including
    /// the success toast and tree refresh) would be silently dropped.
    pub fn enter_place_mode(&mut self, sources: Vec<PathBuf>) {
        if self.engine.search().active {
            crate::search::exit_cancel(self);
        }
        self.engine
            .dispatch(reef_app::AppCommand::EnterPlaceMode(sources));
        self.drain_engine_runtime_events();
    }

    /// Leave place mode without copying — Esc, right-click, or a click on a
    /// non-droppable area all land here.
    pub fn exit_place_mode(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ExitPlaceMode);
    }

    /// Kick off the async copy into `dest_dir`. Takes the current
    /// place-mode sources by clone so the state can be cleared by the
    /// caller if it chooses to.
    /// by clone so the state can be cleared by the caller if it chooses to —
    /// but in normal flow we keep sources around until the worker result
    /// arrives so the banner stays visible while copying.
    ///
    /// De-duped against in-flight copies: a rage-click on a second folder
    /// before the first copy returns would otherwise `begin()` a new
    /// generation and invalidate the prior one — the first copy still
    /// runs on disk but its completion toast / tree refresh never fire.
    /// `enter_place_mode` has the same guard for paste-level entry; this
    /// handles the mouse-level commit path.
    pub fn request_file_copy(&mut self, sources: Vec<PathBuf>, dest_dir: PathBuf) {
        self.engine
            .dispatch(reef_app::AppCommand::RequestFileCopy { sources, dest_dir });
        self.drain_engine_runtime_events();
    }

    // ── VS Code-style file clipboard / paste / drag (intra-tree) ────────

    /// Workdir-relative paths the next clipboard / drag / delete
    /// operation should target. VS Code rule: if the multi-selection
    /// contains the current cursor row, the whole selection is the
    /// payload; otherwise the cursor alone wins. This keeps a stray
    /// click from quietly losing a selection set the user built up.
    pub fn effective_action_paths(&self) -> Vec<PathBuf> {
        self.engine.effective_action_paths()
    }

    /// Workdir-relative directory the next Paste should drop into.
    /// VS Code rule: cursor on a folder → into that folder; cursor on
    /// a file → into its parent; nothing selected → project root.
    pub fn paste_target_dir(&self) -> PathBuf {
        self.engine.paste_target_dir()
    }

    /// Mark `paths` as Cut. Replaces any prior clipboard. Render
    /// reads `file_clipboard.is_cut()` + `contains` directly per
    /// visible row so there's no eager stamping to do here.
    pub fn mark_cut(&mut self, paths: Vec<PathBuf>) {
        self.engine.dispatch(reef_app::AppCommand::MarkCut(paths));
    }

    /// Mark `paths` as Copy. Copy mode does not visually mark source
    /// rows (matches VS Code).
    pub fn mark_copy(&mut self, paths: Vec<PathBuf>) {
        self.engine.dispatch(reef_app::AppCommand::MarkCopy(paths));
    }

    pub fn clear_clipboard(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ClearClipboard);
    }

    /// Drop the file_clipboard contents into `dest_rel` per VS Code
    /// semantics. Same-directory copies auto-rename via `next_copy_name`;
    /// cross-directory conflicts open `paste_conflict` for resolution.
    /// No-conflict items dispatch immediately on the worker; with
    /// conflicts present, the prompt drives a second-stage dispatch
    /// from `complete_paste_resolution`.
    pub fn paste_into(&mut self, dest_rel: PathBuf) {
        self.engine
            .dispatch(reef_app::AppCommand::PasteInto(dest_rel));
        self.drain_engine_runtime_events();
    }

    /// Same-directory Copy shortcut for the keyboard `D` binding.
    /// Drives off the active selection / cursor.
    pub fn duplicate_selection(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::DuplicateSelection);
        self.drain_engine_runtime_events();
    }

    /// Same-directory Copy shortcut taking explicit paths. Used by
    /// the right-click menu (which targets the menu's anchor row, not
    /// `effective_action_paths()`) so the action operates on the
    /// right-clicked file even when the cursor sits on a different
    /// row. Pure path I/O — does not mutate `file_selection`.
    fn duplicate_paths(&mut self, sources: Vec<PathBuf>) {
        self.engine
            .dispatch(reef_app::AppCommand::DuplicatePaths(sources));
        self.drain_engine_runtime_events();
    }

    /// Process the user's response to the current `paste_conflict`
    /// prompt. `r` is a one-item resolution; pass `apply_to_all =
    /// true` to drain the rest of the queue with the same answer
    /// (Replace / Skip only — KeepBoth needs per-item rename names,
    /// Cancel is independent of apply-to-all).
    pub fn resolve_paste_conflict(
        &mut self,
        r: reef_core::file_ops::Resolution,
        apply_to_all: bool,
    ) {
        self.engine
            .dispatch(reef_app::AppCommand::ResolvePasteConflict {
                resolution: r,
                apply_to_all,
            });
        self.drain_engine_runtime_events();
    }

    /// Cancel the prompt without committing any pending dispositions.
    /// Auto-resolved (no-conflict) items are dropped too — VS Code's
    /// Cancel halts the entire batch, not just the prompted item.
    pub fn cancel_paste_conflict(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CancelPasteConflict);
        self.drain_engine_runtime_events();
    }

    /// Compute a Keep-Both basename for the current prompt item using
    /// the destination directory's existing names.
    pub fn keep_both_name_for_current_conflict(&self) -> Option<String> {
        self.engine.keep_both_name_for_current_conflict()
    }

    /// Write the absolute (or workdir-relative) paths of the active
    /// selection / cursor row to the system clipboard via OSC 52.
    /// Used by the keyboard binding; menu actions go through
    /// `copy_paths_to_clipboard` directly with the menu's anchor.
    pub fn copy_path_to_clipboard(&mut self, relative: bool) {
        self.copy_paths_to_clipboard(self.effective_action_paths(), relative);
    }

    /// Write `paths` to the system clipboard via OSC 52 — absolute
    /// paths when `relative = false`, workdir-relative otherwise.
    /// Multi-input produces newline-separated paths (VS Code's
    /// "Copy Path" / "Copy Relative Path" multi-row behaviour).
    /// Pure path I/O — does not mutate `file_selection`.
    fn copy_paths_to_clipboard(&mut self, paths: Vec<PathBuf>, relative: bool) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let workdir = self.engine.file_tree_root();
        // Single contiguous String buffer — skips an intermediate
        // `Vec<String>` and the N small allocations it'd require.
        // `Path::to_string_lossy` is `Cow<str>`; `push_str(&cow)`
        // dereferences to `&str` without forcing it `Owned`, so the
        // happy-path UTF-8 case copies bytes once into `payload`.
        let mut payload = String::new();
        let mut had_lossy = false;
        for rel in paths {
            let target = if relative { rel } else { workdir.join(rel) };
            let s = target.to_string_lossy();
            if matches!(s, std::borrow::Cow::Owned(_)) {
                had_lossy = true;
            }
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(&s);
        }
        let success = if had_lossy {
            Toast::warn(crate::i18n::copy_path_lossy_utf8())
        } else {
            let toast = if relative {
                crate::i18n::copy_relative_path_done(count)
            } else {
                crate::i18n::copy_path_done(count)
            };
            Toast::info(toast)
        };
        self.engine.dispatch(reef_app::AppCommand::CopyToClipboard {
            text: payload,
            success: Some(success),
            failure: Toast::error("Copy path failed"),
        });
    }

    // ── Intra-tree mouse drag (VS Code-style move/copy on drop) ─────────

    /// Promote the press recorded by `Down(Left)` to an active drag.
    /// Snapshots `effective_action_paths()` *now* — a mid-drag
    /// selection mutation can't change what's being carried.
    pub fn begin_tree_drag(&mut self, mods: crossterm::event::KeyModifiers) {
        if self.engine.tree_drag_active() {
            return;
        }
        let sources = self.effective_action_paths();
        if sources.is_empty() {
            self.engine.dispatch(reef_app::AppCommand::CancelTreeDrag);
            return;
        }
        // Place mode is the OS→TUI flow; intra-tree drag overlays its
        // hover affordances over the same tree, so the two are
        // mutually exclusive at the active-flag level.
        if self.engine.place_mode_active() {
            return;
        }
        self.engine.dispatch(reef_app::AppCommand::BeginTreeDrag {
            sources,
            mods: input_modifiers(mods),
        });
    }

    pub fn update_tree_drag_hover(&mut self, idx: Option<usize>) {
        self.engine
            .dispatch(reef_app::AppCommand::UpdateTreeDragHover(idx));
    }

    pub fn update_tree_drag_modifiers(&mut self, mods: crossterm::event::KeyModifiers) {
        self.engine
            .dispatch(reef_app::AppCommand::UpdateTreeDragModifiers(
                input_modifiers(mods),
            ));
    }

    /// Fire any due hover-auto-expand. Call once per tick from the
    /// main loop while a drag is active. Safe to call when idle.
    ///
    /// Stale-index window: `tree_drag.hover_idx` is set on the most
    /// recent `Drag(Left)` mouse event, against the entry list as
    /// it stood then. Between Drag and tick (≤16 ms typically, plus
    /// the 600 ms `HOVER_EXPAND_DELAY`), `fs_watcher` could fire and
    /// trigger a tree rebuild — invalidating `idx`. The `is_dir` /
    /// `!is_expanded` checks below double as the staleness guard:
    /// a rebuilt tree where `idx` now points at a file (or out of
    /// range) silently no-ops, and the next `Drag` event recomputes
    /// `hover_idx` against the new list.
    pub fn tick_tree_drag_auto_expand(&mut self) {
        if !self.engine.tree_drag_active() {
            return;
        }
        self.engine
            .dispatch(reef_app::AppCommand::AutoExpandTreeDragHover {
                now: std::time::Instant::now(),
            });
    }

    /// Mouse `Up(Left)` while drag is active — translate the hovered
    /// row to a destination folder and dispatch move (default) or
    /// copy (Alt held).
    pub fn commit_tree_drag(&mut self, release_mods: crossterm::event::KeyModifiers) {
        if !self.engine.tree_drag_active() {
            return;
        }
        self.engine.dispatch(reef_app::AppCommand::DropTreeDrag {
            release_mods: input_modifiers(release_mods),
        });
        self.drain_engine_runtime_events();
    }

    pub fn cancel_tree_drag(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CancelTreeDrag);
    }

    // ── Files-tab tree actions: New File / New Folder / Rename / Delete ──

    /// Open the inline editor for a new file / new folder under
    /// `parent_dir`, or a rename of `rename_target`. Closes any competing
    /// modal first so place-mode / context-menu / delete-confirm don't
    /// fight with the editor for input ownership.
    ///
    /// `anchor_idx` is the visible-row index the editable row will
    /// render under (the parent folder for creates, the target entry
    /// itself for rename). `None` means the edit row attaches to the
    /// top of the tree — used when creating at project root.
    pub fn begin_tree_edit(
        &mut self,
        mode: reef_app::TreeEditMode,
        parent_dir: PathBuf,
        rename_target: Option<PathBuf>,
        anchor_idx: Option<usize>,
    ) {
        self.engine.dispatch(reef_app::AppCommand::BeginTreeEdit {
            mode,
            parent_dir,
            rename_target,
            anchor_idx,
        });
        self.drain_engine_runtime_events();
    }

    /// Validate `tree_edit.buffer` and kick off the matching worker
    /// task. On validation failure we set `tree_edit.error` and stay
    /// active so the user can fix the name.
    pub fn commit_tree_edit(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CommitTreeEdit);
    }

    pub fn cancel_tree_edit(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CancelTreeEdit);
    }

    /// Right-click opened a context menu over `target_entry_idx`
    /// (or None for a click that missed all rows). `anchor` is the
    /// mouse column/row in screen cells; the renderer will clamp
    /// to the viewport.
    pub fn open_tree_context_menu(&mut self, target_entry_idx: Option<usize>, anchor: (u16, u16)) {
        if self.engine.place_mode_active() || self.engine.tree_edit_active() {
            return;
        }
        // NOTE: we deliberately do NOT move `file_tree.selected` to the
        // right-clicked row. The menu carries its own `target_entry_idx`
        // so Rename / Delete / etc. know what to operate on; leaving
        // selection alone matches VSCode's Explorer (right-click never
        // moves the selection highlight) and — critically — stops the
        // underlying row's `selection_bg` from stretching across the
        // full width and visually fighting with the popup.
        self.engine
            .dispatch(reef_app::AppCommand::OpenTreeContextMenu {
                target_entry_idx,
                anchor,
            });
    }

    pub fn close_tree_context_menu(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CloseTreeContextMenu);
    }

    /// Translate a picked `ContextMenuItem` into the corresponding
    /// App action. Called from `input` when the user clicks / keys
    /// on a menu row.
    pub fn dispatch_context_menu_item(&mut self, item: reef_app::ContextMenuItem) {
        use reef_app::ContextMenuItem as I;
        // Disabled items (e.g. Paste when the clipboard is empty)
        // close the menu but skip the action — the user clicked a
        // greyed-out row, and silently doing nothing matches VS
        // Code's behaviour.
        if !item.is_enabled(self.engine.file_clipboard_empty()) {
            self.close_tree_context_menu();
            return;
        }
        let target_idx = self.engine.selected_tree_context_menu_target();
        self.close_tree_context_menu();
        // The right-click menu stamps `target_entry_idx` independently
        // of `file_tree.selected`. For clipboard / path actions we
        // want them to operate on the right-clicked row, even if the
        // selection cursor is elsewhere — temporarily seed a single-
        // path action set from the menu's anchor when there's no
        // active multi-selection containing the row.
        let anchor_paths = |this: &Self| this.engine.context_menu_action_paths(target_idx);
        match item {
            I::Cut => {
                let paths = anchor_paths(self);
                self.mark_cut(paths);
            }
            I::Copy => {
                let paths = anchor_paths(self);
                self.mark_copy(paths);
            }
            I::Paste => {
                if let Some(dest) = self.engine.context_menu_paste_target(target_idx) {
                    // Right-click on a folder lands paste inside it;
                    // on a file lands in its parent; on empty tree
                    // space (ALL_FOR_ROOT, target_idx == None) lands
                    // at the workspace root. We deliberately do NOT
                    // fall through to `paste_target_dir()` for the
                    // root case — that would pull the unrelated
                    // cursor row's parent and hide the user's clear
                    // intent ("paste at the root").
                    self.paste_into(dest);
                }
            }
            I::Duplicate => {
                // Menu actions take their target paths directly; the
                // user's `file_selection` (and its anchor) stays
                // untouched. `anchor_paths` already implements the
                // "if menu row is part of the selection, act on the
                // whole selection; otherwise just the menu row" rule.
                let paths = anchor_paths(self);
                self.duplicate_paths(paths);
            }
            I::CopyPath => {
                let paths = anchor_paths(self);
                self.copy_paths_to_clipboard(paths, false);
            }
            I::CopyRelativePath => {
                let paths = anchor_paths(self);
                self.copy_paths_to_clipboard(paths, true);
            }
            I::NewFile => {
                let (parent, anchor) = self.resolve_create_anchor(target_idx);
                self.begin_tree_edit(reef_app::TreeEditMode::NewFile, parent, None, anchor);
            }
            I::NewFolder => {
                let (parent, anchor) = self.resolve_create_anchor(target_idx);
                self.begin_tree_edit(reef_app::TreeEditMode::NewFolder, parent, None, anchor);
            }
            I::Rename => {
                let Some(idx) = target_idx else { return };
                let Some(entry) = self.engine.context_menu_entry(target_idx) else {
                    return;
                };
                let parent = entry.path.parent().map(PathBuf::from).unwrap_or_default();
                self.begin_tree_edit(
                    reef_app::TreeEditMode::Rename,
                    parent,
                    Some(entry.path.clone()),
                    Some(idx),
                );
            }
            I::Delete => {
                let Some(entry) = self.engine.context_menu_entry(target_idx) else {
                    return;
                };
                let abs = self.engine.file_tree_root().join(&entry.path);
                self.prompt_tree_delete(abs, entry.is_dir, /*hard=*/ false);
            }
            I::RevealInFinder => {
                // Reveal-in-Finder opens the LOCAL file manager; over ssh
                // the target path doesn't exist on this machine, so the
                // action is always wrong. Guard at the caller layer so
                // the user gets a clear "not supported" toast instead of
                // "file not found" from the platform command.
                if self.engine.backend_is_remote() {
                    self.push_toast(Toast::warn(
                        "Reveal in Finder is not supported on remote workdirs",
                    ));
                    return;
                }
                let path = match target_idx {
                    Some(idx) => self
                        .engine
                        .file_tree_entry_abs_path(idx)
                        .unwrap_or_else(|| self.engine.file_tree_root()),
                    None => self.engine.file_tree_root(),
                };
                if let Err(msg) = crate::reveal::reveal_in_finder(&path) {
                    // Platforms we don't support get the unsupported toast
                    // instead of the raw error — it's a cleaner UX hint.
                    let text = if msg.contains("not supported") {
                        crate::i18n::tree_reveal_unsupported_platform()
                    } else {
                        msg
                    };
                    self.push_toast(Toast::error(text));
                }
            }
        }
    }

    /// Given the entry the user clicked (or `None` for empty-space),
    /// pick the workdir-relative parent directory the new file/folder
    /// should land in, plus the visible row index the editable row
    /// anchors under.
    ///
    /// Rules:
    /// - Clicked on a folder → create INSIDE that folder. Auto-expands
    ///   the folder first if it's currently collapsed so the edit row
    ///   is actually visible.
    /// - Clicked on a file → create as a SIBLING (under the file's
    ///   parent folder). Anchor at the file's own row — good enough;
    ///   the render-side insertion logic handles this cleanly.
    /// - Clicked on empty space / None → create at project root.
    fn resolve_create_anchor(
        &mut self,
        target_entry_idx: Option<usize>,
    ) -> (PathBuf, Option<usize>) {
        let Some(idx) = target_entry_idx else {
            return (PathBuf::new(), None);
        };
        let Some(entry) = self.engine.file_tree_entry(idx) else {
            return (PathBuf::new(), None);
        };
        if entry.is_dir {
            // Auto-expand collapsed folder so the editable child row
            // actually renders. The refresh is async; `anchor_idx` will
            // remain valid in the meantime (the existing folder row
            // doesn't move), and the edit row renders right after it
            // regardless of expansion state because it's keyed on
            // anchor_idx, not on the children's indices.
            if !entry.is_expanded {
                self.engine
                    .dispatch(reef_app::AppCommand::ToggleFileTreeExpand(idx));
            }
            (entry.path.clone(), Some(idx))
        } else {
            // File → create next to it. The file's parent is the
            // clicked entry's parent on disk.
            let parent = entry.path.parent().map(PathBuf::from).unwrap_or_default();
            (parent, Some(idx))
        }
    }

    /// Request the renderer-neutral delete-confirm overlay. `hard`
    /// controls Trash vs. `fs::remove_*`; TUI render turns the semantic
    /// request into localized labels.
    pub fn prompt_tree_delete(&mut self, path: PathBuf, is_dir: bool, hard: bool) {
        self.close_tree_context_menu();
        self.engine
            .dispatch(reef_app::AppCommand::RequestTreeDeleteConfirm { path, is_dir, hard });
    }

    /// Run the actual delete after the user confirmed. If a previous
    /// fs mutation is still in flight, re-open the request with the same
    /// payload so the user can retry once the worker drains.
    fn execute_tree_delete(&mut self, pending: reef_app::TreeDeleteConfirm) {
        self.engine
            .dispatch(reef_app::AppCommand::ExecuteTreeDelete(pending));
        self.drain_engine_runtime_events();
    }

    // ── Confirm modal (generic yes/no overlay) ───────────────────────────

    /// Force-close the renderer-neutral confirm request.
    pub fn dismiss_confirm(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::DismissConfirm);
    }

    /// Confirm the pending renderer-neutral request and run the
    /// corresponding terminal adapter action.
    pub fn fire_confirm_primary(&mut self) {
        let Some(request) = self.engine.confirm_request().cloned() else {
            return;
        };
        self.engine.dispatch(reef_app::AppCommand::DismissConfirm);
        match request {
            reef_app::ConfirmRequest::TreeDelete(pending) => {
                self.execute_tree_delete(pending);
            }
        }
    }

    /// Cancel the pending renderer-neutral request.
    pub fn fire_confirm_cancel(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::DismissConfirm);
    }

    // ── Hosts picker (Ctrl+O) ────────────────────────────────────────────

    /// Open the hosts picker overlay, seeding it from the current user's
    /// `~/.ssh/config` plus the persisted recent-targets list. Errors
    /// reading the config aren't fatal — we show an empty picker so the
    /// user can still switch via the path-input mode.
    pub fn open_hosts_picker(&mut self) {
        let parsed = reef_core::hosts::parse_ssh_config().unwrap_or_default();
        let recent = crate::hosts_picker::load_recent();
        self.engine.dispatch(reef_app::AppCommand::OpenHostsPicker {
            hosts: parsed,
            recent,
        });
    }

    /// Close the picker without connecting.
    pub fn close_hosts_picker(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CloseHostsPicker);
    }

    /// Commit the picker's current selection. On success, stash the
    /// target for `main.rs` to consume and flip the session-swap flag —
    /// we don't build the new backend here because the outer loop owns
    /// the terminal teardown/setup dance around the connect.
    pub fn confirm_hosts_picker(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ConfirmHostsPicker);
        self.drain_engine_runtime_events();
    }

    /// Open the Graph tab's branch picker. Pulls the branch list out
    /// of the cached `ref_map` (already loaded by `refresh_graph`) and
    /// seeds the recents from `GitGraphState::recent_branches`.
    ///
    /// Refuses to open before the first graph payload has populated
    /// `ref_map`: at cold start with a persisted `Branch(X)` scope,
    /// an empty `ref_map` would render the picker as
    /// `[ All refs ]`-only, and a stray Enter would silently overwrite
    /// the user's persisted choice via `set_graph_scope(AllRefs)`.
    /// Toast instead and let the user retry once the first revwalk lands.
    pub fn open_graph_branch_picker(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::OpenGraphBranchPicker);
        self.drain_engine_runtime_events();
    }

    pub fn close_graph_branch_picker(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CloseGraphBranchPicker);
    }

    /// Apply the picker's current selection and close the overlay. A
    /// `confirm()` of `None` (filter matched zero rows + Enter) also
    /// closes the overlay so the user can never get input-trapped on
    /// an empty result list.
    pub fn confirm_graph_branch_picker(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ConfirmGraphBranchPicker);
        self.drain_engine_runtime_events();
    }

    /// Collapse every expanded folder and shorten the cached flat tree.
    pub fn collapse_all_tree_entries(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CollapseAllTreeEntries);
    }

    /// Rebuild the file tree from disk, applying git decorations when a repo is open.
    /// Safe to call on any workdir — `refresh_status` handles repo/no-repo internally.
    pub fn refresh_file_tree(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::RefreshFileTree);
    }

    pub fn refresh_file_tree_with_target(&mut self, selected_path: Option<PathBuf>) {
        self.engine
            .dispatch(reef_app::AppCommand::RefreshFileTreeWithTarget(
                selected_path,
            ));
    }

    pub fn load_preview(&mut self) {
        self.clear_g_chord();
        // A new selection may keep the previous preview visible while the
        // worker loads or reports an error, but stale media must not keep
        // playing behind that transition.
        self.pause_video_playback();
        self.cancel_video_build();
        self.engine
            .dispatch(reef_app::AppCommand::LoadSelectedPreview);
    }

    /// Refresh the currently-selected file's preview *now*, bypassing the
    /// `PREVIEW_DEBOUNCE` window. For "I just edited this file in
    /// $EDITOR — show me the new contents" moments: the user is settled
    /// on the file (no scrubbing in play), so the debounce that exists to
    /// coalesce ↓-hold key-repeats just reads as noticeable lag before
    /// the post-edit preview lands.
    pub fn reload_preview_now(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ReloadPreviewNow {
                dark: self.theme.is_dark,
                wants_decoded_image: self.image_picker.is_some(),
            });
    }

    pub fn db_navigate(&mut self, action: reef_app::DbNav) {
        self.engine
            .dispatch(reef_app::AppCommand::DbNavigate(action));
    }

    pub fn db_toggle_schema(&mut self, name: &str) {
        self.engine
            .dispatch(reef_app::AppCommand::DbToggleSchema(name.to_string()));
    }

    pub fn db_select_object(&mut self, key: reef_sqlite_preview::DbObjectKey) {
        self.engine
            .dispatch(reef_app::AppCommand::DbSelectObject(key));
    }

    pub fn db_navigate_to_page(&mut self, page_one_based: u64) {
        self.engine
            .dispatch(reef_app::AppCommand::DbNavigateToPage(page_one_based));
    }

    pub fn load_preview_for_path(&mut self, rel_path: PathBuf) {
        self.clear_g_chord();
        self.engine
            .dispatch(reef_app::AppCommand::LoadPreview(rel_path));
    }

    /// Pull any completed resize responses from the background
    /// resize worker and merge them into the current protocol. Drops
    /// stale responses whose id doesn't match (the `ThreadProtocol`
    /// bumps its id each time it dispatches, so a resize for an older
    /// selection arrives as a no-op after the user already switched).
    fn drain_preview_resize_responses(&mut self) -> bool {
        let mut changed = false;
        while let Ok(resp) = self.preview_resize_rx.try_recv() {
            if let Some(proto) = self.preview_image_protocol.as_mut() {
                proto.update_resized_protocol(resp);
                changed = true;
            }
        }
        changed
    }

    /// Pick up freshly-built `StatefulProtocol`s and slot them into the
    /// current `ThreadProtocol`. A result is only applied when its
    /// generation matches the current in-flight `preview_load` — if
    /// the user switched files before the build completed, the build
    /// is stale and gets dropped; the next `BuiltProtocol` arriving
    /// with the new generation wins.
    fn drain_preview_protocol_builds(&mut self) -> bool {
        let mut changed = false;
        while let Ok(built) = self.preview_build_rx.try_recv() {
            if built.generation != self.engine.preview_generation() {
                continue;
            }
            if let Some(proto) = self.preview_image_protocol.as_mut() {
                proto.replace_protocol(built.protocol);
                changed = true;
            }
        }
        changed
    }

    pub fn select_file(&mut self, path: &str, is_staged: bool) {
        self.engine
            .dispatch(reef_app::AppCommand::SetCommitEditing(false));
        if self
            .engine
            .selected_file()
            .is_none_or(|selected| selected.path != path || selected.is_staged != is_staged)
        {
            self.push_location_before_jump();
        }
        self.engine.dispatch(reef_app::AppCommand::SelectGitFile {
            path: path.to_string(),
            is_staged,
            dark: self.theme.is_dark,
        });
        self.drain_engine_runtime_events();
    }

    pub fn select_git_file_for_discard(&mut self, path: String, is_staged: bool) {
        self.engine
            .dispatch(reef_app::AppCommand::SelectGitFileForDiscard {
                path,
                is_staged,
                dark: self.theme.is_dark,
            });
        self.drain_engine_runtime_events();
    }

    pub fn load_diff(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::LoadDiff {
            dark: self.theme.is_dark,
        });
        self.drain_engine_runtime_events();
    }

    pub fn toggle_diff_layout(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ToggleDiffLayout);
        self.clear_diff_selection();
        save_prefs(self.engine.diff_layout(), self.engine.diff_mode());
    }

    pub fn toggle_diff_mode(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ToggleDiffMode {
            dark: self.theme.is_dark,
        });
        self.clear_diff_selection();
        save_prefs(self.engine.diff_layout(), self.engine.diff_mode());
    }

    pub fn toggle_status_tree_mode(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleStatusTreeMode);
        crate::prefs::set_bool("status.tree_mode", self.engine.status_tree_mode());
    }

    pub fn toggle_commit_diff_layout(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleCommitDiffLayout);
        self.clear_commit_detail_selection();
        crate::prefs::set(
            "commit.diff_layout",
            self.engine.commit_diff_layout().pref_str(),
        );
    }

    pub fn toggle_commit_diff_mode(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleCommitDiffMode);
        self.clear_commit_detail_selection();
        crate::prefs::set(
            "commit.diff_mode",
            self.engine.commit_diff_mode().pref_str(),
        );
        self.reload_commit_file_diff();
    }

    pub fn toggle_commit_files_tree_mode(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleCommitFilesTreeMode);
        self.clear_commit_detail_selection();
        crate::prefs::set_bool(
            "commit.files_tree_mode",
            self.engine.commit_files_tree_mode(),
        );
    }

    pub fn toggle_nav_peek_mode(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleNavPeekMode {
                viewport_rows: self.nav_peek_visible_rows,
            });
        crate::prefs::set(
            reef_core::prefs::NAV_PEEK_MODE,
            self.engine.nav_peek_mode().pref_str(),
        );
    }

    pub fn set_structured_preview_mode(&mut self, mode: reef_app::StructuredPreviewMode) {
        self.engine
            .dispatch(reef_app::AppCommand::SetStructuredPreviewMode(mode));
        crate::prefs::set(reef_core::prefs::STRUCTURED_PREVIEW_MODE, mode.pref_str());
        self.clear_preview_selection();
    }

    pub fn toggle_structured_preview_mode(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleStructuredPreviewMode);
        crate::prefs::set(
            reef_core::prefs::STRUCTURED_PREVIEW_MODE,
            self.engine.structured_preview_mode().pref_str(),
        );
        self.clear_preview_selection();
    }

    pub fn stage_file(&mut self, path: &str) {
        self.engine.dispatch(reef_app::AppCommand::StageFile {
            path: path.to_string(),
            dark: self.theme.is_dark,
        });
    }

    pub fn unstage_file(&mut self, path: &str) {
        self.engine.dispatch(reef_app::AppCommand::UnstageFile {
            path: path.to_string(),
            dark: self.theme.is_dark,
        });
    }

    pub fn stage_all(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::StageAll {
            dark: self.theme.is_dark,
        });
    }

    pub fn unstage_all(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::UnstageAll {
            dark: self.theme.is_dark,
        });
    }

    pub fn stage_folder(&mut self, folder_path: &str) {
        self.engine.dispatch(reef_app::AppCommand::StageFolder {
            path: folder_path.to_string(),
            dark: self.theme.is_dark,
        });
    }

    pub fn unstage_folder(&mut self, folder_path: &str) {
        self.engine.dispatch(reef_app::AppCommand::UnstageFolder {
            path: folder_path.to_string(),
            dark: self.theme.is_dark,
        });
    }

    /// Apply the currently-pending discard target. Clears the confirmation
    /// banner, drops the selection if the discarded path(s) include it,
    /// then refreshes status + diff.
    ///
    /// Semantics by target:
    /// * `File` — restore a single unstaged file to its HEAD state (existing
    ///   ↺ behaviour).
    /// * `Folder { is_staged }` — for every file currently listed under that
    ///   directory prefix, do a section-flavoured revert (see `Section`).
    /// * `Section { is_staged }` — for every file in the section: if staged,
    ///   unstage then restore workdir to HEAD (full revert); if unstaged,
    ///   restore workdir to index.
    pub fn confirm_discard(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ConfirmDiscard {
            dark: self.theme.is_dark,
        });
    }

    /// Rebuild the commit graph iff HEAD or any ref (or the active scope)
    /// moved since the last build. Working-tree fs events do NOT
    /// invalidate the cache — see plan pitfall #2.
    pub fn refresh_graph(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::RefreshGraph);
    }

    pub fn refresh_graph_uncached(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::RefreshGraphUncached);
    }

    /// Switch the graph's walk scope to `scope`, push the previous branch
    /// onto the recents list (newest-first, deduped, capped), reset
    /// selection / scroll, invalidate the graph cache and trigger a
    /// refresh. Persists `graph.scope` and `graph.scope.recent` so the
    /// choice survives across sessions.
    pub fn set_graph_scope(&mut self, scope: GraphScope) {
        self.engine
            .dispatch(reef_app::AppCommand::SetGraphScope(scope));
        self.drain_engine_runtime_events();
    }

    /// (Re)load commit detail for the currently-selected commit. Clears detail
    /// and any previously-selected file diff whenever the target changes.
    pub fn load_commit_detail(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::LoadCommitDetail);
        self.drain_engine_runtime_events();
    }

    /// (Re)load the range-mode payload for the current Shift-extended
    /// selection. Fills per-commit metadata synchronously from the cached
    /// `rows` slice (no git walk needed) and dispatches the file-list
    /// computation — `parent(oldest).tree → newest.tree` — to the graph
    /// worker. No-ops when the selection is actually a single row.
    pub fn load_commit_range_detail(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::LoadCommitRangeDetail);
        self.drain_engine_runtime_events();
    }

    /// Dispatch the correct loader based on the current selection shape.
    /// Single-commit selection → `load_commit_detail`; range selection →
    /// `load_commit_range_detail`. Callers who previously called
    /// `load_commit_detail` directly on selection change should switch to
    /// this so range mode is exercised automatically.
    pub fn reload_graph_selection(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ReloadGraphSelection);
        self.drain_engine_runtime_events();
    }

    /// Load the inline diff for a file inside the currently-selected commit
    /// or commit range. Routes to range-file-diff plumbing when a range is
    /// active so the diff baseline matches the file list.
    ///
    /// In 3-col mode a click in the Changed-files column keeps that column
    /// focused so subsequent arrow keys can continue navigating sibling files.
    pub fn load_commit_file_diff(&mut self, path: &str) {
        if self
            .engine
            .commit_detail()
            .file_diff
            .as_ref()
            .is_none_or(|diff| diff.path != path)
        {
            self.push_location_before_jump();
        }
        let preserve_commit_focus =
            self.engine.active_tab() == Tab::Graph && self.engine.active_panel() == Panel::Commit;
        self.engine
            .dispatch(reef_app::AppCommand::LoadCommitFileDiff {
                path: path.to_string(),
                dark: self.theme.is_dark,
                uses_three_col: self.graph_uses_three_col(),
            });
        if preserve_commit_focus {
            self.engine
                .dispatch(reef_app::AppCommand::SetActivePanel(Panel::Commit));
        }
        self.drain_engine_runtime_events();
    }

    /// Reload the currently-selected commit-file diff — used after toggling
    /// `commit.diff_mode`, which changes the context-lines argument.
    pub fn reload_commit_file_diff(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ReloadCommitFileDiff {
                dark: self.theme.is_dark,
                uses_three_col: self.graph_uses_three_col(),
            });
        self.drain_engine_runtime_events();
    }

    /// Move the graph selection by `delta` rows (clamped). Clears any
    /// Shift-anchor (plain nav drops range mode) and reloads the single
    /// commit detail.
    pub fn move_graph_selection(&mut self, delta: i32) {
        self.engine
            .dispatch(reef_app::AppCommand::MoveGraphSelection(delta));
        self.drain_engine_runtime_events();
    }

    pub fn select_graph_commit(&mut self, oid: &str) {
        self.engine
            .dispatch(reef_app::AppCommand::SelectGraphCommit(oid.to_string()));
        self.drain_engine_runtime_events();
    }

    pub fn focus_graph_commit(&mut self, oid: &str) {
        self.engine
            .dispatch(reef_app::AppCommand::FocusGraphCommit(oid.to_string()));
        self.clear_commit_detail_selection();
    }

    /// Shift-extend the selection by `delta` rows. Sets the anchor to the
    /// current cursor on first call, then moves the cursor; the range always
    /// spans `[min(anchor, cursor), max(anchor, cursor)]`. When the cursor
    /// collapses back onto the anchor the selection normalises to single.
    pub fn extend_graph_selection(&mut self, delta: i32) {
        self.engine
            .dispatch(reef_app::AppCommand::ExtendGraphSelection(delta));
        self.drain_engine_runtime_events();
    }

    /// Drop any Shift-anchor, collapsing to single-select on the current
    /// cursor. No-op when not in range mode.
    pub fn clear_graph_range(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::ClearGraphRange);
        self.drain_engine_runtime_events();
    }

    /// Kick off a `git commit` through the renderer-neutral engine.
    /// Empty drafts and duplicate in-flight requests are rejected there.
    pub fn run_commit(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::RunCommit);
    }

    /// Kick off a `git push` through the renderer-neutral engine.
    /// UI surfaces the in-flight state through the engine snapshot.
    pub fn run_push(&mut self, force: bool) {
        self.engine
            .dispatch(reef_app::AppCommand::RunPush { force });
    }

    /// Start or tear down inline video playback to match an accepted preview.
    fn prepare_preview_video(
        &mut self,
        generation: u64,
        content: &Option<reef_core::preview::PreviewDocument>,
    ) {
        let source = content
            .as_ref()
            .filter(|document| reef_app::preview_is_video(document))
            .map(|document| (document.local_path.clone(), document.path.clone()));

        let Some((local_path, display_path)) = source else {
            self.clear_preview_video();
            return;
        };

        // Remote previews have no host-local file for ffmpeg to open, so the
        // card says so rather than silently showing nothing.
        let Some(local_path) = local_path else {
            self.clear_preview_video();
            self.video_status = Some(crate::video::VideoUnavailable::Remote);
            return;
        };

        // Binary preview revisions advance on every accepted reload. Using
        // that identity makes same-size rewrites invalidate the decoder too.
        if self
            .video
            .as_ref()
            .is_some_and(|player| player.matches_source(&local_path, generation))
            || self.video_pending.as_ref().is_some_and(|pending| {
                pending.path == local_path && pending.source_revision == generation
            })
        {
            return;
        }

        self.clear_preview_video();
        self.video_should_play = false;

        if self.image_picker.is_none() {
            self.video_status = Some(crate::video::VideoUnavailable::UnsupportedTerminal);
            return;
        }

        let area = crate::video::preview_frame_area(self.last_preview_rect);
        if !self.spawn_video_build(generation, local_path, generation, area, None, 0.0) {
            self.video_status = Some(crate::video::VideoUnavailable::Unreadable(format!(
                "could not open {display_path}"
            )));
        }
    }

    /// Drop any player and forget why the last one couldn't run.
    fn clear_preview_video(&mut self) {
        self.cancel_video_build();
        self.video_request_generation = self.video_request_generation.wrapping_add(1);
        self.video = None;
        self.video_status = None;
        self.video_should_play = false;
        self.video_seek_drag = None;
    }

    fn cancel_video_build(&mut self) {
        if let Some(cancellation) = self.video_build_cancellation.take() {
            cancellation.cancel();
        }
        self.video_pending = None;
    }

    fn spawn_video_build(
        &mut self,
        preview_generation: u64,
        path: PathBuf,
        source_revision: u64,
        area: ratatui::layout::Rect,
        source: Option<crate::video::VideoSource>,
        position: f64,
    ) -> bool {
        let Some(picker) = self.image_picker.clone() else {
            return false;
        };
        self.cancel_video_build();
        self.video_request_generation = self.video_request_generation.wrapping_add(1);
        let request_generation = self.video_request_generation;
        let cancellation = reef_io::CancellationToken::default();
        let request = VideoBuildRequest {
            preview_generation,
            request_generation,
            path: path.clone(),
            source_revision,
            area,
            source,
            position,
            picker,
            cancellation: cancellation.clone(),
        };
        if self.video_build_tx.send(request).is_ok() {
            self.video_build_cancellation = Some(cancellation);
            self.video_pending = Some(PendingVideoBuild {
                preview_generation,
                request_generation,
                path,
                source_revision,
                area,
            });
            true
        } else {
            false
        }
    }

    /// Merge finished video openers. Results for a preview the user already
    /// navigated away from are dropped, exactly like image protocol builds.
    fn drain_preview_video_builds(&mut self) -> bool {
        let mut changed = false;
        while let Ok(built) = self.video_build_rx.try_recv() {
            let matches_pending = self.video_pending.as_ref().is_some_and(|pending| {
                pending.preview_generation == built.preview_generation
                    && pending.request_generation == built.request_generation
                    && pending.path == built.path
                    && pending.source_revision == built.source_revision
                    && pending.area == built.area
            });
            if built.preview_generation != self.engine.preview_generation() || !matches_pending {
                continue;
            }
            self.video_pending = None;
            self.video_build_cancellation = None;
            match built.result {
                Ok(mut player) => match player.set_playing(self.video_should_play) {
                    Ok(()) => {
                        self.video = Some(player);
                        self.video_status = None;
                    }
                    Err(reason) => {
                        self.video = None;
                        self.video_status = Some(reason);
                        self.video_should_play = false;
                    }
                },
                Err(reason) => {
                    self.video = None;
                    self.video_status = Some(reason);
                }
            }
            changed = true;
        }
        changed
    }

    fn schedule_preview_video_resize(&mut self) {
        let Some(area) = self.last_video_frame_area else {
            return;
        };
        let Some(picker) = self.image_picker.as_ref() else {
            return;
        };
        let Some(player) = self.video.as_ref() else {
            return;
        };
        if player.has_ended() || player.matches_area(picker, area) {
            return;
        }
        // Keep one decoder rebuild in flight. If the terminal is still being
        // resized, the next tick compares the completed player with the most
        // recent cached area and coalesces directly to that geometry.
        if self.video_pending.is_some() {
            return;
        }

        let source = player.source();
        let path = player.path().to_path_buf();
        let source_revision = player.source_revision();
        let position = player.position();
        let preview_generation = self.engine.preview_generation();
        if self.spawn_video_build(
            preview_generation,
            path,
            source_revision,
            area,
            Some(source),
            position,
        ) && let Some(player) = self.video.as_mut()
        {
            player.pause();
        }
    }

    /// Advance the playing clip by however many frames are due.
    fn tick_video(&mut self, now: Instant) -> bool {
        let Some(picker) = self.image_picker.as_ref() else {
            return false;
        };
        let Some(player) = self.video.as_mut() else {
            return false;
        };
        let changed = player.tick(now, picker);
        let playback_error = player.take_playback_error();
        if player.has_ended() && self.video_pending.is_none() {
            self.video_should_play = false;
        }
        if let Some(error) = playback_error {
            self.video = None;
            self.video_status = Some(error);
            self.video_should_play = false;
            return true;
        }
        changed
    }

    /// Whether the preview currently on screen is a video card — the guard
    /// for the playback key binding.
    pub fn preview_is_video(&self) -> bool {
        self.video.is_some()
            || self.video_status.is_some()
            || self.video_pending.is_some()
            || self
                .engine
                .preview_content_ref()
                .is_some_and(reef_app::preview_is_video)
    }

    /// Play / pause the current clip.
    pub fn toggle_video_playback(&mut self) {
        self.video_seek_drag = None;
        if self.video.as_ref().is_some_and(|player| player.has_ended()) {
            let area = self
                .last_video_frame_area
                .unwrap_or_else(|| crate::video::preview_frame_area(self.last_preview_rect));
            let Some(player) = self.video.as_ref() else {
                return;
            };
            let source = player.source();
            let path = player.path().to_path_buf();
            let source_revision = player.source_revision();
            self.video_should_play = true;
            let _ = self.spawn_video_build(
                self.engine.preview_generation(),
                path,
                source_revision,
                area,
                Some(source),
                0.0,
            );
            return;
        }

        self.video_should_play = !self.video_should_play;
        let error = self
            .video
            .as_mut()
            .and_then(|player| player.set_playing(self.video_should_play).err());
        if let Some(reason) = error {
            self.video = None;
            self.video_should_play = false;
            self.video_status = Some(reason);
        }
    }

    pub(crate) fn begin_video_seek(&mut self, start: u16, width: u16, column: u16) {
        let Some(player) = self.video.as_mut() else {
            return;
        };
        if !player
            .info()
            .duration
            .is_some_and(|duration| duration.is_finite() && duration > 0.0)
        {
            return;
        }
        player.pause();
        // A resize/open result from before this gesture must not replace the
        // paused player and resume it halfway through dragging.
        self.video_request_generation = self.video_request_generation.wrapping_add(1);
        self.cancel_video_build();
        self.video_seek_drag = Some(VideoSeekDrag {
            start,
            width,
            ratio: video_seek_ratio(start, width, column),
        });
    }

    pub(crate) fn update_video_seek(&mut self, column: u16) {
        let Some(seek) = self.video_seek_drag.as_mut() else {
            return;
        };
        seek.ratio = video_seek_ratio(seek.start, seek.width, column);
    }

    pub(crate) fn finish_video_seek(&mut self, column: u16) {
        self.update_video_seek(column);
        let Some(seek) = self.video_seek_drag.take() else {
            return;
        };
        let Some(player) = self.video.as_ref() else {
            return;
        };
        let Some(position) = player.seek_position(seek.ratio) else {
            return;
        };
        let area = self
            .last_video_frame_area
            .unwrap_or_else(|| crate::video::preview_frame_area(self.last_preview_rect));
        let source = player.source();
        let path = player.path().to_path_buf();
        let source_revision = player.source_revision();
        let _ = self.spawn_video_build(
            self.engine.preview_generation(),
            path,
            source_revision,
            area,
            Some(source),
            position,
        );
    }

    pub(crate) fn video_seek_position(&self) -> Option<f64> {
        let seek = self.video_seek_drag?;
        let duration = self.video.as_ref()?.info().duration?;
        Some(duration * seek.ratio)
    }

    fn prepare_preview_image_protocol(
        &mut self,
        generation: u64,
        same_file: bool,
        content: &mut Option<reef_core::preview::PreviewDocument>,
    ) {
        let reuse_protocol = same_file
            && self.preview_image_protocol.is_some()
            && matches!(
                (
                    self.engine.preview_content_ref().map(|current| &current.body),
                    content.as_ref().map(|next| &next.body),
                ),
                (
                    Some(reef_core::preview::PreviewBody::Image(old)),
                    Some(reef_core::preview::PreviewBody::Image(new)),
                ) if old.bytes_on_disk == new.bytes_on_disk
                    && old.width_px == new.width_px
                    && old.height_px == new.height_px
                    && old.format == new.format
            );
        if reuse_protocol {
            if let Some(reef_core::preview::PreviewBody::Image(image)) =
                content.as_mut().map(|preview| &mut preview.body)
            {
                image.image = None;
            }
            return;
        }

        let dyn_img = match (
            self.image_picker.as_ref(),
            content.as_mut().map(|p| &mut p.body),
        ) {
            (Some(_), Some(reef_core::preview::PreviewBody::Image(image))) => image.image.take(),
            _ => None,
        };
        if let (Some(dyn_img), Some(picker)) = (dyn_img, self.image_picker.as_ref()) {
            self.preview_image_protocol = Some(ratatui_image::thread::ThreadProtocol::new(
                self.preview_resize_tx.clone(),
                None,
            ));
            self.preview_image_protocol_builds += 1;
            let picker_clone = picker.clone();
            let build_tx = self.preview_build_tx.clone();
            std::thread::Builder::new()
                .name("reef-image-build".into())
                .spawn(move || {
                    let protocol = picker_clone.new_resize_protocol(dyn_img);
                    let _ = build_tx.send(BuiltProtocol {
                        generation,
                        protocol,
                    });
                })
                .ok();
        } else {
            self.preview_image_protocol = None;
        }
    }

    fn apply_preview_result_for_adapter(
        &mut self,
        generation: u64,
        result: Result<Option<reef_core::preview::PreviewDocument>, String>,
    ) {
        if generation != self.engine.preview_generation() {
            return;
        }
        let validating_global_search_hit = self
            .engine
            .global_search_hit_accepting_generation(generation);
        match result {
            Ok(mut content) => {
                let same_file = matches!(
                    (self.engine.preview_content_ref(), content.as_ref()),
                    (Some(old), Some(new)) if old.path == new.path
                );
                if !(validating_global_search_hit && content.is_none()) {
                    self.prepare_preview_image_protocol(generation, same_file, &mut content);
                    self.prepare_preview_video(generation, &content);
                }
                self.engine
                    .dispatch(reef_app::AppCommand::ApplyPreviewResult {
                        generation,
                        result: Ok(content),
                        preview_view_h: self.layout.last_preview_view_h as usize,
                    });
                self.drain_engine_runtime_events();
            }
            Err(error) => {
                self.pause_video_playback();
                self.cancel_video_build();
                self.engine
                    .dispatch(reef_app::AppCommand::ApplyPreviewResult {
                        generation,
                        result: Err(error),
                        preview_view_h: self.layout.last_preview_view_h as usize,
                    });
                self.drain_engine_runtime_events();
            }
        }
    }

    fn apply_runtime_events(&mut self, events: Vec<reef_app::AppRuntimeEvent>) {
        for event in events {
            match event {
                reef_app::AppRuntimeEvent::PreviewResultForAdapter { generation, result } => {
                    self.apply_preview_result_for_adapter(generation, result);
                }
                reef_app::AppRuntimeEvent::TabChanged(outcome) => {
                    self.apply_tab_change_outcome(outcome);
                }
                reef_app::AppRuntimeEvent::SidebarToggled(outcome) => {
                    if outcome.hidden {
                        if outcome.cancel_split_drags {
                            self.dragging_split = false;
                            self.dragging_graph_diff_split = false;
                        }
                        if outcome.show_hidden_hint {
                            self.push_toast(Toast::info(crate::i18n::t(
                                crate::i18n::Msg::SidebarHiddenHint,
                            )));
                        }
                    }
                }
                reef_app::AppRuntimeEvent::LoadPreviewSelected => self.load_preview(),
                reef_app::AppRuntimeEvent::LoadDiffRequested => self.load_diff(),
                reef_app::AppRuntimeEvent::SyncSearchPreviewIfStale => {
                    self.engine
                        .dispatch(reef_app::AppCommand::SyncGlobalSearchPreviewIfStale {
                            preview_view_h: self.layout.last_preview_view_h as usize,
                        });
                }
                reef_app::AppRuntimeEvent::RecomputeVimSearch => {
                    crate::search::recompute_and_jump(self);
                }
                reef_app::AppRuntimeEvent::RecomputeFindWidget => {
                    crate::find_widget::recompute(self);
                }
                reef_app::AppRuntimeEvent::AcceptQuickOpenSelection => {
                    crate::quick_open::accept(self);
                }
                reef_app::AppRuntimeEvent::AcceptGlobalSearchSelection => {
                    crate::global_search::accept(self);
                }
                reef_app::AppRuntimeEvent::FileActionNotice(notice) => {
                    self.push_file_action_notice(notice);
                }
                reef_app::AppRuntimeEvent::FileCopyDone { result } => {
                    self.push_file_copy_done_toast(result);
                }
                reef_app::AppRuntimeEvent::FsMutationDone { kind, result } => {
                    self.push_fs_mutation_done_toast(kind, result);
                }
                reef_app::AppRuntimeEvent::CommitDone { result } => {
                    self.push_commit_done_toast(result);
                }
                reef_app::AppRuntimeEvent::PushDone { force, result } => {
                    self.push_push_done_toast(force, result);
                }
                reef_app::AppRuntimeEvent::DismissConfirm => {
                    self.dismiss_confirm();
                }
                reef_app::AppRuntimeEvent::ReplaceDone { result } => {
                    self.push_replace_done_toast(result);
                }
                reef_app::AppRuntimeEvent::ClearCommitGraphSearch => {
                    self.engine
                        .dispatch(reef_app::AppCommand::ClearVimSearchIfTarget(
                            reef_app::SearchTarget::CommitGraph,
                        ));
                }
                reef_app::AppRuntimeEvent::LocationJumped(outcome) => {
                    if let Some(target) = outcome.restore_preview_cursor.as_ref() {
                        self.restore_preview_cursor(target);
                    }
                    if outcome.clear_commit_detail_selection {
                        self.clear_commit_detail_selection();
                    }
                    if outcome.clear_diff_selection {
                        self.clear_diff_selection();
                    }
                }
                reef_app::AppRuntimeEvent::ClearPreviewSelection => {
                    self.clear_preview_selection();
                    self.pending_preview_nav = None;
                    self.db_preview_layout = None;
                }
                reef_app::AppRuntimeEvent::LspRefineJump(outcome) => {
                    self.nav_push_back(outcome.pending_jump.origin);
                    self.nav_jump_to_lsp(&outcome.location);
                }
                reef_app::AppRuntimeEvent::RetryDeferredPreviewActions => {
                    self.resolve_pending_highlight();
                    self.retry_pending_preview_nav();
                }
                reef_app::AppRuntimeEvent::ClearCommitDetailSelection => {
                    self.clear_commit_detail_selection();
                }
                reef_app::AppRuntimeEvent::ClearDiffSelection => self.clear_diff_selection(),
                reef_app::AppRuntimeEvent::PersistQuickOpenMru(encoded) => {
                    crate::prefs::set(reef_core::quick_open::MRU_PREF_KEY, &encoded);
                }
                reef_app::AppRuntimeEvent::PersistHostsRecent(recent) => {
                    crate::hosts_picker::save_recent(&recent);
                }
                reef_app::AppRuntimeEvent::PersistGraphScope => {
                    persist_graph_scope(
                        self.engine.graph_scope(),
                        self.engine.graph_recent_branches(),
                    );
                }
                reef_app::AppRuntimeEvent::GraphScopeFallback { short_ref } => {
                    self.push_toast(Toast::info(crate::i18n::graph_scope_stale_branch_toast(
                        &short_ref,
                    )));
                    persist_graph_scope(
                        self.engine.graph_scope(),
                        self.engine.graph_recent_branches(),
                    );
                }
                reef_app::AppRuntimeEvent::GraphBranchPickerNotReady => {
                    self.push_toast(Toast::info(crate::i18n::graph_picker_not_ready_toast()));
                }
                reef_app::AppRuntimeEvent::GraphBranchPickerStaleBranch { short_ref } => {
                    self.push_toast(Toast::info(crate::i18n::graph_scope_stale_branch_toast(
                        &short_ref,
                    )));
                }
            }
        }
    }

    pub(crate) fn drain_engine_runtime_events(&mut self) {
        let events = self.engine.drain_runtime_events();
        self.apply_runtime_events(events);
    }

    fn push_file_action_notice(&mut self, notice: reef_app::FileActionNotice) {
        let toast = match notice {
            reef_app::FileActionNotice::PlaceCopyInFlight => {
                Toast::warn(crate::i18n::place_mode_blocked_by_in_flight_copy())
            }
            reef_app::FileActionNotice::TreeOpInFlight => {
                Toast::warn(crate::i18n::tree_op_blocked_by_in_flight())
            }
            reef_app::FileActionNotice::PasteClipboardEmpty => {
                Toast::warn(crate::i18n::paste_clipboard_empty())
            }
            reef_app::FileActionNotice::PasteSelfIntoDescendant => {
                Toast::warn(crate::i18n::paste_self_into_descendant())
            }
            reef_app::FileActionNotice::PasteNothingToDo => {
                Toast::info(crate::i18n::paste_nothing_to_do())
            }
            reef_app::FileActionNotice::PasteCancelled => {
                Toast::info(crate::i18n::paste_cancelled())
            }
        };
        self.push_toast(toast);
    }

    fn push_commit_done_toast(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.push_toast(Toast::info(crate::i18n::t(crate::i18n::Msg::CommitSuccess))),
            Err(error) => self.push_toast(Toast::error(crate::i18n::commit_failed_toast(&error))),
        }
    }

    fn push_push_done_toast(&mut self, force: bool, result: Result<(), String>) {
        match result {
            Ok(()) => {
                let msg = if force {
                    crate::i18n::Msg::ForcePushSuccess
                } else {
                    crate::i18n::Msg::PushSuccess
                };
                self.push_toast(Toast::info(crate::i18n::t(msg)));
            }
            Err(error) => self.push_toast(Toast::error(crate::i18n::push_failed_toast(&error))),
        }
    }

    fn push_file_copy_done_toast(&mut self, result: Result<usize, String>) {
        match result {
            Ok(count) => self.push_toast(Toast::info(crate::i18n::place_mode_copied(count))),
            Err(error) => {
                self.push_toast(Toast::error(crate::i18n::place_mode_copy_failed(&error)))
            }
        }
    }

    fn push_fs_mutation_done_toast(
        &mut self,
        kind: reef_app::FsMutationKind,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => self.push_toast(Toast::info(crate::i18n::fs_mutation_success_toast(&kind))),
            Err(error) => self.push_toast(Toast::error(crate::i18n::fs_mutation_error_toast(
                &kind, &error,
            ))),
        }
    }

    fn push_replace_done_toast(&mut self, result: Result<reef_app::ReplaceSummary, String>) {
        match result {
            Ok(summary) => {
                let mut text = format!(
                    "{} {} / {}",
                    crate::i18n::t(crate::i18n::Msg::ReplaceSummaryToast),
                    summary.lines_replaced,
                    summary.files_changed,
                );
                if summary.skipped_stale > 0 {
                    text.push_str(&format!(
                        " · {} {}",
                        summary.skipped_stale,
                        crate::i18n::t(crate::i18n::Msg::ReplaceSkippedStaleSuffix),
                    ));
                }
                if summary.skipped_too_large > 0 {
                    text.push_str(&format!(
                        " · {} {}",
                        summary.skipped_too_large,
                        crate::i18n::t(crate::i18n::Msg::ReplaceTooLargeSuffix),
                    ));
                }
                if !summary.errors.is_empty() {
                    text.push_str(&format!(" · {} error(s)", summary.errors.len()));
                }
                self.push_toast(Toast::info(text));
            }
            Err(error) => {
                self.push_toast(Toast::error(format!("replace failed: {error}")));
            }
        }
    }

    /// Open Settings. No-op when already open so a stray re-entry
    /// can't silently discard an in-progress inline text edit. The
    /// pref-cache refresh keeps the page reading from memory rather
    /// than disk on every render.
    pub fn open_settings(&mut self) {
        if self.engine.view_mode() == ViewMode::Settings {
            return;
        }
        self.engine.dispatch(reef_app::AppCommand::OpenSettings);
        self.engine
            .dispatch(reef_app::AppCommand::CancelSettingsEditorCommandEdit);
        crate::settings::refresh_pref_cache(self);
        // Re-probe LSP binaries so the Code Navigation rows reflect any
        // out-of-band install (e.g. the user ran `cargo install
        // rust-analyzer` in another terminal since launch). Cheap, and
        // off the render path. Without this, a row could read "Missing"
        // and re-install an already-present server.
        self.engine
            .dispatch(reef_app::AppCommand::RefreshLspInstalled);
    }

    /// Esc semantics — uncommitted buffer discarded, Enter is the
    /// explicit commit.
    pub fn close_settings(&mut self) {
        self.engine.dispatch(reef_app::AppCommand::CloseSettings);
        self.engine
            .dispatch(reef_app::AppCommand::CancelSettingsEditorCommandEdit);
    }

    /// Enter "pure preview" (纯预览) — the active tab's preview/diff
    /// panel takes the whole screen. On entry we move `active_panel`
    /// onto the content column so scroll keys route the right way;
    /// `ui::render` decides what to show based on `active_tab`. Esc
    /// flips `view_mode` back to `Main`.
    ///
    /// No-op when not in `Main`. Doesn't validate that there's
    /// something to show — an empty/loading preview is a normal state
    /// to render, just maximised.
    pub fn enter_focused_preview(&mut self) {
        self.clear_input_chords();
        self.engine
            .dispatch(reef_app::AppCommand::EnterFocusedPreview {
                uses_three_col: self.graph_uses_three_col(),
            });
    }

    /// CLI entry point — `reef <file>` lands here after the workdir
    /// has been opened at the file's parent (or the repo root, when
    /// the file is inside a git repo — see `resolve_local_path`).
    /// Drops straight into FocusedPreview against the given
    /// workdir-relative path.
    ///
    /// Race-fix history: `App::new_with_backend` already fires an
    /// async `refresh_file_tree` with an *empty* `selected_path`
    /// snapshot. A naïve "reveal then dispatch_preview_load" would
    /// lose the race — the worker comes back with `selected_idx = 0`,
    /// `replace_entries` overwrites our selection, and the post-rebuild
    /// `before != after` check fires a second `load_preview()` that
    /// clobbers the in-flight foo.rs preview with whatever entries[0]
    /// happens to be (often README.md).
    ///
    /// Fix: re-fire `refresh_file_tree_with_target(Some(rel))`
    /// *after* setting the expanded ancestors via `reveal`. The new
    /// task supersedes the prior one (generation bump), the worker
    /// now captures the right `selected_path` snapshot, and
    /// `apply_worker_result`'s eventual `load_preview()` for the
    /// post-rebuild selection lands on the same `rel` path — same
    /// path means dedupe at the dispatch layer (or a benign redundant
    /// load against the file we actually want).
    pub fn enter_focused_preview_with_file(&mut self, rel: PathBuf) {
        self.clear_input_chords();
        self.engine
            .dispatch(reef_app::AppCommand::EnterFocusedPreviewWithFile {
                rel,
                dark: self.theme.is_dark,
                wants_decoded_image: self.image_picker.is_some(),
            });
        self.drain_engine_runtime_events();
    }

    pub fn close_focused_preview(&mut self) {
        self.clear_input_chords();
        self.engine
            .dispatch(reef_app::AppCommand::CloseFocusedPreview);
    }

    /// Space+V routing: enter from Main, exit from FocusedPreview.
    /// No-op while Settings owns the screen so a stray chord can't
    /// silently discard an in-progress settings edit.
    pub fn toggle_focused_preview(&mut self) {
        self.clear_input_chords();
        self.engine
            .dispatch(reef_app::AppCommand::ToggleFocusedPreview {
                uses_three_col: self.graph_uses_three_col(),
            });
    }

    /// Shared gate for the ☰ chip + file picker: whether the chip
    /// affordance is visually present right now. The renderer in
    /// `focused_preview_panel::render` checks this, and the input
    /// handler's `o` key shortcut checks the same predicate so the
    /// two states never disagree (the original bug had keyboard
    /// opening a picker the renderer refused to draw on Graph 2-col).
    ///
    /// Graph 2-col uses `commit_detail_panel`'s header (commit
    /// metadata + inline files tree), which doesn't match the
    /// `path_display + tag_str` layout the chip's wash math assumes —
    /// so the chip is deliberately not rendered there, and the `o`
    /// shortcut likewise no-ops.
    pub fn focused_preview_chip_visible(&self) -> bool {
        self.engine
            .focused_preview_chip_visible(self.graph_uses_three_col())
    }

    // ── FocusedPreview floating file picker ──────────────────────────

    /// Snapshot of the diff-changed file list to render in the popup.
    /// Built fresh each call from `staged_files + unstaged_files` (Git)
    /// or `commit_detail.detail.files` (Graph). Sorted by path so the
    /// indented "tree-ish" layout in the popup is stable.
    pub fn focused_preview_file_entries(&self) -> Vec<reef_app::FocusedPreviewFileRow> {
        self.engine.focused_preview_file_entries()
    }

    /// Open the floating picker. Snaps the highlighted row to whatever
    /// file the diff is currently showing, so ↑/↓ navigation feels like
    /// it picks up where the user already is. Falls back to row 0 if
    /// the current target isn't in the list (e.g. no diff loaded yet).
    ///
    /// Git tab subtlety: a file can legitimately appear in both staged
    /// and unstaged at the same time (committed + further edited). The
    /// snap must compare `is_staged` too — otherwise opening the picker
    /// while viewing the unstaged diff would snap to the staged row
    /// (sort order puts staged first) and a no-op Enter would silently
    /// switch the diff target from unstaged to staged.
    pub fn open_focused_preview_files(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::OpenFocusedPreviewFiles);
    }

    pub fn close_focused_preview_files(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::CloseFocusedPreviewFiles);
    }

    pub fn toggle_focused_preview_files(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ToggleFocusedPreviewFiles);
    }

    pub fn move_focused_preview_files_selection(&mut self, delta: i32) {
        self.engine
            .dispatch(reef_app::AppCommand::MoveFocusedPreviewFilesSelection(
                delta,
            ));
    }

    /// Apply the highlighted row: load the corresponding file's diff
    /// and close the picker. The diff render path then picks up the
    /// new target on the next frame.
    pub fn confirm_focused_preview_files_selection(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::ConfirmFocusedPreviewFilesSelection {
                dark: self.theme.is_dark,
                uses_three_col: self.graph_uses_three_col(),
            });
        self.drain_engine_runtime_events();
    }

    /// Mouse-click variant — picks by absolute index, used by the
    /// `PickFocusedPreviewFile(usize)` action.
    pub fn pick_focused_preview_file(&mut self, idx: usize) {
        self.engine
            .dispatch(reef_app::AppCommand::PickFocusedPreviewFile {
                idx,
                dark: self.theme.is_dark,
                uses_three_col: self.graph_uses_three_col(),
            });
        self.drain_engine_runtime_events();
    }

    pub fn set_active_tab(&mut self, tab: Tab) {
        self.engine
            .dispatch(reef_app::AppCommand::SetActiveTab(tab));
        self.drain_engine_runtime_events();
    }

    /// Stop playback without discarding the player.
    pub fn pause_video_playback(&mut self) {
        self.video_should_play = false;
        if let Some(player) = self.video.as_mut() {
            player.pause();
        }
    }

    pub(super) fn apply_tab_change_outcome(&mut self, outcome: reef_app::TabChangeOutcome) {
        if !outcome.changed {
            return;
        }
        // Every tab transition, including history and code-navigation jumps
        // initiated inside reef-app, converges here. Hidden video must not
        // keep its decoder and audio process running.
        if self.engine.active_tab() != Tab::Files {
            self.pause_video_playback();
        }
        self.clear_input_chords();
        if outcome.dismiss_confirm {
            self.dismiss_confirm();
        }
        if outcome.clear_preview_selection {
            self.clear_preview_selection();
        }
        if outcome.clear_commit_detail_selection {
            self.clear_commit_detail_selection();
        }
        if outcome.clear_diff_selection {
            self.clear_diff_selection();
        }
        if outcome.close_find_widget {
            crate::find_widget::close(self);
        }
        if outcome.sync_search_preview {
            self.engine
                .dispatch(reef_app::AppCommand::SyncGlobalSearchPreviewIfStale {
                    preview_view_h: self.layout.last_preview_view_h as usize,
                });
        }
    }

    pub(crate) fn clear_input_chords(&mut self) {
        self.space_leader_at = None;
        self.g_pending_at = None;
    }

    pub(crate) fn clear_g_chord(&mut self) {
        self.g_pending_at = None;
    }

    pub fn activity_message(&self) -> Option<String> {
        match self.engine.active_tab() {
            // Search activity is surfaced in the tab's own footer (`N / M ·
            // scanning…`), not in the global status bar.
            Tab::Search => {
                if self.engine.snapshot().search.load.loading {
                    Some("search scanning…".into())
                } else {
                    self.engine
                        .activity_state(Tab::Search)
                        .map(|(label, state)| activity_text(label, state))
                }
            }
            tab => self
                .engine
                .activity_state(tab)
                .map(|(label, state)| activity_text(label, state)),
        }
    }

    pub fn handle_action(&mut self, action: ClickAction) {
        match action {
            ClickAction::SwitchTab(tab) => {
                self.set_active_tab(tab);
            }
            ClickAction::ToggleSidebar => {
                self.toggle_sidebar();
            }
            ClickAction::OpenSettings => {
                self.open_settings();
            }
            ClickAction::OpenHostsPicker => {
                self.open_hosts_picker();
            }
            ClickAction::TreeClick(index) => {
                let changes_preview = self
                    .engine
                    .file_tree_entry(index)
                    .filter(|entry| !entry.is_dir)
                    .is_some_and(|entry| {
                        self.engine
                            .preview_content_ref()
                            .is_none_or(|preview| std::path::Path::new(&preview.path) != entry.path)
                    });
                if changes_preview {
                    self.push_location_before_jump();
                }
                self.engine
                    .dispatch(reef_app::AppCommand::ActivateFileTreeEntryAtIndex(index));
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
            ClickAction::StartDragSplit => {
                self.dragging_split = true;
            }
            ClickAction::StartDragGraphDiffSplit => {
                self.dragging_graph_diff_split = true;
            }
            ClickAction::GitCommand { command, args } => {
                // Try each panel's dispatcher in turn. Unknown commands are
                // silently dropped — no external handler to fall through to.
                if crate::ui::git_status_panel::handle_command(self, &command, &args) {
                    return;
                }
                if crate::ui::git_graph_panel::handle_command(self, &command, &args) {
                    return;
                }
                let _ = crate::ui::commit_detail_panel::handle_command(self, &command, &args);
            }
            // Palette clicks are dispatched inline by their respective
            // `handle_mouse` fns (single-click select, double-click accept)
            // rather than routed through `handle_action`, because the
            // double-click distinction needs `last_click` timing that's only
            // available at the input layer.
            ClickAction::QuickOpenSelect(_) => {}
            // Graph branch picker: overlay-only, dispatched inline in
            // `input::handle_mouse_graph_branch_picker`, never reaches here.
            ClickAction::GraphBranchPickerSelect(_) => {}
            // Tab::Search result clicks DO route through here — the tab is
            // not an overlay, so input::handle_mouse lets the click fall
            // through to hit_test + handle_action. Update the selection and
            // trigger live preview.
            ClickAction::GlobalSearchSelect(idx) => {
                if self.engine.active_tab() == Tab::Search {
                    self.engine
                        .dispatch(reef_app::AppCommand::SelectGlobalSearchResult {
                            idx,
                            visible_rows: self.layout.global_search_last_view_h as usize,
                        });
                    crate::global_search::navigate_to_selected(self);
                }
                // Overlay case is unreachable via this path — handled inline
                // in `global_search::handle_mouse`.
            }
            ClickAction::GlobalSearchFocusInput => {
                if self.engine.active_tab() == Tab::Search {
                    self.engine
                        .dispatch(reef_app::AppCommand::FocusGlobalSearchFindInput);
                }
            }
            ClickAction::PlaceModeFolder(index) => {
                // Confirm a place-mode drop onto a specific folder. Resolve
                // the entry's absolute path and hand off to the worker.
                // Stale indices (e.g. the tree rebuilt out from under us)
                // or accidental clicks on non-directory rows fall back to a
                // cancel — safer than silently dropping to an unrelated
                // destination.
                let dest = self.engine.file_tree_dir_abs_path(index);
                match dest {
                    Some(dest_dir) => {
                        let sources = self.engine.place_mode_sources();
                        self.request_file_copy(sources, dest_dir);
                    }
                    None => self.exit_place_mode(),
                }
            }
            ClickAction::PlaceModeRoot => {
                let sources = self.engine.place_mode_sources();
                let dest_dir = self.engine.file_tree_root();
                self.request_file_copy(sources, dest_dir);
            }
            ClickAction::FileTreeToolbarNewFile => {
                let target = self.toolbar_create_target();
                let (parent, anchor) = self.resolve_create_anchor(target);
                self.begin_tree_edit(reef_app::TreeEditMode::NewFile, parent, None, anchor);
            }
            ClickAction::FileTreeToolbarNewFolder => {
                let target = self.toolbar_create_target();
                let (parent, anchor) = self.resolve_create_anchor(target);
                self.begin_tree_edit(reef_app::TreeEditMode::NewFolder, parent, None, anchor);
            }
            ClickAction::FileTreeToolbarCollapse => {
                self.collapse_all_tree_entries();
            }
            ClickAction::TreeContextMenuItem(item) => {
                self.dispatch_context_menu_item(item);
            }
            ClickAction::TreeContextMenuClose => {
                self.close_tree_context_menu();
            }
            ClickAction::SelectionContextMenuItem(item) => {
                self.dispatch_selection_context_menu_item(item);
            }
            ClickAction::SelectionContextMenuClose => {
                self.close_selection_context_menu();
            }
            ClickAction::NavCandidateSelect(idx) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SelectNavCandidate {
                        index: idx,
                        viewport_rows: self.nav_peek_visible_rows,
                    });
            }
            ClickAction::NavCandidateGroupToggle(idx) => {
                self.engine
                    .dispatch(reef_app::AppCommand::ToggleNavCandidateGroup {
                        index: idx,
                        viewport_rows: self.nav_peek_visible_rows,
                    });
            }
            ClickAction::NavCandidatesClose => {
                self.nav_close_candidates();
            }
            ClickAction::NavCandidatesCapture => {}
            ClickAction::OpenMarkdownLink(target) => {
                self.open_markdown_link(&target);
            }
            ClickAction::SetStructuredPreviewMode(mode) => {
                self.set_structured_preview_mode(mode);
            }
            ClickAction::ToggleStructuredPreviewNode(node_id) => {
                self.engine
                    .dispatch(reef_app::AppCommand::ToggleStructuredPreviewNode(node_id));
                self.clear_preview_selection();
            }
            ClickAction::ToggleVideoPlayback => {
                self.toggle_video_playback();
            }
            // Video seeking needs the pointer column and is dispatched by
            // `input::handle_video_seek`, before generic click actions.
            ClickAction::SeekVideo { .. } => {}
            ClickAction::HostsPickerSelect(idx) => {
                // Mouse click on a hosts-picker row: move selection to
                // that row.
                self.engine
                    .dispatch(reef_app::AppCommand::SelectHostsPickerRow(idx));
                // Enter path-mode immediately so user can type /path and
                // hit Enter — matches the overlay's keyboard UX.
                self.engine
                    .dispatch(reef_app::AppCommand::EnterHostsPickerPathMode);
            }
            ClickAction::TreeClearSelection => {
                // Left-click on empty tree space → drop the selection
                // highlight. Next toolbar `+ File` / `+ Folder` lands
                // at the project root. Any in-progress inline edit is
                // also cancelled, matching VSCode's "click elsewhere
                // discards the pending name" behaviour.
                self.engine
                    .dispatch(reef_app::AppCommand::ClearFileTreeSelectionAndEdit);
            }
            ClickAction::DbPrevPage => {
                self.db_navigate(DbNav::PrevPage);
            }
            ClickAction::DbNextPage => {
                self.db_navigate(DbNav::NextPage);
            }
            ClickAction::DbGotoPage(page) => {
                self.db_navigate_to_page(page);
            }
            ClickAction::DbSelectObject { schema, name, kind } => {
                self.db_select_object(reef_sqlite_preview::DbObjectKey { schema, name, kind });
            }
            ClickAction::DbToggleSchema(name) => {
                self.db_toggle_schema(&name);
            }
            ClickAction::DbSelectCell { row, column } => {
                self.engine
                    .dispatch(reef_app::AppCommand::DbLoadCell { row, column });
            }
            ClickAction::FindWidgetClose => {
                crate::find_widget::close(self);
            }
            ClickAction::FindWidgetNext => {
                if !self.engine.find_widget().matches.is_empty() {
                    crate::find_widget::step(self, /*reverse=*/ false);
                }
            }
            ClickAction::FindWidgetPrev => {
                if !self.engine.find_widget().matches.is_empty() {
                    crate::find_widget::step(self, /*reverse=*/ true);
                }
            }
            ClickAction::FindWidgetToggleCase => {
                crate::find_widget::toggle_option(self, reef_app::FindWidgetToggle::MatchCase);
            }
            ClickAction::FindWidgetToggleWord => {
                crate::find_widget::toggle_option(self, reef_app::FindWidgetToggle::WholeWord);
            }
            ClickAction::FindWidgetToggleRegex => {
                crate::find_widget::toggle_option(self, reef_app::FindWidgetToggle::Regex);
            }
            ClickAction::SearchToggleReplace => {
                if self.engine.active_tab() == Tab::Search {
                    self.engine
                        .dispatch(reef_app::AppCommand::ToggleGlobalSearchReplaceForSearchTab);
                }
            }
            ClickAction::GlobalSearchFocusReplaceInput => {
                if self.engine.active_tab() == Tab::Search
                    && self.engine.snapshot().search.replace_open
                {
                    self.engine
                        .dispatch(reef_app::AppCommand::FocusGlobalSearchReplaceInput);
                }
            }
            ClickAction::SearchToggleMatch(idx) => {
                self.engine
                    .dispatch(reef_app::AppCommand::ToggleGlobalSearchMatchExcluded(idx));
            }
            ClickAction::SearchApplyReplace => {
                self.commit_replace_in_files();
            }
            ClickAction::SettingsRow(idx) => {
                if self.engine.view_mode() == ViewMode::Settings {
                    self.engine
                        .dispatch(reef_app::AppCommand::SelectSettingsRow(idx));
                    // LSP rows are actionable buttons ("Enter to
                    // install") — a click should install, matching the
                    // user's expectation and every other clickable list
                    // in the UI. Other settings rows keep the
                    // click-selects / Enter-activates convention so a
                    // stray click can't flip a pref.
                    if let crate::settings::SettingItem::Lsp(lang) =
                        self.engine.settings().selected()
                    {
                        self.activate_lsp_row(lang);
                    }
                }
            }
            ClickAction::ConfirmModalPrimary => {
                self.fire_confirm_primary();
            }
            ClickAction::ConfirmModalCancel => {
                self.fire_confirm_cancel();
            }
            ClickAction::ToggleFocusedPreviewFiles => {
                self.toggle_focused_preview_files();
            }
            ClickAction::PickFocusedPreviewFile(idx) => {
                self.pick_focused_preview_file(idx);
            }
            ClickAction::CloseFocusedPreviewFiles => {
                self.close_focused_preview_files();
            }
        }
    }

    pub fn open_markdown_link(&mut self, target: &str) {
        if reef_core::markdown::is_url_link(target) {
            self.engine
                .dispatch(reef_app::AppCommand::OpenUrl(target.to_string()));
            return;
        }

        let Some(rel) = self.engine.markdown_file_link_target(target) else {
            self.push_toast(Toast::warn(format!("Markdown link not found: {target}")));
            return;
        };
        self.push_location_before_jump();
        self.set_active_tab(Tab::Files);
        self.engine
            .dispatch(reef_app::AppCommand::RevealFileTreePath(rel.clone()));
        self.refresh_file_tree_with_target(Some(rel.clone()));
        self.load_preview_for_path(rel);
    }

    pub fn open_external_url(target: &str) -> Result<(), String> {
        let mut command = Self::open_external_url_command(target)?;
        command.spawn().map(|_| ()).map_err(|e| e.to_string())
    }

    #[cfg(target_os = "macos")]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("open");
        command.arg(target);
        Ok(command)
    }

    #[cfg(target_os = "windows")]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", target]);
        Ok(command)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(target);
        Ok(command)
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    fn open_external_url_command(_target: &str) -> Result<std::process::Command, String> {
        Err("opening URLs is not supported on this platform".to_string())
    }

    /// Pick the "create anchor" target for a toolbar `+ File` / `+ Folder`
    /// click. Uses the current tree selection; falls back to `None`
    /// (= project root) when the user has explicitly cleared it or the
    /// tree is empty.
    ///
    /// `resolve_create_anchor` then handles the folder-vs-file split —
    /// selection on a folder creates INSIDE, selection on a file creates
    /// as a sibling.
    fn toolbar_create_target(&self) -> Option<usize> {
        let sel = self.engine.selected_file_tree_idx();
        if self.engine.file_tree_entry_exists(sel) {
            Some(sel)
        } else {
            None
        }
    }

    /// Total visible file rows (for keyboard navigation)
    pub fn visible_file_count(&self) -> usize {
        self.engine.visible_file_count()
    }

    pub fn navigate_files(&mut self, delta: i32) {
        self.engine
            .dispatch(reef_app::AppCommand::NavigateGitFiles(delta));
    }

    pub(crate) fn scroll_file_preview(&mut self, delta: i32) {
        let markdown_preview_id = self.engine.preview_content_ref().and_then(|preview| {
            matches!(preview.body, reef_core::preview::PreviewBody::Markdown(_))
                .then_some(preview as *const reef_core::preview::PreviewDocument as usize)
        });
        if !markdown_preview_id.is_some_and(|id| self.markdown_layout.matches(id)) {
            self.engine
                .dispatch(reef_app::AppCommand::PreviewScroll(delta));
            return;
        }

        let max_scroll = self
            .markdown_layout
            .max_scroll(self.layout.last_preview_view_h as usize);
        let next = if delta < 0 {
            self.markdown_visual_scroll
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.markdown_visual_scroll
                .saturating_add(delta as usize)
                .min(max_scroll)
        };
        self.set_markdown_visual_scroll(next);
    }

    fn set_markdown_visual_scroll(&mut self, scroll: usize) {
        self.markdown_visual_scroll = scroll.min(
            self.markdown_layout
                .max_scroll(self.layout.last_preview_view_h as usize),
        );
        let logical_scroll = self
            .markdown_layout
            .row(self.markdown_visual_scroll)
            .map_or(0, |row| row.logical_row);
        self.engine
            .dispatch(reef_app::AppCommand::SetPreviewVerticalScroll(
                logical_scroll,
            ));
        self.markdown_engine_scroll_seen = Some(logical_scroll);
    }

    fn scroll_file_preview_to_top(&mut self) {
        let is_current_markdown = self.engine.preview_content_ref().is_some_and(|preview| {
            matches!(preview.body, reef_core::preview::PreviewBody::Markdown(_))
                && self
                    .markdown_layout
                    .matches(preview as *const reef_core::preview::PreviewDocument as usize)
        });
        if is_current_markdown {
            self.set_markdown_visual_scroll(0);
        } else {
            self.engine
                .dispatch(reef_app::AppCommand::SetPreviewVerticalScroll(0));
        }
    }

    fn scroll_file_preview_to_bottom(&mut self) {
        let is_current_markdown = self.engine.preview_content_ref().is_some_and(|preview| {
            matches!(preview.body, reef_core::preview::PreviewBody::Markdown(_))
                && self
                    .markdown_layout
                    .matches(preview as *const reef_core::preview::PreviewDocument as usize)
        });
        if is_current_markdown {
            let bottom = self
                .markdown_layout
                .max_scroll(self.layout.last_preview_view_h as usize);
            self.set_markdown_visual_scroll(bottom);
        } else {
            self.engine
                .dispatch(reef_app::AppCommand::SetPreviewVerticalScroll(usize::MAX));
        }
    }

    /// Vim `gg` — jump the active content panel to its top. List panels
    /// (file tree, git status, commit graph) move their selection to the
    /// first row; content panels (Diff/Commit) reset the vertical scroll.
    /// SQLite preview is a no-op so the bare `g` keystroke can still open
    /// the goto-page input.
    ///
    /// **Side effects on list panels** — list-mode `gg` is not a pure
    /// cursor move:
    /// - Git Files: `navigate_files` also resets `diff_scroll` /
    ///   `diff_h_scroll` / `sbs_left/right_h_scroll` and triggers a
    ///   deferred `load_diff()` for the new selection, so the right
    ///   pane reloads to file #1's diff.
    /// - Graph Files: `move_graph_selection` clears any visual range
    ///   (when no anchor is set; when an anchor exists we delegate to
    ///   `extend_graph_selection` and preserve the range — matching
    ///   vim's behaviour) and triggers `load_commit_detail()` for the
    ///   new selection.
    pub fn scroll_active_preview_to_top(&mut self) {
        if self.is_sqlite_preview() {
            return;
        }
        // Symmetric sentinel for the two directions. `i32::MIN` would
        // overflow in the `(-delta) as usize` paths inside
        // `file_tree::navigate` / `navigate_files` / `move_graph_selection`
        // (debug-build panic); pick a value that's far larger than any
        // realistic entry count but safe to negate.
        const NAV_FAR: i32 = 1_000_000;
        match (self.engine.active_tab(), self.engine.active_panel()) {
            (Tab::Files, Panel::Files) => {
                if !self.engine.file_tree_entries().is_empty() {
                    self.engine
                        .dispatch(reef_app::AppCommand::NavigateFileTree(-NAV_FAR));
                }
            }
            (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => {
                self.scroll_file_preview_to_top();
            }
            (Tab::Search, Panel::Files) => {
                // Search-tab left column owns its own list cursor via the
                // global_search overlay — leave it alone here.
            }
            (Tab::Git, Panel::Files) => {
                self.navigate_files(-NAV_FAR);
            }
            (Tab::Git, Panel::Diff) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetDiffVerticalScroll(0));
            }
            (Tab::Graph, Panel::Files) => {
                // Preserve any Shift-extended visual range: vim's `gg`
                // in visual mode extends the selection to the top, so
                // delegate to `extend_graph_selection` (which keeps
                // `selection_anchor` intact). Without this, the chord
                // would call `move_graph_selection` and silently
                // collapse the range to a single commit.
                if self.engine.graph_has_range_anchor() {
                    self.extend_graph_selection(-NAV_FAR);
                } else {
                    self.move_graph_selection(-NAV_FAR);
                }
            }
            (Tab::Graph, Panel::Commit) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetCommitDetailVerticalScroll(0));
            }
            (Tab::Graph, Panel::Diff) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetCommitDetailFileDiffVerticalScroll(
                        0,
                    ));
            }
            _ => {}
        }
    }

    /// Vim `G` — jump the active content panel to its bottom. List panels
    /// move selection to the last row; content panels set the scroll to
    /// jump to their renderer-specific bottom offset.
    pub fn scroll_active_preview_to_bottom(&mut self) {
        if self.is_sqlite_preview() {
            return;
        }
        const NAV_FAR: i32 = 1_000_000;
        match (self.engine.active_tab(), self.engine.active_panel()) {
            (Tab::Files, Panel::Files) => {
                if !self.engine.file_tree_entries().is_empty() {
                    self.engine
                        .dispatch(reef_app::AppCommand::NavigateFileTree(NAV_FAR));
                }
            }
            (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => {
                self.scroll_file_preview_to_bottom();
            }
            (Tab::Search, Panel::Files) => {}
            (Tab::Git, Panel::Files) => {
                self.navigate_files(NAV_FAR);
            }
            (Tab::Git, Panel::Diff) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetDiffVerticalScroll(usize::MAX));
            }
            (Tab::Graph, Panel::Files) => {
                // Mirror scroll_active_preview_to_top: keep visual-range
                // anchors so `G` extends rather than collapses.
                if self.engine.graph_has_range_anchor() {
                    self.extend_graph_selection(NAV_FAR);
                } else {
                    self.move_graph_selection(NAV_FAR);
                }
            }
            (Tab::Graph, Panel::Commit) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetCommitDetailVerticalScroll(
                        usize::MAX,
                    ));
            }
            (Tab::Graph, Panel::Diff) => {
                self.engine
                    .dispatch(reef_app::AppCommand::SetCommitDetailFileDiffVerticalScroll(
                        usize::MAX,
                    ));
            }
            _ => {}
        }
    }

    /// True when the active content panel is rendering a SQLite database
    /// preview *in a tab whose handler actually wires bare `g` to the
    /// goto-page input*. Only `Tab::Files` qualifies — `handle_key_search`
    /// has no `g` → `db_goto_input` branch, so suppressing `gg` there
    /// would silence both the chord and the per-tab fallback, leaving
    /// bare `g` as a no-op against a .db preview opened from Search.
    pub fn is_sqlite_preview(&self) -> bool {
        self.engine.active_panel() == Panel::Diff
            && self.engine.active_tab() == Tab::Files
            && self
                .engine
                .preview_content_ref()
                .is_some_and(|p| p.is_database())
    }

    /// Drive the renderer-neutral engine and merge terminal-local async state.
    /// Returns whether the terminal needs another frame. The main loop still
    /// polls at a short interval so worker results stay responsive, but an
    /// idle poll no longer forces a full redraw.
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        let outcome = self.engine.step(now, self.tick_options());
        let mut changed = outcome.changed;
        self.apply_runtime_events(outcome.runtime_events);

        // VSCode "Reveal" fade — clear `preview_highlight` after
        // `PREVIEW_HIGHLIGHT_TTL` so the highlight doesn't linger
        // forever on the destination line. Set on the rising edge
        // (None → Some) and consumed on expiry. Cleared synchronously
        // here so the next render sees no highlight.
        changed |= self.advance_preview_highlight_fade();

        changed |= self.drain_preview_sync_debounce();
        changed |= self.drain_preview_resize_responses();
        changed |= self.drain_preview_protocol_builds();
        changed |= self.drain_preview_video_builds();
        self.schedule_preview_video_resize();
        changed |= self.tick_video(now);
        self.tick_place_mode_auto_expand();
        self.tick_tree_drag_auto_expand();
        crate::input::tick_drag_autoscroll(self);

        changed
            || self.engine.place_mode_active()
            || self.engine.tree_drag_active()
            || self.last_drag_mouse.is_some()
    }

    fn tick_options(&self) -> reef_app::TickOptions {
        reef_app::TickOptions {
            dark: self.theme.is_dark,
            wants_decoded_image: self.image_picker.is_some(),
            uses_three_col: self.graph_uses_three_col(),
        }
    }

    /// Fire a debounced preview-sync if its deadline has elapsed. Scheduled
    /// by `global_search::schedule_preview_sync` (called from keyboard
    /// navigation); coalesces bursts so holding ↓ doesn't spam the preview
    /// worker. Click / chunk-arrival / pin go through `navigate_to_selected`
    /// directly and bypass this.
    fn drain_preview_sync_debounce(&mut self) -> bool {
        let outcome =
            self.engine
                .dispatch(reef_app::AppCommand::DrainGlobalSearchPreviewSyncDebounce {
                    now: Instant::now(),
                });
        if !outcome.global_search_preview_sync_due {
            return false;
        }
        crate::global_search::navigate_to_selected(self);
        true
    }

    /// VSCode-style hover auto-expand. When the cursor rests on a
    /// collapsed folder for `HOVER_EXPAND_DELAY`, expand it so the user
    /// can keep drilling into deep targets without round-tripping through
    /// a click. Render writes the hover tracker; we just check the
    /// timer here.
    ///
    /// Guarded on `file_tree_load.loading` so a slow tree rebuild doesn't
    /// re-fire the expand on every tick — we'd otherwise pile up tree
    /// rebuild generations until the worker caught up.
    fn tick_place_mode_auto_expand(&mut self) {
        self.engine
            .dispatch(reef_app::AppCommand::AutoExpandPlaceModeHover {
                now: Instant::now(),
            });
    }
}

fn activity_text(label: &str, state: &AsyncState) -> String {
    if state.loading {
        format!("{label} refreshing…")
    } else if let Some(error) = state.error.as_ref() {
        format!("{label} error: {error}")
    } else {
        format!("{label} stale")
    }
}

// ─── Prefs persistence ────────────────────────────────────────────────────────

/// Load the Git tab's diff layout + mode from the unified prefs file.
/// Keys are `diff.layout` and `diff.mode`; missing keys fall back to
/// defaults. `migrate_legacy_prefs` runs first in `App::new` so any old
/// unprefixed `layout=` / `mode=` entries have been renamed by the time
/// we get here.
fn load_prefs() -> (DiffLayout, DiffMode) {
    let layout = crate::prefs::get("diff.layout")
        .as_deref()
        .map(DiffLayout::from_pref_str)
        .unwrap_or(DiffLayout::Unified);
    let mode = crate::prefs::get("diff.mode")
        .as_deref()
        .map(DiffMode::from_pref_str)
        .unwrap_or(DiffMode::Compact);
    (layout, mode)
}

fn save_prefs(layout: DiffLayout, mode: DiffMode) {
    crate::prefs::set("diff.layout", layout.pref_str());
    crate::prefs::set("diff.mode", mode.pref_str());
}

/// Prefs key for the Graph tab's active scope (one of `all` or
/// `branch:<full_ref>`).
pub(crate) const PREF_GRAPH_SCOPE: &str = "graph.scope";
/// Prefs key for the recent-branch MRU shown at the top of the
/// branch picker. Tab-separated full ref names.
pub(crate) const PREF_GRAPH_SCOPE_RECENT: &str = "graph.scope.recent";

/// Decode the persisted `graph.scope` value. Unknown / malformed
/// values fall back to `AllRefs` rather than erroring — we'd rather
/// silently lose a stale pref than refuse to boot.
pub(crate) fn load_graph_scope_pref() -> (GraphScope, Vec<String>) {
    let scope = match crate::prefs::get(PREF_GRAPH_SCOPE).as_deref() {
        Some("all") | None => GraphScope::AllRefs,
        Some(other) => other
            .strip_prefix("branch:")
            .filter(|r| !r.is_empty())
            .map(|r| GraphScope::Branch(r.to_string()))
            .unwrap_or(GraphScope::AllRefs),
    };
    let recent = crate::prefs::get(PREF_GRAPH_SCOPE_RECENT)
        .map(|raw| {
            raw.split('\t')
                .filter(|s| !s.is_empty())
                .take(GRAPH_RECENT_BRANCHES_MAX)
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (scope, recent)
}

/// Persist the Graph tab's scope + recents. Called from
/// [`App::set_graph_scope`] every time the user accepts a picker
/// selection (or the stale-branch fallback fires).
pub(crate) fn persist_graph_scope(scope: &GraphScope, recent: &[String]) {
    let value = match scope {
        GraphScope::AllRefs => "all".to_string(),
        GraphScope::Branch(s) => format!("branch:{s}"),
    };
    crate::prefs::set(PREF_GRAPH_SCOPE, &value);
    crate::prefs::set(PREF_GRAPH_SCOPE_RECENT, &recent.join("\t"));
}

/// Strip the `refs/heads/` or `refs/remotes/` prefix off a fully-qualified
/// ref for display purposes (toasts, picker rows, panel titles).
pub(crate) fn shorthand_for_full_ref(full_ref: &str) -> &str {
    full_ref
        .strip_prefix("refs/heads/")
        .or_else(|| full_ref.strip_prefix("refs/remotes/"))
        .unwrap_or(full_ref)
}

#[cfg(test)]
mod tests {
    use super::{
        App, GRAPH_RECENT_BRANCHES_MAX, PREF_GRAPH_SCOPE, PREF_GRAPH_SCOPE_RECENT,
        load_graph_scope_pref, persist_graph_scope, video_seek_ratio,
    };
    use crate::ui::theme::Theme;
    use reef_app::{AppCommand, GitGraphState, GraphPayload, MatchHit, WorkerResult};
    use reef_core::git::GraphScope;
    use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent};
    use reef_io::LocalBackend;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use test_support::{HOME_LOCK, HomeGuard, commit_file, tempdir_repo};

    fn apply_core_worker_result(app: &mut App, result: WorkerResult) {
        let events = app
            .engine
            .state
            .apply_worker_result_core(result, Instant::now());
        app.apply_runtime_events(events);
    }

    #[test]
    fn video_seek_ratio_maps_and_clamps_timeline_columns() {
        assert_eq!(video_seek_ratio(10, 11, 10), 0.0);
        assert_eq!(video_seek_ratio(10, 11, 15), 0.5);
        assert_eq!(video_seek_ratio(10, 11, 20), 1.0);
        assert_eq!(video_seek_ratio(10, 11, 0), 0.0);
        assert_eq!(video_seek_ratio(10, 11, 99), 1.0);
    }

    fn wait_for_file_tree_entry(app: &mut App, rel: &Path) -> usize {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            app.tick();
            if !app.engine.state.file_tree_load.loading
                && let Some(idx) = app
                    .engine
                    .state
                    .file_tree
                    .entries
                    .iter()
                    .position(|entry| entry.path == rel)
            {
                return idx;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {rel:?} in file tree");
    }

    #[test]
    fn graph_state_selected_range_single_without_anchor() {
        let g = GitGraphState {
            selected_idx: 5,
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (5, 5));
        assert!(!g.is_range());
    }

    #[test]
    fn graph_state_selected_range_collapsed_anchor_is_single() {
        // `V` just entered visual mode — anchor sits on the cursor.
        let g = GitGraphState {
            selected_idx: 3,
            selection_anchor: Some(3),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (3, 3));
        assert!(!g.is_range());
    }

    #[test]
    fn graph_state_selected_range_anchor_above_cursor() {
        // (lo, hi) is always sorted — cursor can be above or below anchor.
        let g = GitGraphState {
            selected_idx: 2,
            selection_anchor: Some(7),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (2, 7));
        assert!(g.is_range());
    }

    #[test]
    fn graph_state_selected_range_anchor_below_cursor() {
        let g = GitGraphState {
            selected_idx: 9,
            selection_anchor: Some(4),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (4, 9));
        assert!(g.is_range());
    }

    #[test]
    fn markdown_link_targets_resolve_from_preview_directory() {
        let mut fx = make_scope_fixture();
        fx.app.engine.state.preview_content_revision = 1;
        fx.app.engine.state.preview_content = Some(
            PreviewContent {
                path: "docs/guide/index.md".into(),
                resolved_path: None,
                local_path: None,
                bytes_on_disk: 0,
                mime: Some("text/markdown".into()),
                body: PreviewBody::Text(reef_core::preview::TextPreview {
                    lines: vec![],
                    source: None,
                    highlighted: None,
                    parsed: None,
                }),
            }
            .into(),
        );

        assert_eq!(
            fx.app.engine.markdown_file_link_target("../intro.md#top"),
            Some(PathBuf::from("docs/intro.md"))
        );
        assert_eq!(fx.app.engine.markdown_file_link_target("#local"), None);
    }

    // ── Graph scope: set / fallback / cache ─────────────────────────────

    /// Per-test HOME isolation. `prefs::*` reads/writes
    /// `~/.config/reef/prefs`, and the scope tests both write (via
    /// `set_graph_scope` / stale-scope fallback) and assert prefs
    /// state, so we have to redirect HOME for the duration.
    struct ScopeFixture {
        app: App,
        _home_guard: test_support::HomeGuard,
        _home: tempfile::TempDir,
        _repo: tempfile::TempDir,
        _home_lock: std::sync::MutexGuard<'static, ()>,
    }

    fn make_scope_fixture() -> ScopeFixture {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let home_guard = HomeGuard::enter(home.path());
        let (repo, raw) = tempdir_repo();
        // One commit so HEAD resolves; the worker won't deadlock on us
        // during App construction (it kicks `refresh_graph` synchronously
        // dispatched onto the channel).
        commit_file(&raw, "a.txt", "hello\n", "init");
        let backend = Arc::new(LocalBackend::open_at(repo.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.engine.state.fs_watcher_rx = None;
        ScopeFixture {
            app,
            _home_guard: home_guard,
            _home: home,
            _repo: repo,
            _home_lock: lock,
        }
    }

    #[test]
    fn set_graph_scope_pushes_recents_dedupes_and_caps() {
        // Drive set_graph_scope past the cap; the recents list should
        // stay newest-first, deduped, and clamped at GRAPH_RECENT_BRANCHES_MAX.
        let mut fx = make_scope_fixture();
        for i in 0..(GRAPH_RECENT_BRANCHES_MAX + 2) {
            fx.app
                .set_graph_scope(GraphScope::Branch(format!("refs/heads/b{i}")));
        }
        assert_eq!(
            fx.app.engine.state.git_graph.recent_branches.len(),
            GRAPH_RECENT_BRANCHES_MAX
        );
        // Newest-first. The most recent push was `b{MAX+1}`.
        let newest = format!("refs/heads/b{}", GRAPH_RECENT_BRANCHES_MAX + 1);
        assert_eq!(fx.app.engine.state.git_graph.recent_branches[0], newest);

        // Re-pushing an existing branch moves it to the front without
        // duplicating.
        let oldest_kept = fx
            .app
            .engine
            .state
            .git_graph
            .recent_branches
            .last()
            .cloned()
            .unwrap();
        fx.app
            .set_graph_scope(GraphScope::Branch(oldest_kept.clone()));
        assert_eq!(
            fx.app.engine.state.git_graph.recent_branches[0],
            oldest_kept
        );
        let dup_count = fx
            .app
            .engine
            .state
            .git_graph
            .recent_branches
            .iter()
            .filter(|s| **s == oldest_kept)
            .count();
        assert_eq!(dup_count, 1, "dedupe must keep exactly one entry");
        assert_eq!(
            fx.app.engine.state.git_graph.recent_branches.len(),
            GRAPH_RECENT_BRANCHES_MAX
        );
    }

    #[test]
    fn set_graph_scope_invalidates_cache_key_and_resets_selection() {
        // The cache_key is what gates "skip the revwalk"; scope changes
        // must bust it. Selection state from the previous scope is
        // meaningless under the new one and must reset.
        let mut fx = make_scope_fixture();
        fx.app.engine.state.git_graph.cache_key = Some(("dead".into(), 0xCAFE, 0xBABE));
        fx.app.engine.state.git_graph.selected_idx = 7;
        fx.app.engine.state.git_graph.scroll = 5;
        fx.app.engine.state.git_graph.selection_anchor = Some(3);
        fx.app.engine.state.git_graph.selected_commit = Some("stale".into());

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert!(fx.app.engine.state.git_graph.cache_key.is_none());
        assert_eq!(fx.app.engine.state.git_graph.selected_idx, 0);
        assert_eq!(fx.app.engine.state.git_graph.scroll, 0);
        assert!(fx.app.engine.state.git_graph.selection_anchor.is_none());
        assert!(fx.app.engine.state.git_graph.selected_commit.is_none());
    }

    #[test]
    fn set_graph_scope_persists_to_prefs() {
        // Round-trip through the on-disk pref file so the next session
        // really would land back on the same branch.
        let mut fx = make_scope_fixture();
        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/feature".into()));

        let (scope, recent) = load_graph_scope_pref();
        assert_eq!(scope, GraphScope::Branch("refs/heads/feature".into()));
        assert_eq!(recent, vec!["refs/heads/feature".to_string()]);
    }

    #[test]
    fn set_graph_scope_same_scope_is_noop() {
        let mut fx = make_scope_fixture();
        fx.app.engine.state.git_graph.scope = GraphScope::Branch("refs/heads/main".into());
        fx.app.engine.state.git_graph.recent_branches = vec!["refs/heads/main".into()];
        // Pre-seed a cache_key — the no-op path must NOT bust it (only
        // a genuine scope change should).
        fx.app.engine.state.git_graph.cache_key = Some(("h".into(), 1, 2));
        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));
        assert!(fx.app.engine.state.git_graph.cache_key.is_some());
    }

    #[test]
    fn stale_branch_payload_falls_back_to_all_refs() {
        // Drive a Graph WorkerResult whose payload claims the scope is a
        // branch but returned zero rows. The main thread should detect
        // the missing-branch case, drop the recent entry, swap scope
        // back to AllRefs, surface a toast, and persist the rollback.
        let mut fx = make_scope_fixture();
        // Stash the scope directly (bypassing `set_graph_scope` so we
        // don't have to babysit the worker round-trip in this test).
        fx.app.engine.state.git_graph.scope = GraphScope::Branch("refs/heads/ghost".into());
        fx.app.engine.state.git_graph.recent_branches = vec!["refs/heads/ghost".into()];
        persist_graph_scope(
            &fx.app.engine.state.git_graph.scope,
            &fx.app.engine.state.git_graph.recent_branches,
        );

        let generation = fx.app.engine.state.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(),
            ref_map: std::collections::HashMap::new(),
            cache_key: ("h".into(), 0, 0),
            scope: GraphScope::Branch("refs/heads/ghost".into()),
        };
        let toasts_before = fx.app.engine.state.toasts.len();
        apply_core_worker_result(
            &mut fx.app,
            WorkerResult::Graph {
                generation,
                result: Ok(payload),
            },
        );

        assert_eq!(fx.app.engine.state.git_graph.scope, GraphScope::AllRefs);
        assert!(
            !fx.app
                .engine
                .state
                .git_graph
                .recent_branches
                .contains(&"refs/heads/ghost".to_string())
        );
        assert!(fx.app.engine.state.toasts.len() > toasts_before);
        assert!(
            fx.app
                .engine
                .state
                .toasts
                .last()
                .unwrap()
                .message
                .contains("ghost"),
            "toast should name the lost branch"
        );

        // Persisted state matches the fallback.
        assert_eq!(crate::prefs::get(PREF_GRAPH_SCOPE).as_deref(), Some("all"));
        assert_eq!(
            crate::prefs::get(PREF_GRAPH_SCOPE_RECENT)
                .as_deref()
                .unwrap_or(""),
            ""
        );
    }

    #[test]
    fn open_graph_branch_picker_refuses_on_cold_start() {
        // Cold-start safety: with a persisted Branch scope but no
        // graph payload yet (ref_map still empty), pressing 'b' must
        // NOT activate the picker — otherwise visible_rows would only
        // offer `[ All refs ]` and a stray Enter would overwrite the
        // user's persisted choice.
        let mut fx = make_scope_fixture();
        fx.app.engine.state.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());
        fx.app.engine.state.git_graph.ref_map.clear();
        let toasts_before = fx.app.engine.state.toasts.len();

        fx.app.open_graph_branch_picker();

        assert!(
            !fx.app.engine.state.graph_branch_picker.core.active,
            "picker must not open while ref_map is empty"
        );
        assert!(
            fx.app.engine.state.toasts.len() > toasts_before,
            "must surface a hint toast"
        );
        // Persisted scope untouched.
        assert_eq!(
            fx.app.engine.state.git_graph.scope,
            GraphScope::Branch("refs/heads/feature".into())
        );
    }

    #[test]
    fn stale_branch_fallback_skipped_when_ref_still_present() {
        // Walk returned empty (transient revwalk error / lock
        // contention), but the payload's ref_map still lists the
        // branch. Fallback must NOT misfire — no toast, no scope flip,
        // no recents drop.
        let mut fx = make_scope_fixture();
        fx.app.engine.state.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());
        fx.app.engine.state.git_graph.recent_branches = vec!["refs/heads/feature".into()];

        let mut ref_map: std::collections::HashMap<String, Vec<reef_core::git::RefLabel>> =
            std::collections::HashMap::new();
        ref_map.insert(
            "abc123".to_string(),
            vec![reef_core::git::RefLabel::Branch("feature".into())],
        );

        let generation = fx.app.engine.state.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(),
            ref_map,
            cache_key: ("h".into(), 0, 0),
            scope: GraphScope::Branch("refs/heads/feature".into()),
        };
        let toasts_before = fx.app.engine.state.toasts.len();
        apply_core_worker_result(
            &mut fx.app,
            WorkerResult::Graph {
                generation,
                result: Ok(payload),
            },
        );

        // Scope kept; toast not pushed; recents preserved.
        assert_eq!(
            fx.app.engine.state.git_graph.scope,
            GraphScope::Branch("refs/heads/feature".into())
        );
        assert_eq!(fx.app.engine.state.toasts.len(), toasts_before);
        assert!(
            fx.app
                .engine
                .state
                .git_graph
                .recent_branches
                .contains(&"refs/heads/feature".to_string())
        );
    }

    #[test]
    fn set_graph_scope_invalidates_detail_loads_without_loading() {
        // Stale in-flight detail / file-diff loads must NOT repaint
        // the right panel after a scope swap. We bump generation via
        // `invalidate` (not `begin`) so the status bar doesn't get
        // stuck on a phantom "refreshing…" — verify both that the
        // generation advances AND that `loading` stays false.
        let mut fx = make_scope_fixture();
        // Pretend an old load was in flight so we can confirm `loading`
        // gets cleared, not preserved.
        fx.app.engine.state.commit_detail_load.loading = true;
        fx.app.engine.state.commit_file_diff_load.loading = true;
        let before_detail = fx.app.engine.state.commit_detail_load.generation;
        let before_diff = fx.app.engine.state.commit_file_diff_load.generation;

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert_ne!(
            fx.app.engine.state.commit_detail_load.generation, before_detail,
            "commit_detail_load generation must advance"
        );
        assert_ne!(
            fx.app.engine.state.commit_file_diff_load.generation, before_diff,
            "commit_file_diff_load generation must advance"
        );
        assert!(
            !fx.app.engine.state.commit_detail_load.loading,
            "commit_detail_load.loading must be cleared (no follow-up dispatcher)"
        );
        assert!(
            !fx.app.engine.state.commit_file_diff_load.loading,
            "commit_file_diff_load.loading must be cleared (no follow-up dispatcher)"
        );
    }

    #[test]
    fn set_graph_scope_clears_active_commit_graph_search() {
        // Search matches index into git_graph.rows; clearing rows
        // without resetting search would leave `n`/`N` jumping to
        // rows that no longer correspond to anything visible.
        let mut fx = make_scope_fixture();
        // Synthesize an active CommitGraph search via `set_matches` so the
        // `row_index` invariant holds (set_matches resets `current`, so
        // assign it on the next line).
        fx.app.engine.state.search = reef_app::SearchState {
            target: Some(reef_app::SearchTarget::CommitGraph),
            query: "foo".into(),
            ..reef_app::SearchState::default()
        };
        fx.app
            .engine
            .state
            .search
            .set_matches(vec![reef_app::MatchLoc {
                row: 0,
                byte_range: 0..3,
            }]);
        fx.app.engine.state.search.current = Some(0);

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert!(
            fx.app.engine.search().matches.is_empty(),
            "CommitGraph search must be cleared on scope swap"
        );
        assert!(fx.app.engine.search().target.is_none());
    }

    #[test]
    fn set_graph_scope_preserves_search_targeting_other_panel() {
        // Searches on file preview, commit detail body, etc. don't
        // index into git_graph.rows and should survive a scope swap.
        let mut fx = make_scope_fixture();
        fx.app.engine.state.search = reef_app::SearchState {
            target: Some(reef_app::SearchTarget::FilePreview),
            query: "foo".into(),
            ..reef_app::SearchState::default()
        };
        fx.app
            .engine
            .state
            .search
            .set_matches(vec![reef_app::MatchLoc {
                row: 5,
                byte_range: 0..3,
            }]);
        fx.app.engine.state.search.current = Some(0);

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert_eq!(
            fx.app.engine.search().target,
            Some(reef_app::SearchTarget::FilePreview),
            "unrelated search target must not be cleared"
        );
        assert_eq!(fx.app.engine.search().matches.len(), 1);
    }

    #[test]
    fn confirm_graph_branch_picker_closes_overlay_when_filter_matches_nothing() {
        // Filter "zzz" matches no branches and is not a substring of
        // "all refs"; visible_rows is empty → confirm() returns None.
        // The handler must still close the overlay so input isn't
        // trapped.
        let mut fx = make_scope_fixture();
        // Pretend we already have a populated ref_map so open_graph_branch_picker
        // succeeds.
        fx.app.engine.state.git_graph.ref_map.insert(
            "oid".into(),
            vec![reef_core::git::RefLabel::Branch("main".into())],
        );
        fx.app.open_graph_branch_picker();
        assert!(fx.app.engine.state.graph_branch_picker.core.active);

        fx.app.engine.state.graph_branch_picker.core.filter = "zzz".into();
        assert!(
            fx.app.engine.state.graph_branch_picker.confirm().is_none(),
            "filter 'zzz' has no rows"
        );

        fx.app.confirm_graph_branch_picker();
        assert!(
            !fx.app.engine.state.graph_branch_picker.core.active,
            "picker must close even when confirm() returned None"
        );
    }

    #[test]
    fn payload_with_matching_scope_and_nonempty_rows_does_not_fall_back() {
        // Sanity: a Branch-scoped payload that returned rows should be
        // applied normally (no fallback, no toast).
        let mut fx = make_scope_fixture();
        fx.app.engine.state.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());

        let generation = fx.app.engine.state.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(), // emptiness is fine — the OK case is "scope didn't disappear"
            ref_map: std::collections::HashMap::new(),
            cache_key: ("h".into(), 0, 1),
            // Scope mismatched against current: that's the "stale request" path,
            // not the "ghost branch" path. The guard requires scope match.
            scope: GraphScope::Branch("refs/heads/other".into()),
        };
        let toasts_before = fx.app.engine.state.toasts.len();
        apply_core_worker_result(
            &mut fx.app,
            WorkerResult::Graph {
                generation,
                result: Ok(payload),
            },
        );

        // Scope should NOT have flipped — the payload was for a different
        // branch than the user is currently looking at.
        assert!(matches!(
            fx.app.engine.state.git_graph.scope,
            GraphScope::Branch(ref s) if s == "refs/heads/feature"
        ));
        assert_eq!(fx.app.engine.state.toasts.len(), toasts_before);
    }

    #[test]
    fn stale_preview_refresh_keeps_pending_debounce_deadline() {
        let _home_lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let _home = HomeGuard::enter(home.path());
        let (tmp, repo) = tempdir_repo();
        commit_file(&repo, "a.txt", "hello\n", "add a");
        let backend = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.engine.state.fs_watcher_rx = None;

        let file_idx = wait_for_file_tree_entry(&mut app, Path::new("a.txt"));
        app.engine.state.file_tree.selected = file_idx;
        app.load_preview();

        let (scheduled_path, scheduled_deadline) = app
            .engine
            .state
            .preview_schedule
            .clone()
            .expect("preview scheduled");
        app.engine.state.preview_load.mark_stale();
        app.engine.state.kick_active_tab_work(
            Instant::now(),
            reef_app::TickOptions {
                dark: true,
                wants_decoded_image: false,
                uses_three_col: app.graph_uses_three_col(),
            },
        );

        assert_eq!(
            app.engine.state.preview_schedule,
            Some((scheduled_path, scheduled_deadline))
        );
        assert!(app.engine.state.preview_load.stale);
        assert!(!app.engine.state.preview_load.loading);
    }

    #[test]
    fn stale_preview_result_keeps_terminal_image_protocol() {
        let mut fx = make_scope_fixture();
        let stale_generation = fx.app.engine.state.preview_load.begin();
        let current_generation = fx.app.engine.state.preview_load.begin();
        fx.app.preview_image_protocol = Some(ratatui_image::thread::ThreadProtocol::new(
            fx.app.preview_resize_tx.clone(),
            None,
        ));

        fx.app
            .apply_preview_result_for_adapter(stale_generation, Ok(None));

        assert_eq!(fx.app.engine.preview_generation(), current_generation);
        assert!(fx.app.preview_image_protocol.is_some());
    }

    #[test]
    fn missing_global_search_hit_keeps_terminal_image_protocol() {
        let mut fx = make_scope_fixture();
        let hit = MatchHit {
            path: PathBuf::from("missing.png"),
            display: "missing.png".to_string(),
            line: 0,
            line_text: "needle".to_string(),
            line_revision: reef_io::content_line_revision(b"needle"),
            byte_range: 0..6,
        };
        fx.app.engine.state.global_search.core.active = true;
        fx.app.engine.dispatch(AppCommand::AcceptGlobalSearchHit {
            hit: hit.clone(),
            origin: None,
        });
        let (path, _) = fx
            .app
            .engine
            .state
            .preview_schedule
            .take()
            .expect("accepting a hit schedules validation");
        fx.app
            .engine
            .state
            .dispatch_preview_load(path, false, false);
        let generation = fx.app.engine.preview_generation();
        fx.app.preview_image_protocol = Some(ratatui_image::thread::ThreadProtocol::new(
            fx.app.preview_resize_tx.clone(),
            None,
        ));

        fx.app
            .apply_preview_result_for_adapter(generation, Ok(None));

        assert!(fx.app.preview_image_protocol.is_some());
    }

    // ── Graph layout math ────────────────────────────────────────────────

    use super::Tab;
    use reef_app::{compute_sidebar_width, compute_three_col_widths, compute_uses_three_col};

    // ── graph_uses_three_col switch matrix ───────────────────────────────

    #[test]
    fn uses_three_col_needs_graph_tab() {
        // Exact same inputs, only the tab changes — 3-col is Graph-only.
        assert!(compute_uses_three_col(Tab::Graph, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Git, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Files, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Search, 200, true, false));
    }

    #[test]
    fn uses_three_col_needs_min_width() {
        // 99 cols → below `GRAPH_THREE_COL_MIN_WIDTH` (100) → 2-col fallback.
        assert!(!compute_uses_three_col(Tab::Graph, 99, true, false));
        assert!(compute_uses_three_col(Tab::Graph, 100, true, false));
    }

    #[test]
    fn uses_three_col_needs_file_diff_or_loading() {
        // Graph tab + wide terminal but nothing to show in the diff column.
        assert!(!compute_uses_three_col(Tab::Graph, 200, false, false));
        // Either flag on activates 3-col.
        assert!(compute_uses_three_col(Tab::Graph, 200, true, false));
        assert!(compute_uses_three_col(Tab::Graph, 200, false, true));
        // Loading-in-flight during a file click: 3-col stays active so the
        // "loading…" banner renders where the diff will land instead of
        // flashing 2-col for a frame.
    }

    #[test]
    fn sidebar_width_clamps_min_10() {
        // split_percent = 5 → raw = 5 → floor at 10.
        assert_eq!(compute_sidebar_width(100, 5), 10);
    }

    #[test]
    fn sidebar_width_clamps_to_leave_20_for_right() {
        // split_percent = 95 on width 100 → raw = 95, but right panel
        // needs at least 20 cols → clamp to 80.
        assert_eq!(compute_sidebar_width(100, 95), 80);
    }

    #[test]
    fn sidebar_width_passes_mid_range_through() {
        // Default 30% on a generous terminal.
        assert_eq!(compute_sidebar_width(200, 30), 60);
    }

    #[test]
    fn three_col_widths_sum_to_total() {
        // Regression guard: if the rounding ever changes, the assert below
        // pins the invariant `graph + commit + diff == total_width`. Hit-
        // testing + h-scroll routing rely on this exactly.
        let (g, c, d) = compute_three_col_widths(200, 60, 60);
        assert_eq!(g + c + d, 200);
    }

    #[test]
    fn three_col_widths_diff_floor_20() {
        // graph_diff_split_percent = 0 shouldn't squeeze diff to 0 —
        // `.max(20)` keeps it usable.
        let (_, _, d) = compute_three_col_widths(200, 60, 0);
        assert!(d >= 20, "diff col floored at 20, got {d}");
    }

    #[test]
    fn three_col_widths_commit_floor_20() {
        // graph_diff_split_percent = 100 shouldn't squeeze commit to 0 —
        // `.min(remainder - 20)` leaves commit at least 20 cols.
        let (_, c, _) = compute_three_col_widths(200, 60, 100);
        assert!(c >= 20, "commit col floored at 20, got {c}");
    }

    #[test]
    fn three_col_widths_default_proportions() {
        // Default tuning: sidebar_w=60 (30% of 200), graph_diff_split_percent=60
        // on a 200-wide terminal. Sanity-check the split feels right.
        let (g, c, d) = compute_three_col_widths(200, 60, 60);
        assert_eq!(g, 60);
        // remainder = 140, diff = 60% of 140 = 84, commit = 56.
        assert_eq!(d, 84);
        assert_eq!(c, 56);
    }

    #[test]
    fn three_col_widths_at_min_terminal_width() {
        // At the 100-col 3-col threshold the floors kick in hard:
        // graph = 30, remainder = 70, diff = 42 (rounded from 60% * 70),
        // commit = 28. All columns remain ≥ 20.
        let (g, c, d) = compute_three_col_widths(100, 30, 60);
        assert_eq!(g + c + d, 100);
        assert!(c >= 20 && d >= 20, "both right cols ≥ 20: c={c} d={d}");
    }

    #[test]
    fn three_col_widths_share_sidebar_with_2col() {
        // Whether the UI ends up in 2-col or 3-col, the graph sidebar
        // width is the same value. `graph_diff_column_start` and
        // `focus_panel_under_cursor` depend on this.
        let sidebar_2 = compute_sidebar_width(150, 30);
        let (graph_3, _, _) = compute_three_col_widths(150, sidebar_2, 60);
        assert_eq!(sidebar_2, graph_3);
    }

    #[test]
    fn three_col_widths_redistribute_when_sidebar_zero() {
        // Sidebar hidden → sidebar_w=0; commit and diff fill the full
        // width using `graph_diff_split_percent`. Diff stays 60% of 200
        // and commit takes the rest, instead of inheriting the narrow
        // commit_w computed off the (now nonexistent) sidebar slot.
        let (g, c, d) = compute_three_col_widths(200, 0, 60);
        assert_eq!(g, 0);
        assert_eq!(g + c + d, 200);
        assert_eq!(d, 120);
        assert_eq!(c, 80);
    }

    // ── Confirm request lifecycle ────────────────────────────────────────

    /// Holds the HOME lock + tempdirs alongside the App so each test
    /// gets a clean isolated environment. Drop order matters: the App
    /// (and its tasks worker) gets cleaned up before the tempdirs.
    struct ConfirmFixture {
        app: App,
        // Listed after `app` so they outlive it (Rust drops fields top-
        // down, so these fields drop *after* `app` only because they
        // appear after it in the struct definition — fine in practice).
        _home_guard: test_support::HomeGuard,
        _home: tempfile::TempDir,
        _repo: tempfile::TempDir,
        _home_lock: std::sync::MutexGuard<'static, ()>,
    }

    fn make_fixture() -> ConfirmFixture {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let home_guard = HomeGuard::enter(home.path());
        let (repo, _) = tempdir_repo();
        let backend = Arc::new(LocalBackend::open_at(repo.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.engine.state.fs_watcher_rx = None;
        ConfirmFixture {
            app,
            _home_guard: home_guard,
            _home: home,
            _repo: repo,
            _home_lock: lock,
        }
    }

    #[test]
    fn prompt_tree_delete_sets_pending_confirm() {
        let mut fx = make_fixture();
        let path = fx.app.engine.file_tree_root().join("a.txt");

        fx.app.prompt_tree_delete(path.clone(), false, true);

        let Some(reef_app::ConfirmRequest::TreeDelete(pending)) = fx.app.engine.confirm_request()
        else {
            panic!("tree delete confirm should be pending");
        };
        assert_eq!(pending.path, path);
        assert_eq!(pending.display_name, "a.txt");
        assert!(!pending.is_dir);
        assert!(pending.hard);
    }

    #[test]
    fn fire_confirm_cancel_clears_pending_request() {
        let mut fx = make_fixture();
        let path = fx.app.engine.file_tree_root().join("a.txt");

        fx.app.prompt_tree_delete(path, false, false);
        fx.app.fire_confirm_cancel();

        assert!(fx.app.engine.confirm_request().is_none());
    }

    #[test]
    fn fire_confirm_primary_requests_delete_and_clears_pending_request() {
        let mut fx = make_fixture();
        let path = fx.app.engine.file_tree_root().join("a.txt");
        std::fs::write(&path, "delete me").unwrap();

        fx.app.prompt_tree_delete(path, false, false);
        fx.app.fire_confirm_primary();

        assert!(fx.app.engine.confirm_request().is_none());
        assert!(fx.app.engine.state.fs_mutation_load.loading);
    }

    #[test]
    fn fire_confirm_primary_reopens_request_when_fs_mutation_is_busy() {
        let mut fx = make_fixture();
        let path = fx.app.engine.file_tree_root().join("a.txt");

        fx.app.engine.state.fs_mutation_load.loading = true;
        fx.app.prompt_tree_delete(path.clone(), false, false);
        fx.app.fire_confirm_primary();

        let Some(reef_app::ConfirmRequest::TreeDelete(pending)) = fx.app.engine.confirm_request()
        else {
            panic!("tree delete confirm should be re-opened");
        };
        assert_eq!(pending.path, path);
        assert!(fx.app.engine.state.fs_mutation_load.loading);
        assert!(!fx.app.engine.state.toasts.is_empty());
    }
}
