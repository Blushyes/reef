use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputScope {
    Normal,
    FocusedPreview,
    Settings,
    QuickOpen,
    GlobalSearch,
    FindWidget,
    VimSearch,
    HostsPicker,
    GraphBranchPicker,
    TreeEdit,
    TreeContextMenu,
    NavCandidates,
    ConfirmModal,
    PasteConflict,
    DbGoto,
    PlaceMode,
    TreeDrag,
    CommitEditor,
    SearchInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    Quit,
    OpenHelp,
    ToggleSidebar,
    OpenSettings,
    OpenHostsPicker,
    OpenQuickOpen,
    OpenFindWidget,
    OpenGlobalSearch,
    OpenGlobalReplace,
    ToggleFocusedPreview,
    LocationBack,
    LocationForward,
    GotoDefinition,
    FindReferences,
    ScrollTop,
    ScrollBottom,
    BeginSearchForward,
    BeginSearchBackward,
    NextSearchMatch,
    PrevSearchMatch,
    SwitchTab(usize),
    CyclePanelForward,
    CyclePanelBackward,
    PinGlobalSearch,
    Close,
    Confirm,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    TogglePickerPathMode,
    ToggleCase,
    ToggleWholeWord,
    ToggleRegex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyBinding {
    scope: InputScope,
    key: KeyCode,
    modifiers: KeyModifiers,
    chord: Option<KeyCode>,
    command: Command,
    allow_shadow: bool,
}

impl KeyBinding {
    const fn single(
        scope: InputScope,
        key: KeyCode,
        modifiers: KeyModifiers,
        command: Command,
    ) -> Self {
        Self {
            scope,
            key,
            modifiers,
            chord: None,
            command,
            allow_shadow: false,
        }
    }

    const fn chord(
        scope: InputScope,
        leader: KeyCode,
        chord: KeyCode,
        modifiers: KeyModifiers,
        command: Command,
    ) -> Self {
        Self {
            scope,
            key: leader,
            modifiers,
            chord: Some(chord),
            command,
            allow_shadow: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BindingKey {
    Single {
        scope: InputScope,
        key: KeyCode,
        modifiers: KeyModifiers,
    },
    Chord {
        scope: InputScope,
        leader: KeyCode,
        chord: KeyCode,
        modifiers: KeyModifiers,
    },
}

impl From<&KeyBinding> for BindingKey {
    fn from(binding: &KeyBinding) -> Self {
        match binding.chord {
            Some(chord) => Self::Chord {
                scope: binding.scope,
                leader: binding.key,
                chord,
                modifiers: binding.modifiers,
            },
            None => Self::Single {
                scope: binding.scope,
                key: binding.key,
                modifiers: binding.modifiers,
            },
        }
    }
}

pub struct Keymap;

impl Keymap {
    pub fn resolve(scope: InputScope, key: &KeyEvent) -> Option<Command> {
        bindings()
            .iter()
            .find(|binding| {
                binding.scope == scope
                    && binding.chord.is_none()
                    && binding.key == key.code
                    && binding.modifiers == key.modifiers
            })
            .map(|binding| binding.command)
    }

    pub fn resolve_chord(scope: InputScope, leader: KeyCode, key: &KeyEvent) -> Option<Command> {
        bindings()
            .iter()
            .find(|binding| {
                binding.scope == scope
                    && binding.key == leader
                    && binding.chord == Some(key.code)
                    && binding.modifiers == key.modifiers
            })
            .map(|binding| binding.command)
    }

    pub fn validate() -> Result<(), String> {
        let mut seen: HashMap<BindingKey, Command> = HashMap::new();
        for binding in bindings() {
            let key = BindingKey::from(binding);
            if let Some(previous) = seen.insert(key, binding.command)
                && !binding.allow_shadow
            {
                return Err(format!(
                    "duplicate key binding for {:?}: {:?} and {:?}",
                    key, previous, binding.command
                ));
            }
        }
        Ok(())
    }
}

pub fn scope_for_app(app: &crate::TuiApp) -> InputScope {
    let snapshot = app.engine.snapshot();
    if snapshot.overlays.db_goto {
        InputScope::DbGoto
    } else if snapshot.overlays.hosts_picker {
        InputScope::HostsPicker
    } else if snapshot.overlays.graph_branch_picker {
        InputScope::GraphBranchPicker
    } else if app.engine.find_widget().active {
        InputScope::FindWidget
    } else if snapshot.overlays.global_search {
        InputScope::GlobalSearch
    } else if snapshot.overlays.quick_open {
        InputScope::QuickOpen
    } else if app.engine.search().active {
        InputScope::VimSearch
    } else if snapshot.overlays.tree_edit {
        InputScope::TreeEdit
    } else if snapshot.overlays.tree_context_menu {
        InputScope::TreeContextMenu
    } else if snapshot.overlays.nav_candidates {
        InputScope::NavCandidates
    } else if snapshot.overlays.confirm.is_some() {
        InputScope::ConfirmModal
    } else if snapshot.overlays.paste_conflict {
        InputScope::PasteConflict
    } else if snapshot.overlays.place_mode {
        InputScope::PlaceMode
    } else if snapshot.overlays.tree_drag {
        InputScope::TreeDrag
    } else if snapshot.view_mode == reef_app::ViewMode::Settings {
        InputScope::Settings
    } else if snapshot.view_mode == reef_app::ViewMode::FocusedPreview {
        InputScope::FocusedPreview
    } else if snapshot.overlays.commit_editor {
        InputScope::CommitEditor
    } else if snapshot.overlays.search_input {
        InputScope::SearchInput
    } else {
        InputScope::Normal
    }
}

fn bindings() -> &'static [KeyBinding] {
    use Command::*;
    use InputScope::*;
    use KeyCode::*;
    use KeyModifiers as Mods;

    static BINDINGS: OnceLock<Vec<KeyBinding>> = OnceLock::new();
    BINDINGS
        .get_or_init(|| {
            vec![
                KeyBinding::single(Normal, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(Normal, Char('q'), Mods::NONE, Quit),
                KeyBinding::single(Normal, Char('h'), Mods::NONE, OpenHelp),
                KeyBinding::single(Normal, Char('b'), Mods::CONTROL, ToggleSidebar),
                KeyBinding::single(Normal, Char(','), Mods::CONTROL, OpenSettings),
                KeyBinding::single(Normal, Char('o'), Mods::CONTROL, OpenHostsPicker),
                KeyBinding::single(Normal, Left, Mods::ALT, LocationBack),
                KeyBinding::single(Normal, Right, Mods::ALT, LocationForward),
                KeyBinding::single(Normal, Left, Mods::ALT | Mods::CONTROL, LocationBack),
                KeyBinding::single(Normal, Right, Mods::ALT | Mods::CONTROL, LocationForward),
                KeyBinding::single(Normal, Char('/'), Mods::NONE, BeginSearchForward),
                KeyBinding::single(Normal, Char('?'), Mods::NONE, BeginSearchBackward),
                KeyBinding::single(Normal, Char('n'), Mods::NONE, NextSearchMatch),
                KeyBinding::single(Normal, Char('N'), Mods::NONE, PrevSearchMatch),
                KeyBinding::single(Normal, Char('N'), Mods::SHIFT, PrevSearchMatch),
                KeyBinding::single(Normal, Tab, Mods::NONE, CyclePanelForward),
                KeyBinding::single(Normal, BackTab, Mods::SHIFT, CyclePanelBackward),
                KeyBinding::single(Normal, Tab, Mods::CONTROL, SwitchTab(usize::MAX)),
                KeyBinding::single(Normal, Char('1'), Mods::NONE, SwitchTab(0)),
                KeyBinding::single(Normal, Char('2'), Mods::NONE, SwitchTab(1)),
                KeyBinding::single(Normal, Char('3'), Mods::NONE, SwitchTab(2)),
                KeyBinding::single(Normal, Char('4'), Mods::NONE, SwitchTab(3)),
                KeyBinding::chord(Normal, Char(' '), Char('p'), Mods::NONE, OpenQuickOpen),
                KeyBinding::chord(Normal, Char(' '), Char('P'), Mods::NONE, OpenQuickOpen),
                KeyBinding::chord(Normal, Char(' '), Char('P'), Mods::SHIFT, OpenQuickOpen),
                KeyBinding::chord(Normal, Char(' '), Char('f'), Mods::NONE, OpenFindWidget),
                KeyBinding::chord(Normal, Char(' '), Char('F'), Mods::NONE, OpenGlobalSearch),
                KeyBinding::chord(Normal, Char(' '), Char('F'), Mods::SHIFT, OpenGlobalSearch),
                KeyBinding::chord(Normal, Char(' '), Char('h'), Mods::NONE, OpenGlobalReplace),
                KeyBinding::chord(Normal, Char(' '), Char('H'), Mods::NONE, OpenGlobalReplace),
                KeyBinding::chord(Normal, Char(' '), Char('H'), Mods::SHIFT, OpenGlobalReplace),
                KeyBinding::chord(
                    Normal,
                    Char(' '),
                    Char('v'),
                    Mods::NONE,
                    ToggleFocusedPreview,
                ),
                KeyBinding::chord(
                    Normal,
                    Char(' '),
                    Char('V'),
                    Mods::SHIFT,
                    ToggleFocusedPreview,
                ),
                KeyBinding::chord(
                    Normal,
                    Char(' '),
                    Char('V'),
                    Mods::NONE,
                    ToggleFocusedPreview,
                ),
                KeyBinding::chord(Normal, Char('g'), Char('g'), Mods::NONE, ScrollTop),
                KeyBinding::single(Normal, Char('G'), Mods::NONE, ScrollBottom),
                KeyBinding::single(Normal, Char('G'), Mods::SHIFT, ScrollBottom),
                KeyBinding::chord(Normal, Char('g'), Char('d'), Mods::NONE, GotoDefinition),
                KeyBinding::chord(Normal, Char('g'), Char('r'), Mods::NONE, FindReferences),
                KeyBinding::single(FocusedPreview, Esc, Mods::NONE, Close),
                KeyBinding::single(FocusedPreview, Char('q'), Mods::NONE, Quit),
                KeyBinding::single(FocusedPreview, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(Settings, Esc, Mods::NONE, Close),
                KeyBinding::single(Settings, Char('q'), Mods::NONE, Quit),
                KeyBinding::single(Settings, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(Settings, Up, Mods::NONE, MoveUp),
                KeyBinding::single(Settings, Char('k'), Mods::NONE, MoveUp),
                KeyBinding::single(Settings, Char('p'), Mods::CONTROL, MoveUp),
                KeyBinding::single(Settings, Down, Mods::NONE, MoveDown),
                KeyBinding::single(Settings, Char('j'), Mods::NONE, MoveDown),
                KeyBinding::single(Settings, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(Settings, KeyCode::PageUp, Mods::NONE, ScrollTop),
                KeyBinding::single(Settings, Home, Mods::NONE, ScrollTop),
                KeyBinding::single(Settings, KeyCode::PageDown, Mods::NONE, ScrollBottom),
                KeyBinding::single(Settings, End, Mods::NONE, ScrollBottom),
                KeyBinding::single(Settings, Enter, Mods::NONE, Confirm),
                KeyBinding::single(Settings, Char(' '), Mods::NONE, Confirm),
                KeyBinding::single(QuickOpen, Esc, Mods::NONE, Close),
                KeyBinding::single(QuickOpen, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(QuickOpen, Enter, Mods::NONE, Confirm),
                KeyBinding::single(QuickOpen, Up, Mods::NONE, MoveUp),
                KeyBinding::single(QuickOpen, Char('k'), Mods::CONTROL, MoveUp),
                KeyBinding::single(QuickOpen, Char('p'), Mods::CONTROL, MoveUp),
                KeyBinding::single(QuickOpen, Down, Mods::NONE, MoveDown),
                KeyBinding::single(QuickOpen, Char('j'), Mods::CONTROL, MoveDown),
                KeyBinding::single(QuickOpen, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(QuickOpen, KeyCode::PageUp, Mods::NONE, Command::PageUp),
                KeyBinding::single(QuickOpen, KeyCode::PageDown, Mods::NONE, Command::PageDown),
                KeyBinding::single(GlobalSearch, Esc, Mods::NONE, Close),
                KeyBinding::single(GlobalSearch, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(GlobalSearch, Enter, Mods::NONE, Confirm),
                KeyBinding::single(GlobalSearch, Enter, Mods::ALT, PinGlobalSearch),
                KeyBinding::single(GlobalSearch, Enter, Mods::CONTROL, PinGlobalSearch),
                KeyBinding::single(
                    GlobalSearch,
                    Enter,
                    Mods::CONTROL | Mods::ALT,
                    PinGlobalSearch,
                ),
                KeyBinding::single(GlobalSearch, Up, Mods::NONE, MoveUp),
                KeyBinding::single(GlobalSearch, Char('k'), Mods::CONTROL, MoveUp),
                KeyBinding::single(GlobalSearch, Char('p'), Mods::CONTROL, MoveUp),
                KeyBinding::single(GlobalSearch, Down, Mods::NONE, MoveDown),
                KeyBinding::single(GlobalSearch, Char('j'), Mods::CONTROL, MoveDown),
                KeyBinding::single(GlobalSearch, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(GlobalSearch, KeyCode::PageUp, Mods::NONE, Command::PageUp),
                KeyBinding::single(
                    GlobalSearch,
                    KeyCode::PageDown,
                    Mods::NONE,
                    Command::PageDown,
                ),
                KeyBinding::single(FindWidget, Esc, Mods::NONE, Close),
                KeyBinding::single(FindWidget, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(FindWidget, Enter, Mods::NONE, Confirm),
                KeyBinding::single(FindWidget, Enter, Mods::SHIFT, Confirm),
                KeyBinding::single(FindWidget, Up, Mods::NONE, MoveUp),
                KeyBinding::single(FindWidget, Down, Mods::NONE, MoveDown),
                KeyBinding::single(FindWidget, Char('c'), Mods::ALT, ToggleCase),
                KeyBinding::single(FindWidget, Char('C'), Mods::ALT | Mods::SHIFT, ToggleCase),
                KeyBinding::single(FindWidget, Char('w'), Mods::ALT, ToggleWholeWord),
                KeyBinding::single(
                    FindWidget,
                    Char('W'),
                    Mods::ALT | Mods::SHIFT,
                    ToggleWholeWord,
                ),
                KeyBinding::single(FindWidget, Char('r'), Mods::ALT, ToggleRegex),
                KeyBinding::single(FindWidget, Char('R'), Mods::ALT | Mods::SHIFT, ToggleRegex),
                KeyBinding::single(VimSearch, Esc, Mods::NONE, Close),
                KeyBinding::single(VimSearch, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(VimSearch, Enter, Mods::NONE, Confirm),
                KeyBinding::single(TreeEdit, Esc, Mods::NONE, Close),
                KeyBinding::single(TreeEdit, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(TreeEdit, Enter, Mods::NONE, Confirm),
                KeyBinding::single(TreeContextMenu, Esc, Mods::NONE, Close),
                KeyBinding::single(TreeContextMenu, Up, Mods::NONE, MoveUp),
                KeyBinding::single(TreeContextMenu, Char('k'), Mods::NONE, MoveUp),
                KeyBinding::single(TreeContextMenu, Down, Mods::NONE, MoveDown),
                KeyBinding::single(TreeContextMenu, Char('j'), Mods::NONE, MoveDown),
                KeyBinding::single(TreeContextMenu, Enter, Mods::NONE, Confirm),
                KeyBinding::single(NavCandidates, Esc, Mods::NONE, Close),
                KeyBinding::single(NavCandidates, Char('q'), Mods::NONE, Close),
                KeyBinding::single(NavCandidates, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(NavCandidates, Up, Mods::NONE, MoveUp),
                KeyBinding::single(NavCandidates, Char('k'), Mods::NONE, MoveUp),
                KeyBinding::single(NavCandidates, Down, Mods::NONE, MoveDown),
                KeyBinding::single(NavCandidates, Char('j'), Mods::NONE, MoveDown),
                KeyBinding::single(NavCandidates, Enter, Mods::NONE, Confirm),
                KeyBinding::single(ConfirmModal, Esc, Mods::NONE, Close),
                KeyBinding::single(ConfirmModal, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(PasteConflict, Esc, Mods::NONE, Close),
                KeyBinding::single(PasteConflict, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(DbGoto, Esc, Mods::NONE, Close),
                KeyBinding::single(DbGoto, Enter, Mods::NONE, Confirm),
                KeyBinding::single(PlaceMode, Esc, Mods::NONE, Close),
                KeyBinding::single(PlaceMode, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(PlaceMode, Char('q'), Mods::NONE, Quit),
                KeyBinding::single(TreeDrag, Esc, Mods::NONE, Close),
                KeyBinding::single(TreeDrag, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(HostsPicker, Esc, Mods::NONE, Close),
                KeyBinding::single(HostsPicker, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(HostsPicker, Enter, Mods::NONE, Confirm),
                KeyBinding::single(HostsPicker, Char('p'), Mods::CONTROL, TogglePickerPathMode),
                KeyBinding::single(HostsPicker, Up, Mods::NONE, MoveUp),
                KeyBinding::single(HostsPicker, Char('k'), Mods::CONTROL, MoveUp),
                KeyBinding::single(HostsPicker, Down, Mods::NONE, MoveDown),
                KeyBinding::single(HostsPicker, Char('j'), Mods::CONTROL, MoveDown),
                KeyBinding::single(HostsPicker, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(GraphBranchPicker, Esc, Mods::NONE, Close),
                KeyBinding::single(GraphBranchPicker, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(GraphBranchPicker, Enter, Mods::NONE, Confirm),
                KeyBinding::single(GraphBranchPicker, Up, Mods::NONE, MoveUp),
                KeyBinding::single(GraphBranchPicker, Char('k'), Mods::CONTROL, MoveUp),
                KeyBinding::single(GraphBranchPicker, Char('p'), Mods::CONTROL, MoveUp),
                KeyBinding::single(GraphBranchPicker, Down, Mods::NONE, MoveDown),
                KeyBinding::single(GraphBranchPicker, Char('j'), Mods::CONTROL, MoveDown),
                KeyBinding::single(GraphBranchPicker, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(CommitEditor, Esc, Mods::NONE, Close),
                KeyBinding::single(CommitEditor, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(CommitEditor, Enter, Mods::CONTROL, Confirm),
                KeyBinding::single(CommitEditor, Enter, Mods::CONTROL | Mods::SHIFT, Confirm),
                KeyBinding::single(CommitEditor, Enter, Mods::CONTROL | Mods::ALT, Confirm),
                KeyBinding::single(
                    CommitEditor,
                    Enter,
                    Mods::CONTROL | Mods::ALT | Mods::SHIFT,
                    Confirm,
                ),
                KeyBinding::single(SearchInput, Esc, Mods::NONE, Close),
                KeyBinding::single(SearchInput, Char('c'), Mods::CONTROL, Quit),
                KeyBinding::single(SearchInput, Enter, Mods::NONE, Confirm),
                KeyBinding::single(SearchInput, Enter, Mods::CONTROL, Confirm),
                KeyBinding::single(SearchInput, Enter, Mods::ALT, Confirm),
                KeyBinding::single(SearchInput, Enter, Mods::CONTROL | Mods::ALT, Confirm),
                KeyBinding::single(SearchInput, Up, Mods::NONE, MoveUp),
                KeyBinding::single(SearchInput, Char('k'), Mods::CONTROL, MoveUp),
                KeyBinding::single(SearchInput, Char('p'), Mods::CONTROL, MoveUp),
                KeyBinding::single(SearchInput, Down, Mods::NONE, MoveDown),
                KeyBinding::single(SearchInput, Char('j'), Mods::CONTROL, MoveDown),
                KeyBinding::single(SearchInput, Char('n'), Mods::CONTROL, MoveDown),
                KeyBinding::single(SearchInput, KeyCode::PageUp, Mods::NONE, Command::PageUp),
                KeyBinding::single(
                    SearchInput,
                    KeyCode::PageDown,
                    Mods::NONE,
                    Command::PageDown,
                ),
            ]
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_has_no_duplicate_bindings() {
        Keymap::validate().expect("keymap conflict");
    }

    #[test]
    fn normal_location_history_bindings_include_ctrl_alt_aliases() {
        let alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        let ctrl_alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::CONTROL);

        assert_eq!(
            Keymap::resolve(InputScope::Normal, &alt_left),
            Some(Command::LocationBack)
        );
        assert_eq!(
            Keymap::resolve(InputScope::Normal, &ctrl_alt_left),
            Some(Command::LocationBack)
        );
    }

    #[test]
    fn picker_scopes_resolve_common_navigation() {
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);

        for scope in [
            InputScope::QuickOpen,
            InputScope::GlobalSearch,
            InputScope::HostsPicker,
            InputScope::GraphBranchPicker,
        ] {
            assert_eq!(Keymap::resolve(scope, &up), Some(Command::MoveUp));
            assert_eq!(Keymap::resolve(scope, &ctrl_n), Some(Command::MoveDown));
            assert_eq!(Keymap::resolve(scope, &enter), Some(Command::Confirm));
        }
    }

    #[test]
    fn text_input_scopes_resolve_control_actions() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            Keymap::resolve(InputScope::VimSearch, &esc),
            Some(Command::Close)
        );
        assert_eq!(
            Keymap::resolve(InputScope::SearchInput, &enter),
            Some(Command::Confirm)
        );
        assert_eq!(
            Keymap::resolve(InputScope::CommitEditor, &ctrl_enter),
            Some(Command::Confirm)
        );
    }

    #[test]
    fn find_widget_shift_enter_still_confirms() {
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);

        assert_eq!(
            Keymap::resolve(InputScope::FindWidget, &shift_enter),
            Some(Command::Confirm)
        );
    }

    #[test]
    fn enter_modifier_aliases_keep_replace_and_pin_shortcuts() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            let key = KeyEvent::new(KeyCode::Enter, modifiers);
            assert_eq!(
                Keymap::resolve(InputScope::SearchInput, &key),
                Some(Command::Confirm)
            );
        }

        let ctrl_alt_enter =
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(
            Keymap::resolve(InputScope::GlobalSearch, &ctrl_alt_enter),
            Some(Command::PinGlobalSearch)
        );
    }
}
