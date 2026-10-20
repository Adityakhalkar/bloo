/// Whether System Settings › Accessibility › Display › Reduce motion is on.
#[cfg(target_os = "macos")]
pub fn system_prefers_reduced_motion() -> bool {
    use objc2::msg_send;
    use objc2_app_kit::NSWorkspace;

    let workspace = NSWorkspace::sharedWorkspace();
    // objc2-app-kit only binds this behind its `NSAccessibility` feature, which the workspace
    // doesn't enable, so call the selector directly instead of widening every crate's features.
    unsafe { msg_send![&*workspace, accessibilityDisplayShouldReduceMotion] }
}

#[cfg(not(target_os = "macos"))]
pub fn system_prefers_reduced_motion() -> bool {
    false
}
