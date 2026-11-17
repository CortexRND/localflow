//! Target/action and delegate plumbing for the Rust AppKit layer.
//!
//! AppKit controls keep their target *unretained*, so a single long-lived
//! [`ActionTarget`] object (owned by the Hub through an `Rc`) receives every
//! `perform:` action, text-change delegate call and combo-box selection and
//! dispatches it to a Rust closure registered for that control. Closures hold
//! only `Weak` references to the page/controller state, so no retain cycle
//! exists between the views, the target and the state.
//!
//! Handlers are keyed by the control's object address; call
//! [`ActionTarget::clear`] when the views are torn down (every page rebuild)
//! so a recycled address can never reach a stale closure.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSComboBoxDelegate, NSControl, NSControlTextEditingDelegate, NSTextDelegate,
    NSTextFieldDelegate, NSTextView, NSTextViewDelegate,
};
use objc2_foundation::{NSNotification, NSTimer};

/// A registered handler.
pub type Handler = Rc<dyn Fn()>;

/// Handler tables of an [`ActionTarget`].
#[derive(Default)]
pub struct Handlers {
    actions: RefCell<HashMap<usize, Handler>>,
    text_changes: RefCell<HashMap<usize, Handler>>,
    selections: RefCell<HashMap<usize, Handler>>,
}

fn key(object: &AnyObject) -> usize {
    object as *const AnyObject as usize
}

define_class!(
    /// The one object every control of a Hub targets.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Handlers]
    #[name = "LFActionTarget"]
    pub struct ActionTarget;

    impl ActionTarget {
        #[unsafe(method(perform:))]
        fn perform(&self, sender: &AnyObject) {
            let handler = self.ivars().actions.borrow().get(&key(sender)).cloned();
            if let Some(handler) = handler {
                handler();
            }
        }
    }

    unsafe impl NSObjectProtocol for ActionTarget {}

    unsafe impl NSControlTextEditingDelegate for ActionTarget {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            self.dispatch_text(notification);
        }
    }

    unsafe impl NSTextFieldDelegate for ActionTarget {}

    unsafe impl NSTextViewDelegate for ActionTarget {}

    unsafe impl NSTextDelegate for ActionTarget {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, notification: &NSNotification) {
            self.dispatch_text(notification);
        }
    }

    unsafe impl NSComboBoxDelegate for ActionTarget {
        #[unsafe(method(comboBoxSelectionDidChange:))]
        fn combo_box_selection_did_change(&self, notification: &NSNotification) {
            let object = notification.object();
            let handler = object
                .as_deref()
                .and_then(|object| self.ivars().selections.borrow().get(&key(object)).cloned());
            if let Some(handler) = handler {
                handler();
            }
        }
    }
);

impl ActionTarget {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Handlers::default());
        // SAFETY: plain `init` on NSObject.
        unsafe { msg_send![super(this), init] }
    }

    fn dispatch_text(&self, notification: &NSNotification) {
        let object = notification.object();
        let handler = object.as_deref().and_then(|object| {
            self.ivars()
                .text_changes
                .borrow()
                .get(&key(object))
                .cloned()
        });
        if let Some(handler) = handler {
            handler();
        }
    }

    /// Run `handler` when `control` sends its action (button click, Return in
    /// a field, popup selection).
    pub fn on_action(&self, control: &NSControl, handler: impl Fn() + 'static) {
        let sender: &AnyObject = control.as_ref();
        self.ivars()
            .actions
            .borrow_mut()
            .insert(key(sender), Rc::new(handler));
        // SAFETY: the target is the `ActionTarget`, kept alive by the Hub; the
        // selector exists on it.
        unsafe {
            control.setTarget(Some(self.as_ref()));
            control.setAction(Some(sel!(perform:)));
        }
    }

    /// Run `handler` whenever the text of `field` (an `NSTextField` or combo
    /// box) changes; also installs this object as the field's delegate.
    pub fn on_field_change(&self, field: &NSControl, handler: impl Fn() + 'static) {
        let sender: &AnyObject = field.as_ref();
        self.ivars()
            .text_changes
            .borrow_mut()
            .insert(key(sender), Rc::new(handler));
        // SAFETY: `setDelegate:` is sent to a text field / combo box, which
        // both implement it; the delegate is unretained and outlives the view.
        unsafe {
            let () = msg_send![field, setDelegate: self as &AnyObject];
        }
    }

    /// Run `handler` whenever the text of `view` changes.
    pub fn on_text_change(&self, view: &NSTextView, handler: impl Fn() + 'static) {
        let sender: &AnyObject = view.as_ref();
        self.ivars()
            .text_changes
            .borrow_mut()
            .insert(key(sender), Rc::new(handler));
        // NSTextView delegates are unretained; this object outlives the view.
        view.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(self)));
    }

    /// Run `handler` when the selection of a combo box changes.
    pub fn on_selection_change(&self, combo: &NSControl, handler: impl Fn() + 'static) {
        let sender: &AnyObject = combo.as_ref();
        self.ivars()
            .selections
            .borrow_mut()
            .insert(key(sender), Rc::new(handler));
    }

    /// Forget every handler (views are being torn down).
    pub fn clear(&self) {
        let handlers = self.ivars();
        handlers.actions.borrow_mut().clear();
        handlers.text_changes.borrow_mut().clear();
        handlers.selections.borrow_mut().clear();
    }
}

/// A repeating or one-shot run-loop timer running a Rust closure.
///
/// The closure must hold only `Weak` references; call [`Timer::invalidate`]
/// (also done on drop) so the run loop releases the timer.
pub struct Timer {
    timer: Retained<NSTimer>,
}

impl Timer {
    pub fn schedule(interval: f64, repeats: bool, tick: impl Fn() + 'static) -> Timer {
        let block = block2::RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| tick());
        // SAFETY: the block is copied by the timer; `tick` is `'static`.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(interval, repeats, &block)
        };
        Timer { timer }
    }

    pub fn invalidate(&self) {
        self.timer.invalidate();
    }

    pub fn is_valid(&self) -> bool {
        self.timer.isValid()
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.invalidate();
    }
}
