//! taplogger — observe-only tap logging app.
//!
//! A full-screen click-through canvas stamps a dot for every tap (paint-style), a
//! terminal-style panel shows a live per-event record (time + coords + gaps), and everything
//! is appended to a file under logs/ (relative to the working directory) for offline
//! analysis. The hook never
//! swallows or modifies any event, so apps behave exactly as if this process didn't exist.

mod hooklog;
mod render;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::Result;
use windows::Win32::Foundation::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const DRAIN_MS: u32 = 200;

fn main() -> Result<()> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?; }

    let t0 = hooklog::t0();
    let unix_start_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;

    // ---- screen (physical pixels — Per-Monitor-V2 aware) ----
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };

    // ---- log file (for offline analysis) ----
    std::fs::create_dir_all("logs")?;
    let path = format!("logs/taplogger-{}.txt", unix_start_ms);
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
    writeln!(file, "TAP LOGGER — observe-only mouse event log (no input modification)")?;
    writeln!(file, "started {}   pid={}   screen={}x{} (primary, physical px @PMv2)", wall(unix_start_ms), std::process::id(), screen_w, screen_h)?;
    writeln!(file, "hard cap {}s   drain interval {}ms", render::HARD_LIMIT_MS / 1000, DRAIN_MS)?;
    writeln!(file, "columns: [ms_since_start] wallclock #idx KIND x y <fields>")?;
    writeln!(file, "  LDOWN: gapDown=ms since previous LDOWN   dPrev=ms since any previous event   btn=dwFlags low word")?;
    writeln!(file, "  LUP:   hold=press duration   moved=down→up distance(px)   dPrev")?;
    writeln!(file, "  WHEEL: delta signed (±120 per notch)")?;
    writeln!(file, "  !!CLUSTER = two LBUTTONDOWNs within {}ms (panel -10/+10 buttons) at ≤{}px", hooklog::cluster_window(), hooklog::CLUSTER_RADIUS_PX)?;
    file.flush()?;

    // ---- windows ----
    let mut canvas = render::Canvas::new(screen_w, screen_h)?;
    let panel = render::Panel::new(screen_w - render::PANEL_W - 16, 24)?;
    let flash = render::Flash::new(screen_w - 236, screen_h - 100)?;

    hooklog::install()?;
    unsafe {
        SetTimer(Some(panel.hwnd()), render::UI_TIMER_ID as usize, DRAIN_MS, None);
        SetTimer(Some(panel.hwnd()), render::HARD_TIMER_ID as usize, render::HARD_LIMIT_MS, None);
    }

    println!("taplogger ready — observe-only logger (no input modification)");
    println!("log file: {}", path);
    println!("screen={}x{} panel={}x{} hard cap={}min", screen_w, screen_h, render::PANEL_W, render::PANEL_H, render::HARD_LIMIT_MS / 60_000);

    // ---- message loop ----
    let mut lines: Vec<(String, u32)> = Vec::new();
    let mut stats = render::Stats::default();
    let mut idx_counter: u32 = 0;
    let mut last_snap_t = t0;
    let mut flash_until: Option<u64> = None;
    let mut last_win = hooklog::cluster_window();
    let mut msg = MSG::default();

    while unsafe { GetMessageW(&mut msg, None, 0, 0) == TRUE } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(until) = flash_until {
            if hooklog::t0() >= until {
                flash.hide();
                flash_until = None;
            }
        }
        if render::REPAINT_REQUESTED.swap(false, Ordering::Relaxed) {
            let win = hooklog::cluster_window();
            if win != last_win {
                println!("== cluster window → {}ms", win);
                writeln!(file, "== cluster window → {}ms", win)?;
                file.flush()?;
                last_win = win;
            }
            panel.repaint(&lines, &stats, render::Panel::focused());
        }
        if render::QUIT_REQUESTED.swap(false, Ordering::Relaxed) {
            break;
        }
        if render::CLEAR_REQUESTED.swap(false, Ordering::Relaxed) {
            canvas.clear();
            lines.clear();
            stats = render::Stats::default();
            idx_counter = 0;
            flash.hide();
            flash_until = None;
            let ms_now = hooklog::t0() - t0;
            println!("== CLEAR @ [{}ms] — canvas + panel + stats reset", ms_now);
            writeln!(file, "== CLEAR @ [{}ms] — canvas + panel + stats reset (event numbering restarts)", ms_now)?;
            file.flush()?;
        }
        if render::PUMP_TICK.swap(false, Ordering::Relaxed) {
            let evs = hooklog::drain();
            if !evs.is_empty() {
                let start_idx = idx_counter + 1;
                for e in &evs {
                    idx_counter += 1;
                    update_stats(&mut stats, e);
                    let line = fmt_line(e, idx_counter, t0, unix_start_ms);
                    let color = line_color(e);
                    println!("{}", line);
                    writeln!(file, "{}", line)?;
                    lines.push((fmt_line_panel(e, idx_counter, t0), color));
                    if e.cluster {
                        flash.show(e.gap_down_ms);
                        flash_until = Some(hooklog::t0() + 700);
                    }
                }
                canvas.stamp(&evs, start_idx);
                panel.repaint(&lines, &stats, render::Panel::focused());
                file.flush()?;

                let now = hooklog::t0();
                if now - last_snap_t >= 5000 {
                    last_snap_t = now;
                    writeln!(file, "== snap @ [{}ms] ev={} cl={} maxHold={}ms", now - t0, stats.ev, stats.cl, stats.max_hold)?;
                }
            }
        }
    }

    // ---- final summary ----
    let duration_s = (hooklog::t0() - t0) / 1000;
    writeln!(file, "== SUMMARY")?;
    writeln!(file, "duration={}s events={} Ld={} Lu={} Rd={} Ru={} Wh={} clusters={}", duration_s, stats.ev, stats.ld, stats.lu, stats.rd, stats.ru, stats.wh, stats.cl)?;
    let avg = if stats.n_gap_down > 0 { stats.sum_gap_down / stats.n_gap_down as u64 } else { 0 };
    writeln!(file, "maxHold={}ms avgGapDown={}ms", stats.max_hold, avg)?;
    writeln!(file, "end {}", wall(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64))?;
    file.flush()?;
    println!("taplogger exited — summary written to {}", path);

    canvas.destroy();
    panel.destroy();
    flash.destroy();
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn wall(unix_ms: u64) -> String {
    let s = (unix_ms / 1000) % 86_400;
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600, (s % 3600) / 60, s % 60, unix_ms % 1000)
}

fn kind_str(k: u8) -> &'static str {
    match k {
        hooklog::KIND_LDOWN => "LDOWN",
        hooklog::KIND_LUP => "LUP  ",
        hooklog::KIND_RDOWN => "RDOWN",
        hooklog::KIND_RUP => "RUP  ",
        hooklog::KIND_MDOWN => "MDOWN",
        hooklog::KIND_MUP => "MUP  ",
        hooklog::KIND_WHEEL => "WHEEL",
        _ => "?    ",
    }
}

fn fmt_line(e: &hooklog::Ev, idx: u32, t0: u64, unix_start_ms: u64) -> String {
    let extra = match e.kind {
        hooklog::KIND_LDOWN | hooklog::KIND_RDOWN | hooklog::KIND_MDOWN => {
            format!(" gapDown={:5}ms dPrev={:5}ms btn={:#06x}", e.gap_down_ms, e.d_prev_ms, e.btn & 0xFFFF)
        }
        hooklog::KIND_LUP | hooklog::KIND_RUP | hooklog::KIND_MUP => {
            format!(" hold={:5}ms moved={:4}px dPrev={:5}ms", e.hold_ms, e.moved_px, e.d_prev_ms)
        }
        hooklog::KIND_WHEEL => {
            format!(" delta={:+5} dPrev={:5}ms", e.wheel, e.d_prev_ms)
        }
        _ => String::new(),
    };
    let cluster = if e.cluster { "  !!CLUSTER" } else { "" };
    format!(
        "[{:6}] {} #{:03} {:7} x={:4} y={:4}{}{}",
        e.t - t0,
        wall(unix_start_ms + (e.t - t0)),
        idx,
        kind_str(e.kind),
        e.x,
        e.y,
        extra,
        cluster
    )
}

/// Compact panel line (fits 480px @14px Consolas); the file keeps the full fmt_line.
fn fmt_line_panel(e: &hooklog::Ev, idx: u32, t0: u64) -> String {
    let ms = e.t - t0;
    let clock = format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60);
    let kind = match e.kind {
        hooklog::KIND_LDOWN => "LD",
        hooklog::KIND_LUP => "LU",
        hooklog::KIND_RDOWN => "RD",
        hooklog::KIND_RUP => "RU",
        hooklog::KIND_MDOWN => "MD",
        hooklog::KIND_MUP => "MU",
        hooklog::KIND_WHEEL => "WH",
        _ => "??",
    };
    let extra = match e.kind {
        hooklog::KIND_LDOWN | hooklog::KIND_RDOWN | hooklog::KIND_MDOWN => format!("gap={}ms", e.gap_down_ms),
        hooklog::KIND_LUP | hooklog::KIND_RUP | hooklog::KIND_MUP => format!("hold={}ms mv={}px", e.hold_ms, e.moved_px),
        hooklog::KIND_WHEEL => format!("delta={:+}", e.wheel),
        _ => String::new(),
    };
    let cluster = if e.cluster { " !!CLUSTER" } else { "" };
    format!("[{}] #{} {} ({},{}) {}{}", clock, idx, kind, e.x, e.y, extra, cluster)
}

fn line_color(e: &hooklog::Ev) -> u32 {
    if e.cluster {
        return render::C_RED;
    }
    match e.kind {
        hooklog::KIND_LDOWN => render::C_LAVENDER,
        hooklog::KIND_LUP => 0xFFA8A4B4,
        hooklog::KIND_RDOWN | hooklog::KIND_RUP => render::C_ORANGE,
        hooklog::KIND_MDOWN | hooklog::KIND_MUP => 0xFFB39DDB,
        hooklog::KIND_WHEEL => render::C_TEAL,
        _ => 0xFFFFFFFF,
    }
}

fn update_stats(stats: &mut render::Stats, e: &hooklog::Ev) {
    stats.ev += 1;
    match e.kind {
        hooklog::KIND_LDOWN => {
            stats.ld += 1;
            if e.gap_down_ms > 0 {
                stats.sum_gap_down += e.gap_down_ms;
                stats.n_gap_down += 1;
            }
        }
        hooklog::KIND_LUP => stats.lu += 1,
        hooklog::KIND_RDOWN => stats.rd += 1,
        hooklog::KIND_RUP => stats.ru += 1,
        hooklog::KIND_WHEEL => stats.wh += 1,
        _ => {}
    }
    if e.kind == hooklog::KIND_LDOWN && e.cluster {
        stats.cl += 1;
    }
    if matches!(e.kind, hooklog::KIND_LUP | hooklog::KIND_RUP | hooklog::KIND_MUP) && e.hold_ms > stats.max_hold {
        stats.max_hold = e.hold_ms;
    }
    stats.last_desc = format!("last {} ({},{})", kind_str(e.kind).trim(), e.x, e.y);
}
