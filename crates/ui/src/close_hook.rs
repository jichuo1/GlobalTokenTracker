//! Title-bar close interception: `SetWindowSubclass` on the main HWND swallows
//! `WM_CLOSE` and re-routes it as `Msg::CloseRequested`, so the shell can ask
//! "quit vs hide-to-tray" instead of dying. `AppWindow.Closing` (the WinUI3
//! cancelable event) is unreachable — reactor 0.100.0 exposes no accessor.
//!
//! `LocalSender` is `Rc`-bound, so it lives in thread-local storage: the
//! subclass proc always runs on the window's own (UI) thread. `allow_close`
//! is a consume-once flag set right before `request_close()` — without it our
//! own proc would swallow the programmatic close and the app could never exit.
//!
//! The same subclass answers `WM_GETMINMAXINFO`: below `MIN_OUTER_W` the tables
//! and the title bar (brand + centered nav + caption buttons) no longer fit, so
//! the window simply can't be dragged narrower than that.

use crate::Msg;
use std::cell::{Cell, RefCell};
use windows_reactor::LocalSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MINMAXINFO, WM_CLOSE, WM_GETMINMAXINFO, WM_NCDESTROY,
};

const SUBCLASS_ID: usize = 0x4754_5431; // "GTT1"

/// Smallest outer window size (DIPs, frame included) — a 720-wide client is
/// the narrowest the detail table and the centered nav are laid out for.
pub const MIN_OUTER_W: i32 = 736;
pub const MIN_OUTER_H: i32 = 560;

thread_local! {
    static SENDER: RefCell<Option<LocalSender<Msg>>> = const { RefCell::new(None) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
    /// Consume-once pass-through: the next WM_CLOSE reaches the real close.
    static ALLOW_CLOSE: Cell<bool> = const { Cell::new(false) };
}

/// Idempotent — call every view build; returns early once the subclass is on.
/// The HWND lookup needs the window title applied, so the first few calls may
/// just cache the sender and retry on the next frame.
pub fn ensure_installed(sender: &LocalSender<Msg>) {
    SENDER.with(|s| *s.borrow_mut() = Some(sender.clone()));
    if INSTALLED.with(|i| i.get()) {
        return;
    }
    #[cfg(windows)]
    unsafe {
        // PID-filtered lookup — with a second GTT instance running, a plain
        // title search can land on the other process's HWND and the subclass
        // would never attach (cross-process subclassing fails, retried forever).
        let hwnd = crate::tray::main_hwnd();
        if !hwnd.is_null() && SetWindowSubclass(hwnd, Some(close_proc), SUBCLASS_ID, 0) != 0 {
            INSTALLED.with(|i| i.set(true));
        }
    }
}

/// Set right before `context.window().request_close()` — the programmatic
/// close must pass our own WM_CLOSE swallow or the app can never exit.
pub fn allow_next_close() {
    ALLOW_CLOSE.with(|f| f.set(true));
}

#[cfg(windows)]
unsafe extern "system" fn close_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                if ALLOW_CLOSE.with(|f| f.replace(false)) {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                }
                SENDER.with(|s| {
                    if let Some(sender) = &*s.borrow() {
                        sender.send(Msg::CloseRequested);
                    }
                });
                0 // swallow — the window stays
            }
            WM_GETMINMAXINFO => {
                let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                let mmi = lparam as *mut MINMAXINFO;
                if !mmi.is_null() {
                    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
                    let m = &mut *mmi;
                    m.ptMinTrackSize.x = m.ptMinTrackSize.x.max(MIN_OUTER_W * dpi / 96);
                    m.ptMinTrackSize.y = m.ptMinTrackSize.y.max(MIN_OUTER_H * dpi / 96);
                }
                r
            }
            WM_NCDESTROY => {
                // The HWND is going away — drop the subclass and the stale
                // sender, then let the real proc finish destroying.
                RemoveWindowSubclass(hwnd, Some(close_proc), SUBCLASS_ID);
                SENDER.with(|s| *s.borrow_mut() = None);
                INSTALLED.with(|i| i.set(false));
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
            _ => DefSubclassProc(hwnd, msg, wparam, lparam),
        }
    }
}
