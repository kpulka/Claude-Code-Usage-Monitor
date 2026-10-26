//! Frosted-glass backdrop for the floating card.
//!
//! The widget paints per-pixel alpha through a layered window, so DWM never
//! blurs the desktop behind it on its own. `SetWindowCompositionAttribute` with
//! the acrylic accent asks the compositor to frost the region under the window;
//! when a build or a theme ignores the request, the translucent background is
//! still drawn, only without the blur — the widget never depends on it.

use std::ffi::c_void;
use std::mem::size_of;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

#[repr(C)]
struct AccentPolicy {
    state: u32,
    flags: u32,
    /// Acrylic tint in AABBGGRR order.
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

type SetWindowCompositionAttributeFn =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> i32;

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
const ACCENT_ENABLE_BLURBEHIND: u32 = 3;

/// Ask the compositor to frost the desktop behind `hwnd`.
///
/// `tint` is AABBGGRR; a higher alpha frosts more strongly. Returns true when
/// the compositor accepted the request, so the caller can log which look the
/// user is actually getting.
pub(crate) fn apply_glass(hwnd: HWND, tint: u32) -> bool {
    unsafe {
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else {
            return false;
        };
        let Some(entry) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else {
            return false;
        };
        let set_attribute: SetWindowCompositionAttributeFn = std::mem::transmute(entry);

        // Acrylic first (Windows 10 1803+ and Windows 11); fall back to the
        // plain blur accent that older compositors understand.
        for (state, color) in [
            (ACCENT_ENABLE_ACRYLICBLURBEHIND, tint),
            (ACCENT_ENABLE_BLURBEHIND, 0),
        ] {
            let mut policy = AccentPolicy {
                state,
                flags: 0,
                gradient_color: color,
                animation_id: 0,
            };
            let mut data = WindowCompositionAttributeData {
                attribute: WCA_ACCENT_POLICY,
                data: &mut policy as *mut AccentPolicy as *mut c_void,
                size: size_of::<AccentPolicy>(),
            };
            if set_attribute(hwnd, &mut data) != 0 {
                return true;
            }
        }
        let _ = PCWSTR::null();
        false
    }
}
