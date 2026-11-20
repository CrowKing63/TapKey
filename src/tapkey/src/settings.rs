//! Settings window — lavender owner-draw, the SAME rendering pipeline
//! as the overlay (WS_POPUP layered + per-pixel ARGB + coverage text + premultiplied blit).
//!
//! The tray is a pure launcher; ALL control lives here: double-tap window ±10 steppers
//! (primary), tap radius ±1 (secondary), idle close ±500, detection pause/resume toggle,
//! close, quit.
//! **Auto-save**: every change applies via config atomics AND persists to
//! tapkey.toml immediately — no save button. A failed write shows a status and
//! warns at exit; values stay live in memory either way.
//!
//! While this window is open, tap detection is suspended so button taps never trigger
//! the overlay. Closing resumes detection (the global pause flag is separate).

use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

use windows::core::{Result, w};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::overlay::{
    blit, fill_rounded, stroke_rounded, BTN_PRESSED, BTN_RADIUS, ICON_DARK, ICON_WHITE, PANEL_RADIUS,
};

// Settings is a reading surface — opaque, unlike the see-through overlay.
const SETTINGS_PANEL_BG: u32 = 0xFF1A1033; // solid dark purple
const SETTINGS_BTN_FILL: u32 = 0xFF453670; // solid mid purple

pub const SETTINGS_SIZE: (i32, i32) = (360, 388);
const FLASH_TIMER_ID: usize = 20; // pressed-flash auto-clear if LUP never reaches us
const FLASH_MS: u32 = 250;
const VALUE_LAVENDER: u32 = 0xFFC4B5FD;

static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);
static FLASH: AtomicU8 = AtomicU8::new(255); // pressed button index, 255 none
static STATUS: AtomicU8 = AtomicU8::new(0); // 0 none, 2 auto-save failed (until next success)

// ---------- layout (physical px) ----------
// Button order: 0 win-10, 1 win+10, 2 rad-1, 3 rad+1, 4 idle-500, 5 idle+500,
// 6 pause toggle, 7 close, 8 quit.
fn btn_rects() -> [(f64, f64, f64, f64); 9] {
    [
        (234.0, 58.0, 52.0, 44.0),
        (294.0, 58.0, 52.0, 44.0),
        (234.0, 126.0, 52.0, 44.0),
        (294.0, 126.0, 52.0, 44.0),
        (234.0, 194.0, 52.0, 44.0),
        (294.0, 194.0, 52.0, 44.0),
        (16.0, 264.0, 328.0, 48.0),
        (16.0, 328.0, 158.0, 48.0),
        (186.0, 328.0, 158.0, 48.0),
    ]
}

const ROW_Y: [f64; 3] = [52.0, 120.0, 188.0];
const ROW_LABELS: [&str; 3] = ["Double-tap", "Tap radius", "Idle close"];
const STEP_LABELS: [&str; 6] = ["-10", "+10", "-1", "+1", "-500", "+500"];

fn hit_test(p: POINT) -> Option<usize> {
    for (i, (x, y, w, h)) in btn_rects().iter().enumerate() {
        if (p.x as f64) >= *x && (p.x as f64) < x + w && (p.y as f64) >= *y && (p.y as f64) < y + h {
            return Some(i);
        }
    }
    None
}

fn settings_hwnd() -> HWND {
    HWND(SETTINGS_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

// ---------- render ----------

fn repaint(hwnd: HWND) -> Result<()> {
    let (w, h) = SETTINGS_SIZE;
    let mut buf = vec![0u32; (w * h) as usize];
    fill_rounded(&mut buf, w, h, (0.0, 0.0, w as f64, h as f64), PANEL_RADIUS, SETTINGS_PANEL_BG);

    let paused = !crate::config::detection_enabled();
    let flash = FLASH.load(Ordering::SeqCst);
    let rects = btn_rects();
    for (i, rect) in rects.iter().enumerate() {
        let is_p = flash as usize == i;
        let state_fill = i == 6 && paused; // pause toggle shows lavender while paused
        let fill = if is_p || state_fill { BTN_PRESSED } else { SETTINGS_BTN_FILL };
        fill_rounded(&mut buf, w, h, *rect, BTN_RADIUS, fill);
        if !is_p && !state_fill {
            stroke_rounded(&mut buf, w, h, *rect, BTN_RADIUS, 1.2, 0x40C4B5FD);
        }
    }

    // Labels + per-label colors (values in lavender accent; pressed labels dark).
    let mut labels: Vec<(f64, f64, f64, f64, String)> = Vec::new();
    let mut colors: Vec<u32> = Vec::new();
    let mut push = |rect: (f64, f64, f64, f64), text: String, col: u32| {
        labels.push((rect.0, rect.1, rect.2, rect.3, text));
        colors.push(col);
    };

    push((16.0, 12.0, 200.0, 26.0), "TapKey Settings".into(), ICON_WHITE);
    if STATUS.load(Ordering::SeqCst) == 2 {
        push((220.0, 12.0, 124.0, 26.0), "Save failed".into(), VALUE_LAVENDER);
    }

    let values = [
        format!("{} ms", crate::config::double_tap_window_ms()),
        format!("{} px", crate::config::tap_radius_px()),
        format!("{} ms", crate::config::overlay_idle_ms()),
    ];
    for r in 0..3 {
        let y = ROW_Y[r];
        push((16.0, y, 120.0, 56.0), ROW_LABELS[r].into(), ICON_WHITE);
        // Value box wide enough for "60000 ms" (94px @19px semibold) without touching
        // the steppers at x=234: center 183 -> max span 136..230.
        push((136.0, y, 94.0, 56.0), values[r].clone(), VALUE_LAVENDER);
        for s in 0..2 {
            let i = r * 2 + s;
            let col = if flash as usize == i { ICON_DARK } else { ICON_WHITE };
            push(rects[i], STEP_LABELS[i].into(), col);
        }
    }
    let pause_label = if paused { "Resume detection" } else { "Pause detection" };
    let pause_col = if flash == 6 || paused { ICON_DARK } else { ICON_WHITE };
    push(rects[6], pause_label.into(), pause_col);
    for (i, text) in [(7usize, "Close"), (8, "Quit")] {
        let col = if flash as usize == i { ICON_DARK } else { ICON_WHITE };
        push(rects[i], text.into(), col);
    }

    let refs: Vec<(f64, f64, f64, f64, &str)> = labels
        .iter()
        .map(|(x, y, w, h, s)| (*x, *y, *w, *h, s.as_str()))
        .collect();
    // 19px semibold, vertically centered.
    unsafe { blit(hwnd, w, h, buf, &refs, &colors, 19, true) }
}

// ---------- actions ----------

/// Auto-save: persist after every change. Failure keeps the value live in memory
/// and shows the status until the next successful write (exit warning still applies).
fn persist() {
    if crate::config::save() {
        STATUS.store(0, Ordering::SeqCst);
    } else {
        STATUS.store(2, Ordering::SeqCst);
    }
}

fn execute(hwnd: HWND, i: usize) {
    use crate::config as cfg;
    match i {
        0 => { cfg::set_double_tap_window_ms(cfg::double_tap_window_ms() as i32 - 10); persist(); }
        1 => { cfg::set_double_tap_window_ms(cfg::double_tap_window_ms() as i32 + 10); persist(); }
        2 => { cfg::set_tap_radius_px(cfg::tap_radius_px() - 1); persist(); }
        3 => { cfg::set_tap_radius_px(cfg::tap_radius_px() + 1); persist(); }
        4 => { cfg::set_overlay_idle_ms(cfg::overlay_idle_ms() as i32 - 500); persist(); }
        5 => { cfg::set_overlay_idle_ms(cfg::overlay_idle_ms() as i32 + 500); persist(); }
        6 => {
            let next = !cfg::detection_enabled();
            cfg::set_detection_enabled(next);
            crate::tray::update_icon();
            persist();
        }
        7 => close_settings(hwnd),
        8 => {
            // Quit the whole app (quit lives here, not in a tray menu).
            if crate::overlay::is_visible() {
                crate::overlay::dismiss_all("settings quit"); // releases any hold, hides the anchor dot
            }
            unsafe {
                let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
                let _ = DestroyWindow(hwnd);
            }
            SETTINGS_HWND.store(0, Ordering::SeqCst);
            println!("settings: quit requested");
            unsafe { PostQuitMessage(0) };
        }
        _ => {}
    }
}

fn close_settings(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
        let _ = DestroyWindow(hwnd);
    }
    SETTINGS_HWND.store(0, Ordering::SeqCst);
    FLASH.store(255, Ordering::SeqCst);
    STATUS.store(0, Ordering::SeqCst);
    crate::hook::detection_set(true); // resume (global pause flag stays as configured)
    println!("settings: closed");
}

/// Open from the tray (one tap). Already open → bring to front. Opening dismisses any
/// overlay layer and suspends detection so settings taps never trigger the overlay.
pub fn open() {
    let existing = settings_hwnd();
    if !existing.is_invalid() {
        unsafe {
            let _ = SetWindowPos(existing, Some(HWND_TOPMOST), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        return;
    }
    if crate::overlay::is_visible() {
        crate::overlay::dismiss_all("settings open");
    }
    crate::hook::detection_set(false);
    FLASH.store(255, Ordering::SeqCst);
    STATUS.store(0, Ordering::SeqCst);

    let (w, h) = SETTINGS_SIZE;
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let x = (sw - w) / 2;
    let y = (sh - h) / 2;
    match create(x, y) {
        Ok(hwnd) => {
            if repaint(hwnd).is_err() {
                println!("settings: first blit failed — closing");
                unsafe { let _ = DestroyWindow(hwnd); }
                crate::hook::detection_set(true);
                return;
            }
            SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
            unsafe { let _ = ShowWindow(hwnd, SW_SHOW); }
            // Re-assert topmost: other topmost windows can sit above us.
            unsafe {
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            println!("settings: open @ ({x},{y})");
        }
        Err(e) => {
            println!("settings: create failed: {e:?}");
            crate::hook::detection_set(true);
        }
    }
}

fn create(x: i32, y: i32) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?.into();
        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpszClassName = w!("TapKeySettings");
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(settings_wnd_proc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default(); // see overlay::create_overlay
        let _ = RegisterClassExW(&mut wc);
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("TapKeySettings"),
            w!("TapKey Settings"),
            WS_POPUP, // layered windows need WS_POPUP (style 0 offsets client coords)
            x,
            y,
            SETTINGS_SIZE.0,
            SETTINGS_SIZE.1,
            None,
            None,
            Some(hinst),
            None,
        )
    }
}

unsafe extern "system" fn settings_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let lp = lparam.0;
            let p = POINT {
                x: (lp & 0xFFFF) as i16 as i32,
                y: ((lp >> 16) & 0xFFFF) as i16 as i32,
            };
            let hit = hit_test(p);
            println!("settings: down @ ({},{}) -> btn={:?}", p.x, p.y, hit);
            if let Some(i) = hit {
                FLASH.store(i as u8, Ordering::SeqCst);
                let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
                let _ = SetTimer(Some(hwnd), FLASH_TIMER_ID, FLASH_MS, None);
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID);
            let i = FLASH.swap(255, Ordering::SeqCst);
            if i != 255 {
                execute(hwnd, i as usize); // execute at release (overlay convention)
                if !settings_hwnd().is_invalid() {
                    let _ = repaint(hwnd); // execute may have closed/quit the window
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == FLASH_TIMER_ID => {
            if FLASH.load(Ordering::SeqCst) != 255 {
                FLASH.store(255, Ordering::SeqCst);
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            close_settings(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
