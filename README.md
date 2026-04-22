# Reef

> **AI 时代的最小开发者终端**
> **The minimal dev terminal for the AI coding era**

**Once AI writes your code, 90% of an IDE becomes dead weight. Reef is the other 10%.**

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

---

## Why

When AI writes most of your code, an IDE's surface shrinks to a handful of things: browsing files, reading files, searching, walking git diffs, and shipping the result. Reef is a terminal workbench for exactly that — and nothing else.

No autocomplete. No linter. No language server. Not even a text editor. Write with your AI tool of choice; come here to read, review, and ship — on your laptop or across an SSH connection.

## What's in the box

### Four tabs

- **Files** — tree navigator with a read-only preview: syntax-highlighted code, inline image rendering (Kitty / iTerm2 / halfblocks, auto-detected), and friendly metadata cards for binaries.
- **Search** — workdir-wide content search (ripgrep-powered, honours `.gitignore`) with live preview on the right; modal list / input modes, whole-row horizontal scroll, 1000-hit cap.
- **Git** — status with per-file `+N −M` numstat; stage / unstage per file **or per folder** (keyboard or double-click); unified or side-by-side diff; compact or full-file context; syntax-highlighted; discard (confirmed) and discard-all; push / force-push (`--force-with-lease`) behind a confirm banner.
- **Graph** — commit DAG with ref chips, inline commit detail and per-file diff. Vim-style visual mode (`V`) selects a range of commits and renders the merged diff across them; `Shift+↑/↓` or `Shift+Click` extends the range without entering visual mode.

### Palettes & overlays

- <kbd>Space</kbd> <kbd>P</kbd> — fuzzy quick-open by path.
- <kbd>Space</kbd> <kbd>F</kbd> — global content search overlay (shares the Search tab's backend; <kbd>Alt</kbd>+<kbd>Enter</kbd> pins the query into the tab).
- <kbd>Ctrl</kbd>+<kbd>O</kbd> — hosts picker: SSH hosts parsed from `~/.ssh/config` plus recently-used entries; swaps the backend live without restarting.
- <kbd>h</kbd> — help.

### Remote over SSH

Connect to a remote box and get the full Files / Search / Git / Graph UI — same keys, same feature set — driven through a tiny `reef-agent` daemon over SSH stdio.

```bash
reef --ssh user@host            # open remote $HOME (agent auto-installed)
reef --ssh user@host:/path      # open /path on host
```

- Auto-deploy: Reef detects the remote OS/arch and installs a version-matched `reef-agent` — downloaded from GitHub Releases, or uploaded from your local build as a fallback.
- One ControlMaster SSH session powers the whole thing: JSON-RPC over stdio for data, `scp` over the existing socket for drag-drop upload, `ssh -t` for `$EDITOR`.
- Content search, diff, graph, stage/unstage, file-tree create/rename/delete — all work identically remote.

### File operations

- Create, rename, move-to-trash, and hard-delete from the file tree — via toolbar, right-click context menu, or keyboard (`F2`, `d` / `Del`, `Shift+D` / `Shift+Del`).
- **Drag files from Finder / Explorer onto the terminal** to enter *place mode* — click any directory to copy the dropped files there, `Esc` or right-click to cancel. Works over SSH (uploads via the existing ControlMaster).

### Ergonomics

- **Keyboard first**, mouse where it earns its keep — drag to resize the split, double-click to toggle staging, scroll the panel under the cursor, right-click the tree for a context menu.
- **Vim-style in-panel search** — `/` and `?` open a prompt, `n` / `N` step matches, with `Alt`/`Ctrl` word-editing shortcuts in the prompt.
- **Navigation**: `↑` / `↓`, `j` / `k`, and `Ctrl+P` / `Ctrl+N` / `Ctrl+K` / `Ctrl+J` all move the selection on every tab.
- **Selection & clipboard** — `v` toggles mouse capture so the terminal's native text selection works; or drag inside a preview pane and Reef copies to the system clipboard via OSC 52 (no helper binary, works over SSH).
- **Auto-themes** — OSC 11 probe picks up terminal light/dark mode; locale auto-detects (English / 简体中文).
- **Persistent prefs** — diff layout, diff mode, recent SSH hosts, and friends survive restarts.

## Install

### Via npm (recommended)

```bash
# Run without installing
npx @reef-tui/cli

# Or install globally
npm install -g @reef-tui/cli
reef
```

The correct native binary for your platform is selected automatically.
Supported: macOS (arm64, x64), Linux (arm64, x64), Windows (x64).

### Build from source

```bash
cargo build --release

# Run from inside any git repo:
cd your-git-repo
/path/to/reef/target/release/reef
```

Reef works anywhere. Outside a git repo, the Git and Graph tabs show a "Not a git repository" placeholder; the Files tab still browses the current directory. If you start Reef in a non-repo with no recent hosts, the hosts picker opens so you always have a visible path forward.

## Keybindings

### Global

| Key | Action |
| --- | --- |
| `q`, `Ctrl+C` | quit |
| `1` / `2` / `3` / `4` | Files / Search / Git / Graph tab |
| `Tab` | cycle tabs |
| `Shift+Tab` | switch focused panel |
| `h` | help |
| `Space p` | quick-open file (fuzzy path) |
| `Space f` | global content search overlay |
| `Ctrl+O` | hosts picker (open SSH target) |
| `v` | toggle mouse capture (for terminal text selection) |
| `↑` / `↓`, `j` / `k`, `Ctrl+P` / `Ctrl+N` | navigate |
| `Ctrl+K` / `Ctrl+J` | navigate (alt) |
| `PgUp` / `PgDn` | page |
| `←` / `→`, `Shift+←` / `Shift+→` | horizontal scroll |
| `Home` / `End` | reset / jump h-scroll |
| `/` · `?` · `n` / `N` | vim-style find in preview / diff |
| drag file into terminal | enter place mode |

### Files tab

| Key | Action |
| --- | --- |
| `Enter` | expand/collapse directory, or open file in `$EDITOR` |
| `e` | open selected file in `$EDITOR` |
| `r` | rebuild tree |
| `F2` | rename selected entry |
| `d` / `Del` / `⌫` | move selected entry to trash (confirm) |
| `Shift+D` / `Shift+Del` | hard-delete (confirm) |
| right-click | open file-tree context menu |

### Search tab

Left panel is modal. List mode is the default: global shortcuts (`h`, `q`, digit keys, …) stay live; press `/` or `i` to focus the input, `Esc` to return. Typing updates results live.

| Key | Action |
| --- | --- |
| `/` or `i` | focus the search input |
| `Esc` | return to list mode |
| `Enter`, double-click | reveal match in Files tab (matched line highlighted) |
| `r` | reload current query |
| `Alt+←` / `Alt+→`, `Ctrl+W` / `Ctrl+U` | word-edit the prompt |

### Git tab

| Key | Action |
| --- | --- |
| `s` / `u` | stage / unstage (file, or every file under a folder row) |
| `d` → `y` | discard unstaged file / folder (confirm) |
| `Enter` / `e` | open selected file in `$EDITOR` |
| `r` | refresh |
| `t` | tree / flat view |
| `m` | unified ↔ side-by-side diff layout |
| `f` | compact ↔ full-file diff |

### Graph tab

| Key | Action |
| --- | --- |
| `↑` / `↓`, `j` / `k` | move commit selection |
| `V` | enter visual mode (select commit range) |
| `↑` / `↓` or `PgUp` / `PgDn` (visual) | extend range |
| `Shift+↑` / `Shift+↓`, `Shift+Click` | extend range without visual mode |
| click in range (visual) | set the other end |
| `Esc` | exit visual mode / clear range |
| `m` / `f` | commit-file diff layout / mode |
| `t` | tree / flat changed files |

## Status

Alpha. Single-process Rust binary — no plugins, no IPC on the local path. Files, Git, and Graph are host-native; remote uses a thin `reef-agent` daemon over SSH stdio. One codebase, one UI, one key map — local and remote are feature-parity.
