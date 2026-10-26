//! Frosted-glass backdrop for the floating card.
//!
//! The widget paints per-pixel alpha through a layered window, so DWM never
//! blurs the desktop behind it on its own. `SetWindowCompositionAttribute` with
//! the acrylic accent asks the compositor to frost the region under the window;
//! when a build or a theme ignores the request, the translucent background is
//! still drawn, only without the blur — the widget never depends on it.

use std::ffi::c_void;
use std::mem::size_of;

use std::sync::atomic::{AtomicI32, Ordering};

use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn};
use windows::Win32::UI::HiDpi::GetDpiForWindow;

/// Corner radius of the card in logical pixels, published by the theme loader.
static CARD_RADIUS: AtomicI32 = AtomicI32::new(0);
/// `--no-glass` turns the compositor frost off for an A/B comparison.
static GLASS_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn disable_glass() {
    GLASS_DISABLED.store(true, Ordering::Relaxed);
}

fn glass_disabled() -> bool {
    GLASS_DISABLED.load(Ordering::Relaxed)
}

/// Remember the radius the active theme asked for. Zero disables rounding.
pub(crate) fn set_card_radius(radius: i32) {
    CARD_RADIUS.store(radius.max(0), Ordering::Relaxed);
    crate::diagnose::log_lazy(|| format!("card corner radius: {radius} logical px"));
}

/// Keep the widget out of screen captures.
///
/// The frost samples the desktop with `BitBlt`; without this the capture would
/// include the card itself and the blur would eat its own tail. Windows then
/// reports whatever sits behind the window instead.
pub(crate) fn exclude_from_capture(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    };
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok() }
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
/// rectangle: the acrylic frost behind it, and anything the compositor paints
/// outside the card, would square off the corners. A window region trims the
/// whole window — frost included — to the same curve the theme draws.
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
        }
        if applied == 0 {
            crate::diagnose::log_lazy(|| {
                format!(
                    "window region rejected for hwnd={:?} {width}x{height} radius {radius}",
                    hwnd
                )
            });
        }
    }
}

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
#[allow(dead_code)]
pub(crate) fn apply_glass(hwnd: HWND, tint: u32) -> bool {
    if glass_disabled() {
        return false;
    }
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

// ---------------------------------------------------------------------------
// Self-rendered frost
// ---------------------------------------------------------------------------

/// Capture the desktop behind `hwnd` and blur it into a backdrop the renderer
/// can paint under the card.
///
/// DWM's acrylic tint is applied to the window rectangle and ignores the
/// rounded region, which is what leaves square corners on a rounded card.
/// Blurring the capture ourselves keeps the frost inside the very pixels the
/// card draws, so the cut-out is exactly the theme's outline.
pub(crate) fn capture_behind(hwnd: HWND, width: u32, height: u32) -> Option<Vec<u32>> {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ, SRCCOPY,
    };

    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    let mut rect = windows::Win32::Foundation::RECT::default();
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    }

    // Blur at a fraction of the final size: cheap, and the upscale hides the
    // sampling grid.
    let small_w = (width / 8).max(6);
    let small_h = (height / 8).max(6);

    let mut small = vec![0u8; (small_w * small_h * 4) as usize];
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, small_w as i32, small_h as i32);
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory,
            0,
            0,
            small_w as i32,
            small_h as i32,
            Some(screen),
            rect.left,
            rect.top,
            SRCCOPY,
        )
        .is_ok();

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = small_w as i32;
        info.bmiHeader.biHeight = -(small_h as i32);
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;
        let lines = if copied {
            GetDIBits(
                memory,
                bitmap,
                0,
                small_h,
                Some(small.as_mut_ptr().cast::<c_void>()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        let _ = SelectObject(memory, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        if lines == 0 {
            return None;
        }
    }

    // Two box passes turn the capture into frosted glass without banding.
    let mut small = small;
    box_blur(&mut small, small_w, small_h, 2);
    box_blur(&mut small, small_w, small_h, 2);

    // Upscale to the card and tint it dark and slightly cool, so text keeps its
    // contrast over a bright desktop.
    let mut out = vec![0u32; (width * height) as usize];
    for y in 0..height {
        let sy = (y as f32 + 0.5) * small_h as f32 / height as f32 - 0.5;
        let y0 = sy.floor().max(0.0) as u32;
        let y1 = (y0 + 1).min(small_h - 1);
        let fy = (sy - sy.floor()).clamp(0.0, 1.0);
        for x in 0..width {
            let sx = (x as f32 + 0.5) * small_w as f32 / width as f32 - 0.5;
            let x0 = sx.floor().max(0.0) as u32;
            let x1 = (x0 + 1).min(small_w - 1);
            let fx = (sx - sx.floor()).clamp(0.0, 1.0);
            let sample = |px: u32, py: u32, channel: usize| -> f32 {
                let index = ((py * small_w + px) * 4) as usize + channel;
                small[index] as f32
            };
            let mix = |channel: usize| -> f32 {
                let top = sample(x0, y0, channel) * (1.0 - fx) + sample(x1, y0, channel) * fx;
                let bottom = sample(x0, y1, channel) * (1.0 - fx) + sample(x1, y1, channel) * fx;
                top * (1.0 - fy) + bottom * fy
            };
            // BGRA from GDI; the theme buffer is premultiplied RGBA.
            let b = mix(0);
            let g = mix(1);
            let r = mix(2);
            let tint = 0.42;
            let r = (r * (1.0 - tint) + 22.0 * tint).clamp(0.0, 255.0) as u32;
            let g = (g * (1.0 - tint) + 20.0 * tint).clamp(0.0, 255.0) as u32;
            let b = (b * (1.0 - tint) + 26.0 * tint).clamp(0.0, 255.0) as u32;
            out[(y * width + x) as usize] = (255 << 24) | (r << 16) | (g << 8) | b;
        }
    }
    Some(out)
}

fn box_blur(pixels: &mut [u8], width: u32, height: u32, radius: i32) {
    let (w, h) = (width as i32, height as i32);
    if w <= 0 || h <= 0 || radius <= 0 {
        return;
    }
    let mut source = pixels.to_vec();
    for y in 0..h {
        for x in 0..w {
            let mut sums = [0i32; 3];
            let mut count = 0i32;
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    let sx = x + dx;
                    let sy = y + dy;
                    if sx < 0 || sy < 0 || sx >= w || sy >= h {
                        continue;
                    }
                    let index = ((sy * w + sx) * 4) as usize;
                    sums[0] += source[index] as i32;
                    sums[1] += source[index + 1] as i32;
                    sums[2] += source[index + 2] as i32;
                    count += 1;
                }
            }
            let index = ((y * w + x) * 4) as usize;
            for channel in 0..3 {
                pixels[index + channel] = (sums[channel] / count.max(1)) as u8;
            }
            pixels[index + 3] = 255;
        }
    }
    source.clear();
}
