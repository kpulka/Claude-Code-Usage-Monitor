//! A GPU backdrop plate that sits behind the card.
//!
//! DWM can blur whatever lies behind a window and hand the result straight to
//! the compositor (`CreateHostBackdropBrush`). Clipped by a rounded geometry
//! that blur follows the card's corner radius with antialiasing, and it costs
//! the application nothing per frame - the alternative is a screenshot, which
//! pays a full vblank (~16 ms) every time the desktop behind changes.
//!
//! The plate is a separate, click-through window placed directly under the
//! card: the card keeps presenting premultiplied pixels through
//! `UpdateLayeredWindow`, and its translucent background lets the blurred plate
//! show through. When any step here fails the caller keeps the CPU frost.

use std::cell::RefCell;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_HOSTBACKDROPBRUSH};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::{
    DispatcherQueueOptions, RoInitialize, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
    RO_INIT_MULTITHREADED,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowRect, LoadCursorW, RegisterClassExW, SetWindowPos,
    ShowWindow, HTTRANSPARENT, IDC_ARROW, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING,
    SW_SHOWNOACTIVATE, WM_NCHITTEST, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{Compositor, ContainerVisual, SpriteVisual};
use windows_numerics::Vector2;

struct Plate {
    hwnd: HWND,
    owner: HWND,
    /// Kept alive for as long as the plate exists; dropping any of these tears
    /// the visual tree down and the window goes blank.
    _compositor: Compositor,
    _target: DesktopWindowTarget,
    _root: ContainerVisual,
    visual: SpriteVisual,
    placed: bool,
}

thread_local! {
    static PLATE: RefCell<Option<Plate>> = const { RefCell::new(None) };
}

/// Whether a plate is installed; the caller then skips its own frost capture.
pub(crate) fn is_active() -> bool {
    PLATE.with(|plate| plate.borrow().is_some())
}

/// Install the plate under `owner`. Returns false when the compositor or the
/// backdrop brush is unavailable, leaving the CPU frost in charge.
pub(crate) fn install(owner: HWND) -> bool {
    if is_active() {
        return true;
    }
    // WinRT activation needs an initialized apartment; a UI thread that is
    // already STA reports RPC_E_CHANGED_MODE, which is fine.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    // The compositor refuses to serve a thread without a dispatcher queue, and
    // the windows crate binds no import for it, so it is resolved by hand. The
    // controller is deliberately leaked: it has to outlive the compositor, and
    // the widget lives for the whole process anyway.
    unsafe {
        ensure_dispatcher_queue();
    }
    match build(owner) {
        Ok(plate) => {
            PLATE.with(|slot| *slot.borrow_mut() = Some(plate));
            true
        }
        Err(error) => {
            crate::diagnose::log(format!("glass plate unavailable: {error}"));
            false
        }
    }
}

fn build(owner: HWND) -> windows::core::Result<Plate> {
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = crate::native_interop::wide_str("CCUMGlassPlate");
    let title = crate::native_interop::wide_str("Usage glass plate");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(plate_proc),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        lpszClassName: PCWSTR::from_raw(class.as_ptr()),
        ..Default::default()
    };
    unsafe {
        RegisterClassExW(&wc);
    }
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        )?
    };
    if hwnd.is_invalid() {
        return Err(windows::core::Error::from(
            windows::Win32::Foundation::E_FAIL,
        ));
    }

    // Without this attribute the composition target draws over nothing: DWM
    // refuses to sample the desktop behind the window.
    let enable: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_HOSTBACKDROPBRUSH,
            &enable as *const i32 as *const core::ffi::c_void,
            std::mem::size_of::<i32>() as u32,
        )?;
    }

    let compositor = Compositor::new()?;
    let interop: ICompositorDesktopInterop = compositor.cast()?;
    let target = unsafe { interop.CreateDesktopWindowTarget(hwnd, false) }?;
    let root = compositor.CreateContainerVisual()?;
    // A backdrop brush is only accepted as a sprite's brush, not as a shape's
    // fill; the rounded shape comes from the window region instead.
    let brush = compositor.CreateHostBackdropBrush()?;
    let visual = compositor.CreateSpriteVisual()?;
    visual.SetBrush(&brush)?;
    visual.SetSize(Vector2 { X: 1.0, Y: 1.0 })?;
    root.Children()?.InsertAtTop(&visual)?;
    root.SetRelativeSizeAdjustment(Vector2 { X: 1.0, Y: 1.0 })?;
    target.SetRoot(&root)?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }

    Ok(Plate {
        hwnd,
        owner,
        _compositor: compositor,
        _target: target,
        _root: root,
        visual,
        placed: false,
    })
}

/// Follow the card: same rectangle, same corner radius. Called whenever the
/// card is positioned or resized.
pub(crate) fn follow(card: HWND) {
    PLATE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(plate) = slot.as_mut() else {
            return;
        };
        let mut rect = windows::Win32::Foundation::RECT::default();
        unsafe {
            if GetWindowRect(card, &mut rect).is_err() {
                return;
            }
        }
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        let scale = unsafe { GetDpiForWindow(card) }.max(96) as f32 / 96.0;
        unsafe {
            // Insert the plate right below the card, so both stay in the same
            // topmost band and no other window slips between them.
            let _ = SetWindowPos(
                plate.hwnd,
                Some(plate.owner),
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING,
            );
        }
        let dips_w = width as f32 / scale;
        let dips_h = height as f32 / scale;
        let _ = plate.visual.SetSize(Vector2 {
            X: dips_w,
            Y: dips_h,
        });
        // The compositor clips a window's visuals to its visible region, so the
        // plate is cut to the same curve as the card.
        crate::backdrop::refresh_region(plate.hwnd, width, height);
        if !plate.placed {
            plate.placed = true;
            crate::diagnose::log("glass plate: host backdrop brush is live");
        }
    });
}

/// Create a `DispatcherQueueController` for the calling thread, which the
/// compositor requires before it will hand out a desktop window target.
unsafe fn ensure_dispatcher_queue() {
    let library = match LoadLibraryW(windows::core::w!("CoreMessaging.dll")) {
        Ok(library) => library,
        Err(_) => return,
    };
    let Some(create) = GetProcAddress(
        library,
        windows::core::PCSTR(b"CreateDispatcherQueueController\0".as_ptr()),
    ) else {
        return;
    };
    type Create = unsafe extern "system" fn(
        DispatcherQueueOptions,
        *mut *mut core::ffi::c_void,
    ) -> windows::core::HRESULT;
    let create: Create = std::mem::transmute(create);
    let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };
    let mut controller: *mut core::ffi::c_void = std::ptr::null_mut();
    let _ = create(options, &mut controller);
}

unsafe extern "system" fn plate_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCHITTEST {
        // Clicks belong to whatever is under the plate.
        return LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}
