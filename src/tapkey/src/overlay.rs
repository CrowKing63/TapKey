//! TapKey overlay — main action panel (lavender) + D-pad drag layer (teal).
//!
//! The WH_MOUSE_LL hook in `hook.rs` detects a double-tap,
//! posts WM_TAPKEY_OPEN with anchor P; main.rs creates this panel near P and turns
//! detection off while any layer is visible. Auto-close = the GLOBAL idle timer
//! on the message window (config overlay_idle_ms, default 8s; layer-independent,
//! resets on any button tap) + outside-click dismiss + one-shot action auto-close
//! (right-click/hold-2s/drag-Drop; scroll and move keep the panel). Esc exits the app.
//!
//! Layers:
//!   - main panel 224x318: hold / right-click / drag / scroll up / scroll down / move
//!   - D-pad layer 326x326: plan-then-replay drag — center starts/ends
//!     planning, outer 8 extend the path; release replays it as one SendInput burst.
//!     Deliberately different tone (teal) so the layer switch reads at a glance.
//!   - Move layer 326x326: cross of four edge-pan arrows — each tap pulses the cursor
//!     to that monitor's edge, holds it there briefly (edge-panning games pan while
//!     the cursor sits at the edge), then restores it to the tap point. Repeatable.
//!   - anchor dot: global topmost click-through marker at P (the double-tap point),
//!     visible in all overlay modes.
//!
//! Rendering: manual per-pixel antialiased shapes into a straight ARGB buffer;
//! labels are rasterized by GDI as black-on-white and read back as a coverage
//! map (GDI zeroes the alpha byte of 32bpp DIB text), then composited in our
//! own color space. One UpdateLayeredWindow blit with per-pixel alpha
//! (premultiplied).

use std::sync::{atomic::{AtomicI32, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering}, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{Result, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const OVERLAY_SIZE: (i32, i32) = (224, 318);
pub const DPAD_SIZE: (i32, i32) = (326, 326);
pub const MOVE_SIZE: (i32, i32) = (224, 224); // move layer — 2x2 grid (up/down row, left/right row), cells stay 90px like the D-pad
pub const IDLE_TIMER_ID: u32 = 2;   // global idle close on the message window — layer-independent
pub const HOLD_TIMER_ID: u32 = 3;   // dpad hold -> first repeat
pub const REPEAT_TIMER_ID: u32 = 4; // dpad hold -> subsequent repeats
pub const HOLD2S_TIMER_ID: u32 = 5; // main 'hold-2s' button -> left-up after 2s
pub const FLASH_TIMER_ID: u32 = 6;  // pressed-flash auto-clear if LUP never reaches the panel
pub const RESTORE_TIMER_ID: u32 = 7; // return cursor to the tap point after a scroll action
pub const ACTION_CLOSE_TIMER_ID: u32 = 8; // one-shot action complete -> auto-close the panel
pub const MOVE_HOLD_TIMER_ID: u32 = 9; // move pulse: cursor parked at the screen edge, restore after dwell
pub const PROBE_TIMER_ID: u32 = 10;    // self-test probe state machine tick (`--probe-move`)
const HOLD_FIRST_MS: u32 = 400; // hold-to-repeat: delay before the first repeat
const REPEAT_MS: u32 = 90;      // hold-to-repeat: interval between repeats
const HOLD2S_MS: u32 = 2000;    // hold-2s button: left-down, wait 2s, left-up
// Dwell at the screen edge per pulse — runtime-configurable sensitivity (config::MOVE_HOLD_MS).
const FLASH_MS: u32 = 250;      // pressed-flash auto-clear deadline (LUP fallback)
const RESTORE_MS: u32 = 150;    // cursor-return delay — must outlive the SendInput dispatch (cf. REPLAY_GUARD_MS)
const ACTION_CLOSE_MS: u32 = 250; // one-shot auto-close delay — outlives the SendInput dispatch
const DRAG_STEP_PX: i32 = 20;   // path extension per direction tap
const REPLAY_STEP_MS: u64 = 15; // per-step delay when replaying a planned drag

// Anchor dot (global marker at P) — diameter is configurable.
fn anchor_size() -> i32 { crate::config::anchor_dot_px() } // diameter px (default 20)
const ANCHOR_DOT: u32 = 0xFA8BA7;   // purple #A78BFA as COLORREF (0x00BBGGRR)
const ANCHOR_ALPHA: u8 = 230;       // ~90% opacity
const ANCHOR_KEY: u32 = 0x010203;   // colorkey background → transparent

// ---------- layout (physical px, per-monitor-v2 aware) ----------
const PAD_X: f64 = 16.0; // (224 - 2*90 - 12)/2 — exact horizontal centering
const PAD_Y: f64 = 12.0; // (318 - 3*90 - 2*12)/2 — exact vertical centering
const GAP: f64 = 12.0;
pub(crate) const PANEL_RADIUS: f64 = 20.0;
pub(crate) const BTN_RADIUS: f64 = 0.0; // square buttons

struct Btn {
    rect: (f64, f64, f64, f64), // x, y, w, h
    label: &'static str,
    action: &'static str,
    icon: u8, // 0=scroll-up 1=scroll-down 2=right-click 3=hold-2s 5=drag 7=edge-move
}

/// 2 columns x 3 rows. Left column: hold, drag (D-pad entry), move (edge-pan entry).
/// Right column: right-click, scroll-up, scroll-down.
fn buttons() -> [Btn; 6] {
    let bs = (OVERLAY_SIZE.0 as f64 - 2.0 * PAD_X - GAP) / 2.0; // 90, square
    [
        Btn { rect: (PAD_X, PAD_Y, bs, bs), label: "Hold2s", action: "hold-2s", icon: 3 },
        Btn { rect: (PAD_X + bs + GAP, PAD_Y, bs, bs), label: "RClick", action: "right-click", icon: 2 },
        Btn { rect: (PAD_X, PAD_Y + bs + GAP, bs, bs), label: "Drag", action: "drag", icon: 5 },
        Btn { rect: (PAD_X + bs + GAP, PAD_Y + bs + GAP, bs, bs), label: "Scr Up", action: "scroll-up", icon: 0 },
        Btn { rect: (PAD_X, PAD_Y + 2.0 * (bs + GAP), bs, bs), label: "Move", action: "move", icon: 7 },
        Btn { rect: (PAD_X + bs + GAP, PAD_Y + 2.0 * (bs + GAP), bs, bs), label: "Scr Dn", action: "scroll-down", icon: 1 },
    ]
}

static FLASH: AtomicU8 = AtomicU8::new(255); // main panel pressed button, 255 none
// Actions deferred to button-up (right-click / scroll): executing on down moves the cursor to P,
// and Moonlight sends the user's release at cursor position — it would land on the target and
// dismiss the context menu (or stray into the app) before we're done. hold-2s is NOT deferred:
// the press must start immediately. 255 = none, otherwise a button index.
static PENDING_ACTION: AtomicU8 = AtomicU8::new(255);

pub fn hit_test(p: POINT) -> Option<usize> {
    let bs = buttons();
    for (i, b) in bs.iter().enumerate() {
        if b.label.is_empty() {
            continue; // empty placeholder slot — not interactive
        }
        let (x, y, w, h) = b.rect;
        if (p.x as f64) >= x && (p.x as f64) < x + w && (p.y as f64) >= y && (p.y as f64) < y + h {
            return Some(i);
        }
    }
    None
}

// ---------- D-pad layer ----------

#[derive(Clone, Copy)]
struct DpadCell {
    rect: (f64, f64, f64, f64),
    dir: u8, // 0..7 = arrow direction, 9 = center (press/release toggle)
}

/// 3x3 grid. dir: 0=up 1=up-right 2=right 3=down 4=up-left 5=left 6=down-left 7=down-right, 9=center.
fn dpad_cells() -> [DpadCell; 9] {
    let bs = (DPAD_SIZE.0 as f64 - 2.0 * PAD_X - 2.0 * GAP) / 3.0; // 90, square
    let p = PAD_X; // (326 - 3*90 - 2*12)/2 = 16 — exact centering
    [
        DpadCell { rect: (p, p, bs, bs), dir: 4 },
        DpadCell { rect: (p + bs + GAP, p, bs, bs), dir: 0 },
        DpadCell { rect: (p + 2.0 * (bs + GAP), p, bs, bs), dir: 1 },
        DpadCell { rect: (p, p + bs + GAP, bs, bs), dir: 5 },
        DpadCell { rect: (p + bs + GAP, p + bs + GAP, bs, bs), dir: 9 },
        DpadCell { rect: (p + 2.0 * (bs + GAP), p + bs + GAP, bs, bs), dir: 2 },
        DpadCell { rect: (p, p + 2.0 * (bs + GAP), bs, bs), dir: 6 },
        DpadCell { rect: (p + bs + GAP, p + 2.0 * (bs + GAP), bs, bs), dir: 3 },
        DpadCell { rect: (p + 2.0 * (bs + GAP), p + 2.0 * (bs + GAP), bs, bs), dir: 7 },
    ]
}

static DPAD_HELD: AtomicU8 = AtomicU8::new(255); // held cell (auto-repeat), 255 none
static DPAD_PLANNING: AtomicU8 = AtomicU8::new(0); // center toggle: 0=idle(grab) 1=planning(release)
static DPAD_PENDING_DROP: AtomicU8 = AtomicU8::new(0); // Drop pressed — replay deferred to LUP
// While now < this, our own overlay ignores mouse input. The replay's real clicks are
// queued and dispatched only AFTER the handler returns, so the guard must outlive the
// SendInput span — a short deadline covers the dispatch without eating fast user taps.
static REPLAY_GUARD_UNTIL: AtomicU64 = AtomicU64::new(0);
const REPLAY_GUARD_MS: u64 = 120;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}
static DRAG_PATH: Mutex<Vec<(i32, i32)>> = Mutex::new(Vec::new()); // planned drag path
static ANCHOR_HWND: AtomicIsize = AtomicIsize::new(0); // global anchor dot window
static MAIN_HWNDA: AtomicIsize = AtomicIsize::new(0);
static DPAD_HWNDA: AtomicIsize = AtomicIsize::new(0); // dpad layer while open (0 = none)
static MOVE_HWNDA: AtomicIsize = AtomicIsize::new(0); // move layer while open (0 = none)
/// Currently visible layer (main or dpad) — the hook dismisses clicks outside it. 0 = none.
static VISIBLE_HWND: AtomicIsize = AtomicIsize::new(0);
static ANCHOR_X: AtomicI32 = AtomicI32::new(i32::MIN); // drag anchor P — unset until hook wiring
static ANCHOR_Y: AtomicI32 = AtomicI32::new(0);

fn dir_vec(d: u8) -> (f64, f64) {
    match d {
        0 => (0.0, -1.0),  // up
        1 => (1.0, -1.0),  // up-right
        2 => (1.0, 0.0),   // right
        3 => (0.0, 1.0),   // down
        4 => (-1.0, -1.0), // up-left
        5 => (-1.0, 0.0),  // left
        6 => (-1.0, 1.0),  // down-left
        _ => (1.0, 1.0),   // down-right
    }
}

pub fn dpad_hit_test(p: POINT) -> Option<usize> {
    for (i, c) in dpad_cells().iter().enumerate() {
        let (x, y, w, h) = c.rect;
        if (p.x as f64) >= x && (p.x as f64) < x + w && (p.y as f64) >= y && (p.y as f64) < y + h {
            return Some(i);
        }
    }
    None
}

// ---------- Move layer (edge-pan) ----------

/// 2x2 grid — row 1: up / down, row 2: left / right (no diagonals).
/// 224x224 panel, 90px cells: (224 - 2*16 - 12)/2.
fn move_cells() -> [DpadCell; 4] {
    let bs = (MOVE_SIZE.0 as f64 - 2.0 * PAD_X - GAP) / 2.0; // 90, square — same cells as the D-pad
    let p = PAD_X;
    [
        DpadCell { rect: (p, p, bs, bs), dir: 0 },                            // up     (1,1)
        DpadCell { rect: (p + bs + GAP, p, bs, bs), dir: 3 },                 // down   (1,2)
        DpadCell { rect: (p, p + bs + GAP, bs, bs), dir: 5 },                 // left   (2,1)
        DpadCell { rect: (p + bs + GAP, p + bs + GAP, bs, bs), dir: 2 },      // right  (2,2)
    ]
}

static MOVE_PRESSED: AtomicU8 = AtomicU8::new(255); // pressed cell (flash), 255 none
// Pulse deferred to button-up: executing on down teleports the cursor to the screen edge,
// and Moonlight delivers the tap's release at the CURRENT cursor position — it would land
// outside the panel and dismiss the overlay mid-pulse (same reason as main-panel actions).
static MOVE_PENDING: AtomicU8 = AtomicU8::new(255); // 255 none, otherwise a cell index
static MOVE_TAP_X: AtomicI32 = AtomicI32::new(0); // tap point in SCREEN coords — the restore target
static MOVE_TAP_Y: AtomicI32 = AtomicI32::new(0);
static MOVE_HOLDING: AtomicU8 = AtomicU8::new(0); // 1 while the cursor is parked at the edge

pub fn move_hit_test(p: POINT) -> Option<usize> {
    for (i, c) in move_cells().iter().enumerate() {
        let (x, y, w, h) = c.rect;
        if (p.x as f64) >= x && (p.x as f64) < x + w && (p.y as f64) >= y && (p.y as f64) < y + h {
            return Some(i);
        }
    }
    None
}

fn move_dir_name(d: u8) -> &'static str {
    match d {
        0 => "up",
        2 => "right",
        3 => "down",
        _ => "left",
    }
}

/// Set the anchor P — the double-tap point; the dot follows it and all actions
/// execute from here. Falls back to screen center until first set.
pub fn set_anchor(x: i32, y: i32) {
    ANCHOR_X.store(x, Ordering::SeqCst);
    ANCHOR_Y.store(y, Ordering::SeqCst);
}

fn anchor() -> (i32, i32) {
    let x = ANCHOR_X.load(Ordering::SeqCst);
    if x == i32::MIN {
        unsafe { (GetSystemMetrics(SM_CXSCREEN) / 2, GetSystemMetrics(SM_CYSCREEN) / 2) }
    } else {
        (x, ANCHOR_Y.load(Ordering::SeqCst))
    }
}

fn mouse_send(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) {
    let mut input = INPUT::default();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dx = dx;
    input.Anonymous.mi.dy = dy;
    input.Anonymous.mi.dwFlags = flags;
    input.Anonymous.mi.dwExtraInfo = crate::hook::EXTRA_MARKER as usize; // mark self-injection so the hook skips it
    unsafe {
        let _ = SendInput(&mut [input], core::mem::size_of::<INPUT>() as i32);
    }
}

/// Wheel event at the current cursor position (config::scroll_notches() notches per tap, sign per direction).
fn wheel_send(delta: i16) {
    let mut input = INPUT::default();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dwFlags = MOUSEEVENTF_WHEEL;
    input.Anonymous.mi.mouseData = (delta as i32) as u32; // sign-extend: -120 = 0xFFFFFF88 (not 0x0000FF88 → +32767)
    input.Anonymous.mi.dwExtraInfo = crate::hook::EXTRA_MARKER as usize;
    unsafe {
        let r = SendInput(&mut [input], core::mem::size_of::<INPUT>() as i32);
        println!("wheel_send: delta={} ret={} err={}", delta, r, GetLastError().0);
    }
}

static HOLDING: AtomicU8 = AtomicU8::new(0); // main 'hold-2s' left button currently down

// Cursor return (scroll only): scroll executes
// at anchor P, then the cursor goes back to where the user tapped the button. Captured
// just before SetCursorPos(P); the return is timer-delayed because injected events
// dispatch only after the handler returns — an immediate SetCursorPos back would land
// the queued wheel event at the return point instead of P.
static RESTORE_X: AtomicI32 = AtomicI32::new(0);
static RESTORE_Y: AtomicI32 = AtomicI32::new(0);

fn capture_restore() {
    let mut p = POINT::default();
    unsafe { let _ = GetCursorPos(&mut p); }
    RESTORE_X.store(p.x, Ordering::SeqCst);
    RESTORE_Y.store(p.y, Ordering::SeqCst);
}

fn schedule_restore(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID as usize);
        let _ = SetTimer(Some(hwnd), RESTORE_TIMER_ID as usize, RESTORE_MS, None);
    }
}

fn do_restore() {
    let x = RESTORE_X.load(Ordering::SeqCst);
    let y = RESTORE_Y.load(Ordering::SeqCst);
    unsafe { let _ = SetCursorPos(x, y); }
    println!("cursor restored to ({x},{y})");
}

/// One-shot actions (right-click / double-click / hold-2s / drag) auto-close the panel
/// when done — only repeatable actions (scroll up/down) keep it open. A new button tap
/// cancels a pending auto-close.
fn schedule_action_close(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(Some(hwnd), ACTION_CLOSE_TIMER_ID as usize);
        let _ = SetTimer(Some(hwnd), ACTION_CLOSE_TIMER_ID as usize, ACTION_CLOSE_MS, None);
    }
}

/// Release the hold-2s left button if it is still down — safety path for every
/// close/dismiss route so a panel closing mid-hold never leaves the cursor stuck.
fn release_hold_if_active() {
    if HOLDING.swap(0, Ordering::SeqCst) == 1 {
        mouse_send(0, 0, MOUSEEVENTF_LEFTUP);
        println!("hold-2s: released early (panel closed)");
    }
}

/// Move-layer action: pulse the cursor to the edge of the tapped monitor in `dir`, hold it
/// there for the configured dwell (edge-panning games pan while the cursor sits at the edge),
/// then restore the cursor to the tap point. Repeatable — no auto-close (like scroll).
fn perform_move_action(hwnd: HWND, dir: u8) {
    let tx = MOVE_TAP_X.load(Ordering::SeqCst); // from LBUTTONDOWN lparam — Moonlight may still be moving the physical cursor
    let ty = MOVE_TAP_Y.load(Ordering::SeqCst);
    let (ex, ey) = edge_point(dir, tx, ty);
    let dwell = crate::config::move_hold_ms();
    unsafe {
        let _ = KillTimer(Some(hwnd), MOVE_HOLD_TIMER_ID as usize); // rapid taps restart the dwell
        let _ = SetCursorPos(ex, ey); // immediate — polling games see the edge during the hold
        let _ = SetTimer(Some(hwnd), MOVE_HOLD_TIMER_ID as usize, dwell, None);
    }
    // Input-stream move for message/raw-input tracking: dispatches after this handler returns,
    // when the cursor is already at the edge (positionally a no-op, event still delivered).
    let (nx, ny) = norm_virtual(ex, ey);
    mouse_send_abs(nx, ny, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
    MOVE_HOLDING.store(1, Ordering::SeqCst);
    println!("move: {} -> edge ({ex},{ey}), hold {dwell}ms, restore to ({tx},{ty})", move_dir_name(dir));
}

/// Restore after the dwell: back to the tap point (polling) + input-stream move (message tracking).
fn do_move_restore() {
    MOVE_HOLDING.store(0, Ordering::SeqCst);
    let tx = MOVE_TAP_X.load(Ordering::SeqCst);
    let ty = MOVE_TAP_Y.load(Ordering::SeqCst);
    let mut cp = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cp); // self-check: where the cursor actually sat during the hold
        let _ = SetCursorPos(tx, ty);
    }
    let (nx, ny) = norm_virtual(tx, ty);
    mouse_send_abs(nx, ny, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
    println!("move: restored to ({tx},{ty}) [held at ({},{})]", cp.x, cp.y);
}

/// If a pulse is mid-hold (cursor parked at the screen edge), bring it back now — safety path
/// for every close/dismiss route so closing never leaves the cursor stranded at the edge.
fn finish_move_hold_if_active() {
    if MOVE_HOLDING.swap(0, Ordering::SeqCst) == 1 {
        let tx = MOVE_TAP_X.load(Ordering::SeqCst);
        let ty = MOVE_TAP_Y.load(Ordering::SeqCst);
        unsafe { let _ = SetCursorPos(tx, ty); }
        println!("move: hold interrupted — cursor restored to ({tx},{ty})");
    }
}

// ---------- Self-test probe (`--probe-move`) ----------
// Drives the Move layer end-to-end from inside the process (posted messages, no external
// input) so it can be verified on machines where synthetic SendInput is unreliable:
// main panel -> Move button -> up pulse -> down pulse -> Esc -> back to main.
static PROBE_STEP: AtomicU8 = AtomicU8::new(0); // 1 wait for move layer, 2 sample up-hold, 3 sample down-hold, 4 verify return
static PROBE_MAIN: AtomicIsize = AtomicIsize::new(0); // main panel hwnd (probe-owned)
static PROBE_TICKS: AtomicU32 = AtomicU32::new(0); // ticks within the current phase
static PROBE_TOP: AtomicI32 = AtomicI32::new(0); // primary monitor top/bottom — edge bands
static PROBE_BOT: AtomicI32 = AtomicI32::new(0);
static PROBE_UP_EDGE: AtomicU32 = AtomicU32::new(0); // cursor samples inside the top-edge band
static PROBE_UP_TOTAL: AtomicU32 = AtomicU32::new(0);
static PROBE_DN_EDGE: AtomicU32 = AtomicU32::new(0);
static PROBE_DN_TOTAL: AtomicU32 = AtomicU32::new(0);

/// Arm the probe: post a click on the main panel's Move button and start the tick timer.
pub fn probe_arm(msg_hwnd: HWND, main_hwnd: HWND) {
    unsafe {
        let (l, t, _r, b) = primary_monitor_rect();
        PROBE_TOP.store(t, Ordering::SeqCst);
        PROBE_BOT.store(b, Ordering::SeqCst);
        let _ = l;
        PROBE_MAIN.store(main_hwnd.0 as isize, Ordering::SeqCst);
        // Move button center — found by action name in the main grid.
        let (cx, cy) = buttons()
            .iter()
            .find(|b| b.action == "move")
            .map(|b| {
                let (x, y, w, h) = b.rect;
                ((x + w / 2.0) as i32, (y + h / 2.0) as i32)
            })
            .unwrap_or((61, 261));
        probe_post_click(main_hwnd, cx, cy);
        PROBE_STEP.store(1, Ordering::SeqCst);
        let _ = SetTimer(Some(msg_hwnd), PROBE_TIMER_ID as usize, 30, None);
    }
}

/// One state-machine tick (PROBE_TIMER_ID on the message window).
pub fn probe_tick() {
    unsafe {
        match PROBE_STEP.load(Ordering::SeqCst) {
            1 => {
                let mv = MOVE_HWNDA.load(Ordering::SeqCst);
                if mv != 0 {
                    // Up-arrow cell center.
                    let c = move_cells()[0];
                    let (x, y, w, h) = c.rect;
                    probe_post_click(HWND(mv as *mut core::ffi::c_void), (x + w / 2.0) as i32, (y + h / 2.0) as i32);
                    PROBE_TICKS.store(0, Ordering::SeqCst);
                    PROBE_STEP.store(2, Ordering::SeqCst);
                } else if PROBE_TICKS.fetch_add(1, Ordering::SeqCst) > 40 {
                    probe_finish(false, "move layer did not open");
                }
            }
            2 => {
                // Cursor should be parked at the top edge during the configured dwell.
                let p = sample_cursor();
                PROBE_UP_TOTAL.fetch_add(1, Ordering::SeqCst);
                if p.y <= PROBE_TOP.load(Ordering::SeqCst) + 5 {
                    PROBE_UP_EDGE.fetch_add(1, Ordering::SeqCst);
                }
                if PROBE_TICKS.fetch_add(1, Ordering::SeqCst) >= 7 {
                    let mv = HWND(MOVE_HWNDA.load(Ordering::SeqCst) as *mut core::ffi::c_void);
                    let c = move_cells()[1]; // down-arrow cell — 2x2: index 1 = (1,2)
                    let (x, y, w, h) = c.rect;
                    probe_post_click(mv, (x + w / 2.0) as i32, (y + h / 2.0) as i32);
                    PROBE_TICKS.store(0, Ordering::SeqCst);
                    PROBE_STEP.store(3, Ordering::SeqCst);
                }
            }
            3 => {
                let p = sample_cursor();
                PROBE_DN_TOTAL.fetch_add(1, Ordering::SeqCst);
                if p.y >= PROBE_BOT.load(Ordering::SeqCst) - 6 {
                    PROBE_DN_EDGE.fetch_add(1, Ordering::SeqCst);
                }
                if PROBE_TICKS.fetch_add(1, Ordering::SeqCst) >= 7 {
                    let mv = HWND(MOVE_HWNDA.load(Ordering::SeqCst) as *mut core::ffi::c_void);
                    // Esc -> close_move -> back to the main panel.
                    let _ = PostMessageW(Some(mv), WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
                    let _ = PostMessageW(Some(mv), WM_KEYUP, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
                    PROBE_TICKS.store(0, Ordering::SeqCst);
                    PROBE_STEP.store(4, Ordering::SeqCst);
                }
            }
            4 => {
                if PROBE_TICKS.fetch_add(1, Ordering::SeqCst) < 3 {
                    return; // give close_move a moment
                }
                let main = HWND(PROBE_MAIN.load(Ordering::SeqCst) as *mut core::ffi::c_void);
                let restored = MOVE_HWNDA.load(Ordering::SeqCst) == 0 && IsWindowVisible(main).as_bool();
                probe_finish(restored, if restored { "ok" } else { "Esc did not restore the main panel" });
            }
            _ => {}
        }
    }
}

fn sample_cursor() -> POINT {
    let mut p = POINT::default();
    unsafe { let _ = GetCursorPos(&mut p); }
    p
}

fn primary_monitor_rect() -> (i32, i32, i32, i32) {
    unsafe {
        let hm = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(hm, &mut mi).as_bool() {
            (mi.rcMonitor.left, mi.rcMonitor.top, mi.rcMonitor.right, mi.rcMonitor.bottom)
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        }
    }
}

fn probe_post_click(hwnd: HWND, cx: i32, cy: i32) {
    unsafe {
        let lp = LPARAM(((cy as u32 & 0xFFFF) << 16 | (cx as u32 & 0xFFFF)) as isize);
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(1), lp);
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), lp);
    }
}

fn probe_finish(ok: bool, note: &str) {
    unsafe { KillTimer(None, PROBE_TIMER_ID as usize); }
    println!(
        "probe: up-edge {}/{} samples, down-edge {}/{} samples, main restored={} ({note})",
        PROBE_UP_EDGE.load(Ordering::SeqCst),
        PROBE_UP_TOTAL.load(Ordering::SeqCst),
        PROBE_DN_EDGE.load(Ordering::SeqCst),
        PROBE_DN_TOTAL.load(Ordering::SeqCst),
        ok,
    );
    println!("PROBE {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}

/// Execute a main-panel action at anchor P: right-click, scroll up/down
/// (config::scroll_notches() wheel notches) or hold-2s (left-down now, left-up via HOLD2S_TIMER_ID).
fn perform_main_action(hwnd: HWND, action: &str) {
    let (px, py) = anchor();
    capture_restore(); // tap point — scroll returns the cursor here after executing
    unsafe {
        let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID as usize); // a stale return must not fire mid-action
        let _ = SetCursorPos(px, py);
    }
    match action {
        "right-click" => {
            mouse_send(0, 0, MOUSEEVENTF_RIGHTDOWN);
            std::thread::sleep(std::time::Duration::from_millis(40)); // apps expect a real press duration
            mouse_send(0, 0, MOUSEEVENTF_RIGHTUP);
            println!("action: right-click @ ({px},{py})");
            schedule_action_close(hwnd); // one-shot — panel closes once the click lands
            // Self-check on a worker thread (don't block the panel message loop): did a context
            // menu (class #32768) appear? Lets us verify the result from the log alone.
            std::thread::spawn(|| {
                // "#32768" as UTF-16LE + null terminator (ASCII → 1:1); PCWSTR is !Send so build it here
                let cls: [u16; 7] = [0x23, 0x33, 0x32, 0x37, 0x36, 0x38, 0];
                let none: [u16; 1] = [0];
                let cls_w = windows::core::PCWSTR::from_raw(cls.as_ptr());
                let none_w = windows::core::PCWSTR::from_raw(none.as_ptr());
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(120));
                    if !unsafe { FindWindowW(cls_w, none_w) }.is_err() {
                        println!("menu check: context menu appeared");
                        return;
                    }
                }
                println!("menu check: NO context menu at anchor");
            });
        }
        "scroll-up" | "scroll-down" => {
            // Windows wheel: ±120 = one notch. mouseData must be sign-extended
            // (i32 as u32) — zero-extending a negative delta yields +32767 = a huge UP scroll.
            let delta = (120 * crate::config::scroll_notches()) as i16;
            wheel_send(if action == "scroll-up" { delta } else { -delta });
            println!("action: {} @ ({px},{py})", action);
            schedule_restore(hwnd);
        }
        "hold-2s" => {
            if HOLDING.load(Ordering::SeqCst) == 1 {
                println!("action: hold-2s ignored — already holding");
                return;
            }
            mouse_send(0, 0, MOUSEEVENTF_LEFTDOWN);
            HOLDING.store(1, Ordering::SeqCst);
            unsafe {
                let _ = SetTimer(Some(hwnd), HOLD2S_TIMER_ID as usize, HOLD2S_MS, None);
            }
            println!("action: hold-2s — left-down @ ({px},{py}), up in {HOLD2S_MS}ms");
        }
        _ => {} // unknown action
    }
}

/// Fire one D-pad action. Plan-then-replay model: the center starts/ends
/// planning, direction cells extend the planned path while planning — no mouse events
/// during planning. Release replays the recorded path as a single drag burst.
fn perform_dpad_action(hwnd: HWND, i: usize) {
    let cell = dpad_cells()[i];
    if cell.dir == 9 {
        // Center: start/end planning toggle — 0 = idle (Grab), 1 = planning (Drop).
        let was = DPAD_PLANNING.load(Ordering::SeqCst);
        if was == 0 {
            let (px, py) = anchor();
            *DRAG_PATH.lock().unwrap() = vec![(px, py)];
            DPAD_PLANNING.store(1, Ordering::SeqCst);
            move_anchor(px, py);
            println!("dpad: grab — planning from ({px},{py})");
        } else {
            let path = DRAG_PATH.lock().unwrap().clone();
            DPAD_PLANNING.store(0, Ordering::SeqCst);
            replay_drag(&path);
            // Guard AFTER the blocking replay: injected clicks dispatch only once this
            // handler returns, so the deadline must start here to still cover them.
            REPLAY_GUARD_UNTIL.store(now_ms() + REPLAY_GUARD_MS, Ordering::SeqCst);
            schedule_action_close(hwnd); // drag done — close the overlay
            let (px, py) = anchor();
            move_anchor(px, py); // back to P — the dot marks the reference point
            println!("dpad: release — replay {} point(s)", path.len());
        }
    } else if DPAD_PLANNING.load(Ordering::SeqCst) == 1 {
        const NAMES: [&str; 8] = ["up", "up-right", "right", "down", "up-left", "left", "down-left", "down-right"]; // matches dir_vec numbering
        let (vx, vy) = dir_vec(cell.dir);
        let mut path = DRAG_PATH.lock().unwrap();
        let (lx, ly) = path.last().copied().unwrap_or_else(|| anchor());
        let nx = lx + (vx * DRAG_STEP_PX as f64) as i32;
        let ny = ly + (vy * DRAG_STEP_PX as f64) as i32;
        path.push((nx, ny));
        drop(path);
        move_anchor(nx, ny);
        println!("dpad: step {} -> ({nx},{ny})", NAMES[cell.dir as usize]);
    } else {
        println!("dpad: step ignored — not planning (tap center first)");
    }
}

/// Replay a recorded path as one drag burst: DOWN at the first point, MOVE along
/// the points (REPLAY_STEP_MS apart), UP at the last. A single-point path is just
/// a click at P. Every event is an absolute-coordinate SendInput so the whole drag
/// flows through the input stream in order with consistent button state —
/// SetCursorPos moves bypass the stream (invisible to WH_MOUSE_LL, delivered without
/// MK_LBUTTON while the queued DOWN is still pending), so the target never saw a
/// held-button drag and icons never moved.
fn replay_drag(path: &[(i32, i32)]) {
    if path.is_empty() {
        return;
    }
    // Virtual-desktop normalization: with MOUSEEVENTF_VIRTUALDESK the
    // 0..65535 range spans ALL monitors, origin at the virtual screen's top-left
    // (negative when a monitor sits left of/above the primary).
    let (vx, vy, vw, vh) = unsafe {
        (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN),
         GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
    };
    let norm = |x: i32, y: i32| -> (i32, i32) {
        (((x - vx) as i64 * 65535 / (vw as i64 - 1)) as i32,
         ((y - vy) as i64 * 65535 / (vh as i64 - 1)) as i32)
    };
    let (x0, y0) = norm(path[0].0, path[0].1);
    mouse_send_abs(x0, y0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTDOWN);
    for &(px, py) in &path[1..] {
        std::thread::sleep(std::time::Duration::from_millis(REPLAY_STEP_MS));
        let (nx, ny) = norm(px, py);
        mouse_send_abs(nx, ny, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
    }
    let &(lx, ly) = path.last().unwrap();
    let (nx, ny) = norm(lx, ly);
    mouse_send_abs(nx, ny, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_LEFTUP);
}

/// Absolute-position mouse event (normalized 0..65535 over the VIRTUAL desktop —
/// all monitors), marker-tagged like all self-injection.
fn mouse_send_abs(nx: i32, ny: i32, flags: MOUSE_EVENT_FLAGS) {
    let mut input = INPUT::default();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dx = nx;
    input.Anonymous.mi.dy = ny;
    input.Anonymous.mi.dwFlags = flags | MOUSEEVENTF_VIRTUALDESK;
    input.Anonymous.mi.dwExtraInfo = crate::hook::EXTRA_MARKER as usize;
    unsafe {
        let _ = SendInput(&mut [input], core::mem::size_of::<INPUT>() as i32);
    }
}

/// Normalize an absolute screen point to the 0..65535 VIRTUAL-desktop space
/// (same math as replay_drag — all monitors, origin at the virtual top-left).
fn norm_virtual(x: i32, y: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = unsafe {
        (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN),
         GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN))
    };
    (((x - vx) as i64 * 65535 / (vw as i64 - 1)) as i32,
     ((y - vy) as i64 * 65535 / (vh as i64 - 1)) as i32)
}

/// The edge point a move pulse targets: the middle of the given side of the monitor
/// containing the tap point. Bottom/right use the last valid pixel row/column.
fn edge_point(dir: u8, px: i32, py: i32) -> (i32, i32) {
    unsafe {
        let hm = MonitorFromPoint(POINT { x: px, y: py }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let (l, t, r, b) = if GetMonitorInfoW(hm, &mut mi).as_bool() {
            (mi.rcMonitor.left, mi.rcMonitor.top, mi.rcMonitor.right, mi.rcMonitor.bottom)
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        };
        let cx = l + (r - l) / 2;
        let cy = t + (b - t) / 2;
        match dir {
            0 => (cx, t),     // up — top edge center
            3 => (cx, b - 1), // down — bottom edge
            5 => (l, cy),     // left
            _ => (r - 1, cy), // right
        }
    }
}

// ---------- colors (straight ARGB, 0xAARRGGBB) ----------
// main panel — purple/lavender theme (shared with the settings window)
pub(crate) const PANEL_BG: u32 = 0x661A1033; // ~40% opaque dark-purple panel — clearly see-through
pub(crate) const BTN_FILL: u32 = 0x8C453670; // ~55% opaque mid-purple — background visible through buttons
pub(crate) const BTN_PRESSED: u32 = 0xFFC4B5FD; // lavender accent (brand color)
pub(crate) const ICON_WHITE: u32 = 0xFFFFFFFF;
pub(crate) const ICON_DARK: u32 = 0xFF2A1B4D; // dark purple, for icons on the pressed lavender fill
// dpad layer — teal tone (deliberately different from main)
const DPAD_PANEL_BG: u32 = 0x660C2B2E; // ~40% opaque dark-teal panel
const DPAD_BTN_FILL: u32 = 0x8C11555A; // ~55% opaque mid-teal — background visible through buttons
const DPAD_PRESSED: u32 = 0xFF2DD4BF; // teal-300 accent
const DPAD_ICON_DARK: u32 = 0xFF0B3236; // dark teal, for icons on the pressed fill

// move layer — amber tone (deliberately different from main lavender and dpad teal)
const MOVE_PANEL_BG: u32 = 0x662B1F0C; // ~40% opaque dark warm-brown panel
const MOVE_BTN_FILL: u32 = 0x8C4A3811; // ~55% opaque mid amber-brown — background visible through buttons
const MOVE_ACCENT: u32 = 0xFFFBBF24; // amber-400 accent (pressed fill)
const MOVE_ICON_DARK: u32 = 0xFF2B1F0C; // dark brown, for icons on the pressed fill

#[inline]
fn a(c: u32) -> f64 { ((c >> 24) & 0xFF) as f64 / 255.0 }
#[inline]
fn r(c: u32) -> f64 { ((c >> 16) & 0xFF) as f64 }
#[inline]
fn g(c: u32) -> f64 { ((c >> 8) & 0xFF) as f64 }
#[inline]
fn b(c: u32) -> f64 { (c & 0xFF) as f64 }

/// Per-pixel "over" compositing in straight color space.
#[inline]
pub(crate) fn over(dst: &mut u32, src: u32, cov: f64) {
    let sa = a(src) * cov;
    if sa <= 0.0 {
        return;
    }
    let da = a(*dst);
    let omsa = 1.0 - sa;
    let out_a = (sa + da * omsa) * 255.0;
    let out_r = r(src) * sa + r(*dst) * da * omsa;
    let out_g = g(src) * sa + g(*dst) * da * omsa;
    let out_b = b(src) * sa + b(*dst) * da * omsa;
    *dst = ((out_a.round() as u32 & 0xFF) << 24)
        | ((out_r.round() as u32 & 0xFF) << 16)
        | ((out_g.round() as u32 & 0xFF) << 8)
        | (out_b.round() as u32 & 0xFF);
}

/// Signed distance to a rounded box (negative inside).
#[inline]
fn sd_rounded(px: f64, py: f64, x: f64, y: f64, w: f64, h: f64, rad: f64) -> f64 {
    let qx = (px - (x + w / 2.0)).abs() - (w / 2.0 - rad);
    let qy = (py - (y + h / 2.0)).abs() - (h / 2.0 - rad);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - rad
}

/// Antialiased rounded-rect fill into a straight ARGB buffer.
pub(crate) fn fill_rounded(buf: &mut [u32], img_w: i32, img_h: i32, rect: (f64, f64, f64, f64), rad: f64, color: u32) {
    let (x, y, w, h) = rect;
    let x0 = x.ceil() as i32 - 1;
    let y0 = y.ceil() as i32 - 1;
    let x1 = (x + w).floor() as i32 + 1;
    let y1 = (y + h).floor() as i32 + 1;
    for py in y0.max(0)..y1.min(img_h) {
        for px in x0.max(0)..x1.min(img_w) {
            let d = sd_rounded(px as f64, py as f64, x, y, w, h, rad);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                over(&mut buf[(py * img_w + px) as usize], color, cov);
            }
        }
    }
}

#[inline]
fn dist_seg(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let px = px - ax;
    let py = py - ay;
    let bx = bx - ax;
    let by = by - ay;
    let t = (px * bx + py * by) / (bx * bx + by * by).max(1e-9);
    let t = t.clamp(0.0, 1.0);
    let dx = px - t * bx;
    let dy = py - t * by;
    (dx * dx + dy * dy).sqrt()
}

/// Antialiased single line segment (distance-field stroke).
fn stroke_seg(buf: &mut [u32], img_w: i32, img_h: i32, ax: f64, ay: f64, bx: f64, by: f64, stroke: f64, color: u32) {
    let min_x = ax.min(bx).floor() as i32 - 4;
    let max_x = ax.max(bx).ceil() as i32 + 4;
    let min_y = ay.min(by).floor() as i32 - 4;
    let max_y = ay.max(by).ceil() as i32 + 4;

    for py in min_y.max(0)..max_y.min(img_h) {
        for px in min_x.max(0)..max_x.min(img_w) {
            let d = dist_seg(px as f64, py as f64, ax, ay, bx, by);
            let cov = (stroke / 2.0 + 0.5 - d).clamp(0.0, 1.0);
            if cov > 0.0 {
                over(&mut buf[(py * img_w + px) as usize], color, cov);
            }
        }
    }
}

/// Antialiased rounded-rect outline (edge distance field), stroke centered on the edge.
pub(crate) fn stroke_rounded(buf: &mut [u32], img_w: i32, img_h: i32, rect: (f64, f64, f64, f64), rad: f64, stroke: f64, color: u32) {
    let (x, y, w, h) = rect;
    let x0 = (x - stroke).floor() as i32 - 1;
    let y0 = (y - stroke).floor() as i32 - 1;
    let x1 = (x + w + stroke).ceil() as i32 + 1;
    let y1 = (y + h + stroke).ceil() as i32 + 1;
    for py in y0.max(0)..y1.min(img_h) {
        for px in x0.max(0)..x1.min(img_w) {
            let d = sd_rounded(px as f64, py as f64, x, y, w, h, rad);
            let cov = (stroke / 2.0 + 0.5 - d.abs()).clamp(0.0, 1.0);
            if cov > 0.0 {
                over(&mut buf[(py * img_w + px) as usize], color, cov);
            }
        }
    }
}

/// Edge-move glyph: rounded square (the screen) with four arrows pointing out of its edges.
fn draw_move_edge(buf: &mut [u32], img_w: i32, img_h: i32, btn: &Btn, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42; // leave room for the label below
    const S: f64 = 13.0; // half-size of the screen square
    stroke_rounded(buf, img_w, img_h, (cx - S, cy - S, 2.0 * S, 2.0 * S), 4.0, 2.5, color);
    draw_arrow(buf, img_w, img_h, cx, cy - S - 2.0, 0, 13.0, color); // up
    draw_arrow(buf, img_w, img_h, cx, cy + S + 2.0, 3, 13.0, color); // down
    draw_arrow(buf, img_w, img_h, cx - S - 2.0, cy, 5, 13.0, color); // left
    draw_arrow(buf, img_w, img_h, cx + S + 2.0, cy, 2, 13.0, color); // right
}

/// Antialiased chevron (two segments), centered in the button's icon area.
fn draw_chevron(buf: &mut [u32], img_w: i32, img_h: i32, btn: &Btn, dir: u8, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42; // leave room for the label below
    const HW: f64 = 13.0; // half-width of the V
    const VH: f64 = 8.0; // half-height of the V
    const S: f64 = 4.0;

    let (ax, ay, bx, by, cx2, cy2): (f64, f64, f64, f64, f64, f64) = match dir {
        0 => (cx - HW, cy + VH, cx, cy - VH, cx + HW, cy + VH), // up
        _ => (cx - HW, cy - VH, cx, cy + VH, cx + HW, cy - VH), // down
    };
    stroke_seg(buf, img_w, img_h, ax, ay, bx, by, S, color);
    stroke_seg(buf, img_w, img_h, bx, by, cx2, cy2, S, color);
}

/// Antialiased circle outline.
fn draw_ring(buf: &mut [u32], img_w: i32, img_h: i32, cx: f64, cy: f64, radius: f64, stroke: f64, color: u32) {
    let m = (radius + stroke).ceil() as i32 + 1;
    for py in (cy - m as f64).max(0.0).floor() as i32..(cy + m as f64).min(img_h as f64).ceil() as i32 {
        for px in (cx - m as f64).max(0.0).floor() as i32..(cx + m as f64).min(img_w as f64).ceil() as i32 {
            let d = ((px as f64 - cx) * (px as f64 - cx) + (py as f64 - cy) * (py as f64 - cy)).sqrt() - radius;
            let cov = (stroke / 2.0 + 0.5 - d.abs()).clamp(0.0, 1.0);
            if cov > 0.0 {
                over(&mut buf[(py * img_w + px) as usize], color, cov);
            }
        }
    }
}

/// Mouse glyph: capsule outline + button-divider line (right-click).
fn draw_mouse(buf: &mut [u32], img_w: i32, img_h: i32, btn: &Btn, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42;
    const MW: f64 = 14.0; // body width
    const MH: f64 = 19.0; // body height
    const S: f64 = 2.5;

    let (bx, by) = (cx - MW / 2.0, cy - MH / 2.0);
    let x0 = bx.ceil() as i32 - 5;
    let y0 = by.ceil() as i32 - 5;
    let x1 = (bx + MW).floor() as i32 + 5;
    let y1 = (by + MH).floor() as i32 + 5;
    for py in y0.max(0)..y1.min(img_h) {
        for px in x0.max(0)..x1.min(img_w) {
            let d = sd_rounded(px as f64, py as f64, bx, by, MW, MH, MW / 2.0); // capsule
            let cov = (S / 2.0 + 0.5 - d.abs()).clamp(0.0, 1.0);
            if cov > 0.0 {
                over(&mut buf[(py * img_w + px) as usize], color, cov);
            }
        }
    }
    // divider line across the body (button split)
    stroke_seg(buf, img_w, img_h, bx + S, cy - MH * 0.18, bx + MW - S, cy - MH * 0.18, S, color);
}

/// Clock glyph: ring + hand pointing to 12 (hold duration).
fn draw_clock(buf: &mut [u32], img_w: i32, img_h: i32, btn: &Btn, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42;
    const R: f64 = 10.5;
    const S: f64 = 2.5;

    draw_ring(buf, img_w, img_h, cx, cy, R, S, color);
    stroke_seg(buf, img_w, img_h, cx, cy, cx, cy - R * 0.72, S, color); // hand
}

/// Antialiased arrow (shaft + head), centered at (cx, cy), pointing in dir.
fn draw_arrow(buf: &mut [u32], img_w: i32, img_h: i32, cx: f64, cy: f64, dir: u8, len: f64, color: u32) {
    let (dx, dy) = dir_vec(dir);
    let n = (dx * dx + dy * dy).sqrt();
    let ux = dx / n;
    let uy = dy / n;
    const HS: f64 = 9.0; // head size
    const S: f64 = 4.0;

    let tx = cx + ux * (len / 2.0);
    let ty = cy + uy * (len / 2.0);
    let bx = cx - ux * (len / 2.0);
    let by = cy - uy * (len / 2.0);
    stroke_seg(buf, img_w, img_h, bx, by, tx, ty, S, color);
    // head: from the tip, back along the shaft rotated ±45°
    const C: f64 = 0.70710678; // cos45 = sin45
    let hx = -ux;
    let hy = -uy;
    let h1x = hx * C - hy * C;
    let h1y = hx * C + hy * C;
    let h2x = hx * C + hy * C;
    let h2y = -hx * C + hy * C;
    stroke_seg(buf, img_w, img_h, tx, ty, tx + HS * h1x, ty + HS * h1y, S, color);
    stroke_seg(buf, img_w, img_h, tx, ty, tx + HS * h2x, ty + HS * h2y, S, color);
}

/// Move glyph: 4-way arrow cross (main panel drag button).
fn draw_move(buf: &mut [u32], img_w: i32, img_h: i32, btn: &Btn, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42;
    for d in [0u8, 2, 3, 5] {
        draw_arrow(buf, img_w, img_h, cx, cy, d, 26.0, color);
    }
}

/// Center cell icon: small "button" — outline (up/idle) or filled (pressed/grabbed).
fn draw_center_btn(buf: &mut [u32], img_w: i32, img_h: i32, btn: &DpadCell, filled: bool, color: u32) {
    let (x, y, w, h) = btn.rect;
    let cx = x + w / 2.0;
    let cy = y + h * 0.42;
    const BW: f64 = 22.0;
    const BH: f64 = 15.0;
    let (bx, by) = (cx - BW / 2.0, cy - BH / 2.0);
    if filled {
        fill_rounded(buf, img_w, img_h, (bx, by, BW, BH), 4.0, color);
    } else {
        stroke_rounded(buf, img_w, img_h, (bx, by, BW, BH), 4.0, 2.5, color);
    }
}

fn is_pressed(set: &[u8], i: usize) -> bool { set.iter().any(|&s| s as usize == i) }

/// Render the main panel into a straight ARGB buffer (not yet premultiplied).
fn render_main(pressed: &[u8]) -> Vec<u32> {
    let (w, h) = OVERLAY_SIZE;
    let mut buf = vec![0u32; (w * h) as usize];

    // 1. panel background
    fill_rounded(&mut buf, w, h, (0.0, 0.0, w as f64, h as f64), PANEL_RADIUS, PANEL_BG);

    // 2. buttons + icons
    for (i, btn) in buttons().iter().enumerate() {
        let is_p = is_pressed(pressed, i);
        fill_rounded(&mut buf, w, h, btn.rect, BTN_RADIUS, if is_p { BTN_PRESSED } else { BTN_FILL });
        if !is_p {
            // subtle lavender edge so buttons read as distinct tiles on the panel
            stroke_rounded(&mut buf, w, h, btn.rect, BTN_RADIUS, 1.2, 0x40C4B5FD);
        }
        let col = if is_p { ICON_DARK } else { ICON_WHITE };
        match btn.icon {
            0 => draw_chevron(&mut buf, w, h, btn, 0, col), // scroll-up
            1 => draw_chevron(&mut buf, w, h, btn, 1, col), // scroll-down
            2 => draw_mouse(&mut buf, w, h, btn, col), // right-click
            3 => draw_clock(&mut buf, w, h, btn, col), // hold-2s
            5 => draw_move(&mut buf, w, h, btn, col), // drag entry
            7 => draw_move_edge(&mut buf, w, h, btn, col), // edge-move entry
            _ => {} // no icon
        }
    }

    buf
}

/// Render the D-pad layer (teal). The center cell stays in pressed state while planning.
fn render_dpad(pressed: &[u8]) -> Vec<u32> {
    let (w, h) = DPAD_SIZE;
    let mut buf = vec![0u32; (w * h) as usize];

    fill_rounded(&mut buf, w, h, (0.0, 0.0, w as f64, h as f64), PANEL_RADIUS, DPAD_PANEL_BG);

    for (i, cell) in dpad_cells().iter().enumerate() {
        let is_p = is_pressed(pressed, i);
        fill_rounded(&mut buf, w, h, cell.rect, BTN_RADIUS, if is_p { DPAD_PRESSED } else { DPAD_BTN_FILL });
        if !is_p {
            stroke_rounded(&mut buf, w, h, cell.rect, BTN_RADIUS, 1.2, 0x402DD4BF);
        }
        let col = if is_p { DPAD_ICON_DARK } else { ICON_WHITE };
        if cell.dir == 9 {
            draw_center_btn(&mut buf, w, h, cell, is_p, col); // filled while grabbed
        } else {
            let (x, y, cw, ch) = cell.rect;
            draw_arrow(&mut buf, w, h, x + cw / 2.0, y + ch * 0.42, cell.dir, 32.0, col);
        }
    }

    buf
}

// ---------- GDI label coverage + layered-window blit ----------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Rasterize all labels in black on a white-filled DIB and return each label's
/// bounding box (x, y, w, h) in pixel coordinates. The caller reads the DIB as
/// a coverage map: cov = (255 - min(R,G,B)) / 255 per pixel.
/// `font_px`: font size; `center`: true = vertically centered (text-only settings
/// buttons), false = top at 60% of the box (overlay: icon above, label below).
unsafe fn draw_labels_black(hdc: HDC, labels: &[(f64, f64, f64, f64, &str)], font_px: i32, center: bool) -> Vec<[u32; 4]> {
    let mut lf = LOGFONTW::default();
    lf.lfHeight = -font_px;
    lf.lfWeight = 600; // semibold
    lf.lfCharSet = FONT_CHARSET(9); // HANGEUL_CHARSET — required for GDI font linking of Hangul glyphs
    for (i, ch) in wide("Malgun Gothic").iter().enumerate() {
        lf.lfFaceName[i] = *ch;
    }

    let font = CreateFontIndirectW(&lf);
    SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00000000)); // black on white — coverage pass

    let mut boxes = Vec::new();
    for &(lx, ly, lw, lh, text) in labels {
        let wc16 = wide(text);
        let mut sz = SIZE::default();
        GetTextExtentPoint32W(hdc, wc16.as_slice(), &mut sz);

        let tx = ((lx + lw / 2.0).round() - sz.cx as f64 / 2.0).round() as u32;
        let ty = if center {
            (ly + (lh - sz.cy as f64) / 2.0).round() as u32
        } else {
            (ly + lh * 0.60) as u32
        };

        TextOutW(hdc, tx as i32, ty as i32, wc16.as_slice());
        boxes.push([tx, ty, sz.cx as u32, sz.cy as u32]);
    }
    DeleteObject(font.into());
    boxes
}

fn main_labels() -> Vec<(f64, f64, f64, f64, &'static str)> {
    buttons().iter().map(|b| (b.rect.0, b.rect.1, b.rect.2, b.rect.3, b.label)).collect()
}

fn dpad_labels() -> Vec<(f64, f64, f64, f64, &'static str)> {
    let planning = DPAD_PLANNING.load(Ordering::SeqCst) == 1;
    dpad_cells()
        .iter()
        .map(|c| {
            (
                c.rect.0,
                c.rect.1,
                c.rect.2,
                c.rect.3,
                if c.dir == 9 {
                    if planning { "Drop" } else { "Grab" }
                } else {
                    "" // arrows are self-evident — no text labels
                },
            )
        })
        .collect()
}

/// Full repaint: manual shapes + GDI label coverage -> premultiply -> UpdateLayeredWindow.
/// `label_colors` gives each label its composite color (one per label entry).
/// `font_px`/`center`: label font size and vertical placement (see draw_labels_black).
pub(crate) unsafe fn blit(hwnd: HWND, w: i32, h: i32, shapes: Vec<u32>, labels: &[(f64, f64, f64, f64, &str)], label_colors: &[u32], font_px: i32, center: bool) -> Result<()> {
    let hdc_mem = CreateCompatibleDC(None);
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits = std::ptr::null_mut();
    let dib = CreateDIBSection(Some(hdc_mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    SelectObject(hdc_mem, dib.into());

    // Fill the DIB white and rasterize labels in black -> per-pixel coverage.
    {
        let m = bits as *mut u32;
        for i in 0..(w * h) as usize {
            *m.add(i) = 0xFFFFFFFF;
        }
    }
    let boxes = draw_labels_black(hdc_mem, labels, font_px, center);

    // Composite: shapes, then each label's color at its coverage, then premultiply in place.
    let n = (w * h) as usize;
    let dst = bits as *mut u32;
    for i in 0..n {
        let mut c = shapes[i];
        let px = (i % w as usize) as u32;
        let py = (i / w as usize) as u32;
        for (k, lb) in boxes.iter().enumerate() {
            if px >= lb[0] && px < lb[0] + lb[2] && py >= lb[1] && py < lb[1] + lb[3] {
                let p = *dst.add(i);
                let m = ((p & 0xFF).min((p >> 8) & 0xFF)).min((p >> 16) & 0xFF) as f64;
                let cov = (255.0 - m) / 255.0;
                if cov > 0.0 {
                    let col = label_colors[k];
                    over(&mut c, col, cov);
                }
                break;
            }
        }
        let al = ((c >> 24) & 0xFF) as u32;
        let rr = (((c >> 16) & 0xFF) as u32 * al) / 255;
        let gg = (((c >> 8) & 0xFF) as u32 * al) / 255;
        let bb = ((c & 0xFF) as u32 * al) / 255;
        *dst.add(i) = (al << 24) | (rr << 16) | (gg << 8) | bb;
    }

    let mut wp = RECT::default();
    GetWindowRect(hwnd, &mut wp)?;
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    UpdateLayeredWindow(
        hwnd,
        None,
        Some(&POINT { x: wp.left, y: wp.top }),
        Some(&SIZE { cx: w, cy: h }),
        Some(hdc_mem),
        Some(&POINT { x: 0, y: 0 }),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    )?;

    DeleteDC(hdc_mem);
    Ok(())
}

/// Repaint the main panel (lavender).
pub fn repaint(hwnd: HWND) -> Result<()> {
    unsafe {
        let mut pressed = Vec::new();
        if FLASH.load(Ordering::SeqCst) != 255 {
            pressed.push(FLASH.load(Ordering::SeqCst));
        }
        let labels = main_labels();
        let colors: Vec<u32> = (0..labels.len())
            .map(|k| if is_pressed(&pressed, k) { ICON_DARK } else { ICON_WHITE })
            .collect();
        blit(hwnd, OVERLAY_SIZE.0, OVERLAY_SIZE.1, render_main(&pressed), &labels, &colors, 15, false)
    }
}

/// Repaint the D-pad layer (teal).
pub fn repaint_dpad(hwnd: HWND) -> Result<()> {
    unsafe {
        let mut pressed = Vec::new();
        if DPAD_PLANNING.load(Ordering::SeqCst) == 1 {
            pressed.push(4); // center cell index
        }
        if DPAD_HELD.load(Ordering::SeqCst) != 255 {
            pressed.push(DPAD_HELD.load(Ordering::SeqCst));
        }
        let labels = dpad_labels();
        let colors: Vec<u32> = (0..labels.len())
            .map(|k| if is_pressed(&pressed, k) { DPAD_ICON_DARK } else { ICON_WHITE })
            .collect();
        blit(hwnd, DPAD_SIZE.0, DPAD_SIZE.1, render_dpad(&pressed), &labels, &colors, 15, false)
    }
}

/// Render the Move layer (amber, 2x2 grid — arrows are self-evident, no labels).
fn render_move(pressed: &[u8]) -> Vec<u32> {
    let (w, h) = MOVE_SIZE;
    let mut buf = vec![0u32; (w * h) as usize];

    // Amber tone — deliberately distinct from main (lavender) and dpad (teal).
    fill_rounded(&mut buf, w, h, (0.0, 0.0, w as f64, h as f64), PANEL_RADIUS, MOVE_PANEL_BG);

    for (i, cell) in move_cells().iter().enumerate() {
        let is_p = is_pressed(pressed, i);
        fill_rounded(&mut buf, w, h, cell.rect, BTN_RADIUS, if is_p { MOVE_ACCENT } else { MOVE_BTN_FILL });
        if !is_p {
            stroke_rounded(&mut buf, w, h, cell.rect, BTN_RADIUS, 1.2, 0x40FBBF24);
        }
        let col = if is_p { MOVE_ICON_DARK } else { ICON_WHITE };
        let (x, y, cw, ch) = cell.rect;
        draw_arrow(&mut buf, w, h, x + cw / 2.0, y + ch * 0.45, cell.dir, 32.0, col);
    }

    buf
}

/// Repaint the Move layer (amber).
pub fn repaint_move(hwnd: HWND) -> Result<()> {
    unsafe {
        let mut pressed = Vec::new();
        if MOVE_PRESSED.load(Ordering::SeqCst) != 255 {
            pressed.push(MOVE_PRESSED.load(Ordering::SeqCst));
        }
        blit(hwnd, MOVE_SIZE.0, MOVE_SIZE.1, render_move(&pressed), &[], &[], 15, false)
    }
}

// ---------- window class + message loop glue ----------

fn kill_all_timers(hwnd: HWND) {
    unsafe {
        for id in [HOLD_TIMER_ID, REPEAT_TIMER_ID, HOLD2S_TIMER_ID, FLASH_TIMER_ID, RESTORE_TIMER_ID, ACTION_CLOSE_TIMER_ID, MOVE_HOLD_TIMER_ID] {
            let _ = KillTimer(Some(hwnd), id as usize);
        }
    }
}

/// Global idle timer — lives on the message window, NOT on the layer:
/// one countdown covers every layer, reset by any button tap on any layer.
pub fn reset_idle_timer() {
    let h = crate::hook::msg_hwnd();
    if h.is_invalid() {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(h), IDLE_TIMER_ID as usize);
        let _ = SetTimer(Some(h), IDLE_TIMER_ID as usize, crate::config::overlay_idle_ms(), None);
    }
}

fn kill_idle_timer() {
    let h = crate::hook::msg_hwnd();
    if !h.is_invalid() {
        unsafe { let _ = KillTimer(Some(h), IDLE_TIMER_ID as usize); }
    }
}

fn close_app(hwnd: HWND) {
    unsafe {
        release_hold_if_active(); // never leave the left button down on exit
        PENDING_ACTION.store(255, Ordering::SeqCst); // stale action must not fire after exit
        kill_all_timers(hwnd);
        hide_anchor(); // global dot goes away with the app
        let _ = DestroyWindow(hwnd);
        MAIN_HWNDA.store(0, Ordering::SeqCst);
        DPAD_HWNDA.store(0, Ordering::SeqCst);
        MOVE_HWNDA.store(0, Ordering::SeqCst);
        VISIBLE_HWND.store(0, Ordering::SeqCst);
        PostQuitMessage(0);
    }
    kill_idle_timer();
    crate::hook::guard_clear();
}

/// Mark an overlay layer as the visible one; while set, a click outside it dismisses the overlay.
pub fn set_visible(h: HWND) {
    VISIBLE_HWND.store(h.0 as isize, Ordering::SeqCst);
    crate::hook::guard_set(h);
}

/// Is any overlay layer currently visible? (settings open/quit dismiss only when open.)
pub fn is_visible() -> bool {
    VISIBLE_HWND.load(Ordering::SeqCst) != 0
}

/// Close every layer — the visible one and the hidden main when the dpad was up —
/// kill timers, clear state, resume detection. Auto-close routes: global idle,
/// outside-click dismiss and one-shot action completion; settings open/quit also
/// dismiss via this path.
pub fn dismiss_all(reason: &str) {
    unsafe {
        release_hold_if_active(); // close mid-hold — don't leave the button down
        finish_move_hold_if_active(); // close mid-pulse — don't strand the cursor at the edge
        let vh = HWND(VISIBLE_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        if !vh.is_invalid() {
            kill_all_timers(vh);
            let _ = DestroyWindow(vh);
        }
        let mh = HWND(MAIN_HWNDA.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        if !mh.is_invalid() && (mh.0 as isize) != vh.0 as isize {
            kill_all_timers(mh);
            let _ = DestroyWindow(mh);
        }
        MAIN_HWNDA.store(0, Ordering::SeqCst);
        DPAD_HWNDA.store(0, Ordering::SeqCst);
        MOVE_HWNDA.store(0, Ordering::SeqCst);
        VISIBLE_HWND.store(0, Ordering::SeqCst);
    }
    kill_idle_timer();
    hide_anchor(); // dot goes away with the panel
    crate::hook::guard_clear();
    crate::hook::detection_set(true);
    println!("overlay: {reason} — panel closed, detection resumed");
}

/// Re-show the main panel after the D-pad layer closes. The global idle timer keeps
/// running on the message window — the Drop tap that got us here already reset it.
fn restart_main(mh: HWND) {
    unsafe {
        ShowWindow(mh, SW_SHOW);
    }
    set_visible(mh);
}

/// Close the D-pad layer and return to the main panel. Cancel-safe:
/// if planning is active, drop the path — nothing was ever pressed, so no mouse
/// events are needed. The anchor dot stays visible (global) and parks back at P.
fn close_dpad(dh: HWND) {
    DPAD_PENDING_DROP.store(0, Ordering::SeqCst); // a pending drop must not fire after close
    if DPAD_PLANNING.load(Ordering::SeqCst) == 1 {
        *DRAG_PATH.lock().unwrap() = Vec::new();
        DPAD_PLANNING.store(0, Ordering::SeqCst);
        let (px, py) = anchor();
        move_anchor(px, py);
        println!("dpad: planning cancelled on close — no mouse events");
    }
    unsafe {
        kill_all_timers(dh);
        let _ = DestroyWindow(dh);
        DPAD_HWNDA.store(0, Ordering::SeqCst);
        let mh = HWND(MAIN_HWNDA.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        if !mh.is_invalid() {
            restart_main(mh);
        }
    }
}

/// Close the Move layer and return to the main panel. A mid-hold pulse restores the
/// cursor first — never leave it stranded at the screen edge.
fn close_move(mv: HWND) {
    finish_move_hold_if_active();
    unsafe {
        kill_all_timers(mv);
        let _ = DestroyWindow(mv);
        MOVE_HWNDA.store(0, Ordering::SeqCst);
        println!("move: closed — back to main");
        let mh = HWND(MAIN_HWNDA.load(Ordering::SeqCst) as *mut core::ffi::c_void);
        if !mh.is_invalid() {
            restart_main(mh);
        }
    }
}

unsafe extern "system" fn main_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            if now_ms() < REPLAY_GUARD_UNTIL.load(Ordering::SeqCst) {
                return LRESULT(0); // our own replay click — it belongs to the target app under P, not us
            }
            let lp = lparam.0;
            let p = POINT {
                x: (lp & 0xFFFF) as i16 as i32,
                y: ((lp >> 16) & 0xFFFF) as i16 as i32,
            };
            if let Some(i) = hit_test(p) {
                reset_idle_timer();
                FLASH.store(i as u8, Ordering::SeqCst);
                // LUP fallback: tool windows can miss the button-up (no implicit capture) —
                // clear the pressed flash on a short deadline so a tap never sticks.
                unsafe {
                    // A fresh tap cancels any pending one-shot auto-close (e.g. right-click
                    // then quickly scroll — the scroll must keep the panel open).
                    let _ = KillTimer(Some(hwnd), ACTION_CLOSE_TIMER_ID as usize);
                    let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID as usize);
                    let _ = SetTimer(Some(hwnd), FLASH_TIMER_ID as usize, FLASH_MS, None);
                }
                let action = buttons()[i].action;
                println!("tap: {} @ ({}, {})", action, p.x, p.y);
                if action == "drag" {
                    // Open the D-pad layer in place of the main panel.
                    let _ = repaint(hwnd);
                    // Place relative to anchor P — the larger D-pad centered on the
                    // panel would cover P and eat the replay's LEFTDOWN.
                    let (px, py) = anchor();
                    let (dx, dy) = position_for_size(px, py, DPAD_SIZE);
                    MAIN_HWNDA.store(hwnd.0 as isize, Ordering::SeqCst);
                    PENDING_ACTION.store(255, Ordering::SeqCst); // main hidden — stale action must not fire
                    release_hold_if_active(); // main is about to hide — finish any hold now
                    kill_all_timers(hwnd); // don't run while hidden
                    ShowWindow(hwnd, SW_HIDE);
                    match create_dpad(dx, dy) {
                        Ok(dh) => {
                            if repaint_dpad(dh).is_ok() {
                                DPAD_HWNDA.store(dh.0 as isize, Ordering::SeqCst);
                                ShowWindow(dh, SW_SHOW);
                                set_visible(dh);
                            } else {
                                restart_main(hwnd); // blit failed — restore main
                            }
                        }
                        Err(_) => restart_main(hwnd), // dpad creation failed — restore main
                    }
                } else if action == "move" {
                    // Open the Move layer in place of the main panel.
                    let _ = repaint(hwnd);
                    let (px, py) = anchor();
                    let (dx, dy) = position_for_size(px, py, MOVE_SIZE);
                    MAIN_HWNDA.store(hwnd.0 as isize, Ordering::SeqCst);
                    PENDING_ACTION.store(255, Ordering::SeqCst); // main hidden — stale action must not fire
                    release_hold_if_active(); // main is about to hide — finish any hold now
                    kill_all_timers(hwnd); // don't run while hidden
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    match create_move(dx, dy) {
                        Ok(mv) => {
                            if repaint_move(mv).is_ok() {
                                MOVE_HWNDA.store(mv.0 as isize, Ordering::SeqCst);
                                let _ = ShowWindow(mv, SW_SHOW);
                                set_visible(mv);
                            } else {
                                restart_main(hwnd); // blit failed — restore main
                            }
                        }
                        Err(_) => restart_main(hwnd), // move layer creation failed — restore main
                    }
                } else if action == "hold-2s" {
                    perform_main_action(hwnd, action); // hold must start on press — execute now
                    let _ = repaint(hwnd);
                } else {
                    PENDING_ACTION.store(i as u8, Ordering::SeqCst); // defer to button-up (see static)
                }
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_LBUTTONUP => {
            unsafe { let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID as usize); }
            if PENDING_ACTION.load(Ordering::SeqCst) != 255 {
                // Execute at release: the cursor is still on the button, so nothing the user
                // does afterwards can land on the target while a menu is opening.
                let i = PENDING_ACTION.swap(255, Ordering::SeqCst) as usize;
                perform_main_action(hwnd, buttons()[i].action);
            }
            if FLASH.load(Ordering::SeqCst) != 255 {
                FLASH.store(255, Ordering::SeqCst);
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            close_app(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == HOLD2S_TIMER_ID as usize => {
            release_hold_if_active(); // hold-2s complete — left-up at P
            println!("action: hold-2s — released after 2s");
            schedule_action_close(hwnd); // one-shot — panel closes once the hold completes
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == RESTORE_TIMER_ID as usize => {
            unsafe { let _ = KillTimer(Some(hwnd), RESTORE_TIMER_ID as usize); }
            do_restore();
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ACTION_CLOSE_TIMER_ID as usize => {
            dismiss_all("action done"); // one-shot action complete — auto-close
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == FLASH_TIMER_ID as usize => {
            // Fallback fired: the button-up never reached us (finger slid off the panel).
            if PENDING_ACTION.load(Ordering::SeqCst) != 255 {
                let i = PENDING_ACTION.swap(255, Ordering::SeqCst) as usize;
                perform_main_action(hwnd, buttons()[i].action);
            }
            if FLASH.load(Ordering::SeqCst) != 255 {
                println!("flash: auto-cleared — no LUP received (capture miss confirmed)");
                FLASH.store(255, Ordering::SeqCst);
                let _ = repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn dpad_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            if now_ms() < REPLAY_GUARD_UNTIL.load(Ordering::SeqCst) {
                return LRESULT(0); // our own replay click — it belongs to the target app under P, not us
            }
            let lp = lparam.0;
            let p = POINT {
                x: (lp & 0xFFFF) as i16 as i32,
                y: ((lp >> 16) & 0xFFFF) as i16 as i32,
            };
            if let Some(i) = dpad_hit_test(p) {
                reset_idle_timer();
                DPAD_HELD.store(i as u8, Ordering::SeqCst);
                if dpad_cells()[i].dir == 9 && DPAD_PLANNING.load(Ordering::SeqCst) == 1 {
                    // Drop: defer the replay to button-up (same reason as main-panel actions) —
                    // Moonlight delivers the tap's release at the CURRENT cursor position, and
                    // the replay moves the cursor to P; executing on down lets that release
                    // land mid-replay and cancel the drag before it starts.
                    DPAD_PENDING_DROP.store(1, Ordering::SeqCst);
                    let _ = SetTimer(Some(hwnd), FLASH_TIMER_ID as usize, FLASH_MS, None); // LUP-miss fallback
                } else {
                    perform_dpad_action(hwnd, i);
                    // Direction cells auto-repeat while held. The center is a tap
                    // toggle — no repeat.
                    if dpad_cells()[i].dir != 9 {
                        SetTimer(Some(hwnd), HOLD_TIMER_ID as usize, HOLD_FIRST_MS, None);
                    }
                }
                let _ = repaint_dpad(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if DPAD_HELD.load(Ordering::SeqCst) != 255 {
                DPAD_HELD.store(255, Ordering::SeqCst);
                let _ = KillTimer(Some(hwnd), HOLD_TIMER_ID as usize);
                let _ = KillTimer(Some(hwnd), REPEAT_TIMER_ID as usize);
                let _ = repaint_dpad(hwnd);
            }
            let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID as usize);
            if DPAD_PENDING_DROP.swap(0, Ordering::SeqCst) == 1 {
                perform_dpad_action(hwnd, 4); // center: run the deferred replay now — cursor still on the button, so the user's release can't land inside it
                let _ = repaint_dpad(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0 as u32;
            match id {
                HOLD_TIMER_ID | REPEAT_TIMER_ID => {
                    let held = DPAD_HELD.load(Ordering::SeqCst);
                    if held != 255 {
                        perform_dpad_action(hwnd, held as usize);
                        SetTimer(Some(hwnd), REPEAT_TIMER_ID as usize, REPEAT_MS, None);
                    }
                    LRESULT(0)
                }
                FLASH_TIMER_ID => {
                    // LUP never reached us (capture miss) — run the pending drop anyway.
                    if DPAD_PENDING_DROP.swap(0, Ordering::SeqCst) == 1 {
                        perform_dpad_action(hwnd, 4);
                        let _ = repaint_dpad(hwnd);
                    }
                    LRESULT(0)
                }
                ACTION_CLOSE_TIMER_ID => {
                    dismiss_all("action done"); // drag replay complete — auto-close
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            close_dpad(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn move_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let lp = lparam.0;
            let p = POINT {
                x: (lp & 0xFFFF) as i16 as i32,
                y: ((lp >> 16) & 0xFFFF) as i16 as i32,
            };
            if let Some(i) = move_hit_test(p) {
                reset_idle_timer();
                // Tap point in SCREEN coords (lparam is client) — the pulse's restore target.
                let mut sp = p;
                let _ = ClientToScreen(hwnd, &mut sp);
                MOVE_TAP_X.store(sp.x, Ordering::SeqCst);
                MOVE_TAP_Y.store(sp.y, Ordering::SeqCst);
                MOVE_PRESSED.store(i as u8, Ordering::SeqCst);
                println!("tap: move-{} @ ({}, {})", move_dir_name(move_cells()[i].dir), p.x, p.y);
                unsafe {
                    let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID as usize);
                    let _ = SetTimer(Some(hwnd), FLASH_TIMER_ID as usize, FLASH_MS, None); // LUP-miss fallback
                }
                MOVE_PENDING.store(i as u8, Ordering::SeqCst); // defer the pulse to button-up
                let _ = repaint_move(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if MOVE_PRESSED.load(Ordering::SeqCst) != 255 {
                MOVE_PRESSED.store(255, Ordering::SeqCst);
                let _ = repaint_move(hwnd);
            }
            unsafe { let _ = KillTimer(Some(hwnd), FLASH_TIMER_ID as usize); }
            if MOVE_PENDING.load(Ordering::SeqCst) != 255 {
                // Execute at release: the cursor is still on the button, so Moonlight's
                // release can't land outside the panel and dismiss the layer mid-pulse.
                let i = MOVE_PENDING.swap(255, Ordering::SeqCst) as usize;
                perform_move_action(hwnd, move_cells()[i].dir);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0 as u32;
            match id {
                FLASH_TIMER_ID => {
                    // LUP never reached us (capture miss) — run the pending pulse anyway.
                    if MOVE_PENDING.load(Ordering::SeqCst) != 255 {
                        let i = MOVE_PENDING.swap(255, Ordering::SeqCst) as usize;
                        perform_move_action(hwnd, move_cells()[i].dir);
                    }
                    LRESULT(0)
                }
                MOVE_HOLD_TIMER_ID => {
                    do_move_restore();
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
        WM_KEYDOWN if wparam.0 as u16 == VK_ESCAPE.0 => {
            close_move(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn create_overlay(x: i32, y: i32) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?.into();

        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpszClassName = w!("TapKeyOverlay");
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(main_wnd_proc);
        wc.hInstance = hinst;
        // Class cursor is required: with NULL nobody ever sets the cursor over our
        // client area, so a busy cursor picked up at the action target sticks here
        // (hourglass after button taps). DefWindowProc now asserts the arrow.
        wc.hCursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        RegisterClassExW(&mut wc);

        let h = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("TapKeyOverlay"),
            w!("TapKey"),
            WS_POPUP,
            x,
            y,
            OVERLAY_SIZE.0,
            OVERLAY_SIZE.1,
            None,
            None,
            Some(hinst),
            None,
        )?;

        // Global anchor dot — created once, shown in every mode; best-effort.
        if anchor_hwnd().is_invalid() {
            if let Ok(ah) = create_anchor() {
                ANCHOR_HWND.store(ah.0 as isize, Ordering::SeqCst);
            }
        }
        move_anchor(anchor().0, anchor().1); // dot at P (set via set_anchor before open)
        Ok(h)
    }
}

/// Place the main panel near anchor P without covering it: offset ≥24px to the right,
/// flip left near the right edge, clamp inside the monitor work area.
pub fn position_for_anchor(px: i32, py: i32) -> (i32, i32) {
    position_for_size(px, py, OVERLAY_SIZE)
}

/// Same never-cover-P placement for any layer size. The D-pad is larger than the main
/// panel — centering it on the panel would cover P and eat the replay's LEFTDOWN.
fn position_for_size(px: i32, py: i32, (w, h): (i32, i32)) -> (i32, i32) {
    const OFF: i32 = 24;
    // Clamp to the work area of the monitor containing P — the panel
    // must stay on P's monitor, not get pulled to the primary's edge.
    let (left, top, right, bottom) = unsafe {
        let hm = MonitorFromPoint(POINT { x: px, y: py }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetMonitorInfoW(hm, &mut mi).as_bool() {
            (mi.rcWork.left, mi.rcWork.top, mi.rcWork.right, mi.rcWork.bottom)
        } else {
            (0, 0, GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN))
        }
    };
    let mut x = px + OFF;
    if x + w > right {
        x = px - OFF - w;
    }
    if x < left {
        x = left;
    }
    let mut y = py - h / 2;
    if y < top {
        y = top;
    }
    if y + h > bottom {
        y = bottom - h;
    }
    (x, y)
}

// ---------- Anchor dot (global marker at P) ----------

unsafe extern "system" fn anchor_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => {
            let hdc = GetDC(Some(hwnd));
            let brush = CreateSolidBrush(COLORREF(ANCHOR_KEY));
            let obj = HGDIOBJ(brush.0);
            let r = RECT { left: 0, top: 0, right: anchor_size(), bottom: anchor_size() };
            let _ = FillRect(hdc, &r as *const RECT, brush);
            let _ = DeleteObject(obj);
            ReleaseDC(Some(hwnd), hdc);
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let brush = CreateSolidBrush(COLORREF(ANCHOR_DOT));
            let obj = HGDIOBJ(brush.0);
            let old = SelectObject(hdc, obj);
            let r = RECT { left: 0, top: 0, right: anchor_size(), bottom: anchor_size() };
            let _ = Ellipse(hdc, r.left, r.top, r.right, r.bottom);
            if !old.is_invalid() {
                let _ = SelectObject(hdc, old);
            }
            let _ = DeleteObject(obj);
            let _ = EndPaint(hwnd, &mut ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Create the global anchor dot window (topmost, click-through).
fn create_anchor() -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?.into();

        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpszClassName = w!("TapKeyAnchor");
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(anchor_wnd_proc);
        wc.hInstance = hinst;
        let _ = RegisterClassExW(&mut wc);

        let (ax, ay) = anchor();
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("TapKeyAnchor"),
            w!("TapKey Anchor"),
            WS_POPUP,
            ax - anchor_size() / 2,
            ay - anchor_size() / 2,
            anchor_size(),
            anchor_size(),
            None,
            None,
            Some(hinst),
            None,
        )
    }
}

fn anchor_hwnd() -> HWND {
    HWND(ANCHOR_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

/// Move the anchor dot to (x, y) and make sure it's visible.
pub fn move_anchor(x: i32, y: i32) {
    let h = anchor_hwnd();
    if h.is_invalid() {
        return;
    }
    unsafe {
        let _ = SetLayeredWindowAttributes(h, COLORREF(ANCHOR_KEY), ANCHOR_ALPHA, LWA_COLORKEY);
        let s = anchor_size(); // config-driven diameter — applied on every move
        let _ = SetWindowPos(
            h,
            None,
            x - s / 2,
            y - s / 2,
            s,
            s,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
        );
    }
}

/// Hide the anchor dot (e.g. when the whole overlay system closes).
pub fn hide_anchor() {
    let h = anchor_hwnd();
    if h.is_invalid() {
        return;
    }
    unsafe {
        let _ = ShowWindow(h, SW_HIDE);
    }
}

/// Create the D-pad layer (teal) window at (x, y).
pub fn create_dpad(x: i32, y: i32) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?.into();

        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpszClassName = w!("TapKeyDpad");
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(dpad_wnd_proc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default(); // see create_overlay
        RegisterClassExW(&mut wc);

        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("TapKeyDpad"),
            w!("TapKey D-pad"),
            WS_POPUP,
            x,
            y,
            DPAD_SIZE.0,
            DPAD_SIZE.1,
            None,
            None,
            Some(hinst),
            None,
        )
    }
}

/// Create the Move layer (lavender cross) window at (x, y).
pub fn create_move(x: i32, y: i32) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?.into();

        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpszClassName = w!("TapKeyMove");
        wc.style = CS_HREDRAW | CS_VREDRAW;
        wc.lpfnWndProc = Some(move_wnd_proc);
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default(); // see create_overlay
        RegisterClassExW(&mut wc);

        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("TapKeyMove"),
            w!("TapKey Move"),
            WS_POPUP,
            x,
            y,
            MOVE_SIZE.0,
            MOVE_SIZE.1,
            None,
            None,
            Some(hinst),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_layer_is_2x2_grid_with_90px_cells() {
        assert_eq!(MOVE_SIZE, (224, 224));
        let cells = move_cells();
        // (1,1)=up (1,2)=down (2,1)=left (2,2)=right — no diagonals.
        let centers: [(f64, f64); 4] = [
            (cells[0].rect.0 as f64 + cells[0].rect.2 / 2.0, cells[0].rect.1 as f64 + cells[0].rect.3 / 2.0),
            (cells[1].rect.0 as f64 + cells[1].rect.2 / 2.0, cells[1].rect.1 as f64 + cells[1].rect.3 / 2.0),
            (cells[2].rect.0 as f64 + cells[2].rect.2 / 2.0, cells[2].rect.1 as f64 + cells[2].rect.3 / 2.0),
            (cells[3].rect.0 as f64 + cells[3].rect.2 / 2.0, cells[3].rect.1 as f64 + cells[3].rect.3 / 2.0),
        ];
        assert_eq!(centers[0], (61.0, 61.0));   // up:     top-left
        assert_eq!(centers[1], (163.0, 61.0));  // down:    top-right
        assert_eq!(centers[2], (61.0, 163.0));  // left:    bottom-left
        assert_eq!(centers[3], (163.0, 163.0)); // right:   bottom-right
        for c in &cells {
            assert!((c.rect.2 - 90.0).abs() < f64::EPSILON); // square 90px cells
        }
    }

    #[test]
    fn move_hit_test_maps_taps_to_directions() {
        let p = |x: i32, y: i32| POINT { x, y };
        assert_eq!(move_hit_test(p(61, 61)), Some(0));  // up
        assert_eq!(move_hit_test(p(163, 61)), Some(1));  // down
        assert_eq!(move_hit_test(p(61, 163)), Some(2));  // left
        assert_eq!(move_hit_test(p(163, 163)), Some(3)); // right
        // gaps and padding are not interactive
        assert_eq!(move_hit_test(p(109, 61)), None);    // vertical gap between columns
        assert_eq!(move_hit_test(p(61, 109)), None);    // horizontal gap between rows
        assert_eq!(move_hit_test(p(8, 61)), None);      // padding
        assert_eq!(move_hit_test(p(223, 223)), None);   // bottom-right corner
    }
}
