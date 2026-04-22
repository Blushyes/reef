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
//!   - `off` / `none`         — disable image preview entirely.
//!   - `halfblocks`           — force the unicode-halfblocks renderer.
//!   - `kitty`                — force Kitty protocol.
//!   - `iterm` / `iterm2`     — force iTerm2 inline-image protocol.
//!   - `sixel`                — force Sixel (icy_sixel encoder bundled
//!                              with `ratatui-image`; no libsixel needed).
//!   - anything else / unset  — auto-detect via `Picker::from_query_stdio`.

use ratatui_image::picker::{Picker, ProtocolType};

/// Parsed form of the `REEF_IMAGE_PROTOCOL` override. `None` means the
/// env var was unset / empty / unrecognised → auto-detect. `Some(Off)`
/// means the user explicitly disabled image preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolOverride {
    Off,
    Halfblocks,
    Kitty,
    Iterm2,
    Sixel,
}

impl ProtocolOverride {
    fn from_env() -> Option<Self> {
        let raw = std::env::var("REEF_IMAGE_PROTOCOL").ok()?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "halfblocks" => Some(Self::Halfblocks),
            "kitty" => Some(Self::Kitty),
            "iterm" | "iterm2" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            _ => None,
        }
    }
}

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
    match ProtocolOverride::from_env() {
        Some(ProtocolOverride::Off) => None,
        Some(ProtocolOverride::Halfblocks) => {
            // `Picker::halfblocks` uses a built-in 10×20 cell size — good
            // enough for aspect-ratio; halfblocks doesn't care about
            // pixel-accurate cell measurements.
            Some(Picker::halfblocks())
        }
        Some(forced) => {
            // Honor the user's protocol choice, but still try the stdio
            // query so we get an accurate cell size for the terminal.
            // Fall back to halfblocks if the query fails (silent
            // terminal): at least we have *some* picker to carry the
            // protocol override onto.
            let mut p = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            p.set_protocol_type(match forced {
                ProtocolOverride::Kitty => ProtocolType::Kitty,
                ProtocolOverride::Iterm2 => ProtocolType::Iterm2,
                ProtocolOverride::Sixel => ProtocolType::Sixel,
                // Off and Halfblocks handled in earlier arms.
                ProtocolOverride::Off | ProtocolOverride::Halfblocks => p.protocol_type(),
            });
            Some(p)
        }
        // Default path: auto-detect. The call does a short round-trip; on
        // a silent terminal ratatui-image returns an error and we fall
        // through to `None` so the feature degrades to the metadata card.
        None => Picker::from_query_stdio().ok(),
    }
}
