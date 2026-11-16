//! Queries and selection of the current macOS keyboard input source (IME) via
//! the Carbon Text Input Source (TIS) APIs.
//!
//! TIS APIs must run on the main thread; selection is dispatched onto the main
//! queue, queries run on the caller's (already main-thread) context.

use std::ffi::c_void;
use std::ptr::NonNull;

use dispatch2::DispatchQueue;
use objc2_core_foundation::{CFArray, CFRetained, CFString, CFType};

// Carbon Text Input Services. See crates/computer_use/src/mac/keycode_cache.rs for
// the same API family used for keyboard layout lookups.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> Option<NonNull<CFType>>;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *mut CFType;
    fn TISCreateInputSourceList(
        properties: *const c_void,
        include_all_input_sources: u8,
    ) -> Option<NonNull<CFArray>>;
    fn TISSelectInputSource(source: &CFType) -> i32;

    static kTISPropertyInputSourceID: &'static CFString;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: &CFArray) -> isize;
    fn CFArrayGetValueAtIndex(array: &CFArray, index: isize) -> *const c_void;
}

/// Returns the unique ID (e.g. "com.apple.keylayout.ABC") of the currently
/// selected keyboard input source.
pub fn current_input_source_id() -> Option<String> {
    // TISCopy* follows CF "Copy" semantics; wrap in CFRetained to release on drop.
    let source = unsafe { CFRetained::from_raw(TISCopyCurrentKeyboardInputSource()?) };
    // TISGet* returns a borrowed reference owned by the source.
    let property = unsafe { TISGetInputSourceProperty(&source, kTISPropertyInputSourceID) };
    let id = unsafe { &*NonNull::new(property)?.cast::<CFString>().as_ptr() };
    Some(id.to_string())
}

/// Queues selection of the keyboard input source with the given unique ID on
/// the main queue.
///
/// TISSelectInputSource blocks on a synchronous XPC round trip with the IME
/// host process and may spin a nested run loop while waiting, during which
/// AppKit re-enters Warp's input handling. Calling it while AppContext is
/// borrowed (e.g. from within event dispatch) therefore double-borrows its
/// RefCell and panics; deferring to the main queue runs the call outside the
/// dispatch that requested it.
pub fn select_input_source(source_id: &str) {
    let source_id = source_id.to_owned();
    DispatchQueue::main().exec_async(move || {
        apply_input_source_selection(&source_id);
    });
}

/// Enumerates the available sources, matches on ID and selects. Logs failures.
fn apply_input_source_selection(source_id: &str) {
    if current_input_source_id().as_deref() == Some(source_id) {
        return;
    }
    // Passing NULL properties with include_all=true enumerates every registered
    // input source, including built-in keyboard layouts that are not in the
    // user's enabled IME list.
    let Some(ptr) = (unsafe { TISCreateInputSourceList(std::ptr::null(), 1) }) else {
        log::warn!("Failed to enumerate input sources while selecting {source_id}");
        return;
    };
    // TISCreate* follows CF "Create" semantics; wrap in CFRetained to release on drop.
    let list = unsafe { CFRetained::from_raw(ptr) };

    let count = unsafe { CFArrayGetCount(&list) };
    for index in 0..count {
        let element = unsafe { CFArrayGetValueAtIndex(&list, index) };
        if element.is_null() {
            continue;
        }
        // Elements are borrowed from the (retained) list.
        let source = unsafe { &*(element as *const CFType) };
        let property = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
        if property.is_null() {
            continue;
        }
        let id = unsafe { &*(property as *const CFString) };
        if id.to_string() == source_id {
            if unsafe { TISSelectInputSource(source) } != 0 {
                log::warn!("Failed to select input source {source_id}");
            }
            return;
        }
    }
    log::warn!("Input source {source_id} not found");
}
