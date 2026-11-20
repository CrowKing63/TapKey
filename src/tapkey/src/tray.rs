//! Tray residency: a PURE LAUNCHER — one tap (left click) opens the settings
//! window. No context menu, no double-click action. All control (pause/resume,
//! quit) lives in the settings window. The icon changes while detection is paused
//! so the tray shows state at a glance.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::core::{Result, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Tray callback message: lParam carries the mouse message (WM_LBUTTONUP…).
pub const WM_TRAY: u32 = WM_APP + 3;

static MSG_HWND: AtomicIsize = AtomicIsize::new(0);
static ICON_ACTIVE: AtomicIsize = AtomicIsize::new(0);
static ICON_PAUSED: AtomicIsize = AtomicIsize::new(0);
static ADDED: AtomicBool = AtomicBool::new(false);
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

fn msg_hwnd() -> HWND {
    HWND(MSG_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

fn icon(paused: bool) -> HICON {
    let raw = if paused { &ICON_PAUSED } else { &ICON_ACTIVE }.load(Ordering::SeqCst);
    HICON(raw as *mut core::ffi::c_void)
}

fn base_nid() -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = msg_hwnd();
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // Version 4 (same as WinForms NotifyIcon): a tap arrives as NIN_SELECT (0x0400)
    // with the icon id packed into lParam's HIWORD. Tray callbacks are SENT messages
    // from explorer — they must be handled in msg_wnd_proc, never in the loop body.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let tip = "TapKey — tap to open settings";
    for (i, ch) in tip.encode_utf16().take(127).enumerate() {
        nid.szTip[i] = ch;
    }
    nid
}

/// Register the tray icon. Call from the message-pump thread after the msg window exists.
pub fn install(hwnd: HWND) -> Result<()> {
    MSG_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        ICON_ACTIVE.store(make_icon(false)?.0 as isize, Ordering::SeqCst);
        ICON_PAUSED.store(make_icon(true)?.0 as isize, Ordering::SeqCst);
        TASKBAR_MSG.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
    }
    readd()
}

/// (Re-)add the icon — also used after explorer restarts (TaskbarCreated).
pub fn readd() -> Result<()> {
    let mut nid = base_nid();
    nid.hIcon = icon(!crate::config::detection_enabled());
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid).ok()? };
    let sv = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };
    println!("tray: NIM_SETVERSION(v4) = {}", sv.as_bool());
    ADDED.store(true, Ordering::SeqCst);
    println!("tray: icon added (launcher — tap opens settings, v4)");
    Ok(())
}

/// Swap the icon to match the detection state (paused = gray + pause bars).
pub fn update_icon() {
    if !ADDED.load(Ordering::SeqCst) {
        return;
    }
    let mut nid = base_nid();
    nid.hIcon = icon(!crate::config::detection_enabled());
    unsafe {
        if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            println!("tray: icon updated (detection {})", if crate::config::detection_enabled() { "on" } else { "paused" });
        }
    }
}

pub fn remove() {
    if ADDED.swap(false, Ordering::SeqCst) {
        let nid = base_nid();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        println!("tray: icon removed");
    }
    unsafe {
        for s in [&ICON_ACTIVE, &ICON_PAUSED] {
            let raw = s.swap(0, Ordering::SeqCst);
            if raw != 0 {
                let _ = DestroyIcon(HICON(raw as *mut core::ffi::c_void));
            }
        }
    }
}

/// Registered "TaskbarCreated" message — explorer restarted, re-add the icon.
pub fn taskbar_created() -> u32 {
    TASKBAR_MSG.load(Ordering::SeqCst)
}

// ---------- icon drawing (supersampled software render into a 32bpp DIB) ----------

/// 96×96 source icon (integer downscale to 16/24/32/48 tray slots), 4×4
/// supersampling for antialiased edges. Active = full-bleed lavender squircle +
/// dark "t" mark with detached tap dot (master: docs/assets/tapkey-icon.png);
/// paused = gray squircle + dark pause bars. Geometry lives in the same 1024
/// design space as scripts/render_icon_concepts.py.
unsafe fn make_icon(paused: bool) -> Result<HICON> {
    const S: i32 = 96;
    const SS: i32 = 4; // supersamples per axis

    // straight-RGB palette: lavender #C4B5FD / gray #9CA3AF tile, dark purple #2A1B4D glyph
    let (tile, glyph) = if paused {
        ((156u32, 163u32, 175u32), (42u32, 27u32, 77u32))
    } else {
        ((196u32, 181u32, 253u32), (42u32, 27u32, 77u32))
    };

    fn disc(x: f64, y: f64, cx: f64, cy: f64, r: f64) -> bool {
        (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
    }
    fn rrect(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, r: f64) -> bool {
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        disc(x, y, cx, cy, r)
    }
    fn tile_hit(x: f64, y: f64) -> bool {
        // full-bleed squircle (superellipse n=4) — touches all four canvas edges
        let dx = (x - 512.0).abs();
        let dy = (y - 512.0).abs();
        let dx2 = dx * dx;
        let dy2 = dy * dy;
        dx2 * dx2 + dy2 * dy2 <= 512.0 * 512.0 * 512.0 * 512.0
    }
    fn glyph_hit(x: f64, y: f64, paused: bool) -> bool {
        if paused {
            return rrect(x, y, 350.0, 300.0, 450.0, 724.0, 50.0)
                || rrect(x, y, 574.0, 300.0, 674.0, 724.0, 50.0);
        }
        // t mark: ONE continuous stroke — round top cap + straight stem + quarter-arc
        // tail (arc starts on the stem centerline, tangent to both edges: no seam).
        let stem = disc(x, y, 496.0, 222.0, 80.0)
            || (x >= 416.0 && x <= 576.0 && y >= 222.0 && y <= 642.0);
        let tail = {
            let dx = x - 656.0;
            let dy = y - 642.0;
            let d = if dx <= 0.0 && dy >= 0.0 {
                (dx.hypot(dy) - 160.0).abs() // within the arc's quadrant: radial distance
            } else {
                (dx + 160.0).hypot(dy).min(dx.hypot(dy - 160.0)) // nearest endpoint
            };
            d <= 80.0
        };
        stem || tail
            || rrect(x, y, 266.0, 302.0, 576.0, 462.0, 80.0) // crossbar left arm
            || disc(x, y, 716.0, 382.0, 95.0) // detached tap dot (right arm)
    }

    let hdc = CreateCompatibleDC(None);
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: S,
            biHeight: -S, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let dib = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let old = SelectObject(hdc, dib.into());

    // Per pixel: count tile/glyph coverage over SS×SS subsamples, then composite
    // glyph-over-tile with straight alpha. A 32bpp BI_RGB DIB pixel read as u32
    // is 0xAARRGGBB (memory bytes B,G,R,A) — NOT the COLORREF 0x00BBGGRR order.
    let scale = 1024.0 / (S * SS) as f64;
    for py in 0..S {
        for px in 0..S {
            let mut t = 0u32;
            let mut g = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f64 + 0.5) * scale;
                    let y = ((py * SS + sy) as f64 + 0.5) * scale;
                    if tile_hit(x, y) {
                        t += 1;
                        if glyph_hit(x, y, paused) {
                            g += 1;
                        }
                    }
                }
            }
            let c = if t == 0 {
                0
            } else {
                let a = (t * 255 + 8) / 16;
                let mix = |gc: u32, tc: u32| (gc * g + tc * (t - g) + t / 2) / t;
                let r = mix(glyph.0, tile.0);
                let gr = mix(glyph.1, tile.1);
                let b = mix(glyph.2, tile.2);
                (a << 24) | (r << 16) | (gr << 8) | b
            };
            *(bits as *mut u32).add((py * S + px) as usize) = c;
        }
    }

    // All-zero mask: with a 32bpp alpha color bitmap the mask is ignored.
    let mut mask_bits = std::ptr::null_mut();
    let mask = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut mask_bits, None, 0)?;
    for i in 0..(S * S) as usize {
        *(mask_bits as *mut u32).add(i) = 0;
    }

    let ii = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: dib };
    let hicon = CreateIconIndirect(&ii)?;
    SelectObject(hdc, old);
    let _ = DeleteObject(dib.into());
    let _ = DeleteObject(mask.into());
    let _ = DeleteDC(hdc);
    Ok(hicon)
}
