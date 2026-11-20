//! Portable config: `tapkey.toml` next to the exe.
//!
//! Values live in atomics — the hook and overlay read them per event, so a settings
//! change applies from the next event without restart. `save()` writes the file;
//! on failure (read-only location) we keep the memory values and warn once at exit.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use windows::core::w;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK, MB_TOPMOST};

// Default window 250ms covers the measured natural double-tap rhythm (223–257ms, docs/tap-signature.md).
pub static DOUBLE_TAP_WINDOW_MS: AtomicU32 = AtomicU32::new(250);
pub static TAP_RADIUS_PX: AtomicI32 = AtomicI32::new(0);
pub static OVERLAY_IDLE_MS: AtomicU32 = AtomicU32::new(8000);
pub static ANCHOR_DOT_PX: AtomicI32 = AtomicI32::new(20);
pub static DETECTION_ENABLED: AtomicBool = AtomicBool::new(true); // global pause (≠ overlay-open DETECTION_ON)
static SAVE_FAILED: AtomicBool = AtomicBool::new(false);

pub fn double_tap_window_ms() -> u64 {
    DOUBLE_TAP_WINDOW_MS.load(Ordering::Relaxed) as u64
}
pub fn tap_radius_px() -> i32 {
    TAP_RADIUS_PX.load(Ordering::Relaxed)
}
pub fn overlay_idle_ms() -> u32 {
    OVERLAY_IDLE_MS.load(Ordering::Relaxed)
}
pub fn anchor_dot_px() -> i32 {
    ANCHOR_DOT_PX.load(Ordering::Relaxed)
}
pub fn detection_enabled() -> bool {
    DETECTION_ENABLED.load(Ordering::Relaxed)
}

pub fn set_double_tap_window_ms(v: i32) {
    let v = v.clamp(20, 1000);
    DOUBLE_TAP_WINDOW_MS.store(v as u32, Ordering::Relaxed);
    println!("config: double_tap_window_ms = {v}");
}
pub fn set_tap_radius_px(v: i32) {
    let v = v.clamp(0, 20);
    TAP_RADIUS_PX.store(v, Ordering::Relaxed);
    println!("config: tap_radius_px = {v}");
}
pub fn set_overlay_idle_ms(v: i32) {
    let v = v.clamp(1000, 60000);
    OVERLAY_IDLE_MS.store(v as u32, Ordering::Relaxed);
    println!("config: overlay_idle_ms = {v}");
}
pub fn set_detection_enabled(v: bool) {
    DETECTION_ENABLED.store(v, Ordering::Relaxed);
    println!("config: detection_enabled = {v}");
}

/// Config file path: next to the exe (portable — the folder carries its settings).
pub fn path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("tapkey.toml")))
        .unwrap_or_else(|| PathBuf::from("tapkey.toml"))
}

/// Load at startup. Missing file → built-in defaults. Unknown keys are ignored.
pub fn load() {
    let p = path();
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => {
            println!("config: no tapkey.toml next to exe — built-in defaults");
            return;
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        let v = v.split('#').next().unwrap_or("").trim(); // strip inline comment
        match k {
            "double_tap_window_ms" => {
                if let Ok(n) = v.parse::<i32>() {
                    DOUBLE_TAP_WINDOW_MS.store(n.clamp(20, 1000) as u32, Ordering::Relaxed);
                }
            }
            "tap_radius_px" => {
                if let Ok(n) = v.parse::<i32>() {
                    TAP_RADIUS_PX.store(n.clamp(0, 20), Ordering::Relaxed);
                }
            }
            "overlay_idle_ms" => {
                if let Ok(n) = v.parse::<i32>() {
                    OVERLAY_IDLE_MS.store(n.clamp(1000, 60000) as u32, Ordering::Relaxed);
                }
            }
            "anchor_dot_px" => {
                if let Ok(n) = v.parse::<i32>() {
                    ANCHOR_DOT_PX.store(n.clamp(4, 100), Ordering::Relaxed);
                }
            }
            "detection_enabled" => {
                DETECTION_ENABLED.store(v != "false", Ordering::Relaxed);
            }
            _ => {} // future keys (drag_step_px, scroll_notches, ...) — ignore for now
        }
    }
    println!(
        "config: loaded {} (window={}ms radius={}px idle={}ms dot={}px detection={})",
        p.display(),
        double_tap_window_ms(),
        tap_radius_px(),
        overlay_idle_ms(),
        anchor_dot_px(),
        if detection_enabled() { "on" } else { "paused" }
    );
}

/// Persist current values. Returns false on write failure (memory values stay live).
pub fn save() -> bool {
    let p = path();
    let text = format!(
        "# TapKey settings (portable — this file lives next to tapkey.exe)\n\
         double_tap_window_ms = {}\n\
         tap_radius_px = {}\n\
         overlay_idle_ms = {}\n\
         anchor_dot_px = {}\n\
         detection_enabled = {}\n",
        DOUBLE_TAP_WINDOW_MS.load(Ordering::Relaxed),
        TAP_RADIUS_PX.load(Ordering::Relaxed),
        OVERLAY_IDLE_MS.load(Ordering::Relaxed),
        ANCHOR_DOT_PX.load(Ordering::Relaxed),
        if DETECTION_ENABLED.load(Ordering::Relaxed) { "true" } else { "false" },
    );
    match std::fs::write(&p, text) {
        Ok(_) => {
            SAVE_FAILED.store(false, Ordering::Relaxed);
            println!("config: saved {}", p.display());
            true
        }
        Err(e) => {
            SAVE_FAILED.store(true, Ordering::Relaxed);
            println!("config: SAVE FAILED ({}): {e} — keeping values in memory", p.display());
            false
        }
    }
}

/// Exit-time warning: a failed save kept values in memory only.
pub fn warn_if_save_failed() {
    if SAVE_FAILED.swap(false, Ordering::Relaxed) {
        println!("config: warning — last save failed; changes were not persisted");
        unsafe {
            MessageBoxW(
                None,
                w!("Settings could not be saved.\nChanges apply to this session only.\n\nMove TapKey to a writable folder."),
                w!("TapKey"),
                MB_OK | MB_ICONWARNING | MB_TOPMOST,
            );
        }
    }
}
