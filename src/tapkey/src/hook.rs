//! WH_MOUSE_LL hook + double-tap detection (pass-through model).
//!
//! t1 passes through to the app immediately (zero latency, zero swallowed input). A same-point
//! t2 within `DOUBLE_TAP_WINDOW_MS` is swallowed (down+up) and posts WM_TAPKEY_OPEN with anchor P.
//! No t2 within the window → nothing happens (t1 was already delivered as a normal click).
//!
//! NO re-injection path exists: the only swallowed event is t2, and it is consumed by the
//! overlay, so there is never a buffered tap to replay — no flush-deadline / orphan-up /
//! exactly-once-delivery complexity and no re-injection feedback loop (docs/tap-signature.md).
//! Trade-off: on trigger, t1 lands as a single click on the target (harmless, e.g. file
//! selection); a false trigger is just a spurious overlay pop.
//!
//! The callback runs on the installing thread's message loop and must stay fast:
//! atomics + one mutex + PostMessage only — no UI work, no SendInput, no timers.

use std::sync::{atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicU8, Ordering}, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::Result;
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const EXTRA_MARKER: u32 = 0x54415050;   // "TAPP" — marks self-injected events so the hook skips them
// Detection parameters are runtime-configurable: read crate::config atomics per event —
// double_tap_window_ms (default 250, natural rhythm 223–257ms per docs/tap-signature.md)
// and tap_radius_px (default 0 = exact match; raise to 2 if jitter appears).

/// Posted to the message window on double-tap: wParam=x, lParam=y (anchor P).
pub const WM_TAPKEY_OPEN: u32 = WM_APP + 1;
/// Posted to the message window when a click lands outside the visible overlay layer.
pub const WM_TAPKEY_OUTSIDE: u32 = WM_APP + 2;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// The last passed-through tap — a potential t1 awaiting a same-point t2.
#[derive(Clone, Copy)]
struct LastTap { t: u64, x: i32, y: i32 }

static DETECTION_ON: AtomicBool = AtomicBool::new(true);
static LAST: Mutex<Option<LastTap>> = Mutex::new(None);
/// t2's swallowed LUP still owed (the only up we ever eat). Time-bounded orphan guard.
static PENDING_UPS: AtomicU8 = AtomicU8::new(0);
static HOLD_DOWN_AT: AtomicU64 = AtomicU64::new(0); // ms of the swallowed t2 LDOWN
static MSG_HWND: AtomicIsize = AtomicIsize::new(0);
/// Visible overlay layer — clicks outside it dismiss the overlay. 0 = none.
static GUARD_HWND: AtomicIsize = AtomicIsize::new(0);

/// The message window — hosts the hook posts, the tray callback and the global idle timer.
pub fn msg_hwnd() -> HWND {
    HWND(MSG_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

/// Install the low-level mouse hook. Must be called from the thread that pumps messages.
pub fn install(msg_hwnd: HWND) -> Result<()> {
    MSG_HWND.store(msg_hwnd.0 as isize, Ordering::SeqCst);
    let h = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) }?;
    println!("hook: WH_MOUSE_LL installed (pass-through double-tap, window {}ms)", crate::config::double_tap_window_ms());
    let _ = h; // process exit uninstalls the hook
    Ok(())
}

/// Enable/disable tap detection (off while any overlay layer is visible).
pub fn detection_set(enabled: bool) {
    DETECTION_ON.store(enabled, Ordering::Release);
    if !enabled {
        // Forget the armed t1. Do NOT clear PENDING_UPS: t2's LUP arrives ~15ms after its
        // swallowed LDOWN, often AFTER the overlay opens — the time-bounded orphan guard in
        // on_tap_up must still eat it, or the app gets a stray button-up (t2 leak).
        *LAST.lock().unwrap() = None;
    }
}

/// Mark the currently visible overlay layer. While set, any click outside it posts
/// WM_TAPKEY_OUTSIDE (dismiss); clicks inside are left to the layer itself.
pub fn guard_set(h: HWND) {
    GUARD_HWND.store(h.0 as isize, Ordering::SeqCst);
}

pub fn guard_clear() {
    GUARD_HWND.store(0, Ordering::SeqCst);
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 {
        let s = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if wparam.0 == WM_LBUTTONUP as usize && s.dwExtraInfo != EXTRA_MARKER as usize {
            // Only t2's up is ever swallowed (t1 passes through whole). Eat the owed orphan up.
            if on_tap_up(s.pt.x, s.pt.y) == TapVerdict::Swallow {
                return LRESULT(1); // nonzero blocks the event (WH_MOUSE_LL: 0 passes, nonzero swallows)
            }
        }
        if wparam.0 == WM_LBUTTONDOWN as usize && s.dwExtraInfo != EXTRA_MARKER as usize {
            // Settings window taps are inert to TapKey — the settings window consumes
            // them (no detection, no outside-click dismiss, no armed t1 left behind).
            if crate::settings::point_in_settings(s.pt.x, s.pt.y) {
                *LAST.lock().unwrap() = None;
            } else {
            // Outside-click dismiss: while a layer is visible, a click outside
            // it closes everything. The click itself passes through to whatever is underneath.
            let g = GUARD_HWND.load(Ordering::SeqCst);
            if g != 0 && !point_in(g as *mut core::ffi::c_void, s.pt.x, s.pt.y) {
                let h = msg_hwnd();
                if !h.is_invalid() {
                    unsafe { let _ = PostMessageW(None, WM_TAPKEY_OUTSIDE, WPARAM(0), LPARAM(0)); }
                }
            } else if DETECTION_ON.load(Ordering::Acquire)
                && crate::config::detection_enabled() // global pause (settings toggle / tray state)
                && on_tap_down(s.pt.x, s.pt.y) == TapVerdict::Swallow
            {
                return LRESULT(1); // nonzero blocks t2 (WH_MOUSE_LL: 0 passes, nonzero swallows)
            }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Is (x,y) inside the window's current screen rect?
fn point_in(hwnd_raw: *mut core::ffi::c_void, x: i32, y: i32) -> bool {
    let h = HWND(hwnd_raw);
    if h.is_invalid() {
        return false;
    }
    unsafe {
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).is_ok() && x >= r.left && x < r.right && y >= r.top && y < r.bottom
    }
}

#[derive(PartialEq)]
enum TapVerdict { Pass, Swallow }

/// Handle a real LBUTTONDOWN: complete a double-tap (swallow t2) or arm a fresh t1 (pass through).
fn on_tap_down(x: i32, y: i32) -> TapVerdict {
    let now = now_ms();
    let mut last = LAST.lock().unwrap();
    if let Some(l) = *last {
        if now - l.t <= crate::config::double_tap_window_ms() && within(x, y, l.x, l.y) {
            // Double-tap: t1 already passed; swallow t2 and open the overlay at P.
            println!("double-tap @ ({},{}) gap={}ms — opening overlay (t2 swallowed, t1 passed)", l.x, l.y, now - l.t);
            let h = msg_hwnd();
            if !h.is_invalid() {
                unsafe { let _ = PostMessageW(None, WM_TAPKEY_OPEN, WPARAM(l.x as usize), LPARAM(l.y as isize)); }
            }
            PENDING_UPS.store(1, Ordering::SeqCst); // exactly one t2 up owed — eaten by on_tap_up even after the overlay opens
            HOLD_DOWN_AT.store(now, Ordering::SeqCst);
            *last = None; // clear so a t3 can't chain-trigger (detection also goes off on open)
            return TapVerdict::Swallow;
        }
    }
    // Fresh t1: record it and pass through untouched (zero latency, zero swallowed input).
    *last = Some(LastTap { t: now, x, y });
    TapVerdict::Pass
}

/// Handle a real LBUTTONUP: eat t2's orphan up if owed; everything else (incl. t1's up) passes.
fn on_tap_up(x: i32, y: i32) -> TapVerdict {
    let now = now_ms();
    if PENDING_UPS.load(Ordering::SeqCst) > 0
        && now.saturating_sub(HOLD_DOWN_AT.load(Ordering::SeqCst)) <= crate::config::double_tap_window_ms()
    {
        PENDING_UPS.fetch_sub(1, Ordering::SeqCst);
        println!("lup: swallowed t2 up @ ({x},{y})");
        return TapVerdict::Swallow;
    }
    TapVerdict::Pass
}

fn within(x: i32, y: i32, cx: i32, cy: i32) -> bool {
    let r = crate::config::tap_radius_px() as i64;
    let dx = (x - cx) as i64;
    let dy = (y - cy) as i64;
    dx * dx + dy * dy <= r * r
}