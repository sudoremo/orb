//! The Accessibility permission, which `navigation.escalate_command` needs.
//!
//! The command runs as Orb's child, so macOS attributes what it asks of other
//! applications' windows to Orb. Without the grant, focusing another window
//! fails silently, and the keys that should have left Orb seem to do nothing.

/// Ask for the permission unless Orb already has it. macOS shows its own
/// prompt, which leads to the switch in System Settings; nothing waits on the
/// answer, since the command is only run once a key asks for it.
#[cfg(target_os = "macos")]
pub fn request() {
    use objc2_foundation::{NSDictionary, NSNumber, NSString};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
    }

    // The value of `kAXTrustedCheckOptionPrompt`, which is a CFString and
    // reads the same as an NSString.
    let key = NSString::from_str("AXTrustedCheckOptionPrompt");
    let prompt = NSNumber::new_bool(true);
    let options = NSDictionary::from_slices(&[&*key], &[&*prompt]);
    // SAFETY: an NSDictionary is toll-free bridged to the CFDictionary the
    // function takes, and `options` outlives the call.
    unsafe {
        AXIsProcessTrustedWithOptions(objc2::rc::Retained::as_ptr(&options).cast());
    }
}

#[cfg(not(target_os = "macos"))]
pub fn request() {}
