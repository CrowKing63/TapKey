//! Rendering: a full-screen click-through dot canvas (paint-style) and a terminal-style
//! log panel. Both are per-pixel-alpha layered windows blitted with UpdateLayeredWindow.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::{Result, w};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- colors (0xAARRGGBB)

const BG_PANEL: u32 = 0xFF17141F;
pub const C_LAVENDER: u32 = 0xFFC4B5FD;
pub const C_TEAL: u32 = 0xFF7FD1C8;
pub const C_ORANGE: u32 = 0xFFF0A868;
pub const C_PINK: u32 = 0xFFFF9ECF;
pub const C_LIME: u32 = 0xFFA3E635;
pub const C_BLUE: u32 = 0xFF8AB4F8;
const C_GRAY: u32 = 0xFFA8A4B4;
const C_DIM: u32 = 0xFF8F8B9E;
pub const C_RED: u32 = 0xFFFF6B6B;
pub const C_YELLOW: u32 = 0xFFE8D26B;
const C_WHITE: u32 = 0xFFFFFFFF;
const C_DARK: u32 = 0xFF2A2438;

const DOWN_PALETTE: [u32; 6] = [C_LAVENDER, C_TEAL, C_ORANGE, C_PINK, C_LIME, C_BLUE];
/// Newest events kept on the canvas (~30 taps); older marks are redrawn away.
const MAX_MARKS: usize = 60;

// ---------------------------------------------------------------- shared blit

/// Blit a premultiplied RGBA buffer (w*h u32) onto a layered window at its top-left.
unsafe fn blit(hwnd: HWND, w: i32, h: i32, buf: &[u32]) -> Result<()> {
    let hdc_mem = CreateCompatibleDC(None);
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits = std::ptr::null_mut();
    let dib = CreateDIBSection(Some(hdc_mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    SelectObject(hdc_mem, dib.into());
    std::ptr::copy_nonoverlapping(buf.as_ptr(), bits as *mut u32, (w * h) as usize);

    let mut wp = RECT::default();
    GetWindowRect(hwnd, &mut wp)?;
    let blend = BLENDFUNCTION {
        BlendOp: 0, // AC_SRC_OVER
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1, // AC_SRC_ALPHA
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

// ---------------------------------------------------------------- GDI text helper

/// Render `text` into a transparent premultiplied RGBA tile via a black-on-white
/// coverage pass. GDI TextOut zeroes alpha on 32bpp DIBs, so drawing text straight
/// into an ARGB buffer renders faint/invisible — coverage readback is the reliable path.
fn text_rgba(text: &str, height_px: i32, fg: u32, mono: bool) -> Option<(Vec<u32>, i32, i32)> {
    let wc16: Vec<u16> = text.encode_utf16().collect();
    if wc16.is_empty() {
        return None;
    }
    unsafe {
        let hdc = CreateCompatibleDC(None);
        let mut lf = LOGFONTW::default();
        lf.lfHeight = -height_px;
        let face: Vec<u16> = if mono {
            lf.lfWeight = 400;
            lf.lfCharSet = FONT_CHARSET(0); // DEFAULT
            "Consolas".encode_utf16().collect()
        } else {
            lf.lfWeight = 700;
            lf.lfCharSet = FONT_CHARSET(9); // HANGEUL — covers Hangul + digits + arrows
            "Malgun Gothic".encode_utf16().collect()
        };
        for (i, c) in face.iter().enumerate() {
            if i < lf.lfFaceName.len() {
                lf.lfFaceName[i] = *c;
            }
        }
        let font = CreateFontIndirectW(&lf);
        SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, COLORREF(0x00000000)); // black glyphs on white → coverage

        let mut sz = SIZE::default();
        if !GetTextExtentPoint32W(hdc, &wc16, &mut sz).as_bool() {
            DeleteObject(font.into());
            DeleteDC(hdc);
            return None;
        }
        let tw = sz.cx.max(1) + 4;
        let th = sz.cy.max(1) + 4;

        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let mut bits = std::ptr::null_mut();
        let dib = match CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(d) => d,
            Err(_) => {
                DeleteObject(font.into());
                DeleteDC(hdc);
                return None;
            }
        };
        SelectObject(hdc, dib.into());
        let px = bits as *mut u32;
        for i in 0..(tw * th) as usize {
            *px.add(i) = 0xFFFFFFFF; // white background
        }
        if !TextOutW(hdc, 2, 2, &wc16).as_bool() {
            DeleteObject(font.into());
            DeleteDC(hdc);
            return None;
        }

        // coverage = 255 - luminance; composite fg premultiplied by coverage
        let (fr, fg_c, fb) = ((fg >> 16) & 0xFF, (fg >> 8) & 0xFF, fg & 0xFF);
        let mut out = vec![0u32; (tw * th) as usize];
        for i in 0..out.len() {
            let p = *px.add(i);
            let lum = (((p >> 16) & 0xFF) + ((p >> 8) & 0xFF) + (p & 0xFF)) / 3;
            let cov = 255 - lum;
            if cov == 0 {
                continue;
            }
            out[i] = (cov << 24) | ((fr * cov / 255) << 16) | ((fg_c * cov / 255) << 8) | (fb * cov / 255);
        }

        DeleteObject(font.into());
        DeleteDC(hdc);
        Some((out, tw, th))
    }
}

/// Render + composite text with its top-left at (x, y).
fn put_text(buf: &mut [u32], w: i32, h: i32, x: i32, y: i32, text: &str, size: i32, color: u32, mono: bool) {
    if let Some(tile) = text_rgba(text, size, color, mono) {
        let (tw, th) = (tile.1, tile.2);
        paste(buf, w, h, x + tw / 2, y + th / 2, &tile);
    }
}

// ---------------------------------------------------------------- small shape primitives
// All operate on a premultiplied RGBA buffer, origin top-left, y down.

fn over(dst: &mut u32, src: u32) {
    let sa = ((src >> 24) & 0xFF) as f64;
    if sa == 0.0 {
        return;
    }
    if sa == 255.0 {
        *dst = src;
        return;
    }
    let da = ((*dst >> 24) & 0xFF) as f64;
    let oa = sa + da * (1.0 - sa / 255.0);
    macro_rules! ch {
        ($s: expr, $d: expr) => {
            (($s * sa + $d * da * (1.0 - sa / 255.0)) / oa) as u32
        };
    }
    let sr = ((src >> 16) & 0xFF) as f64;
    let sg = ((src >> 8) & 0xFF) as f64;
    let sb = (src & 0xFF) as f64;
    let dr = ((*dst >> 16) & 0xFF) as f64;
    let dg = ((*dst >> 8) & 0xFF) as f64;
    let db = (*dst & 0xFF) as f64;
    *dst = (oa as u32) << 24 | (ch!(sr, dr)) << 16 | (ch!(sg, dg)) << 8 | ch!(sb, db);
}

fn fill_circle(buf: &mut [u32], w: i32, h: i32, cx: f64, cy: f64, r: f64, c: u32) {
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            if dx * dx + dy * dy <= r * r {
                buf[(y * w + x) as usize] = c;
            }
        }
    }
}

fn stroke_ring(buf: &mut [u32], w: i32, h: i32, cx: f64, cy: f64, r: f64, th: f64, c: u32) {
    let x0 = (cx - r - th).floor() as i32;
    let x1 = (cx + r + th).ceil() as i32;
    let y0 = (cy - r - th).floor() as i32;
    let y1 = (cy + r + th).ceil() as i32;
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= r && d >= r - th {
                over(&mut buf[(y * w + x) as usize], c);
            }
        }
    }
}

fn fill_rect(buf: &mut [u32], w: i32, h: i32, x0: i32, y0: i32, x1: i32, y1: i32, c: u32) {
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            buf[(y * w + x) as usize] = c;
        }
    }
}

/// Composite a text tile (from text_rgba) onto the buffer centered at (cx, cy).
fn paste(buf: &mut [u32], w: i32, h: i32, cx: i32, cy: i32, tile: &(Vec<u32>, i32, i32)) {
    let px: &Vec<u32> = &tile.0;
    let tw: i32 = tile.1;
    let th: i32 = tile.2;
    let ox = cx - tw / 2;
    let oy = cy - th / 2;
    for ty in 0..th {
        let dy = oy + ty;
        if dy < 0 || dy >= h {
            continue;
        }
        for tx in 0..tw {
            let dx = ox + tx;
            if dx < 0 || dx >= w {
                continue;
            }
            let src = px[(ty * tw + tx) as usize];
            if (src >> 24) & 0xFF != 0 {
                over(&mut buf[(dy * w + dx) as usize], src);
            }
        }
    }
}

// ---------------------------------------------------------------- canvas (paint layer)

/// Full-screen click-through layered window: every tap stamps a persistent mark.
pub struct Canvas {
    hwnd: HWND,
    w: i32,
    h: i32,
    buf: Vec<u32>,
    recent: Vec<(u32, crate::hooklog::Ev)>,
}

impl Canvas {
    pub fn new(w: i32, h: i32) -> Result<Self> {
        let hinstance = unsafe { GetModuleHandleW(None) }?.into();
        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(canvas_wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = w!("TapLoggerCanvas");
        let _atom = unsafe { RegisterClassExW(&wc) };

        let r = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("TapLoggerCanvas"),
                w!("taplogger canvas"),
                WS_POPUP,
                0,
                0,
                w,
                h,
                None,
                None,
                Some(hinstance),
                None,
            )
        };
        let hwnd = match r {
            Ok(hwnd) => hwnd,
            Err(e) => {
                eprintln!("diag canvas: CreateWindowExW failed {:?} gle={}", e, unsafe { windows::Win32::Foundation::GetLastError() }.0);
                return Err(e);
            }
        };
        unsafe { ShowWindow(hwnd, SW_SHOW) };

        Ok(Self {
            hwnd,
            w,
            h,
            buf: vec![0u32; (w * h) as usize], // fully transparent
            recent: Vec::new(),
        })
    }

    /// Stamp marks for a batch of new events; `start_idx` = log index of evs[0].
    /// Only the newest MAX_MARKS events stay on screen — older marks are redrawn away.
    pub fn stamp(&mut self, evs: &[crate::hooklog::Ev], start_idx: u32) {
        if evs.is_empty() {
            return;
        }
        for (i, e) in evs.iter().enumerate() {
            self.recent.push((start_idx + i as u32, *e));
        }
        if self.recent.len() > MAX_MARKS {
            let drop = self.recent.len() - MAX_MARKS;
            self.recent.drain(..drop);
            self.buf.fill(0);
            for i in 0..self.recent.len() {
                let (idx, e) = self.recent[i];
                self.draw_mark(idx, &e);
            }
        } else {
            for (i, e) in evs.iter().enumerate() {
                self.draw_mark(start_idx + i as u32, e);
            }
        }
        let _ = unsafe { blit(self.hwnd, self.w, self.h, &self.buf) };
    }

    fn draw_mark(&mut self, idx: u32, e: &crate::hooklog::Ev) {
        match e.kind {
            crate::hooklog::KIND_LDOWN => {
                let c = DOWN_PALETTE[((idx.saturating_sub(1)) / 2) as usize % DOWN_PALETTE.len()];
                fill_circle(&mut self.buf, self.w, self.h, e.x as f64, e.y as f64, 10.0, c);
                if let Some(tile) = text_rgba(&idx.to_string(), 13, C_DARK, false) {
                    paste(&mut self.buf, self.w, self.h, e.x, e.y, &tile);
                }
                if e.cluster {
                    stroke_ring(&mut self.buf, self.w, self.h, e.x as f64, e.y as f64, 14.0, 3.0, C_RED);
                }
            }
            crate::hooklog::KIND_LUP => {
                let c = if e.moved_px >= 8 { C_YELLOW } else { C_GRAY };
                stroke_ring(&mut self.buf, self.w, self.h, e.x as f64, e.y as f64, 5.0, 2.0, c);
            }
            crate::hooklog::KIND_RDOWN => {
                fill_rect(&mut self.buf, self.w, self.h, e.x - 6, e.y - 6, e.x + 7, e.y + 7, C_ORANGE);
            }
            crate::hooklog::KIND_RUP => {
                stroke_ring(&mut self.buf, self.w, self.h, e.x as f64, e.y as f64, 7.0, 2.0, C_ORANGE);
            }
            crate::hooklog::KIND_WHEEL => {
                let glyph = if e.wheel > 0 { "▲" } else { "▼" };
                if let Some(tile) = text_rgba(glyph, 15, C_TEAL, false) {
                    paste(&mut self.buf, self.w, self.h, e.x, e.y - 2, &tile);
                }
            }
            _ => {}
        }
    }

    pub fn clear(&mut self) {
        self.buf.fill(0);
        self.recent.clear();
        let _ = unsafe { blit(self.hwnd, self.w, self.h, &self.buf) };
    }

    pub fn destroy(&self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn canvas_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------- panel (terminal-style log)

pub static CLEAR_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static REPAINT_REQUESTED: AtomicBool = AtomicBool::new(false);
pub static PUMP_TICK: AtomicBool = AtomicBool::new(false);
static FOCUSED: AtomicBool = AtomicBool::new(false);

/// Timer ids (hosted on the panel window) and the hard runtime cap.
pub const UI_TIMER_ID: u32 = 101;
pub const HARD_TIMER_ID: u32 = 102;
/// Observe-only tool — hard cap so it can't run forever unnoticed.
pub const HARD_LIMIT_MS: u32 = 1_800_000; // 30 min

pub const PANEL_W: i32 = 480;
pub const PANEL_H: i32 = 680;
const BTN_CLEAR: (i32, i32, i32, i32) = (16, 118, 220, 36);   // x,y,w,h (client coords)
const BTN_QUIT: (i32, i32, i32, i32) = (244, 118, 220, 36);
const BTN_MINUS: (i32, i32, i32, i32) = (110, 90, 44, 26);
const BTN_PLUS: (i32, i32, i32, i32) = (160, 90, 44, 26);
const BODY_TOP: i32 = 170;
const LINE_H: i32 = 22;

pub struct Stats {
    pub ev: u32,
    pub ld: u32,
    pub lu: u32,
    pub rd: u32,
    pub ru: u32,
    pub wh: u32,
    pub cl: u32,
    pub max_hold: u64,
    pub sum_gap_down: u64,
    pub n_gap_down: u32,
    pub last_desc: String,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            ev: 0,
            ld: 0,
            lu: 0,
            rd: 0,
            ru: 0,
            wh: 0,
            cl: 0,
            max_hold: 0,
            sum_gap_down: 0,
            n_gap_down: 0,
            last_desc: String::new(),
        }
    }
}

pub struct Panel {
    hwnd: HWND,
}

impl Panel {
    /// The panel's window handle (hosts the drain + hard-cap timers).
    pub fn hwnd(&self) -> HWND { self.hwnd }

    /// Whether the panel currently has keyboard focus (clicking it gives focus).
    pub fn focused() -> bool {
        FOCUSED.load(Ordering::Relaxed)
    }

    pub fn new(x: i32, y: i32) -> Result<Self> {
        let hinstance = unsafe { GetModuleHandleW(None) }?.into();
        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(panel_wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = w!("TapLoggerPanel");
        let _atom = unsafe { RegisterClassExW(&wc) };

        let r = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                w!("TapLoggerPanel"),
                w!("taplogger"),
                WS_POPUP,
                x,
                y,
                PANEL_W,
                PANEL_H,
                None,
                None,
                Some(hinstance),
                None,
            )
        };
        let hwnd = match r {
            Ok(hwnd) => hwnd,
            Err(e) => {
                eprintln!("diag panel: CreateWindowExW failed {:?} gle={}", e, unsafe { windows::Win32::Foundation::GetLastError() }.0);
                return Err(e);
            }
        };
        unsafe { ShowWindow(hwnd, SW_SHOW) };
        Ok(Self { hwnd })
    }

    /// `lines`: (text, color), newest last. Always shows the tail that fits.
    pub fn repaint(&self, lines: &[(String, u32)], stats: &Stats, focused: bool) {
        let w = PANEL_W;
        let h = PANEL_H;
        let mut buf = vec![BG_PANEL; (w * h) as usize];

        // focus dot (top-right)
        fill_circle(&mut buf, w, h, (w - 24) as f64, 30.0, 6.0, if focused { C_LIME } else { C_DIM });
        // button backgrounds
        for &(bx, by, bw, bh) in &[BTN_CLEAR, BTN_QUIT, BTN_MINUS, BTN_PLUS] {
            fill_rect(&mut buf, w, h, bx, by, bx + bw, by + bh, 0xFF2A2438);
        }

        // title + status
        put_text(&mut buf, w, h, 16, 14, "TAP LOGGER", 22, C_LAVENDER, false);
        put_text(
            &mut buf, w, h, 16, 50,
            &format!(
                "ev={} Ld={} Lu={} Rd={} Ru={} Wh={} cl={}",
                stats.ev, stats.ld, stats.lu, stats.rd, stats.ru, stats.wh, stats.cl
            ),
            14, C_GRAY, true,
        );
        let avg = if stats.n_gap_down > 0 { stats.sum_gap_down / stats.n_gap_down as u64 } else { 0 };
        put_text(&mut buf, w, h, 16, 72, &format!("maxHold={}ms  avgGapDown={}ms", stats.max_hold, avg), 14, C_GRAY, true);

        // cluster window row + -10/+10 adjust buttons
        put_text(&mut buf, w, h, 16, 94, &format!("win={}ms", crate::hooklog::cluster_window()), 14, C_YELLOW, true);
        put_text(&mut buf, w, h, BTN_MINUS.0 + 8, BTN_MINUS.1 + 5, "-10", 14, C_LAVENDER, true);
        put_text(&mut buf, w, h, BTN_PLUS.0 + 8, BTN_PLUS.1 + 5, "+10", 14, C_LAVENDER, true);
        if stats.last_desc.is_empty() {
            put_text(&mut buf, w, h, 214, 94, "C=clear Q=quit", 13, C_DIM, true);
        } else {
            put_text(&mut buf, w, h, 214, 94, &stats.last_desc, 14, C_DIM, true);
        }

        // buttons
        put_text(&mut buf, w, h, BTN_CLEAR.0 + 20, BTN_CLEAR.1 + 8, "CLEAR", 15, C_LAVENDER, false);
        put_text(&mut buf, w, h, BTN_QUIT.0 + 20, BTN_QUIT.1 + 8, "QUIT", 15, C_RED, false);

        // log lines (tail)
        let body_h = h - BODY_TOP - 14;
        let max_lines = (body_h / LINE_H) as usize;
        let start = lines.len().saturating_sub(max_lines);
        for (i, (text, color)) in lines.iter().enumerate().skip(start).take(max_lines) {
            let y = BODY_TOP + i as i32 * LINE_H;
            put_text(&mut buf, w, h, 16, y, text, 14, *color, true);
        }

        let _ = unsafe { blit(self.hwnd, w, h, &buf) };
    }

    pub fn destroy(&self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

// ---------------------------------------------------------------- double-tap flash (corner toast)

/// Brief corner toast shown when a cluster (same-point double-tap within 300ms) fires.
/// Click-through + topmost; the main loop hides it ~700ms after the last cluster.
pub struct Flash {
    hwnd: HWND,
    w: i32,
    h: i32,
}

impl Flash {
    pub fn new(x: i32, y: i32) -> Result<Self> {
        let (w, h) = (220, 84);
        let hinstance = unsafe { GetModuleHandleW(None) }?.into();
        let mut wc = WNDCLASSEXW::default();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(flash_wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = w!("TapLoggerFlash");
        let _atom = unsafe { RegisterClassExW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("TapLoggerFlash"),
                w!("taplogger flash"),
                WS_POPUP,
                x, y, w, h,
                None, None, Some(hinstance), None,
            )
        }?;
        Ok(Self { hwnd, w, h })
    }

    /// Render + show the toast with the detected down-to-down gap.
    pub fn show(&self, gap_ms: u64) {
        let (w, h) = (self.w, self.h);
        let mut buf = vec![0xFF1A1033u32; (w * h) as usize];
        // 2px red border
        fill_rect(&mut buf, w, h, 0, 0, w, 2, C_RED);
        fill_rect(&mut buf, w, h, 0, h - 2, w, h, C_RED);
        fill_rect(&mut buf, w, h, 0, 0, 2, h, C_RED);
        fill_rect(&mut buf, w, h, w - 2, 0, w, h, C_RED);
        put_text(&mut buf, w, h, 16, 10, "더블탭", 30, C_RED, false);
        put_text(&mut buf, w, h, 16, 52, &format!("gap {}ms", gap_ms), 16, C_LAVENDER, true);
        let _ = unsafe { blit(self.hwnd, w, h, &buf) };
        unsafe {
            // re-assert topmost (other topmost apps may have re-ordered above us)
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    pub fn hide(&self) {
        unsafe { ShowWindow(self.hwnd, SW_HIDE) };
    }

    pub fn destroy(&self) {
        unsafe { let _ = DestroyWindow(self.hwnd); }
    }
}

unsafe extern "system" fn flash_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn panel_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            let wp = wparam.0 as u32;
            if wp == UI_TIMER_ID {
                PUMP_TICK.store(true, Ordering::Relaxed);
            } else if wp == HARD_TIMER_ID {
                println!("hard cap {}s reached — quitting", HARD_LIMIT_MS / 1000);
                QUIT_REQUESTED.store(true, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SETFOCUS => {
            FOCUSED.store(true, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            FOCUSED.store(false, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let vk = wparam.0 as u32;
            match vk {
                0x1B | 0x51 => QUIT_REQUESTED.store(true, Ordering::Relaxed), // Esc / Q
                0x43 => CLEAR_REQUESTED.store(true, Ordering::Relaxed),        // C
                _ => {}
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let cx = (lparam.0 & 0xFFFF) as i32;
            let cy = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let hit = |b: (i32, i32, i32, i32)| cx >= b.0 && cx < b.0 + b.2 && cy >= b.1 && cy < b.1 + b.3;
            if hit(BTN_CLEAR) {
                CLEAR_REQUESTED.store(true, Ordering::Relaxed);
            } else if hit(BTN_QUIT) {
                QUIT_REQUESTED.store(true, Ordering::Relaxed);
            } else if hit(BTN_MINUS) {
                crate::hooklog::adjust_cluster_window(-10);
                REPAINT_REQUESTED.store(true, Ordering::Relaxed);
            } else if hit(BTN_PLUS) {
                crate::hooklog::adjust_cluster_window(10);
                REPAINT_REQUESTED.store(true, Ordering::Relaxed);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
