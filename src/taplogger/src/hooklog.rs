//! WH_MOUSE_LL hook — pure-observation tap logger.
//!
//! Never swallows or modifies any event: every callback ends in CallNextHookEx, so apps
//! see exactly the same input stream as if this process didn't exist. We only record
//! timestamps + coordinates so tap/double-tap behavior (e.g. Moonlight/iPad input) can
//! be studied offline.
//!
//! The hook callback runs on the installing thread's message loop, so `push` and `drain`
//! are strictly sequential — the mutex guards against future refactors, not a race.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::Result;
use windows::Win32::Foundation::{LRESULT, LPARAM, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const KIND_LDOWN: u8 = 0;
pub const KIND_LUP: u8 = 1;
pub const KIND_RDOWN: u8 = 2;
pub const KIND_RUP: u8 = 3;
pub const KIND_MDOWN: u8 = 4;
pub const KIND_MUP: u8 = 5;
pub const KIND_WHEEL: u8 = 6;

/// Cluster rule: two LBUTTONDOWNs within this many ms at ≤ CLUSTER_RADIUS_PX of each
/// other — the double-tap signature, with a ±2px tolerance for coordinate jitter.
pub const CLUSTER_WINDOW_MS: u64 = 300;
pub const CLUSTER_RADIUS_PX: i32 = 2;

/// Live cluster window (ms) — adjustable from the panel (-10/+10 buttons), 20..=1000.
static WINDOW_MS: AtomicU32 = AtomicU32::new(CLUSTER_WINDOW_MS as u32);

pub fn cluster_window() -> u64 {
    WINDOW_MS.load(Ordering::Relaxed) as u64
}

/// Adjust the cluster window by `delta` ms, clamped to 20..=1000. Returns the new value.
pub fn adjust_cluster_window(delta: i32) -> u64 {
    let cur = WINDOW_MS.load(Ordering::Relaxed) as i32;
    let next = (cur + delta).clamp(20, 1000);
    WINDOW_MS.store(next as u32, Ordering::Relaxed);
    next as u64
}

#[derive(Clone, Copy)]
pub struct Ev {
    pub t: u64,          // GetTickCount64 ms (system-boot reference)
    pub kind: u8,
    pub x: i32,
    pub y: i32,
    pub wheel: i16,     // WHEEL only, signed
    pub btn: u32,       // raw dwFlags
    pub d_prev_ms: u64,  // since previous event of any kind (0 = first)
    pub gap_down_ms: u64, // DOWN only: since last down of the same button (0 = none yet)
    pub hold_ms: u64,     // UP only: duration of the press just released
    pub moved_px: u32,    // UP only: distance from the matching down to this up
    pub cluster: bool,    // LDOWN only: within window+radius of previous LDOWN
}

struct History {
    last_any_t: u64,
    last_down_t: [u64; 3],      // 0=left 1=right 2=middle
    down_pos: [(i32, i32); 3],
    prev_ldown: (u64, i32, i32),
}

struct LogState {
    pending: Vec<Ev>,
    hist: History,
}

static STATE: Mutex<LogState> = Mutex::new(LogState {
    pending: Vec::new(),
    hist: History {
        last_any_t: 0,
        last_down_t: [0; 3],
        down_pos: [(0, 0); 3],
        prev_ldown: (0, 0, 0),
    },
});

/// Tick reference captured at install — everything is measured from this.
pub fn t0() -> u64 {
    unsafe { GetTickCount64() }
}

/// Install the observation hook. Call from the thread that pumps messages.
pub fn install() -> Result<()> {
    let h = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) }?;
    println!("taplogger: WH_MOUSE_LL installed (observe-only — all events pass through)");
    let _ = h;
    Ok(())
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 {
        let s = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let kind = match msg {
            WM_LBUTTONDOWN => KIND_LDOWN,
            WM_LBUTTONUP => KIND_LUP,
            WM_RBUTTONDOWN => KIND_RDOWN,
            WM_RBUTTONUP => KIND_RUP,
            WM_MBUTTONDOWN => KIND_MDOWN,
            WM_MBUTTONUP => KIND_MUP,
            WM_MOUSEWHEEL => KIND_WHEEL,
            _ => u8::MAX, // moves + everything else: not logged
        };
        if kind != u8::MAX {
            let now = GetTickCount64();
            let wheel = if msg == WM_MOUSEWHEEL { s.mouseData as i16 } else { 0 };
            STATE.lock().unwrap().push(now, kind, s.pt.x, s.pt.y, wheel, s.dwExtraInfo as u32);
        }
    }
    // Observe-only: never intercept, never delay.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

impl LogState {
    fn push(&mut self, t: u64, kind: u8, x: i32, y: i32, wheel: i16, btn: u32) {
        let d_prev = if self.hist.last_any_t == 0 { 0 } else { t - self.hist.last_any_t };
        let bi = match kind {
            KIND_LDOWN | KIND_LUP => 0,
            KIND_RDOWN | KIND_RUP => 1,
            KIND_MDOWN | KIND_MUP => 2,
            KIND_WHEEL => 3, // wheel has no button history
            _ => 3,          // unknown kinds: treat as no-history
        } as usize;

        let mut ev = Ev {
            t,
            kind,
            x,
            y,
            wheel,
            btn,
            d_prev_ms: d_prev,
            gap_down_ms: 0,
            hold_ms: 0,
            moved_px: 0,
            cluster: false,
        };

        if bi < 3 {
            if matches!(kind, KIND_LDOWN | KIND_RDOWN | KIND_MDOWN) {
                ev.gap_down_ms = if self.hist.last_down_t[bi] == 0 {
                    0
                } else {
                    t - self.hist.last_down_t[bi]
                };
                self.hist.last_down_t[bi] = t;
                self.hist.down_pos[bi] = (x, y);
            } else {
                ev.hold_ms = if self.hist.last_down_t[bi] == 0 {
                    0
                } else {
                    t - self.hist.last_down_t[bi]
                };
                let (dx, dy) = self.hist.down_pos[bi];
                ev.moved_px = dist(x, y, dx, dy);
            }
        }

        if kind == KIND_LDOWN {
            let (pt, px, py) = self.hist.prev_ldown;
            ev.cluster = pt != 0
                && t - pt <= WINDOW_MS.load(Ordering::Relaxed) as u64
                && dist(x, y, px, py) <= CLUSTER_RADIUS_PX as u32;
            self.hist.prev_ldown = (t, x, y);
        }

        self.hist.last_any_t = t;
        if self.pending.len() >= 32768 {
            self.pending.drain(..self.pending.len() / 2); // never grow unbounded
        }
        self.pending.push(ev);
    }
}

/// Take everything recorded since the last drain (hook and drain share one thread).
pub fn drain() -> Vec<Ev> {
    std::mem::take(&mut STATE.lock().unwrap().pending)
}

fn dist(x: i32, y: i32, cx: i32, cy: i32) -> u32 {
    let dx = (x - cx) as i64;
    let dy = (y - cy) as i64;
    ((dx * dx + dy * dy) as f64).sqrt() as u32
}
