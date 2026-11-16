"""AppKit glue for LocalFlow.app (macOS only).

The app process outlives its windows, so LaunchServices delivers a reopen
Apple event (Dock click, double-click) to this process instead of launching it
again. The delegate answers it by showing a window.
"""

import AppKit
import objc
from AppKit import NSApplication
from Foundation import NSObject, NSTimer
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


_POLL_SECONDS = 0.25
_POLLER = None


class PageRequestPoller(NSObject):
    """Run-loop timer target: serves page requests left by other processes.

    LaunchServices cannot pass new arguments to this running app, so callers
    leave a request file (hub_route) and `poll` consumes it. Any failure is
    swallowed: a bad request must never take the event loop down.
    """

    def initWithPoll_(self, poll):
        instance = objc.super(PageRequestPoller, self).init()
        if instance is None:
            return None
        instance._poll = poll
        return instance

    def tick_(self, _timer):
        try:
            if self._poll():
                NSApplication.sharedApplication().activateIgnoringOtherApps_(True)
        except Exception:  # noqa: BLE001, S110 - a bad request must not stop the run loop
            pass


def run_app(show, poll=None) -> None:
    """Show the first window with `show`, then serve events until quit.

    `poll` (optional) returns True after serving a page request; it runs on a
    run-loop timer for the life of the app.
    """
    global _DELEGATE, _POLLER
    app = NSApplication.sharedApplication()
    app.setActivationPolicy_(AppKit.NSApplicationActivationPolicyRegular)
    _DELEGATE = MainAppDelegate.alloc().initWithShow_(show)
    app.setDelegate_(_DELEGATE)
    show()
    if poll is not None:
        _POLLER = PageRequestPoller.alloc().initWithPoll_(poll)
        NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            _POLL_SECONDS, _POLLER, objc.selector(_POLLER.tick_, signature=b"v@:@"),
            None, True,
        )
    app.activateIgnoringOtherApps_(True)
    AppHelper.runEventLoop()
