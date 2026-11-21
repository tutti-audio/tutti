//! Host-side support for AU Cocoa editor views.
//!
//! [`AuEditor`] loads the `NSView` the plugin advertises via
//! `kAudioUnitProperty_CocoaUI`, retains it, and optionally attaches it as a
//! subview of a caller-provided parent `NSView`.

// AudioUnit is a raw opaque C pointer; every public helper in this module
// takes one and delegates to AudioToolbox calls that expect a valid unit.

mod cocoa;

use objc2::msg_send;
use objc2::runtime::AnyObject;
use objc2_foundation::NSRect;

use dpi::LogicalSize;
use tutti_plugin_types::{MainThreadMarker, WindowHandle};

use crate::error::Result;
use crate::property::{CocoaUi, Element, Scope};
use crate::unit::Unit;

/// A retained `NSView` on its way to the main thread, where it is closed.
struct OwnedView(*mut AnyObject);

// SAFETY: the pointer is dereferenced only by `close`, which takes the
// `MainThreadMarker`; moving the value between threads touches nothing.
unsafe impl Send for OwnedView {}

impl OwnedView {
    /// Removes the view from its superview and releases the +1 reference the
    /// editor held.
    fn close(self, _main: MainThreadMarker) {
        // SAFETY: the `MainThreadMarker` parameter proves this is the main
        // thread, where AppKit may be called. `self.0` is the live +1 view an
        // editor owned and handed over (non-null; it is released once, here,
        // as `self` is consumed).
        unsafe {
            let _: () = msg_send![self.0, removeFromSuperview];
            let _: () = msg_send![self.0, release];
        }
    }
}

/// Owned handle to an AU's Cocoa editor view.
///
/// The editor view is released when the handle is dropped. The handle is
/// `Send` because the retained `NSView` pointer is self-contained, but all
/// AppKit calls must still originate from the main thread: every entry takes
/// the [`MainThreadMarker`] that proves it, and `Drop`, which cannot take one,
/// asks `MainThreadMarker::new`: off the main thread it hands the close to the
/// unit's [`MainWorkHandle`](tutti_plugin_types::MainWorkHandle), counted in
/// `MainWork::deferred_drops`. If that collector is gone the view leaks, counted
/// in `MainWork::stranded`.
pub struct AuEditor {
    view: *mut AnyObject,
    unit: Unit,
}

impl std::fmt::Debug for AuEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuEditor").finish_non_exhaustive()
    }
}

// SAFETY: the view is touched only on the main thread: `open`, `close` and
// `editor_size` take the marker, and `Drop` hands the close to the main
// thread instead of making it off that thread, so a moved value never makes
// an AppKit call from another thread. The `Unit` it holds is `Send`.
unsafe impl Send for AuEditor {}

impl AuEditor {
    /// Instantiates the AU's Cocoa editor view and attach it as a subview of
    /// the caller's parent `NSView`. Pass `None` to instantiate the view
    /// without attaching it to a parent hierarchy.
    ///
    /// `preferred` is the size the host would like, in points (a Cocoa view lays
    /// out in them), passed to the factory as `inPreferredSize`
    /// (`AUCocoaUIView.h:47-48`). It is a *hint*: the plugin
    /// may return a view of any size, so read the actual frame back afterwards
    /// rather than assuming the request was honoured. It must be the host's real
    /// window size: a fixed figure tells every AU the host wants that size
    /// whatever window the editor is about to live in.
    ///
    /// `parent` is a `WindowHandle`, whose constructor carries the claim that
    /// its pointer is a live `NSView`.
    ///
    /// # Errors
    /// If the AU advertises no Cocoa view or the view factory fails to load.
    pub fn open(
        _main: MainThreadMarker,
        unit: &Unit,
        parent: Option<WindowHandle>,
        preferred: LogicalSize<u32>,
    ) -> Result<Self> {
        let view = cocoa::create_view(unit, preferred)?;

        if let Some(handle) = parent {
            let parent_obj = handle.as_ptr() as *mut AnyObject;
            if !parent_obj.is_null() {
                // SAFETY: the `MainThreadMarker` parameter proves this is the
                // main thread, where AppKit may be called. `WindowHandle`
                // carries its constructor's claim that the parent is a live
                // NSView; `view` is the live view just created, and
                // `addSubview:` retains it.
                let _: () = unsafe { msg_send![parent_obj, addSubview: view] };
            }
        }

        Ok(Self {
            view,
            unit: unit.clone(),
        })
    }

    /// Whether the AU advertises a Cocoa editor view.
    ///
    /// Calls `AudioUnitGetPropertyInfo(kAudioUnitProperty_CocoaUI)` and
    /// reports `true` if the property exists with a non-zero size.
    pub fn has_editor(unit: &Unit) -> bool {
        let info = unit.property_info::<CocoaUi>(Scope::GLOBAL, Element::ZERO);
        matches!(info, Ok(i) if i.size > 0)
    }

    /// Removes the view from its superview (if any) and releases it. Later calls
    /// are no-ops. This is the teardown on the main thread; `Drop` runs it there
    /// and defers it to the main thread elsewhere.
    pub fn close(&mut self, main: MainThreadMarker) {
        if let Some(view) = self.take_view() {
            view.close(main);
        }
    }

    /// Whether the editor still holds its view: `true` from [`open`](Self::open)
    /// until [`close`](Self::close).
    pub fn is_open(&self) -> bool {
        !self.view.is_null()
    }

    /// The view this editor owns, which it no longer does afterwards.
    fn take_view(&mut self) -> Option<OwnedView> {
        let view = std::mem::replace(&mut self.view, std::ptr::null_mut());
        (!view.is_null()).then_some(OwnedView(view))
    }

    /// Editor view frame size in points, rounded to whole points. Returns a
    /// zero size after [`close`](Self::close).
    pub fn editor_size(&self, _main: MainThreadMarker) -> LogicalSize<u32> {
        if self.view.is_null() {
            return LogicalSize::new(0, 0);
        }
        // SAFETY: the `MainThreadMarker` parameter proves this is the main
        // thread, where AppKit may be called. `view` is the live view this
        // editor owns (non-null, checked).
        unsafe {
            let frame: NSRect = msg_send![self.view, frame];
            LogicalSize::new(frame.size.width, frame.size.height).cast()
        }
    }

    /// The raw `NSView*` this editor owns, null after [`close`](Self::close), for
    /// a test that reads the view's retain count or compares two views.
    #[cfg(feature = "testing")]
    pub fn view_ptr(&self) -> *mut std::os::raw::c_void {
        self.view as *mut std::os::raw::c_void
    }

    /// Raw `AudioUnit` this editor was created for.
    pub fn unit(&self) -> &Unit {
        &self.unit
    }
}

impl Drop for AuEditor {
    fn drop(&mut self) {
        // Off the main thread the close goes to the main thread's collector
        // rather than to AppKit from the wrong thread. The queued closure holds
        // a clone of the unit, so the close runs before the unit can be
        // disposed: this editor's own `unit` field may be the last handle, and
        // a last drop on the unit's home thread disposes inline. The clone drops
        // after the close, on the main thread. A collector that is gone and a
        // caller off the main thread strand the closure: the view and the unit
        // both leak, counted in `MainWork::stranded`.
        match MainThreadMarker::new() {
            Some(main) => self.close(main),
            None => {
                if let Some(view) = self.take_view() {
                    let unit = self.unit.clone();
                    self.unit.work().defer_drop(move |main| {
                        view.close(main);
                        drop(unit);
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **AUDelay advertises a Cocoa view**: `has_editor` reads the property's
    /// size, and the host's lifecycle suite opens that view.
    #[test]
    fn audelay_advertises_a_cocoa_view() {
        let key = crate::ComponentKey::new(
            coreaudio_sys::kAudioUnitType_Effect,
            u32::from_be_bytes(*b"dely"),
            u32::from_be_bytes(*b"appl"),
        )
        .expect("an exact key");
        let comp = crate::ComponentRef::find(key).expect("AUDelay should be present");
        let (_work, handle) = tutti_plugin_types::MainWork::new();
        let unit = Unit::new(comp, &handle).expect("instantiate");
        unit.initialize().expect("initialize");
        assert!(
            AuEditor::has_editor(&unit),
            "AUDelay advertises kAudioUnitProperty_CocoaUI"
        );
    }
}
