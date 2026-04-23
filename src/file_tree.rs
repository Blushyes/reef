use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A visible entry in the flattened file tree.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf, // relative to workdir
    pub name: String,  // display name (filename only)
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub git_status: Option<char>, // 'M', 'A', 'D', '?', etc.
}

/// File preview content. The `body` enum carries three mutually-exclusive
/// shapes — text (with optional syntect tokens), a decoded raster image
/// (pixels live on the main thread after the worker hands them back), or
/// a friendly metadata card for everything else (PDF, video, SVG, oversized
/// image, corrupt image, …). Keeping them apart this way lets render pick
/// a codepath without boolean gymnastics, and lets the worker surface
/// *why* something isn't renderable (too large vs unsupported vs decode
/// error) so the UI can tell the user.
#[derive(Debug, Clone)]
pub struct PreviewContent {
    pub file_path: String,
    pub body: PreviewBody,
}

#[derive(Debug, Clone)]
pub enum PreviewBody {
    Text {
        lines: Vec<String>,
        highlighted: Option<Vec<Vec<(ratatui::style::Style, String)>>>,
    },
    Image(ImagePreview),
    Binary(BinaryInfo),
}

impl PreviewContent {
    /// Convenience for the handful of sites that need to gate behaviour
    /// that only makes sense on text bodies (search-jump, scroll clamping,
    /// drag-select, copy). Keeps the `matches!(…)` churn out of call sites.
    pub fn is_text(&self) -> bool {
        matches!(self.body, PreviewBody::Text { .. })
    }
}

/// Decoded raster image plus the metadata a render caller needs to show
/// dimensions / format / size without re-inspecting the file.
///
/// `image` is the transport slot for the decoded pixels: the worker
/// sets `Some(DynamicImage)`, then `App::apply_worker_result` **takes**
/// it out on the main thread and hands ownership to ratatui-image's
/// `StatefulProtocol`. Once that's done, the stored `PreviewContent`
/// carries `image: None` and just the metadata — avoids keeping a
/// second copy of the pixels alongside the protocol's own buffer.
///
/// `meta_line` is the pre-rendered "40×40 · PNG · 242 B" string, built
/// once at load time instead of on every render frame. render hits 60
/// fps during active interaction (mouse drag, scroll), and rebuilding
/// the same String through two `format!()` calls per frame is pure
/// allocator churn — cache it.
#[derive(Debug, Clone)]
pub struct ImagePreview {
    pub image: Option<image::DynamicImage>,
    pub width_px: u32,
    pub height_px: u32,
    pub format: image::ImageFormat,
    pub bytes_on_disk: u64,
    /// `true` for GIFs with more than one frame — we still render only
    /// the first frame in v1, but surface the animated-ness in the
    /// metadata line ("animated") so the still isn't mistaken for the
    /// whole thing. Exact frame count isn't worth re-decoding the entire
    /// GIF just to display.
    pub animated: bool,
    pub meta_line: String,
}

#[derive(Debug, Clone)]
pub struct BinaryInfo {
    pub bytes_on_disk: u64,
    /// MIME as reported by `infer`, e.g. "application/pdf". `None` when
    /// `infer` had no magic-byte match and we fell back to the null-byte
    /// heuristic.
    pub mime: Option<&'static str>,
    pub reason: BinaryReason,
    /// Pre-rendered "application/pdf · 2.4 MB" line for the metadata
    /// card. Empty when we have neither a MIME nor a size to show
    /// (e.g. the `Empty` reason). Cached at load time so render doesn't
    /// reallocate per frame.
    pub meta_line: String,
}

#[derive(Debug, Clone)]
pub enum BinaryReason {
    /// Not an image at all — PDF, zip, video, audio, font, etc.
    NonImage,
    /// Recognised as an image but unsupported by the `image` crate under
    /// our feature set (SVG, AVIF, HEIC, …).
    UnsupportedImage,
    /// Image file larger than `DECODE_CAP_BYTES`, or pixel count larger
    /// than `MAX_PIXELS`. We refuse to decode to keep UI responsive and
    /// bound memory.
    TooLarge,
    /// `image` crate rejected the bytes mid-decode. The String is a short
    /// diagnostic (one line, no source chain) for the UI. Already
    /// truncated at construction to keep the metadata card single-line
    /// and to bound accidental disclosure from future decoder backends.
    DecodeError(String),
    /// Legacy fallback: `infer` couldn't identify it, but the first 8KB
    /// contained null bytes so we treat it as binary. Rendered the same
    /// way as `NonImage` — the distinction is for telemetry / tests.
    NullBytes,
    /// 0-byte file. Shown as "(empty file)" in the preview card.
    Empty,
}

/// Fast-path image header read for the `wants_decoded_image == false`
/// branch of `load_image_preview`. 64 KB covers the header of every
/// format we care about (PNG IHDR lives at byte 16; JPEG SOF markers
/// usually within the first few KB; GIF LSD is the first 13 bytes; WebP
/// VP8 chunk header is near the start). Huge files that put metadata
/// farther in would miss dimensions, but such files are rare and the
/// fallback is "unsupported image" — never a crash.
const METADATA_HEADER_BYTES: usize = 64 * 1024;

/// Cap the length of an error message we stuff into a `BinaryReason::
/// DecodeError`. Longer strings get ellipsis-truncated. Keeps the
/// metadata card on a single terminal row even for pathological
/// messages, and limits the blast radius if a future image decoder
/// decides to embed paths or hex dumps in its error text.
const MAX_DECODE_ERROR_LEN: usize = 100;

fn decode_error(msg: impl Into<String>) -> BinaryReason {
    let mut s: String = msg.into();
    if s.len() > MAX_DECODE_ERROR_LEN {
        s.truncate(MAX_DECODE_ERROR_LEN);
        s.push('…');
    }
    BinaryReason::DecodeError(s)
}

/// Short display name for an `image::ImageFormat`. The `image` crate
/// has no `Display` impl so we map the variants we decode.
fn image_format_name(f: image::ImageFormat) -> &'static str {
    match f {
        image::ImageFormat::Png => "PNG",
        image::ImageFormat::Jpeg => "JPEG",
        image::ImageFormat::Gif => "GIF",
        image::ImageFormat::WebP => "WebP",
        image::ImageFormat::Bmp => "BMP",
        image::ImageFormat::Tiff => "TIFF",
        image::ImageFormat::Ico => "ICO",
        _ => "image",
    }
}

/// Human-readable byte size: "512 B" / "2.4 KB" / "5.7 MB" / "1.2 GB".
/// Single-precision is enough for the preview metadata card where
/// users just need a rough sense of scale.
fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn image_meta_line(
    width_px: u32,
    height_px: u32,
    format: image::ImageFormat,
    bytes_on_disk: u64,
    animated: bool,
) -> String {
    let fmt = image_format_name(format);
    let size = human_bytes(bytes_on_disk);
    if animated {
        format!("{width_px}×{height_px} · {fmt} · {size} · animated (first frame shown)")
    } else {
        format!("{width_px}×{height_px} · {fmt} · {size}")
    }
}

fn binary_meta_line(mime: Option<&'static str>, bytes_on_disk: u64) -> String {
    match mime {
        Some(m) if bytes_on_disk > 0 => format!("{m} · {}", human_bytes(bytes_on_disk)),
        Some(m) => m.to_string(),
        None if bytes_on_disk > 0 => human_bytes(bytes_on_disk),
        None => String::new(),
    }
}

/// Read the first `cap` bytes of `path` without slurping the rest.
/// Returns fewer bytes if the file is shorter than the cap.
fn read_up_to(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Build an `ImagePreview` with `image: None` and dimensions probed from
/// raw header bytes. Used on terminals without a graphics protocol —
/// we still want the user to see "1920×1080 · PNG · 2.4 MB" instead of
/// a generic binary card, but we don't decode pixels we'd throw away.
fn metadata_only_from_bytes<F>(
    bytes: &[u8],
    rel_str: &str,
    file_size: u64,
    card: F,
) -> PreviewContent
where
    F: Fn(BinaryReason) -> PreviewContent,
{
    use image::ImageReader;
    use std::io::Cursor;
    let reader = match ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(e) => return card(decode_error(e.to_string())),
    };
    let format = match reader.format() {
        Some(f) => f,
        None => return card(BinaryReason::UnsupportedImage),
    };
    let (w, h) = match reader.into_dimensions() {
        Ok(d) => d,
        Err(e) => {
            let reason = if matches!(e, image::ImageError::Unsupported(_)) {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(e.to_string())
            };
            return card(reason);
        }
    };
    PreviewContent {
        file_path: rel_str.to_string(),
        body: PreviewBody::Image(ImagePreview::metadata_only(w, h, format, file_size, false)),
    }
}

impl BinaryInfo {
    /// Build a `BinaryInfo` with its metadata line pre-rendered. Keeps
    /// the render path allocation-free and centralises the construction
    /// so the `meta_line` format stays consistent across the five
    /// binary-classification call sites.
    pub fn new(bytes_on_disk: u64, mime: Option<&'static str>, reason: BinaryReason) -> Self {
        Self {
            bytes_on_disk,
            mime,
            reason,
            meta_line: binary_meta_line(mime, bytes_on_disk),
        }
    }
}

impl ImagePreview {
    /// Build an `ImagePreview` with its metadata line pre-rendered.
    /// Takes ownership of the decoded `DynamicImage` so callers don't
    /// have to wrap it in `Some(..)` themselves.
    fn new(
        image: image::DynamicImage,
        width_px: u32,
        height_px: u32,
        format: image::ImageFormat,
        bytes_on_disk: u64,
        animated: bool,
    ) -> Self {
        Self {
            image: Some(image),
            width_px,
            height_px,
            format,
            bytes_on_disk,
            animated,
            meta_line: image_meta_line(width_px, height_px, format, bytes_on_disk, animated),
        }
    }

    /// Metadata-only preview for terminals that don't support a
    /// graphics protocol: we still want the user to see dimensions and
    /// size, but we skipped the full decode to save the CPU cost.
    fn metadata_only(
        width_px: u32,
        height_px: u32,
        format: image::ImageFormat,
        bytes_on_disk: u64,
        animated: bool,
    ) -> Self {
        Self {
            image: None,
            width_px,
            height_px,
            format,
            bytes_on_disk,
            animated,
            meta_line: image_meta_line(width_px, height_px, format, bytes_on_disk, animated),
        }
    }
}

/// Manages the file tree state.
pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded: HashSet<PathBuf>,
    git_statuses: HashMap<String, char>,
}

impl FileTree {
    pub fn new(workdir: &Path) -> Self {
        let mut tree = Self {
            root: workdir.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: HashMap::new(),
        };
        tree.rebuild();
        tree
    }

    /// Regenerate the flat entries list from the filesystem.
    pub fn rebuild(&mut self) {
        self.entries = build_entries(&self.root, &self.expanded, &self.git_statuses);
        // Clamp selection
        if !self.entries.is_empty() {
            self.selected = self.selected.min(self.entries.len() - 1);
        } else {
            self.selected = 0;
        }
    }

    pub fn toggle_expand(&mut self, index: usize) {
        if let Some(entry) = self.entries.get(index) {
            if entry.is_dir {
                let path = entry.path.clone();
                if self.expanded.contains(&path) {
                    self.expanded.remove(&path);
                } else {
                    self.expanded.insert(path);
                }
            }
        }
    }

    /// Collapse every currently-expanded directory. The `expanded` set is
    /// cleared rather than toggling each entry so the next rebuild emits
    /// only the top-level rows. Selection clamps to index 0 so the viewport
    /// doesn't end up pointing past the shortened entry list.
    ///
    /// Does not rebuild by itself — callers drive the async refresh path
    /// (`App::refresh_file_tree_with_target`) so the file worker gets a
    /// chance to also re-read git decorations atomically with the reshape.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.selected = 0;
    }

    pub fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        // Cleared-selection sentinel (`selected >= entries.len()`): treat
        // the first arrow key as "land on an edge" — Down → first row,
        // Up → last row, matching VSCode's Explorer when nothing is
        // selected. Without this, `selected + 1` on the sentinel would
        // arithmetic-overflow.
        if self.selected > last {
            self.selected = if delta > 0 { 0 } else { last };
            return;
        }
        if delta > 0 {
            self.selected = (self.selected + delta as usize).min(last);
        } else {
            self.selected = self.selected.saturating_sub((-delta) as usize);
        }
    }

    /// VSCode-style "nothing selected" state. `clear_selection` drops the
    /// highlight so a subsequent toolbar `+ File` / `+ Folder` creates at
    /// the project root, and right-click menu / F2 / Del no-op until the
    /// user picks a row again.
    ///
    /// Implementation: sets `selected` to `entries.len()`, a value that's
    /// always out of range so `selected_entry()` returns `None` and
    /// `is_selected == global_idx` never matches in render. Avoids the
    /// invasive refactor to `Option<usize>` all callers would need.
    pub fn clear_selection(&mut self) {
        self.selected = self.entries.len();
    }

    /// Whether `selected` currently points past the last entry (i.e. the
    /// "cleared" sentinel state).
    pub fn selected_cleared(&self) -> bool {
        self.selected >= self.entries.len()
    }

    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry().map(|entry| entry.path.clone())
    }

    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        self.expanded.iter().cloned().collect()
    }

    pub fn git_statuses(&self) -> HashMap<String, char> {
        self.git_statuses.clone()
    }

    pub fn replace_entries(&mut self, entries: Vec<TreeEntry>, selected_idx: usize) {
        self.entries = entries;
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = selected_idx.min(self.entries.len() - 1);
        }
    }

    /// Expand every ancestor directory of `rel` and move `selected` to the
    /// row that displays `rel` in the flattened tree. Used by the quick-open
    /// palette on accept, so the chosen file is visible and the preview
    /// panel shows it immediately. Silently no-ops if the file isn't in the
    /// tree after rebuild (e.g. deleted between index and accept).
    pub fn reveal(&mut self, rel: &Path) {
        for ancestor in rel.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            self.expanded.insert(ancestor.to_path_buf());
        }
        if let Some(idx) = self.entries.iter().position(|e| e.path == rel) {
            self.selected = idx;
        }
    }

    /// Merge git status from the host's file lists.
    pub fn refresh_git_statuses(
        &mut self,
        staged: &[crate::git::FileEntry],
        unstaged: &[crate::git::FileEntry],
    ) {
        self.git_statuses.clear();
        for f in staged {
            self.git_statuses.insert(
                f.path.clone(),
                f.status.label().chars().next().unwrap_or(' '),
            );
        }
        for f in unstaged {
            let ch = f.status.label().chars().next().unwrap_or(' ');
            self.git_statuses.entry(f.path.clone()).or_insert(ch);
        }
        // Propagate status to parent directories
        let paths: Vec<String> = self.git_statuses.keys().cloned().collect();
        for path in paths {
            let p = Path::new(&path);
            for ancestor in p.ancestors().skip(1) {
                let a = ancestor.to_string_lossy().to_string();
                if a.is_empty() {
                    break;
                }
                self.git_statuses.entry(a).or_insert('●');
            }
        }
        self.apply_git_statuses_to_entries();
    }

    fn apply_git_statuses_to_entries(&mut self) {
        for entry in &mut self.entries {
            let rel = entry.path.to_string_lossy().to_string();
            entry.git_status = self.git_statuses.get(&rel).copied();
        }
    }
}

pub fn build_entries(
    root: &Path,
    expanded: &HashSet<PathBuf>,
    git_statuses: &HashMap<String, char>,
) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    walk_dir(root, root, expanded, git_statuses, &mut entries, 0);
    entries
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    expanded: &HashSet<PathBuf>,
    git_statuses: &HashMap<String, char>,
    out: &mut Vec<TreeEntry>,
    depth: usize,
) {
    let mut children: Vec<(String, PathBuf, bool)> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        children.push((name, path, is_dir));
    }

    children.sort_by(|a, b| match (a.2, b.2) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
    });

    for (name, full_path, is_dir) in children {
        let rel = full_path
            .strip_prefix(root)
            .unwrap_or(&full_path)
            .to_path_buf();
        let rel_str = rel.to_string_lossy().to_string();
        let is_expanded = is_dir && expanded.contains(&rel);
        let git_status = git_statuses.get(&rel_str).copied();

        out.push(TreeEntry {
            path: rel.clone(),
            name,
            depth,
            is_dir,
            is_expanded,
            git_status,
        });

        if is_dir && is_expanded {
            walk_dir(root, &full_path, expanded, git_statuses, out, depth + 1);
        }
    }
}

/// Largest file the worker will attempt to decode as an image. Anything
/// larger is reported as `Binary(TooLarge)` with friendly metadata
/// instead — keeps the UI responsive and caps worst-case memory.
pub const DECODE_CAP_BYTES: u64 = 20 * 1024 * 1024;

/// Largest image we'll decode measured in pixels. A 20000×20000 PNG is
/// ~2 MB on disk (highly compressed) but expands to ~1.6 GB of pixels in
/// `DynamicImage`. We probe dimensions first and bail before that
/// allocation happens.
pub const MAX_PIXELS: u64 = 50_000_000;

/// How many leading bytes we hand to `infer::get` for MIME sniffing.
/// `infer` only needs the first few dozen bytes for most formats, but
/// we pass 8KB to match our existing null-byte probe window.
const PROBE_BYTES: usize = 8192;

/// Largest file the fallback null-byte heuristic will slurp into memory
/// when `infer` returned no magic-byte match. Anything bigger is
/// classified `Binary(NullBytes)` on the spot: a 500 MB random-bytes
/// file with no magic header shouldn't blow up RAM just so we can
/// confirm it isn't text.
const MAX_TEXT_PROBE_BYTES: u64 = 10 * 1024 * 1024;

/// Load a file for preview. Returns None if the file can't be read.
///
/// `dark` picks the syntect theme (OneHalfDark vs OneHalfLight) so the
/// highlighted tokens read correctly against whichever UI theme is active.
///
/// The worker first sniffs the file's MIME via `infer` (magic bytes — so
/// a `.jpg` extension on a PNG still routes correctly), then dispatches:
///
/// - `image/*` → probe dimensions, decode via the `image` crate, return
///   `PreviewBody::Image`. Oversized files/dimensions or unsupported
///   formats (SVG/AVIF/HEIC) become `Binary(TooLarge | UnsupportedImage)`.
/// - `text/*` (HTML/XML-shaped text; `.vue`, `.svelte`, etc. land here
///   because `infer` sniffs them as `text/html`) → fall through to the
///   text decode path so the preview shows source, not "binary file".
/// - other MIME (PDF/zip/video/audio/font…) → `Binary(NonImage)`
///   with the MIME so the UI can show "(application/pdf · 2.4 MB)".
/// - `infer` returns `None` → legacy 8KB null-byte heuristic decides
///   text vs `Binary(NullBytes)`.
pub fn load_preview(
    root: &Path,
    rel_path: &Path,
    dark: bool,
    wants_decoded_image: bool,
) -> Option<PreviewContent> {
    use std::io::Read;
    let full = root.join(rel_path);
    let rel_str = rel_path.to_string_lossy().to_string();

    // Single `File::open` serves three purposes: file-type check, size
    // (via handle `metadata`, no extra stat), and probe bytes. The old
    // path did `is_file` + `metadata` + `File::open` + `read` — three
    // syscalls to the same inode.
    let mut file = std::fs::File::open(&full).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let file_size = meta.len();

    if file_size == 0 {
        return Some(PreviewContent {
            file_path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(0, None, BinaryReason::Empty)),
        });
    }

    // Read enough bytes for MIME sniffing. For small files this is the
    // whole file; we'll reuse the buffer for the text branch below
    // instead of reopening.
    let probe_len = (file_size as usize).min(PROBE_BYTES);
    let mut probe = vec![0u8; probe_len];
    let n = file.read(&mut probe).ok()?;
    probe.truncate(n);

    let mime: Option<&'static str> = infer::get(&probe).map(|k| k.mime_type());

    // ── Image branch ────────────────────────────────────────────────
    if let Some(m) = mime
        && m.starts_with("image/")
    {
        return Some(load_image_preview(
            &full,
            &rel_str,
            file_size,
            m,
            wants_decoded_image,
        ));
    }

    // ── Non-image binary branch ─────────────────────────────────────
    // `text/*` stays out of this branch — `infer` sniffs `.vue`,
    // `.svelte`, and HTML-ish fragments as `text/html`, and the user
    // wants to read the source, not a "binary file" stub. Fall
    // through to the text decode path below.
    if let Some(m) = mime
        && !m.starts_with("text/")
    {
        return Some(PreviewContent {
            file_path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(file_size, Some(m), BinaryReason::NonImage)),
        });
    }

    // ── Unknown: fall back to null-byte heuristic for text ──────────
    // Huge files without a recognised magic header stay out of RAM —
    // we wouldn't be able to render them as text anyway (10K-line cap
    // in the text branch below), and reading 500 MB of random bytes
    // just to confirm they're binary is a bad trade.
    if file_size > MAX_TEXT_PROBE_BYTES {
        return Some(PreviewContent {
            file_path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(file_size, None, BinaryReason::NullBytes)),
        });
    }

    if probe.contains(&0) {
        return Some(PreviewContent {
            file_path: rel_str,
            body: PreviewBody::Binary(BinaryInfo::new(file_size, None, BinaryReason::NullBytes)),
        });
    }

    // Continue from the same file handle to fetch the remaining bytes.
    // `probe` already holds the first `probe_len` bytes — append the
    // rest instead of reopening from scratch.
    let mut raw = probe;
    if file_size as usize > raw.len() {
        raw.reserve((file_size as usize).saturating_sub(raw.len()));
        file.read_to_end(&mut raw).ok()?;
    }

    let content = String::from_utf8_lossy(&raw);
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    let lines = if lines.len() > 10_000 {
        lines[..10_000].to_vec()
    } else {
        lines
    };

    let highlighted = if raw.len() <= 512 * 1024 && lines.len() <= 5_000 {
        crate::ui::highlight::highlight_file(&rel_str, &lines, dark)
    } else {
        None
    };

    Some(PreviewContent {
        file_path: rel_str,
        body: PreviewBody::Text { lines, highlighted },
    })
}

/// Build a `Binary` card for a file that sniffed as `image/*` but which
/// we chose not to decode (too large, unsupported, corrupt). Factored
/// out of `load_image_preview` so each early-exit site stays a
/// one-liner instead of a four-line struct literal.
fn binary_card(
    rel_str: &str,
    file_size: u64,
    mime: &'static str,
    reason: BinaryReason,
) -> PreviewContent {
    PreviewContent {
        file_path: rel_str.to_string(),
        body: PreviewBody::Binary(BinaryInfo::new(file_size, Some(mime), reason)),
    }
}

/// Worker-side image decode. Split out from `load_preview` so the happy
/// path stays readable. Returns a `PreviewContent` — either a successful
/// `Image` or a `Binary` card explaining why we wouldn't decode.
fn load_image_preview(
    full: &Path,
    rel_str: &str,
    file_size: u64,
    mime: &'static str,
    wants_decoded_image: bool,
) -> PreviewContent {
    use image::ImageReader;
    use std::io::Cursor;

    let card = |reason| binary_card(rel_str, file_size, mime, reason);

    if file_size > DECODE_CAP_BYTES {
        return card(BinaryReason::TooLarge);
    }

    // Fast path: caller doesn't have a graphics protocol anyway, so
    // don't spend 50-200 ms decoding pixels that'll be dropped. Only
    // read enough bytes to parse the header (PNG/JPEG/GIF/WebP all fit
    // in 64 KB even for big files), then return a metadata-only
    // `ImagePreview` (image: None). Render shows the "image preview
    // unavailable" card with real dimensions.
    if !wants_decoded_image {
        let header = match read_up_to(full, METADATA_HEADER_BYTES) {
            Ok(b) => b,
            Err(e) => return card(decode_error(e.to_string())),
        };
        return metadata_only_from_bytes(&header, rel_str, file_size, card);
    }

    let bytes = match std::fs::read(full) {
        Ok(b) => b,
        Err(e) => return card(decode_error(e.to_string())),
    };

    // Build a single ImageReader and reuse it: `format()` is cheap
    // (reads the guessed format stored on the reader, no I/O). The
    // dimension probe DOES consume the reader, so we rebuild once
    // more before `decode()`. Three header parses → two.
    let reader = match ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
        Ok(r) => r,
        Err(e) => return card(decode_error(e.to_string())),
    };

    let format = match reader.format() {
        Some(f) => f,
        None => return card(BinaryReason::UnsupportedImage),
    };

    let (w, h) = match reader.into_dimensions() {
        Ok(d) => d,
        Err(e) => {
            // UnsupportedError here means the `image` crate can read the
            // header but can't decode the payload (e.g. an image format
            // we didn't enable — AVIF without the feature, or HEIC).
            // Everything else is a genuine parse/IO failure.
            let reason = if matches!(e, image::ImageError::Unsupported(_)) {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(e.to_string())
            };
            return card(reason);
        }
    };

    if (w as u64).saturating_mul(h as u64) > MAX_PIXELS {
        return card(BinaryReason::TooLarge);
    }

    // Full decode now that dimensions are known safe. Rebuild the
    // reader because `into_dimensions` consumed the previous one.
    let decoded = match ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .and_then(|r| r.decode().map_err(std::io::Error::other))
    {
        Ok(img) => img,
        Err(e) => {
            // Unwrap our std::io::Error wrapper to see if the cause was
            // an image::ImageError::Unsupported — if so, report format
            // as unsupported rather than a generic decode failure.
            let msg = e.to_string();
            let reason = if msg.contains("unsupported") || msg.contains("Unsupported") {
                BinaryReason::UnsupportedImage
            } else {
                decode_error(msg)
            };
            return card(reason);
        }
    };

    // Animated GIFs: v1 renders the first frame only. We only need to
    // know "is it animated?" — `take(2)` short-circuits the LZW decode
    // loop after two frames, so a 1000-frame GIF doesn't cost 1000×
    // the decode time just to populate the metadata line.
    let animated = if format == image::ImageFormat::Gif {
        use image::AnimationDecoder;
        use image::codecs::gif::GifDecoder;
        GifDecoder::new(Cursor::new(&bytes))
            .map(|dec| dec.into_frames().take(2).count() > 1)
            .unwrap_or(false)
    } else {
        false
    };

    // Downsample if the image is much bigger than any sensible preview
    // panel would need. A 4K image in the protocol buffer is ~33 MB of
    // RGBA and every panel-resize re-encodes the full pixel grid; by
    // pre-shrinking to `MAX_PROTOCOL_DIM` we bound both memory and
    // per-resize cost while keeping enough detail for halfblocks /
    // Kitty at typical split ratios.  Report the ORIGINAL dimensions
    // in metadata — users care about "how big is this PNG," not "how
    // big is the copy we fed to the protocol."
    let decoded = downscale_if_oversized(decoded);

    PreviewContent {
        file_path: rel_str.to_string(),
        body: PreviewBody::Image(ImagePreview::new(
            decoded, w, h, format, file_size, animated,
        )),
    }
}

/// Largest dimension (width OR height) we'll hand to the protocol.
/// 2048 px covers a full-screen preview on a 5K display with room to
/// spare and keeps the worst-case RGBA buffer under ~16 MB. Anything
/// bigger gets resized down preserving aspect ratio.
const MAX_PROTOCOL_DIM: u32 = 2048;

fn downscale_if_oversized(img: image::DynamicImage) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= MAX_PROTOCOL_DIM && h <= MAX_PROTOCOL_DIM {
        return img;
    }
    // `resize` preserves aspect ratio and uses Lanczos3 (high quality
    // downsample). The cost is a one-time step in the worker, well off
    // the render path.
    img.resize(
        MAX_PROTOCOL_DIM,
        MAX_PROTOCOL_DIM,
        image::imageops::FilterType::Lanczos3,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileEntry, FileStatus};

    fn make_entry(path: &str, status: FileStatus) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status,
            additions: 0,
            deletions: 0,
        }
    }

    fn make_tree_with_entries(entries: Vec<TreeEntry>) -> FileTree {
        FileTree {
            root: PathBuf::from("/nonexistent"),
            entries,
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: std::collections::HashMap::new(),
        }
    }

    fn dummy_entry(name: &str) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            depth: 0,
            is_dir: false,
            is_expanded: false,
            git_status: None,
        }
    }

    // ── navigate ─────────────────────────────────────────────────────────────

    #[test]
    fn navigate_forward() {
        let mut tree =
            make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        tree.navigate(1);
        assert_eq!(tree.selected, 1);
    }

    #[test]
    fn navigate_backward_at_zero_stays_zero() {
        let mut tree = make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b")]);
        tree.navigate(-1);
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn navigate_clamps_at_end() {
        let mut tree =
            make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        tree.navigate(9999);
        assert_eq!(tree.selected, 2);
    }

    #[test]
    fn navigate_no_op_on_empty() {
        let mut tree = make_tree_with_entries(vec![]);
        tree.navigate(1);
        assert_eq!(tree.selected, 0); // no crash, stays at 0
    }

    // ── selected_entry ───────────────────────────────────────────────────────

    #[test]
    fn selected_entry_empty_returns_none() {
        let tree = make_tree_with_entries(vec![]);
        assert!(tree.selected_entry().is_none());
    }

    #[test]
    fn selected_entry_returns_correct_entry() {
        let mut tree = make_tree_with_entries(vec![
            dummy_entry("file0.rs"),
            dummy_entry("file1.rs"),
            dummy_entry("file2.rs"),
        ]);
        tree.selected = 2;
        assert_eq!(tree.selected_entry().unwrap().name, "file2.rs");
    }

    // ── refresh_git_statuses ─────────────────────────────────────────────────

    #[test]
    fn refresh_git_statuses_clears_previous() {
        let mut tree = make_tree_with_entries(vec![]);
        tree.git_statuses.insert("old.rs".to_string(), 'X');
        tree.refresh_git_statuses(&[], &[]);
        assert!(tree.git_statuses.is_empty());
    }

    #[test]
    fn refresh_git_statuses_inserts_staged_files() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("src/main.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &[]);
        let ch = tree.git_statuses.get("src/main.rs").copied();
        assert_eq!(ch, Some('M'));
    }

    #[test]
    fn refresh_git_statuses_propagates_to_parent_dir() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("src/main.rs", FileStatus::Added)];
        tree.refresh_git_statuses(&staged, &[]);
        // Parent directory "src" should have been given the propagated marker
        assert!(
            tree.git_statuses.contains_key("src"),
            "parent dir should appear in git_statuses"
        );
    }

    #[test]
    fn refresh_git_statuses_unstaged_does_not_overwrite_staged() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("a.rs", FileStatus::Added)];
        let unstaged = vec![make_entry("a.rs", FileStatus::Modified)];
        // staged sets 'A'; unstaged uses or_insert so 'A' stays
        tree.refresh_git_statuses(&staged, &unstaged);
        assert_eq!(tree.git_statuses.get("a.rs").copied(), Some('A'));
    }

    #[test]
    fn refresh_git_statuses_updates_visible_entries_without_rebuild() {
        let mut src = dummy_entry("src");
        src.is_dir = true;
        let mut file = dummy_entry("main.rs");
        file.path = PathBuf::from("src/main.rs");
        file.depth = 1;
        let mut tree = make_tree_with_entries(vec![src, file]);

        let staged = vec![make_entry("src/main.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &[]);

        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].git_status, Some('●'));
        assert_eq!(tree.entries[1].git_status, Some('M'));
    }

    // ── load_preview: MIME + image decode path ──────────────────────────────

    /// Build a tiny in-memory PNG to feed to load_preview. Kept as a
    /// helper so individual tests stay focused on the assertion, not the
    /// encoding setup.
    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        use image::{ImageBuffer, ImageFormat, Rgb};
        use std::io::Cursor;
        let img = ImageBuffer::from_pixel(w, h, Rgb([255u8, 0, 0]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    fn write_bytes(dir: &Path, name: &str, data: &[u8]) {
        std::fs::write(dir.join(name), data).unwrap();
    }

    #[test]
    fn load_preview_detects_png_by_magic_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "red.png", &tiny_png(4, 4));

        let content = load_preview(tmp.path(), Path::new("red.png"), true, true).expect("some");
        match content.body {
            PreviewBody::Image(img) => {
                assert_eq!(img.width_px, 4);
                assert_eq!(img.height_px, 4);
                assert_eq!(img.format, image::ImageFormat::Png);
            }
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_png_with_wrong_extension() {
        // Magic bytes override the extension — a PNG saved as `.jpg`
        // still decodes via the PNG branch. This matters because git
        // repos occasionally have misnamed assets.
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "shot.jpg", &tiny_png(2, 2));

        let content = load_preview(tmp.path(), Path::new("shot.jpg"), true, true).expect("some");
        match content.body {
            PreviewBody::Image(img) => assert_eq!(img.format, image::ImageFormat::Png),
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_refuses_huge_dimensions() {
        // Fabricate a PNG whose header claims 40000×40000 but whose body
        // is empty. `into_dimensions()` reads the IHDR chunk and reports
        // the claimed size BEFORE we attempt to decode — our MAX_PIXELS
        // guard fires there, so we never allocate gigabytes of pixels.
        //
        // A valid minimal IHDR: signature (8) + length(4) + "IHDR"(4) +
        // width(4,BE) + height(4,BE) + bit_depth(1) + color_type(1) +
        // compression(1) + filter(1) + interlace(1) + CRC(4). We don't
        // bother computing a correct CRC — ImageReader reads the header
        // and will produce an Unsupported/IoError after dimensions is
        // called. Either way, the path returns `Binary(TooLarge)` or
        // `Binary(DecodeError)`; both are acceptable "don't OOM" outcomes.
        let mut png = Vec::<u8>::new();
        png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]); // signature
        png.extend_from_slice(&13u32.to_be_bytes()); // IHDR length
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&40000u32.to_be_bytes()); // width
        png.extend_from_slice(&40000u32.to_be_bytes()); // height
        png.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth, color type, rest
        png.extend_from_slice(&[0u8; 4]); // bogus CRC

        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "huge.png", &png);

        let content = load_preview(tmp.path(), Path::new("huge.png"), true, true).expect("some");
        match content.body {
            PreviewBody::Binary(info) => {
                assert!(
                    matches!(
                        info.reason,
                        BinaryReason::TooLarge | BinaryReason::DecodeError(_)
                    ),
                    "expected TooLarge or DecodeError, got {:?}",
                    info.reason
                );
                assert_eq!(info.mime, Some("image/png"));
            }
            other => panic!("expected Binary body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_text_unchanged() {
        // Regression guard: a plain source file still parses as text
        // and carries syntect tokens on the happy path.
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "src.rs", b"fn main() {}\n");

        let content = load_preview(tmp.path(), Path::new("src.rs"), true, true).expect("some");
        match content.body {
            PreviewBody::Text { lines, .. } => {
                assert_eq!(lines, vec!["fn main() {}".to_string()]);
            }
            other => panic!("expected Text body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_zero_byte_reports_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "empty.bin", b"");

        let content = load_preview(tmp.path(), Path::new("empty.bin"), true, true).expect("some");
        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::Empty));
                assert_eq!(info.bytes_on_disk, 0);
            }
            other => panic!("expected Binary(Empty), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_without_decode_skips_pixels_keeps_metadata() {
        // wants_decoded_image=false (terminal without graphics protocol):
        // the worker returns `ImagePreview { image: None }` — dimensions
        // and format still probed via header, but the expensive decode
        // is skipped. The render layer shows "image preview unavailable"
        // with real metadata instead of a generic `Binary` card.
        let tmp = tempfile::tempdir().unwrap();
        write_bytes(tmp.path(), "red.png", &tiny_png(8, 8));

        let content = load_preview(tmp.path(), Path::new("red.png"), true, false).expect("some");
        match content.body {
            PreviewBody::Image(img) => {
                assert_eq!(img.width_px, 8);
                assert_eq!(img.height_px, 8);
                assert_eq!(img.format, image::ImageFormat::Png);
                assert!(
                    img.image.is_none(),
                    "wants_decoded_image=false should skip decode"
                );
            }
            other => panic!("expected Image body, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_pdf_is_non_image_binary() {
        // %PDF- magic bytes → infer returns "application/pdf" → we
        // produce a `Binary(NonImage)` card with the MIME string for
        // the UI to show.
        let tmp = tempfile::tempdir().unwrap();
        // Minimal PDF header + junk. infer only needs the leading magic.
        let pdf = b"%PDF-1.4\n%bogus content\n".to_vec();
        write_bytes(tmp.path(), "doc.pdf", &pdf);

        let content = load_preview(tmp.path(), Path::new("doc.pdf"), true, true).expect("some");
        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::NonImage));
                assert_eq!(info.mime, Some("application/pdf"));
            }
            other => panic!("expected Binary(NonImage), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_vue_sfc_renders_as_text() {
        // `.vue` single-file components start with `<template>`, which
        // `infer` sniffs as `text/html`. Before this regression guard
        // the MIME branch classified any non-image match as binary, so
        // Vue/Svelte/XML source files showed "binary file" in the
        // preview panel instead of their actual contents.
        let tmp = tempfile::tempdir().unwrap();
        let sfc = b"<template>\n  <div>hello</div>\n</template>\n";
        write_bytes(tmp.path(), "General.vue", sfc);

        let content = load_preview(tmp.path(), Path::new("General.vue"), true, true).expect("some");
        match content.body {
            PreviewBody::Text { lines, .. } => {
                assert_eq!(lines[0], "<template>");
                assert_eq!(lines[1], "  <div>hello</div>");
                assert_eq!(lines[2], "</template>");
            }
            other => panic!("expected Text body for .vue, got {other:?}"),
        }
    }

    #[test]
    fn load_preview_unknown_binary_falls_back_to_null_byte_heuristic() {
        // Bytes that don't match any known magic but contain a null byte
        // → legacy heuristic routes to `Binary(NullBytes)` instead of
        // corrupting the preview panel with control characters.
        let tmp = tempfile::tempdir().unwrap();
        let mut data = vec![b'A'; 1024];
        data[512] = 0;
        write_bytes(tmp.path(), "weird.dat", &data);

        let content = load_preview(tmp.path(), Path::new("weird.dat"), true, true).expect("some");
        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::NullBytes));
            }
            other => panic!("expected Binary(NullBytes), got {other:?}"),
        }
    }

    #[test]
    fn load_preview_huge_unknown_skips_full_read() {
        // Unknown-magic file over `MAX_TEXT_PROBE_BYTES` short-circuits
        // to `Binary(NullBytes)` without reading the whole file. We
        // can't observe "didn't read" directly, but we CAN observe the
        // result shape: the metadata says bytes_on_disk == file_size
        // and reason is NullBytes regardless of whether the body
        // contains a null byte or not.
        let tmp = tempfile::tempdir().unwrap();
        // Write `MAX_TEXT_PROBE_BYTES + 1` bytes of a single value with
        // NO null byte and no magic-byte header. Without the size guard
        // we'd slurp the whole thing and then classify as text (no
        // null); with the guard we bail to NullBytes early.
        let path = tmp.path().join("big.dat");
        let big_size = super::MAX_TEXT_PROBE_BYTES + 1;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            // Write in 1 MB chunks of 0x41 ('A') — no null byte
            // anywhere, no known magic.
            let chunk = vec![b'A'; 1024 * 1024];
            let mut remaining = big_size;
            while remaining > 0 {
                let n = (remaining as usize).min(chunk.len());
                f.write_all(&chunk[..n]).unwrap();
                remaining -= n as u64;
            }
        }

        let content = load_preview(tmp.path(), Path::new("big.dat"), true, true).expect("some");
        match content.body {
            PreviewBody::Binary(info) => {
                assert!(matches!(info.reason, BinaryReason::NullBytes));
                assert_eq!(info.bytes_on_disk, big_size);
            }
            other => panic!("expected Binary(NullBytes), got {other:?}"),
        }
    }

    #[test]
    fn decode_error_truncates_long_messages() {
        // Ensures the truncation helper keeps the payload single-line.
        // `BinaryReason::DecodeError` is rendered verbatim in the UI, so
        // a multi-line or 10 KB error from a decoder would blow up the
        // metadata card.
        let long = "x".repeat(super::MAX_DECODE_ERROR_LEN + 500);
        match super::decode_error(long) {
            BinaryReason::DecodeError(s) => {
                assert!(
                    s.chars().count() <= super::MAX_DECODE_ERROR_LEN + 1,
                    "truncated string + ellipsis exceeded cap: {} chars",
                    s.chars().count()
                );
                assert!(s.ends_with('…'), "expected ellipsis suffix, got: {s:?}");
            }
            other => panic!("expected DecodeError, got {other:?}"),
        }
    }
}
