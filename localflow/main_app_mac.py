"""AppKit glue for LocalFlow.app (macOS only).

The app process outlives its windows, so LaunchServices delivers a reopen
Apple event (Dock click, double-click) to this process instead of launching it
again. The delegate answers it by showing a window.
"""

import AppKit
import objc
from AppKit import NSApplication
from Foundation import NSObject
from PyObjCTools import AppHelper

_DELEGATE = None  # NSApplication keeps only a weak reference to its delegate


class MainAppDelegate(NSObject):
    def initWithShow_(self, show):
        instance = objc.super(MainAppDelegate, self).init()
        if instance is None:
            return None
        instance._show = show
        return instance

    @objc.typedSelector(b"c@:@c")
    def applicationShouldHandleReopen_hasVisibleWindows_(self, app, has_visible):
        if not has_visible:
            self._show()
        return True


def run_app(show) -> None:
    """Show the first window with `show`, then serve events until quit."""
    global _DELEGATE
    app = NSApplication.sharedApplication()
    app.setActivationPolicy_(AppKit.NSApplicationActivationPolicyRegular)
    _DELEGATE = MainAppDelegate.alloc().initWithShow_(show)
    app.setDelegate_(_DELEGATE)
    show()
    app.activateIgnoringOtherApps_(True)
    AppHelper.runEventLoop()
