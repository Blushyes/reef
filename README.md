# Reef

> **AI 时代的最小开发者终端**
> **The minimal dev terminal for the AI coding era**

**Once AI writes your code, 90% of an IDE becomes dead weight. Reef is the other 10%.**

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

---

## Why

When AI writes most of your code, an IDE's surface shrinks to four things: browsing files, reading files, searching, and walking git diffs before a commit. Reef is a terminal workbench for exactly that — and nothing else.

No autocomplete. No linter. No language server. Not even a text editor. Write with your AI tool of choice; come here to read and ship.

## What's in the box

- **Files tab** — tree navigator with a read-only preview pane
- **Search tab** — workdir-wide content search (ripgrep-powered, honours `.gitignore`) with live preview on the right; modal list / input modes, whole-row horizontal scroll, 1000-hit cap
- **Git tab** — status, stage / unstage (keyboard or double-click), unified or side-by-side diff, compact or full-file context, discard with confirmation, push / force-push (`--force-with-lease`) with confirm banner
- **Graph tab** — commit DAG with ref chips, selectable rows, inline commit detail and per-file diff
- **Palettes** — <kbd>Space</kbd> <kbd>P</kbd> quick-open a file by fuzzy path, <kbd>Space</kbd> <kbd>F</kbd> global content search (same backend as the Search tab; <kbd>Alt</kbd>+<kbd>Enter</kbd> pins the query into the tab)
- **Keyboard first**, mouse where it earns its keep — drag to resize the split, double-click to toggle staging, scroll the panel under the cursor
- **Persistent prefs** — diff layout and mode survive restarts

## Install

**Via npm (recommended):**

```bash
# Run without installing
npx @reef-tui/cli

# Or install globally
npm install -g @reef-tui/cli
reef
```

The correct native binary for your platform is selected automatically.
Supported: macOS (arm64, x64), Linux (arm64, x64), Windows (x64).

**Build from source:**

```bash
cargo build --release

# Run from inside any git repo:
cd your-git-repo
/path/to/reef/target/release/reef
```

Reef works anywhere, but the Git and Graph tabs only light up inside a git repo. Outside one, they show a "Not a git repository" placeholder and the Files tab still browses the current directory.

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
| `v` | toggle mouse capture (for terminal text selection) |

### Files tab

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | navigate |
| `PgUp` / `PgDn` | page |
| `Enter` | expand/collapse directory, or open file in `$EDITOR` |
| `e` | open selected file in `$EDITOR` (no-op on dirs) |
| `r` | rebuild tree |

### Search tab

Left panel is modal. List mode is the default: global shortcuts (`h`, `q`, digit keys, …) stay live; press `/` or `i` to focus the input, `Esc` to return. Typing updates results live.

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k`, `Ctrl+N`/`Ctrl+P` | navigate results |
| `/` or `i` | focus the search input |
| `Esc` | return to list mode |
| `Enter`, double-click | reveal match in Files tab (matched line highlighted) |
| `r` | reload current query |
| `←`/`→`, `Shift+←`/`Shift+→` | horizontal scroll (whole row) |
| `Home` / `End` | reset / jump h-scroll |

### Git tab

| Key | Action |
| --- | --- |
| `s` / `u` | stage / unstage |
| `d` → `y` | discard unstaged file (confirm) |
| `Enter` / `e` | open selected file in `$EDITOR` |
| `r` | refresh |
| `t` | tree / flat view |
| `m` | unified ↔ side-by-side |
| `f` | compact ↔ full-file diff |

### Graph tab

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | move commit selection |
| `m` / `f` | commit-file diff layout / mode |
| `t` | tree / flat changed-files |

## Status

Alpha. Single-process Rust binary — no plugins, no IPC. Files, Git, and Graph are all host-native.
