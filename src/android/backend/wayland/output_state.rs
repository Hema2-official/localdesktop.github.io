/// Tell the guest link the Android window's UI scale, for a nested wlroots compositor's outputs
/// (`guest::output`).
pub fn set_guest_output_scale(scale: i32) {
    if scale > 0 {
        crate::android::guest::output::set_scale(scale as u32);
    }
}
