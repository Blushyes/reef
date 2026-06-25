#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

allowed_mut='^(dispatch|tick|drain_effects|drain_runtime_events)$'
mut_methods="$(
  awk '
    /pub fn [A-Za-z0-9_]+/ {
      line = $0
      name = line
      sub(/^.*pub fn /, "", name)
      sub(/\(.*/, "", name)
      if (line ~ /&mut self/) {
        print name
        next
      }
      in_sig = 1
      pending = name
      next
    }
    in_sig {
      if ($0 ~ /&mut self/) {
        print pending
        in_sig = 0
        pending = ""
        next
      }
      if ($0 ~ /\)/ || $0 ~ /\{/) {
        in_sig = 0
        pending = ""
      }
    }
  ' crates/reef-app/src/engine.rs | sort
)"
unexpected_mut="$(printf '%s\n' "$mut_methods" | rg -v "$allowed_mut" || true)"
if [[ -n "$unexpected_mut" ]]; then
  printf 'reef-app public mutable API must stay behind dispatch/tick/effects. Unexpected:\n%s\n' "$unexpected_mut" >&2
  exit 1
fi

if rg -n '\b(self|app|engine)\.settings\b' crates/reef-tui/src | rg -v 'settings\(' >&2; then
  printf 'TUI must read settings through reef-app accessors/commands, not direct settings state.\n' >&2
  exit 1
fi

if rg -n 'apply_adapter_preview_result|take_matching_nav_pending_lsp_jump|dispatch_lsp_refine_definition\(' crates/reef-app/src crates/reef-tui/src >&2; then
  printf 'Old preview/LSP bypass entry point found; route through AppCommand instead.\n' >&2
  exit 1
fi

if rg -n '^pub mod (app|command|effect|engine|features|location|runtime|snapshot|tab|tasks|text_input);' crates/reef-app/src/lib.rs >&2; then
  printf 'reef-app internal modules must stay private; export renderer-neutral API from crate root.\n' >&2
  exit 1
fi

if rg -n 'reef_app::(app|features|tasks|runtime|text_input)::|use reef_app::(app|features|tasks|runtime|text_input)::' crates --glob '!crates/reef-app/**' >&2; then
  printf 'Crates outside reef-app must use reef_app root exports, not internal module paths.\n' >&2
  exit 1
fi

if rg -n 'reef_io::local::build_entries|build_entries\(' crates/reef-app/src/features/file_tree.rs >&2; then
  printf 'FileTree state must not do synchronous local tree IO; rebuild through TaskCoordinator/backend.\n' >&2
  exit 1
fi

if rg -n 'std::fs::|\.(exists|is_dir|is_file)\(' crates/reef-app/src/app >&2; then
  printf 'reef-app app state must not probe the local filesystem directly; use reef-io backend workers.\n' >&2
  exit 1
fi

if rg -n 'std::thread::spawn|thread::spawn|mpsc::channel\(' crates/reef-app/src/app crates/reef-app/src/engine.rs crates/reef-app/src/features >&2; then
  printf 'reef-app business state must not create ad-hoc worker channels; route background work through TaskCoordinator/WorkerResult.\n' >&2
  exit 1
fi

if rg -n 'GitRepo::|reef_core::git::GitRepo|\bGitRepo\b' crates/reef-app/src/app crates/reef-app/src/engine.rs >&2; then
  printf 'reef-app app state must not hold or open reef_core::git::GitRepo directly; use reef-io Backend.\n' >&2
  exit 1
fi

if rg -n 'std::fs::(read|read_dir|metadata|canonicalize)|ignore::WalkBuilder|WalkBuilder::new' crates/reef-app/src/tasks.rs >&2; then
  printf 'reef-app workers must use reef-io Backend for workspace IO, not direct local filesystem walking.\n' >&2
  exit 1
fi

if rg -n 'ratatui|crossterm|ratatui_image' crates/reef-app/Cargo.toml crates/reef-app/src >&2; then
  printf 'reef-app must stay renderer-neutral; terminal dependencies belong in reef-tui.\n' >&2
  exit 1
fi
