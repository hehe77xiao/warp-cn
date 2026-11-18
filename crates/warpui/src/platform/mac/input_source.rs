//! Queries and selection of the current macOS keyboard input source (IME) via
//! the Carbon Text Input Source (TIS) APIs.
//!
//! TIS APIs must run on the main thread; selection is dispatched onto the main
//! queue, queries run on the caller's (already main-thread) context.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use dispatch2::DispatchQueue;
use instant::Instant;
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

unsafe extern "C" {
    /// host_view.m: forces the active text input context to re-resolve its IME
    /// instance against the selected source (see the comment there for why
    /// deactivate/activate is needed). Returns nonzero only when an active
    /// context existed and the fix actually ran.
    fn warp_fix_current_input_context() -> std::os::raw::c_char;
}

thread_local! {
    /// The input source the active context was last rebuilt for. Pane-focus
    /// events fire on nearly every keystroke in this app, and rebuilding the
    /// context each time would re-run the AppKit/IMK session handshake per key.
    /// Only recorded when a fix actually ran (an active context existed), so a
    /// startup attempt before any context is up does not poison the cache.
    static LAST_FIXED_SOURCE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Queues a re-resolve of the active text input context on the main queue,
/// applied only when the selected input source changed since the last
/// completed rebuild.
///
/// Must not run synchronously inside view/model event dispatch: the underlying
/// TSM fix can make synchronous IMK XPC calls that spin a nested run loop,
/// during which AppKit re-enters Warp's input handling (see
/// [`select_input_source`] for the same reasoning).
pub fn fix_current_input_context() {
    DispatchQueue::main().exec_async(|| {
        let Some(current) = current_input_source_id() else {
            return;
        };
        let already_fixed = LAST_FIXED_SOURCE
            .with(|last_fixed| last_fixed.borrow().as_deref() == Some(current.as_str()));
        if already_fixed {
            return;
        }
        let started = Instant::now();
        let fixed = unsafe { warp_fix_current_input_context() != 0 };
        log::info!(
            "input-context fix for {current} ran={fixed} in {:?}",
            started.elapsed(),
        );
        if fixed {
            LAST_FIXED_SOURCE.with(|last_fixed| *last_fixed.borrow_mut() = Some(current));
        }
    });
}

/// Resolves the cached source handle and selects it, logging the elapsed time:
/// the blocking XPC wait inside `TISSelectInputSource` (plus AppKit's follow-up
/// IMK menu rebuild) is exactly what the user perceives as a UI freeze, so the
/// duration is recorded unconditionally to make future stalls diagnosable from
/// logs alone.
fn apply_input_source_selection(source_id: &str) {
    let Some(current) = current_input_source_id() else {
        return;
    };
    if current == source_id {
        return;
    }
    let Some(source) = cached_input_source(source_id) else {
        log::warn!("Input source {source_id} not found");
        return;
    };
    let started = Instant::now();
    let status = unsafe { TISSelectInputSource(&source) };
    log::info!(
        "input-source switch {current} -> {source_id} completed in {:?} (status {status})",
        started.elapsed(),
    );
    if status != 0 {
        log::warn!("Failed to select input source {source_id}");
        // A removed/renamed source keeps its stale handle cached; drop it so the
        // next lookup re-enumerates.
        SOURCE_CACHE.with(|cache| cache.borrow_mut().remove(source_id));
    } else {
        // TISSelectInputSource only changes the global source: the focused
        // window's TSM document stays bound to the previous IME session until
        // it is fixed, and until then every keystroke pays a synchronous IMK
        // XPC round trip to the old host (multi-second main-thread stalls
        // observed with third-party IMEs). Rebind it on the next main-queue
        // turn so key events resolve against the newly selected source.
        fix_current_input_context();
    }
}

thread_local! {
    /// Source ID -> retained TIS handle, filled by one full enumeration.
    /// Enumerating on every switch costs tens of ms, and handles survive the
    /// list they came from once retained; an entry is only dropped after a
    /// failed selection, which triggers a fresh enumeration on next use.
    static SOURCE_CACHE: RefCell<HashMap<String, CFRetained<CFType>>> = RefCell::new(HashMap::new());
}

fn cached_input_source(source_id: &str) -> Option<CFRetained<CFType>> {
    if let Some(handle) = SOURCE_CACHE.with(|cache| cache.borrow().get(source_id).cloned()) {
        return Some(handle);
    }
    populate_source_cache();
    SOURCE_CACHE.with(|cache| cache.borrow().get(source_id).cloned())
}

fn populate_source_cache() {
    // Passing NULL properties with include_all=true enumerates every registered
    // input source, including built-in keyboard layouts that are not in the
    // user's enabled IME list.
    let Some(ptr) = (unsafe { TISCreateInputSourceList(std::ptr::null(), 1) }) else {
        log::warn!("Failed to enumerate input sources while populating the source cache");
        return;
    };
    // TISCreate* follows CF "Create" semantics; wrap in CFRetained to release on drop.
    let list = unsafe { CFRetained::from_raw(ptr) };

    let count = unsafe { CFArrayGetCount(&list) };
    let mut entries: Vec<(String, CFRetained<CFType>)> = Vec::new();
    for index in 0..count {
        let element = unsafe { CFArrayGetValueAtIndex(&list, index) };
        if element.is_null() {
            continue;
        }
        // Elements are borrowed from the (retained) list; retain each handle we
        // keep so it outlives the list.
        let source = unsafe { &*(element as *const CFType) };
        let property = unsafe { TISGetInputSourceProperty(source, kTISPropertyInputSourceID) };
        if property.is_null() {
            continue;
        }
        let id = unsafe { &*(property as *const CFString) };
        let handle = unsafe { CFRetained::retain(NonNull::from(source)) };
        entries.push((id.to_string(), handle));
    }
    SOURCE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.clear();
        for (id, handle) in entries {
            cache.insert(id, handle);
        }
    });
}
