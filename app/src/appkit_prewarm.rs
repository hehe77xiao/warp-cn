//! Warms AppKit's lazily soft-linked system frameworks shortly after launch.
//!
//! Stack captures repeatedly caught the main thread stalling ~0.5-1s during a
//! live session inside `_sl_dlopen`: AppKit soft-loads the WritingTools
//! frameworks when it first customizes the text-input menu, and HIServices
//! loads the accessibility bundles when it first services an incoming AX
//! attribute query. Paying that cost once during the idle window after launch
//! makes the real first-use instant.

use dispatch2::{DispatchQueue, DispatchTime};
use instant::Duration;

const SOFT_LINKED_FRAMEWORKS: [&std::ffi::CStr; 3] = [
    c"/System/Library/PrivateFrameworks/WritingTools.framework/Versions/A/WritingTools",
    c"/System/Library/PrivateFrameworks/WritingToolsUI.framework/Versions/A/WritingToolsUI",
    c"/System/Library/PrivateFrameworks/AccessibilityBundles.framework/Versions/A/AccessibilityBundles",
];

pub fn schedule() {
    let when =
        DispatchTime::try_from(Duration::from_secs(3)).unwrap_or(dispatch2::DispatchTime::NOW);
    // Loaded on the main queue exactly as AppKit would soft-load them later;
    // image initializers in these frameworks may assume the main thread.
    let _ = DispatchQueue::main().after(when, || {
        for path in SOFT_LINKED_FRAMEWORKS {
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if handle.is_null() {
                log::debug!("appkit prewarm dlopen failed for {path:?}");
            }
        }
    });
}
