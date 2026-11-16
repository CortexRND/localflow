//! Clipboard seam for `history copy` / `history raw`.

use anyhow::{anyhow, Result};

/// Something text can be copied to. Errors mean nothing was copied.
pub trait Clipboard {
    fn set_text(&mut self, text: &str) -> Result<()>;
}

/// The system clipboard via `arboard` (Cocoa pasteboard on macOS, Win32 on
/// Windows, X11 selection on Linux/BSD, including XWayland).
///
/// On X11 the copying process owns the selection: arboard hands the text to a
/// running clipboard manager when the handle is dropped. Without a clipboard
/// manager the text is gone once `lf-rs` exits, like any X11 client that
/// does not fork a selection-owner daemon (Python's pyperclip forks `xclip`).
pub struct SystemClipboard;

impl Clipboard for SystemClipboard {
    fn set_text(&mut self, text: &str) -> Result<()> {
        let mut clipboard =
            arboard::Clipboard::new().map_err(|error| anyhow!("clipboard unavailable: {error}"))?;
        clipboard
            .set_text(text.to_owned())
            .map_err(|error| anyhow!("could not copy to the clipboard: {error}"))
    }
}
