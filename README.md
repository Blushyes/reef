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
- **Git tab** — status, stage / unstage (keyboard or double-click), unified or side-by-side diff, compact or full-file context
- **Keyboard first**, mouse where it earns its keep — drag to resize the split, double-click to toggle staging, scroll the panel under the cursor
- **Persistent prefs** — diff layout and mode survive restarts

## Architecture

Reef is a host plus plugins. Plugins are **isolated subprocesses** that speak JSON-RPC 2.0 over stdin/stdout using LSP-style framing. They return `StyledLine[]`; the host renders them with [ratatui](https://github.com/ratatui/ratatui). The manifest format mirrors VSCode's `contributes.*`.

The name is the architecture: a reef is built up by many small independent organisms living next to each other. Each plugin is its own process; together they are the workspace.

See [docs/plugin-protocol.md](./docs/plugin-protocol.md) for the full protocol.

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
# Release build is required — the bundled git plugin's manifest
# points at target/release/reef-git.
cargo build --release

# Run from inside any git repo:
cd your-git-repo
/path/to/reef/target/release/reef
```

Reef exits immediately if the current directory isn't inside a git repo.

### Plugin discovery

Reef searches three locations, in order:

1. `<reef binary>/plugins/` — shipped alongside the binary
2. `<workspace>/plugins/` — dev mode, when running from this repo
3. `~/.config/reef/plugins/` — user plugins

Each plugin is a directory containing a `reef.json` manifest and an executable.

## Keybindings

### Global

| Key | Action |
| --- | --- |
| `q`, `Ctrl+C` | quit |
| `1` / `2` | Files / Git tab |
| `Tab` | cycle tabs |
| `Shift+Tab` | switch focused panel |
| `h` | help |
| `v` | toggle mouse capture (for terminal text selection) |

### Files tab

| Key | Action |
| --- | --- |
| `↑`/`↓`, `j`/`k` | navigate |
| `PgUp` / `PgDn` | page |
| `Enter` | expand/collapse directory |
| `r` | rebuild tree |

### Git tab

| Key | Action |
| --- | --- |
| `s` / `u` | stage / unstage |
| `r` | refresh |
| `t` | tree / flat view |
| `m` | unified ↔ side-by-side |
| `f` | compact ↔ full-file diff |

## Status

Alpha. Bundled plugin: `git`. Host-native: file tree and preview. On the roadmap as standalone plugins: `file-search`, `grep`, a richer `file-viewer`.
