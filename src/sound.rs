//! Alert sound.
//!
//! The Windows notification chime is played through `PlaySoundW` with a system
//! alias, so the app ships no audio assets and respects the user's current
//! sound scheme. When the alias is unavailable, a plain message beep keeps the
//! alert audible instead of failing silently.
//!
//! `play_alert` reports which path it took so `--test-alert-sound` and the
//! diagnostics log can show that the call reached the sound device.

#[cfg(windows)]
pub(crate) fn play_alert() -> &'static str {
    use windows::core::w;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_ICONASTERISK;

    let played = unsafe {
        PlaySoundW(
            w!("SystemNotification"),
            None,
            SND_ALIAS | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if played.as_bool() {
        return "system notification alias";
    }
    // No such alias in this sound scheme (or no audio device): fall back to the
    // standard beep so the reset is still announced.
    match unsafe { MessageBeep(MB_ICONASTERISK) } {
        Ok(()) => "message beep",
        Err(_) => "silent (no audio device)",
    }
}
