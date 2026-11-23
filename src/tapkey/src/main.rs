#![windows_subsystem = "windows"]
//! TapKey — tray-resident entry: portable config next to the exe,
//! WH_MOUSE_LL double-tap detector (pass-through: t1 passes, t2 swallowed),
//! overlay near anchor P on double-tap, tray icon as a pure launcher — one tap
//! opens the settings window where all control (pause/save/quit) lives.
mod overlay;
mod hook;
mod config;
mod tray;
mod settings;

use windows::core::{Result, w};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

unsafe extern "system" fn msg_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == tray::WM_TRAY {
        // Tray callbacks arrive as SENT messages from explorer: GetMessageW dispatches
        // them internally, so they NEVER appear in the loop body — handle them here.
        // v4 packs the icon id into lParam's HIWORD; the notification is the LOWORD.
        let lp = (lparam.0 & 0xFFFF) as u32;
        println!("tray: callback wp={} lp=0x{:X} (code=0x{lp:X})", wparam.0, lparam.0);
        // One tap = open. Accept button-up (v0) and NIN_SELECT/KEYSELECT (v4).
        if lp == WM_LBUTTONUP || lp == 0x0400 || lp == 0x0401 {
            settings::open();
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn main() -> Result<()> {
    // Per-Monitor-V2 DPI: we work in physical pixels.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?; }

    config::load(); // portable tapkey.toml next to the exe (defaults if missing)

    // Message-only window (HWND_MESSAGE parent — same host style as WinForms NotifyIcon):
    // hosts the hook's PostMessage posts, the tray callback and the global idle timer.
    let hinst = unsafe { GetModuleHandleW(None) }?.into();
    let mut wc = WNDCLASSEXW::default();
    wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    wc.lpszClassName = w!("TapKeyMsg");
    wc.lpfnWndProc = Some(msg_wnd_proc);
    wc.hInstance = hinst;
    unsafe { RegisterClassExW(&mut wc) };
    let msg_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0), w!("TapKeyMsg"), w!("TapKeyMsg"), WINDOW_STYLE(0),
            0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst), None,
        )?
    };

    hook::install(msg_hwnd)?;
    tray::install(msg_hwnd)?;
    println!("tapkey ready — tray-resident; double-tap opens the overlay, tray tap opens settings");

    // Self-test mode: `tapkey --probe-move` drives the Move layer end-to-end (posted
    // messages, no external input) and exits with a PASS/FAIL code.
    if std::env::args().any(|a| a == "--probe-move") {
        let cx = unsafe { GetSystemMetrics(SM_CXSCREEN) } / 2;
        let cy = unsafe { GetSystemMetrics(SM_CYSCREEN) } / 2;
        if let Some(h) = open_overlay(cx, cy) {
            overlay::probe_arm(msg_hwnd, h);
        }
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) == TRUE } {
        let m = msg.message;
        if m == hook::WM_TAPKEY_OPEN {
            let _ = open_overlay(msg.wParam.0 as i32, msg.lParam.0 as i32);
        } else if m == hook::WM_TAPKEY_OUTSIDE {
            overlay::dismiss_all("outside click"); // click outside the panel — close + reset detection
        } else if m == WM_TIMER && msg.hwnd == msg_hwnd && msg.wParam.0 == overlay::IDLE_TIMER_ID as usize {
            overlay::dismiss_all("idle"); // global idle — closes whichever layer is open
        } else if m == WM_TIMER && msg.hwnd == msg_hwnd && msg.wParam.0 == overlay::PROBE_TIMER_ID as usize {
            overlay::probe_tick(); // self-test state machine (`--probe-move`)
        } else if m == tray::taskbar_created() && m != 0 {
            let _ = tray::readd(); // explorer restarted — restore the icon
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tray::remove();
    config::warn_if_save_failed(); // failed save → one warning at exit
    println!("tapkey: exited");
    Ok(())
}

/// Open the main panel near anchor P (the double-tap point) and suspend detection.
fn open_overlay(px: i32, py: i32) -> Option<HWND> {
    overlay::set_anchor(px, py);
    let (x, y) = overlay::position_for_anchor(px, py);
    let h = match overlay::create_overlay(x, y) {
        Ok(h) => h,
        Err(e) => { println!("overlay: create failed: {e:?}"); return None; }
    };
    if overlay::repaint(h).is_err() {
        println!("overlay: first blit failed — closing panel");
        unsafe { let _ = DestroyWindow(h); }
        hook::detection_set(true);
        return None;
    }
    unsafe { ShowWindow(h, SW_SHOW); }
    overlay::set_visible(h); // outside-click dismiss targets this panel
    overlay::reset_idle_timer(); // global idle on the message window — layer-independent
    overlay::move_anchor(px, py);
    hook::detection_set(false);
    println!("overlay: open @ ({x},{y}), anchor P ({px},{py})");
    Some(h)
}
