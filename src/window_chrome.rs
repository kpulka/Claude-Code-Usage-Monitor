//! Window chrome the themes rely on: the card's rounded region, the corner
//! radius the active theme published, and the DWM frame suppression that stops
//! Windows drawing its own outline around the widget.
//!
//! Frost is deliberately not handled here. `crate::frost` owns the blurred plate
//! DWM composites behind the card; when that plate cannot be installed the card
//! still renders its own translucent background, just without a blur.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn};
use windows::Win32::UI::HiDpi::GetDpiForWindow;

/// Corner radius of the card in logical pixels, published by the theme loader.
static CARD_RADIUS: AtomicI32 = AtomicI32::new(0);

/// Remember the radius the active theme asked for. Zero disables rounding.
pub(crate) fn set_card_radius(radius: i32) {
    CARD_RADIUS.store(radius.max(0), Ordering::Relaxed);
    crate::diagnose::log_lazy(|| format!("card corner radius: {radius} logical px"));
}

/// Stop DWM from drawing its own frame around the widget.
///
/// Windows 11 rounds every top-level window and paints a 1px border plus a
/// shadow along *its* corner radius. Over a card with a different radius that
/// shows up as an outline that does not follow the cut. Turning the system
/// corner, border and non-client rendering off leaves the visible shape to the
/// window region the theme asks for.
pub(crate) fn suppress_system_frame(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE,
    };

    /// DWMWCP_DONOTROUND
    const NO_ROUND: u32 = 1;
    /// DWMWA_COLOR_NONE: the border colour value that means "no border".
    const NO_BORDER: u32 = 0xFFFF_FFFE;

    unsafe {
        let corner = NO_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
        let border = NO_BORDER;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        );
        let policy = DWMNCRP_DISABLED;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &policy as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );
    }
}

/// Clip the window to the card's rounded outline.
///
/// The widget renders its own rounded background, but the window itself is a
/// rectangle: anything painted outside the card — the frost plate peeking past
/// the curve, a compositor surface, a drop shadow — would square off the
/// corners. A window region trims the whole window to the same curve the theme
/// draws.
pub(crate) fn refresh_region(hwnd: HWND, width: i32, height: i32) {
    let logical = CARD_RADIUS.load(Ordering::Relaxed);
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(96) as f64 / 96.0;
    let radius = (logical as f64 * scale).round() as i32;
    if radius <= 0 || width <= 0 || height <= 0 {
        // A theme without rounding gets its square window back.
        unsafe {
            let _ = SetWindowRgn(hwnd, None, true);
        }
        return;
    }
    static LAST: AtomicI32 = AtomicI32::new(-1);
    let signature = (logical << 16) ^ (width << 4) ^ height;
    if LAST.swap(signature, Ordering::Relaxed) != signature {
        crate::diagnose::log_lazy(|| {
            format!("card window region: {width}x{height} radius {radius}px")
        });
    }
    unsafe {
        // CreateRoundRectRgn takes an exclusive bottom-right corner.
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2);
        if region.is_invalid() {
            return;
        }
        let applied = SetWindowRgn(hwnd, Some(region), true);
        if applied == 0 {
            // The window keeps ownership only when the call succeeds.
            let _ = DeleteObject(region.into());
            crate::diagnose::log_lazy(|| {
                format!(
                    "window region rejected for hwnd={:?} {width}x{height} radius {radius}",
                    hwnd
                )
            });
        }
    }
}
