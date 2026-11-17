//! Offscreen PNG snapshots (port of `phrases_mac.snapshot_png`).
//!
//! The window is rendered through `cacheDisplayInRect:toBitmapImageRep:`, which
//! needs no screen-recording permission. Failures are real errors: a missing
//! bitmap, PNG data or file is never reported as success.
#![allow(deprecated)]

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use objc2::rc::Retained;
use objc2::Message;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSBitmapImageFileType, NSView, NSWindow,
};
use objc2_foundation::{NSDate, NSDictionary, NSRunLoop, NSString};

/// What to render.
#[derive(Clone, Copy)]
pub enum SnapshotTarget<'a> {
    /// A whole window (its frame view, like Python's `contentView().superview()`).
    Window(&'a NSWindow),
    /// A single view.
    View(&'a NSView),
}

/// The Aqua (light) or DarkAqua appearance.
fn appearance(dark: bool) -> Result<Retained<NSAppearance>> {
    // SAFETY: immutable global appearance-name constants.
    let name = unsafe {
        if dark {
            NSAppearanceNameDarkAqua
        } else {
            NSAppearanceNameAqua
        }
    };
    NSAppearance::appearanceNamed(name).ok_or_else(|| {
        anyhow!(
            "appearance {} is not available",
            if dark { "darkAqua" } else { "aqua" }
        )
    })
}

/// Run the main run loop for `seconds` so deferred work (appearance
/// notifications, delegate rebuilds, layout) fires.
pub fn spin_run_loop(seconds: f64) {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(seconds));
}

/// Force `window` (or `view`) into the light or dark appearance.
///
/// Callers that rebuild their views from the palette (the Hub) must rebuild
/// after this and before [`snapshot_png`], because the palette is re-read from
/// the effective appearance on every render.
pub fn set_appearance(target: SnapshotTarget<'_>, dark: bool) -> Result<()> {
    let appearance = appearance(dark)?;
    match target {
        SnapshotTarget::Window(window) => window.setAppearance(Some(&appearance)),
        SnapshotTarget::View(view) => view.setAppearance(Some(&appearance)),
    }
    Ok(())
}

/// Render `target` to a PNG at `path`.
///
/// Sets the aqua/darkAqua appearance, shows the window offscreen-capable
/// (`orderFrontRegardless`), lets layout and the appearance change settle,
/// then draws the frame view into a bitmap and writes it atomically.
pub fn snapshot_png(target: SnapshotTarget<'_>, path: &Path, dark: bool) -> Result<()> {
    set_appearance(target, dark)?;
    let view: Retained<NSView> = match target {
        SnapshotTarget::Window(window) => {
            window.orderFrontRegardless();
            window.displayIfNeeded();
            // Let layout and the appearance change settle before reading pixels.
            spin_run_loop(0.2);
            let content = window
                .contentView()
                .ok_or_else(|| anyhow!("the window has no content view"))?;
            // SAFETY: plain getter on a live view.
            match unsafe { content.superview() } {
                Some(frame_view) => frame_view,
                None => content,
            }
        }
        SnapshotTarget::View(view) => {
            if let Some(window) = view.window() {
                window.displayIfNeeded();
            }
            spin_run_loop(0.2);
            view.retain()
        }
    };
    view.layoutSubtreeIfNeeded();
    view.displayIfNeeded();
    let bounds = view.bounds();
    if bounds.size.width < 1.0 || bounds.size.height < 1.0 {
        bail!(
            "cannot snapshot an empty view ({}x{})",
            bounds.size.width,
            bounds.size.height
        );
    }
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or_else(|| anyhow!("could not create a bitmap for the view"))?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let properties = NSDictionary::new();
    // SAFETY: an empty properties dictionary is valid for PNG output.
    let data =
        unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
            .ok_or_else(|| anyhow!("PNG encoding failed"))?;
    if data.is_empty() {
        bail!("PNG encoding produced no data");
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    let text = path
        .to_str()
        .ok_or_else(|| anyhow!("snapshot path is not valid UTF-8: {}", path.display()))?;
    if !data.writeToFile_atomically(&NSString::from_str(text), true) {
        bail!("could not write {}", path.display());
    }
    let written =
        std::fs::metadata(path).with_context(|| format!("checking {}", path.display()))?;
    if written.len() == 0 {
        bail!("{} was written empty", path.display());
    }
    Ok(())
}
