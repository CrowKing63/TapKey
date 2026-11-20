# TapKey — Design & Implementation Notes

TapKey is a PC-side Windows utility for touch-only Moonlight streaming. A same-point
double-tap opens a small button overlay at the cursor position; the buttons execute
right-click, wheel scroll, drag, hold and double-click at that point. Single taps pass
through untouched.

This document describes the architecture and the technical decisions behind it.
Measured input data: [`tap-signature.md`](tap-signature.md).

## 1. Problem

When a PC is controlled through Moonlight from a touch device (e.g. an iPad while
wearing a Vision Pro as the display), touch input maps to the mouse — but
**right-click, wheel scroll and precise drag** are hard to perform with the built-in
gestures:

- Moonlight touch mode: long-press = right-click (timing-sensitive), no scroll.
- Trackpad mode: two-finger tap = right-click, two-finger drag = scroll (requires
  precise multi-touch).

Goal: single taps pass through as normal clicks; a same-point double-tap opens a
context overlay near the tap point; overlay buttons perform the missing mouse actions
at that anchor point.

## 2. Prior art

| Solution | Assessment |
|---|---|
| Moonlight built-in touch mode | Long-press / two-finger gestures are the problem itself; no button-overlay concept |
| VoidLink (iPad, Moonlight fork) | Closest: on-screen widgets for click / double-click / right-click / drag. But it replaces the streaming client, uses a fixed game-style panel, and has no double-tap → cursor-position context trigger |
| StarLight / Steam Link | Virtual mouse = fixed panel option; no cursor-linked context overlay |
| PC-side macro pads (Touch Portal etc.) | Fixed macro panels, independent of cursor position |

Nothing combines **passive click pass-through + double-tap interception → a context
overlay at the cursor position**. That is TapKey's niche.

## 3. Architecture

### 3.1 Why a PC-side utility

iOS does not allow intercepting another app's touch input, so the only client-side
path would be forking Moonlight itself — maintaining a full Swift streaming client.
A PC-side hook utility is far cheaper and works with any streaming client that
injects standard mouse input.

### 3.2 Data flow

```
iPad touch → Moonlight → Windows mouse events (SendInput)
                              ↓
                  TapKey: WH_MOUSE_LL hook
                    ├─ t1 LDOWN@P → passes through (recorded as LAST)
                    ├─ t2 LDOWN within the window, same point (r=0)
                    │     → t2 down+up swallowed, WM_TAPKEY_OPEN posted with anchor P
                    └─ while the overlay is open: detection OFF (no re-trigger)

Overlay (layered topmost window near P) → captured by Moonlight → visible in the stream
Button tap = a click at that position → the overlay window receives it directly
Actions execute at anchor P: SetCursorPos(P) + marker-tagged SendInput
```

### 3.3 Input model (measured)

- The input is **tap-only**: no continuous move stream; each tap is a position jump
  plus a click.
- **One physical tap = 1 LDOWN + 1 LUP.** A MOVE preceding them is not a tap.
- Events arrive quantized to the stream's frame grid (60 fps → 15.625 ms); hold and
  gap values are integer multiples of it.
- Same-point re-taps arrive at **exactly identical coordinates (r = 0)**.
- Read coordinates from the hook event struct (`MSLLHOOKSTRUCT`: pt.x @ offset 0,
  pt.y @ offset 4 on x64) — **not** `GetCursorPos()`, which is stale for injected
  input.
- Double-tap detection needs only LDOWN time + coordinates; no move analysis.

Full measurement data and parameter guidance: [`tap-signature.md`](tap-signature.md).

### 3.4 DPI

The app is Per-Monitor-V2 DPI-aware and works in **physical pixels** end to end;
hook coordinates are physical desktop pixels, so no conversion is needed. (The
legacy C# PoC was DPI-unaware and had to divide physical coordinates by the display
scale for both WinForms positioning and `SetCursorPos`.)

## 4. Trigger model — pass-through double-tap

- **t1 passes through immediately** (zero latency, zero swallowed input). A
  same-point **t2 within `double_tap_window_ms` (default 250 ms) is swallowed**
  (down + up) and opens the overlay at P. No t2 within the window → nothing happens;
  t1 was already delivered as a normal click.
- **No re-injection path exists.** The only swallowed event is t2, and the overlay
  consumes it — there is never a buffered tap to replay. This eliminates
  flush-deadline / orphan-up / exactly-once-delivery complexity and the re-injection
  feedback loop (see tap-signature.md §6).
- **Trade-off:** on trigger, t1 lands as a single click on the target (harmless, e.g.
  file selection). A false trigger is just a spurious popup — one outside click
  dismisses it. Opening files uses the overlay's **DblClk** button (§5.1), since a
  fast double-tap is now the trigger itself.
- **Window rationale:** measured natural double-taps fall in 223–257 ms; deliberate
  re-taps start at ≥328 ms. 250 ms sits inside the band. Runtime-adjustable 20–1000 ms.
- **WH_MOUSE_LL swallow rule:** return **nonzero** (`LRESULT(1)`) to block an event;
  returning 0 / `CallNextHookEx` **passes** it. Returning 0 on a "swallow" path leaks
  the event to the app (overlay opens *and* the target reacts).
- **Orphan-up guard:** t2's LUP arrives ~15 ms after its swallowed LDOWN, often after
  the overlay has opened. A time-bounded `PENDING_UPS` counter eats exactly that up;
  the bound prevents swallowing a legitimate later click.
- **Detection OFF while any overlay layer is visible** — button taps must not
  re-trigger the overlay. Re-armed on close.
- **Self-injection marker:** every injected event carries
  `dwExtraInfo = 0x54415050` ("TAPP"); the hook skips marked events (no feedback).
- **Callback constraints:** the hook runs on the installing thread's message loop
  and must stay fast — atomics + one mutex + `PostMessage` only; no UI work, no
  `SendInput`, no file I/O. A slow callback gets the hook removed by Windows.
- **Outside-click dismiss:** while a layer is visible, any click outside its rect
  posts `WM_TAPKEY_OUTSIDE` → close everything and resume detection. The click
  itself passes through to whatever is underneath.

## 5. Overlay UI

**Stack: Rust + windows-rs, raw Win32 windows** — no UI framework.
`WS_EX_LAYERED | TOPMOST | NOACTIVATE | TOOLWINDOW` popup + `UpdateLayeredWindow`
with per-pixel alpha.

**Rendering pipeline:** manual antialiased shapes (signed-distance rounded rects,
circles, strokes) into a straight ARGB buffer; labels rasterized by GDI as
black-on-white and read back as a **coverage map** (GDI `TextOut` zeroes the alpha
byte of 32bpp DIB text, so drawing text directly renders invisible); composite,
premultiply, one blit. The settings window reuses the same pipeline.

### 5.1 Main panel

2 columns × 3 rows of square 90×90 px buttons; PAD_X 16 / PAD_Y 12 / GAP 12 →
panel **224×318**.

| Slot | Button | Action |
|---|---|---|
| row 1 left | Hold2s (clock) | left-down at P → hold 2 s → left-up |
| row 1 right | RClick (mouse) | right-click at P |
| row 2 left | Drag (4-way arrow) | switch to the D-pad layer (§5.3) |
| row 2 right | Scr Up (chevron) | wheel **+240** at P (2 notches) |
| row 3 left | DblClk (two rings) | proper double-click at P — the file-open path |
| row 3 right | Scr Dn (chevron) | wheel **−240** at P (2 notches) |

- **All actions execute at anchor P** (the double-tap point), not at the button tap
  position: `SetCursorPos(P)` before injection.
- **Cursor-moving actions run on WM_LBUTTONUP, not DOWN.** Moonlight delivers each
  tap's release at the *current* cursor position; executing on DOWN would move the
  cursor to P first, so the release lands on the target — dismissing a context menu
  that just opened, or straying into the app. Right-click / scroll / double-click
  are therefore deferred to button-up (a 250 ms flash timer is the LUP-miss
  fallback). Hold-2s is the exception: the press must start immediately.
- **Cursor return — scroll only.** Scroll executes at P, then the cursor returns to
  the button tap point: capture `GetCursorPos` just before `SetCursorPos(P)`, return
  via a **150 ms one-shot timer**. The delay is mandatory — injected events dispatch
  only after the handler returns, so an immediate return would land the queued wheel
  event at the return point instead of P. Other actions leave the cursor at P / the
  path end.
- **One-shot auto-close.** Right-click, double-click, hold-2s and drag-Drop close the
  panel **250 ms after completion** (after the 150 ms cursor return — closing kills
  all timers, so the return must fire first). Scroll keeps the panel open
  (repeatable). A new button tap cancels a pending auto-close (right-click → quick
  scroll keeps the panel).
- **Wheel encoding rule:** `MOUSEINPUT.mouseData` is `u32` — a negative delta must be
  **sign-extended**: `(delta as i32) as u32` (−120 → `0xFFFFFF88`). Zero-extending
  (`delta as u16 as u32`) yields `0x0000FF88`, which Windows clamps to **+32767** in
  `WM_MOUSEWHEEL`'s HIWORD — a maximal UP scroll. Symptom signature: *both buttons
  scroll the same direction with different intensity* ⇒ encoding bug, not a
  direction-mapping error. Verify direction with a probe window logging
  `WM_MOUSEWHEEL` wParam, not screenshots — at the top of a document, scroll-down
  also makes content move up on screen.
- **Double-click button:** two ~30 ms presses with a ~50 ms inter-click gap — well
  inside the double-click time, so Windows merges them into one
  `WM_LBUTTONDBLCLK`. Marker-tagged so the hook skips them.

### 5.2 Theme

Purple/lavender, accent **#C4B5FD**:

- Panel: dark purple `#1A1033` @ ~40% alpha; buttons: mid purple `#453670` @ ~55%
  alpha — both visibly see-through, so the streamed content behind stays readable.
- Icons/labels white; pressed state = lavender fill + dark-purple icon.
- Square buttons; only the panel has rounded corners (radius 20).
- Touch targets ≥64 px, bold icons + short English labels (viewed at a distance
  through a headset).

**Auto-close summary:** global idle timer (default 8 s, layer-independent, reset by
any button tap on any layer) + outside-click dismiss + one-shot action completion.
Esc: main panel = quit app, D-pad = return to main. The anchor dot hides with the
overlay and reappears at P on the next open.

### 5.3 D-pad drag layer

The Drag button replaces the main panel with a 3×3 D-pad (**326×326**, same 90 px
cells) in a deliberately different tone (teal) so the layer switch reads at a glance.

```
┌─────────┬─────────┬─────────┐
│   ↖     │    ↑    │   ↗    │
├─────────┼─────────┼─────────┤
│    ←    │ ● Grab  │    →   │
├─────────┼─────────┼─────────┤
│   ↙     │    ↓    │   ↘    │
└─────────┴─────────┴─────────┘
```

- **Plan-then-replay:** center tap #1 (**Grab**) starts planning at P — *zero mouse
  events while planning*. The 8 direction cells extend the recorded path by
  ±`DRAG_STEP_PX` (20 px); the anchor dot follows the planned point. Center tap #2
  (**Drop**) ends planning and replays the path as one `SendInput` burst, then the
  overlay auto-closes.
- **Drop executes on button-up** (same release-interference reason as main-panel
  actions): the replay moves the cursor to P, and a tap release delivered mid-replay
  would cancel the drag before it starts. Flash timer as LUP-miss fallback.
- **Replay = all absolute-coordinate SendInput** (`MOUSEEVENTF_MOVE | ABSOLUTE |
  VIRTUALDESK`, normalized 0..65535 over the virtual screen): MOVE+DOWN at the first
  point, MOVE per point (15 ms apart), MOVE+UP at the last. **Never mix in
  `SetCursorPos`** — those moves bypass the input stream (invisible to
  `WH_MOUSE_LL`, delivered without `MK_LBUTTON` while the queued DOWN is still
  pending), so the target never sees a held-button drag.
- **Placement: every layer is positioned relative to P** (offset ≥24 px, flip near
  edges, clamp to P's monitor work area). Centering the larger D-pad on the main
  panel would cover P, and the replay's LEFTDOWN would land on the overlay itself.
- **Replay guard:** ignore the overlay's own mouse input for 120 ms **after the
  blocking replay ends** — injected clicks dispatch only once the handler returns,
  so a deadline set *before* a long replay is already expired at dispatch time.
- **Hold auto-repeat:** direction cells repeat after 400 ms, then every 90 ms
  (long drags). The center is a tap toggle — no repeat.
- **Esc while planning is safe:** nothing was ever pressed; the path is simply
  dropped and the main panel returns.

### 5.4 Anchor dot

A separate click-through topmost window (`WS_EX_TRANSPARENT`, class `TapKeyAnchor`):
lavender `#A78BFA`, diameter 20 px (configurable), ~90% alpha. Created with the main
panel and visible in every overlay mode; follows the planned point while planning,
parks back at P on release, hides whenever the overlay closes.

### 5.5 Layout as data (planned)

The grid is describable as data — geometry `{cols, rows, padding, gap}` plus a slot
list `[{pos, kind, icon, label, ...}]` with `kind` = `action` | `empty` | `dir`
(dx/dy) | `center-toggle`. Built-in profiles: main 2×3, D-pad 3×3. A future
custom-slot editor would modify the slot array; render/tap logic only branches on
`kind`.

## 6. Settings, tray, portable config

### 6.1 Portable config

**`tapkey.toml` next to the exe** — the folder carries its settings; no installer,
no AppData. Missing file → built-in defaults; unknown keys ignored. Minimal
line parser (`key = value`, `#` comments). Write failure (read-only location) →
values stay live in memory + one warning at exit.

| Key | Default | Range | Meaning |
|---|---|---|---|
| `double_tap_window_ms` | 250 | 20–1000 | t1→t2 trigger window |
| `tap_radius_px` | 0 | 0–20 | same-point radius (0 = exact match; raise to 2 if jitter appears) |
| `overlay_idle_ms` | 8000 | 1000–60000 | idle auto-close |
| `anchor_dot_px` | 20 | 4–100 | anchor dot diameter |
| `detection_enabled` | true | — | global pause switch |

### 6.2 Runtime apply (atomic)

Detection parameters live in atomics; the hook reads them per event. A settings
change applies **from the next event — no restart**. `detection_enabled = false` is
a global pause, separate from the overlay-open detection suspend.

### 6.3 Settings window

Same owner-draw lavender pipeline as the overlay (design unity), but **opaque** —
it is a reading surface. 360×388, screen-centered, 19 px semibold labels.

- Rows: **Double-tap** (±10 steppers), **Tap radius** (±1), **Idle close** (±500).
- **Pause / Resume detection** toggle (lavender fill while paused), **Close**, **Quit**.
- **Auto-save:** every change applies via atomics AND persists to `tapkey.toml`
  immediately — no save button. Failed write → "Save failed" status + exit warning;
  values stay live in memory either way.
- Opening dismisses any overlay layer and suspends detection (settings taps never
  trigger the overlay); closing resumes detection.

### 6.4 Tray — pure launcher

`Shell_NotifyIconW` with `NOTIFYICON_VERSION_4`, hosted on a **message-only window**
(`HWND_MESSAGE`). **One tap (left click) opens the settings window.** No context
menu, no double-click action — all control lives in the settings window. The icon
switches to gray + pause bars while detection is paused, and is re-added on
`TaskbarCreated` (explorer restart).

Two verified pitfalls:

- **Tray callbacks are SENT messages from explorer.** `GetMessageW` dispatches them
  internally to the wndproc — they never appear in the loop body. Handle `WM_TRAY`
  inside the wndproc.
- **v4 lParam packing:** the notification code is the LOWORD; the icon id is packed
  into the HIWORD. Accept `WM_LBUTTONUP` (v0) and `NIN_SELECT`/`NIN_KEYSELECT`
  (`0x0400`/`0x0401`, v4).

Windows 11 places new tray icons in the overflow (hidden area) by default; drag the
icon out of the overflow for one-tap access.

### 6.5 App / tray icon

Full-bleed lavender squircle (superellipse n=4) + dark-purple "t" mark whose right
crossbar arm is a detached tap dot; paused = gray squircle + pause bars. Master:
`docs/assets/tapkey-icon.png`.

- Tray: 96×96 source (integer downscale to the 16/24/32/48 tray slots), 4×4
  supersampled software render into a 32bpp DIB. Geometry lives in the same 1024
  design space as `scripts/render_icon_concepts.py`.
- exe icon: multi-size `tapkey.ico` (PNG-compressed entries) via `icon.rc` +
  `windres` (compiled to COFF and linked by `build.rs`).
- Pitfall: a 32bpp DIB pixel read as `u32` is **`0xAARRGGBB`** (memory bytes
  B,G,R,A) — *not* COLORREF's `0x00BBGGRR`. Packing COLORREF-style swaps R/B
  (lavender renders pink). GDI shape drawing also zeroes alpha; the software
  renderer composites straight alpha explicitly.

## 7. Multi-monitor

- Hook coordinates are already virtual-desktop physical pixels — no change needed.
- **Drag replay:** every absolute event carries `MOUSEEVENTF_VIRTUALDESK`, and
  normalization spans the virtual screen (`SM_XVIRTUALSCREEN` etc.), handling
  negative origins (monitors left of / above the primary).
- **Overlay placement:** clamped to the **work area of the monitor containing P**
  (`MonitorFromPoint` + `GetMonitorInfoW` `rcWork`, primary-monitor fallback) — the
  panel stays on P's monitor instead of being pulled to the primary's edge.
- The settings window stays centered on the primary monitor (launcher context).
- Mixed-DPI setups: PMv2 keeps everything in physical pixels and absolute
  normalization is over the virtual desktop; worth re-verifying on a mixed-DPI rig.

## 8. Implementation pitfalls

- **Register a class cursor** (`hCursor = IDC_ARROW`) on every visible window class.
  With NULL and no `WM_SETCURSOR` handler, nobody sets the cursor over the client
  area — whatever cursor was last set sticks (a busy cursor picked up at the action
  target lingers over the overlay). Click-through windows are exempt.
- **Stateful actions must release on every close path.** A hold-2s left-down that
  outlives its panel leaves the button stuck; one release helper is called from
  idle close, outside-click dismiss, settings open/quit, layer switch and app exit.
- **Every window-creation path must arm its timers** (idle etc.) — not just the
  layer-switch paths.
- **Timer ordering:** cursor return (150 ms) must fire before action auto-close
  (250 ms), because closing kills all timers. A fresh button tap cancels a pending
  auto-close.
- **`SendInput` clicks dispatch after the handler returns** — a flag set/cleared
  around the call does not stop your own window from receiving its own queued
  click. Use a short deadline checked in the wndproc.
- **`WS_POPUP` for layered windows** — style 0 creates a caption and offsets client
  coordinates.
- **GDI text into a 32bpp DIB zeroes alpha** — use the black-on-white coverage pass.
- **Keep the hook callback fast** (<~300 ms) or Windows removes the hook.
- **Windows Sandbox / VMs isolate input** — `WH_MOUSE_LL` receives zero events
  there; test on real hardware.
- **UIPI:** `SendInput` is blocked against elevated (admin) target apps unless
  TapKey runs elevated too.
- GDI labels: Malgun Gothic with `HANGEUL_CHARSET` for font linking; measure text
  with `GetTextExtentPoint32W` before placing value columns.

## 9. Build

**Main app (`src/tapkey`) and taplogger (`src/taplogger`)** — Rust, GNU toolchain:

```bash
# mingw-w64 gcc + windres must be on PATH (e.g. WinLibs UCRT), plus cargo
cd src/tapkey
cargo build --release     # target/release/tapkey.exe
```

- Kill any running `tapkey` first — the live exe locks the link.
- Release profile: LTO + strip → ~400 KB, Windows system DLL imports only.
- GUI subsystem (`#![windows_subsystem = "windows"]`): double-click shows no
  console; stdout logs survive only via terminal redirection.
- `build.rs` compiles `icon.rc` with `windres` — windres must be on PATH.

**taplogger** (observe-only tap measurement tool): `cargo build` in
`src/taplogger`. Writes `logs/taplogger-*.txt` relative to the working directory;
30-minute hard cap; never modifies input.

**Legacy PoC (`src/TapKey.PoC`)** — C#/WinForms hook reference (net10.0-windows):

```bash
cd src/TapKey.PoC
dotnet run
```

5-minute auto-exit safety timer. Kept as a minimal reference for the hook logic;
the Rust crate is the real app.

## 10. Limitations & roadmap

- **Timing cannot perfectly separate intended double-taps from natural re-taps**
  (tap-signature.md §8). Pass-through makes false triggers harmless; tune the window
  to the user's own rhythm rather than seeking a perfect constant.
- Multi-monitor support is implemented; real dual-display and mixed-DPI
  verification is still pending.
- Roadmap: custom slot ↔ action mapping (§5.5), middle-click, per-slot multi-tap
  actions (opt-in — a multi-tap slot delays its single-tap firing by the
  identification window), auto-start, deployment packaging, `tap_radius` default
  review (0 vs 2 for jittery environments), hook re-install if Windows removes it
  under heavy load.