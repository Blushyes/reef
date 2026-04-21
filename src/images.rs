//! Terminal image-protocol probing for the file-preview panel.
//!
//! We use the `ratatui-image` crate to render images through whichever
//! graphics protocol the terminal supports (Kitty, iTerm2, Sixel, or the
//! universal Halfblocks fallback). Setting up the `Picker` needs a
//! synchronous stdio round-trip (the terminal answers capability queries
//! on stdin), so this runs **before** raw mode is enabled — the same
//! invariant `Theme::resolve`'s OSC 11 probe relies on.
//!
//! `REEF_IMAGE_PROTOCOL` is an escape hatch:
//!   - `off` / `none`  — disable image preview entirely (friendly metadata
//!                        cards still show for binary files).
//!   - `halfblocks`    — force the unicode-halfblocks renderer. Works on
//!                        every terminal; used by integration tests so
//!                        snapshots stay deterministic against the
//!                        ratatui `TestBackend`.
//!   - `kitty`         — force Kitty protocol.
//!   - `iterm` / `iterm2` — force iTerm2 inline-image protocol.
//!   - `sixel`         — force Sixel (icy_sixel encoder bundled with
//!                        `ratatui-image`; no libsixel needed).
//!   - anything else / unset — auto-detect via `Picker::from_query_stdio`.

use ratatui_image::picker::{Picker, ProtocolType};

/// Probe the current terminal for image-rendering capabilities.
///
/// Returns `None` when the user disabled preview via `REEF_IMAGE_PROTOCOL=off`
/// or when auto-detection fails on a terminal that doesn't reply to the
/// query CSI sequence (legacy Terminal.app, some SSH tunnels, piped
/// stdout). A `None` picker means the image branch in the preview panel
/// will render the "image preview unavailable" card instead of pixels.
///
/// MUST be called before `enable_raw_mode()` in `main.rs` — the probe
/// reads from stdin synchronously and the reply lines would otherwise
/// fragment onto the TUI.
pub fn probe_picker() -> Option<Picker> {
    match std::env::var("REEF_IMAGE_PROTOCOL")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("off") | Some("none") => return None,
        Some("halfblocks") => {
            // `Picker::halfblocks` uses a built-in 10×20 cell size — good
            // enough for aspect-ratio; halfblocks doesn't care about
            // pixel-accurate cell measurements.
            return Some(Picker::halfblocks());
        }
        Some(forced @ ("kitty" | "iterm" | "iterm2" | "sixel")) => {
            // Honor the user's protocol choice, but still try the stdio
            // query so we get an accurate cell size for the terminal.
            // Fall back to halfblocks if the query fails (silent
            // terminal): at least we have *some* picker to carry the
            // protocol override onto.
            let mut p = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            let pt = match forced {
                "kitty" => ProtocolType::Kitty,
                "iterm" | "iterm2" => ProtocolType::Iterm2,
                "sixel" => ProtocolType::Sixel,
                _ => p.protocol_type(),
            };
            p.set_protocol_type(pt);
            return Some(p);
        }
        _ => {}
    }

    // Default path: auto-detect. The call does a short round-trip; on a
    // silent terminal ratatui-image returns an error and we fall through
    // to `None` so the feature degrades to the metadata card.
    Picker::from_query_stdio().ok()
}
