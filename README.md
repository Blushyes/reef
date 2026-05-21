# Reef

> **The minimal dev terminal for the AI coding era**

When AI writes your code, 90% of an IDE becomes dead weight. Reef is the other 10%.

<a href="./README.md"><img alt="English" src="https://img.shields.io/badge/lang-EN-blue?style=flat-square"></a>
<a href="./README.zh-CN.md"><img alt="简体中文" src="https://img.shields.io/badge/lang-%E4%B8%AD%E6%96%87-red?style=flat-square"></a>

A single-binary terminal workbench for browsing, reviewing, and shipping — locally or over SSH. No editor, no language server, no autocomplete. Write with your AI of choice; come here for the rest.

<p align="center">
  <img src="https://github.com/Blushyes/reef/releases/download/v0.33.0/reef.png" alt="Reef" width="900">
</p>

## Features

- **Files** — tree + read-only preview; syntax highlighting, inline images (Kitty / iTerm2 / halfblocks).
- **SQLite browser** — open any `.db` to walk schemas, tables, views, indexes, and triggers; borderless data grid with type-tinted columns and index/trigger detail cards.
- **Search** — ripgrep-powered workdir content search with live preview, gitignore-aware.
- **Git** — status with `+N −M` per file or folder; stage / unstage / discard / push (including `--force-with-lease`); unified or side-by-side diff.
- **Graph** — commit DAG with ref chips, inline detail, visual-mode range diffs.
- **Remote over SSH** — same UI, same keys, driven by an auto-deployed `reef-agent` daemon. One ControlMaster session covers RPC, file upload, and `$EDITOR` handoff.
- **Drag-and-drop upload** from Finder / Explorer (works over SSH).
- **Auto theme** (OSC 11) and locale; persistent prefs.

Keyboard-first, mouse where it earns its keep. Press `h` in-app for the full keymap.

## Install

```bash
npx @reef-tui/cli                # one-off
npm install -g @reef-tui/cli     # or global
```

Supported: macOS (arm64, x64), Linux (arm64, x64), Windows (x64). Or `cargo build --release` from source.

```bash
reef                             # current directory
reef --ssh user@host             # remote $HOME (agent auto-installed)
reef --ssh user@host:/path       # remote /path
```

## Status

Alpha. Single Rust binary, no plugins. Local and remote are feature-parity.
