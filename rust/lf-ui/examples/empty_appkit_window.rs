//! Empty native-window RSS baseline; deliberately no Hub services or timers.
//! Run the release binary in the same CI job as `lf-hub` for comparison.

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    let mtm = MainThreadMarker::new().expect("the baseline must run on AppKit's main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            NSRect {
                origin: NSPoint::new(0.0, 0.0),
                size: NSSize::new(640.0, 480.0),
            },
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the process retains this one window until AppKit terminates it.
    unsafe { window.setReleasedWhenClosed(false) };
    window.orderFrontRegardless();
    app.finishLaunching();
    app.run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("empty AppKit window baseline is available only on macOS");
}
