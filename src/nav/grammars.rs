//! Language detection and tree-sitter grammar dispatch.
//!
//! Adding a language: (1) add its `tree-sitter-<lang>` crate to
//! Cargo.toml, (2) add the `NavLang` variant and its extension list
//! here, (3) add an intra-file query module under `crate::nav::intrafile`.

use std::path::Path;

/// Languages the navigation engine ships grammars for. Each variant is
/// statically linked — no runtime download. The matching crate is the
/// upstream `tree-sitter-<lang>` so the bundled grammar tracks the
/// language's canonical parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavLang {
    Rust,
    /// Plain TypeScript (`.ts` / `.mts` / `.cts`). TSX is a separate
    /// variant because tree-sitter-typescript ships them as two
    /// distinct languages — TSX accepts JSX syntax, TypeScript rejects
    /// it.
    TypeScript,
    Tsx,
    Python,
    Go,
    /// Vue SFC (`.vue`). Tree-sitter-vue parses the SFC envelope but
    /// the `<script>` body is a `raw_text` blob — so intra-file
    /// semantic queries don't see identifiers inside the script.
    /// Cross-file nav is provided by `vue-language-server` (volar);
    /// the tree-sitter tier only contributes file-type detection and
    /// the status badge.
    Vue,
}

impl NavLang {
    /// Every supported language, in a stable order. Single source of
    /// truth — iterate this instead of re-listing the variants (the
    /// list was previously hardcoded in `refresh_lsp_installed`,
    /// `SettingItem::ALL`, and a test, so adding a language meant
    /// editing several disconnected sites with no compile-time catch).
    pub const ALL: &'static [NavLang] = &[
        NavLang::Rust,
        NavLang::TypeScript,
        NavLang::Tsx,
        NavLang::Python,
        NavLang::Go,
        NavLang::Vue,
    ];

    /// Recognise a language from a file extension. Extensions are
    /// lowercased before comparison so `.RS` and `.rs` map the same.
    /// Returns `None` for everything we don't have a grammar for —
    /// `gd` simply falls through to a no-op in that case (status
    /// bar may surface "no parser for .ext" as a Phase 3 polish).
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "vue" => Some(Self::Vue),
            _ => None,
        }
    }

    /// Recognise from a path by its extension. Convenience for the
    /// preview-worker dispatch site.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    /// The tree-sitter `Language` handle. Grammar functions come from
    /// the `LANGUAGE: LanguageFn` constant each crate exports — this is
    /// the modern (post-tree-sitter-language 0.1) pattern that all four
    /// of our grammars follow.
    pub fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            // tree-sitter-vue-updated pre-dates the `LanguageFn`
            // constant pattern, so it still exposes the older
            // `language() -> Language` function. Same end shape, just
            // a function call instead of `.into()`.
            Self::Vue => tree_sitter_vue::language(),
        }
    }

    /// Human-readable name. Surfaced in status-bar messages
    /// ("Parsed Rust file", "indexed 42 Python files") and in error
    /// strings. Stable; do NOT change without auditing UI strings.
    pub fn name(self) -> &'static str {
        self.profile().display_name
    }

    /// Per-language runtime profile — LSP binary, args, JSON-RPC
    /// language id, status-bar badge glyph, display name. Centralises
    /// what used to be five separate `match self` blocks scattered
    /// across `lsp.rs`, `tasks.rs`, `ui/mod.rs`, `app.rs`. Adding a
    /// language now means: extend `NavLang` + `from_extension` +
    /// `language()` here, then write a profile constant + intrafile
    /// query + reference query. No other call site changes.
    pub fn profile(self) -> &'static LangProfile {
        match self {
            Self::Rust => &PROFILE_RUST,
            Self::TypeScript => &PROFILE_TYPESCRIPT,
            Self::Tsx => &PROFILE_TSX,
            Self::Python => &PROFILE_PYTHON,
            Self::Go => &PROFILE_GO,
            Self::Vue => &PROFILE_VUE,
        }
    }

    /// Whether this language carries semantic intra-file queries
    /// (definition / reference). Some languages (Vue) parse the
    /// SFC envelope but the embedded script is a raw blob, so
    /// intra-file `gd` has nothing to match against — we skip the
    /// "empty popup" fallthrough for them and rely on LSP entirely.
    pub fn has_semantic_queries(self) -> bool {
        !matches!(self, Self::Vue)
    }
}

/// Compact metadata each language carries through the nav engine. v1
/// covers what the LSP supervisor + status bar need; tree-sitter
/// grammar fns and per-language queries stay in their own modules
/// because they're large enough to deserve dedicated files.
pub struct LangProfile {
    pub display_name: &'static str,
    /// 2-char glyph for the status-bar badge — `RA●` / `TS●` / `PY●`
    /// / `GO●`. Kept short so the badge doesn't crowd the panel chip
    /// on narrow terminals.
    pub badge_glyph: &'static str,
    /// `None` means "no LSP wired up for this language yet" — the
    /// nav engine still uses tree-sitter and the workspace index.
    pub lsp: Option<LspProfile>,
}

/// LSP server invocation profile. `bin` is searched on PATH; `args`
/// are passed verbatim (most stdio servers want `--stdio` to opt out
/// of socket / pipe transports); `language_id` is the per-LSP magic
/// string used in `textDocument/didOpen.languageId`.
pub struct LspProfile {
    pub bin: &'static str,
    pub args: &'static [&'static str],
    pub language_id: &'static str,
    /// Human install hint shown in Settings when the binary is missing.
    pub install_command: Option<&'static str>,
}

const PROFILE_RUST: LangProfile = LangProfile {
    display_name: "Rust",
    badge_glyph: "RA",
    lsp: Some(LspProfile {
        bin: "rust-analyzer",
        args: &[],
        language_id: "rust",
        install_command: Some("rustup component add rust-analyzer"),
    }),
};

const PROFILE_TYPESCRIPT: LangProfile = LangProfile {
    display_name: "TypeScript",
    badge_glyph: "TS",
    lsp: Some(LspProfile {
        bin: "typescript-language-server",
        args: &["--stdio"],
        language_id: "typescript",
        install_command: Some("npm i -g typescript-language-server typescript"),
    }),
};

const PROFILE_TSX: LangProfile = LangProfile {
    display_name: "TSX",
    badge_glyph: "TX",
    lsp: Some(LspProfile {
        bin: "typescript-language-server",
        args: &["--stdio"],
        language_id: "typescriptreact",
        install_command: Some("npm i -g typescript-language-server typescript"),
    }),
};

const PROFILE_PYTHON: LangProfile = LangProfile {
    display_name: "Python",
    badge_glyph: "PY",
    lsp: Some(LspProfile {
        bin: "pyright-langserver",
        args: &["--stdio"],
        language_id: "python",
        install_command: Some("npm i -g pyright"),
    }),
};

const PROFILE_GO: LangProfile = LangProfile {
    display_name: "Go",
    badge_glyph: "GO",
    lsp: Some(LspProfile {
        bin: "gopls",
        args: &[],
        language_id: "go",
        install_command: Some("go install golang.org/x/tools/gopls@latest"),
    }),
};

const PROFILE_VUE: LangProfile = LangProfile {
    display_name: "Vue",
    badge_glyph: "VU",
    lsp: Some(LspProfile {
        bin: "vue-language-server",
        args: &["--stdio"],
        language_id: "vue",
        install_command: Some("npm i -g @vue/language-server"),
    }),
};
