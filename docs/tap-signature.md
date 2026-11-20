# Tap Signature — iPad + Moonlight Input, Measured

Event-level definition of taps and double-taps arriving from an iPad through
Moonlight, so trigger parameters can be set against measured data instead of
guesses. All numbers come from observed logs.

**Data:** 23 taplogger log files, **1119 Moonlight taps** (synthetic probe events
excluded), single session on a 2560×1440 @ 150% host (Per-Monitor-V2, physical
pixels). Reproduce with `src/taplogger` (observe-only) on the target setup.

---

## 1. Input path

```
iPad touch → Moonlight (iOS) → network → Moonlight host (Windows) → SendInput → WH_MOUSE_LL
```

- Events arrive **quantized to the 60 fps frame grid (15.625 ms)**. Hold and gap
  values are integer multiples of it (±1 ms tick rounding).
- Coordinates are physical pixels (PMv2). Same-point re-taps arrive at **exactly
  identical coordinates (r = 0)**.
- **One physical tap = 1 LDOWN + 1 LUP.** MOVE events exist separately but are not
  taps (the logger records button events only).

## 2. What a tap is (event level)

| Property | Measured |
|---|---|
| Structure | LDOWN → LUP, same (x, y) |
| hold (down→up) | **only {0, 15, 16} ms** — 0 ms ×262, 15 ms ×331, 16 ms ×526 |
| hold meaning | 0 = down/up within one frame; 15/16 = one frame (15.625 ms rounded) |
| moved (down→up distance) | 0 px for taps; moved ≥ 8 px is a drag |

- **Hold is not a discriminator.** 0/15/16 are all normal taps and appear randomly
  in both the first and second tap of a double-tap (§4). Do not filter on hold.
- Count taps by **LDOWN**. LUP confirms completion; an LDOWN without an LUP is a
  drag start.

## 3. What a double-tap is

**Two taps at the same point, down-to-down gap inside the double band.**

- Measured band: **172–297 ms, 100 pairs.** 99 pairs concentrate in 203–297;
  mode 250 ms (18 pairs).
- Coordinates: every observed pair at **dist = 0** (r = 0). Recommended radius ≤2 px
  (headroom).
- Gaps are frame-quantized: 13–19 frames.

| gap (ms) | frames | pairs |
|---|---|---|
| 172 | 11 | 1 |
| 203 | 13 | 4 |
| 218/219 | 14 | 19 |
| 234/235 | 15 | 19 |
| 250 | 16 | 18 |
| 265/266 | 17 | 17 |
| 281 | 18 | 15 |
| 296/297 | 19 | 7 |
| **total** | | **100** |

(±1 ms splits like 218/219 are `GetTickCount64` rounding — the same frame value.)

## 4. What is NOT a double-tap (measured separation)

**① Tap splitting: absent in this data.**
- Zero LDOWNs with gapDown < 150 ms (any coordinates, synthetic excluded). Minimum
  same-point gap: 172 ms.
- "One physical tap splits into two LDOWNs" was not observed in 1119 taps.
- hold = 0 taps (262) are just fast taps, not splits: cluster rate hold=0 4.2% vs
  hold>0 5.5% — no difference.

**② Slow re-taps: same point, ≥328 ms.**
- 328 (×2), 359, 360, 438, 453, 468, 469, 500 (×3)…
- **The 31 ms gap between 297 and 328 is the double-tap / re-tap separation margin.**
  A 300 ms window sits in that gap.
- ⚠️ This is a **controlled test** (deliberate fast double-taps + deliberate
  re-taps). In real use, unconscious double-taps are slower and the bands overlap
  (§8).

**③ Fast taps at different points: ≤300 ms but outside the radius.**
- 13 pairs observed, distances **93–238 px**. A ≤2 px radius rule excludes them
  exactly.
- Minimum distance 93 px ≫ 2 px — ample headroom, but keep the radius tight to catch
  only intended double-taps.

**④ Chains of 3+ taps: 6 observed.**
- Three same-point LDOWNs, consecutive gaps ≤350 ms. Examples: [235, 281],
  [219, 203], [266, 234].
- The logger flags **pairwise** → a 3-chain raises two cluster flags (t2, t3). It
  does not interpret gestures (§5 policy).

## 5. Trigger parameter guide

| Parameter | Recommended | Rationale |
|---|---|---|
| **window** | **300 ms** | accepts the double band up to 297, excludes re-taps from 328. Safe tuning range **200–320 ms** |
| window < 200 | not recommended | misses most double-taps (only 1 pair below 200) |
| window ≥ 328 | not recommended | misfires on slow re-taps |
| **radius** | **≤2 px** | observed r = 0; other-point minimum 93 px gives headroom, stay tight |
| **hold filter** | **none** | 0/15/16 are all normal taps (§2) |
| counting | LDOWN only | LUP confirms completion; absent LUP = drag |

**Chain (3+ taps) policy — decide when implementing a trigger:**
- (a) **Trigger once on the first cluster, suppress until the sequence ends**
  (recommended): no further trigger until a gap > window. Prevents a 3-chain firing
  as two double-taps.
- (b) Pairwise counting: 3-chain = two double-taps. For counting deliberate
  rapid taps separately.
- (c) Separate gesture: branch 3-chains into a triple-tap.

The logger records raw pairs, so any policy can be validated after the fact.

## 6. Why pass-through has no re-injection feedback

A **buffered** trigger model swallows t1 (down+up) and must re-inject it when no t2
arrives — "repaying the swallowed debt". If marker-skipping fails, the re-injected
click re-enters detection as a fresh same-point LDOWN within the window → a
false-trigger feedback loop. Buffered models also carry flush-exit, orphan-up and
exactly-once-delivery complexity: every exit path must deliver the swallowed tap
exactly once.

The **pass-through** model removes that entire class: t1 passes immediately (the
app already received it — no debt), only a same-point t2 within the window is
swallowed, and the overlay consumes it. **Nothing is ever re-injected**, so there
is no feedback path and no delivery accounting.

Trade-off: on trigger, t1 lands as a single click on the target (file-selection
level, harmless). A false trigger is a harmless popup — one outside click dismisses
it. Simple and safe.

## 7. Limits / open points

- **No intent labels:** a 200–300 ms pair cannot be classified as intended
  double-tap vs finger bounce from the log alone. The band matches deliberate
  double-tap rhythm; annotated sessions would confirm it.
- **Frame-rate dependent:** 60 fps assumed. At 30/120 fps the quantization grid and
  gaps scale — re-measure.
- **hold = 0 origin:** same-frame down/up (23%). Harmless, but which gesture in the
  input path produces 0 ms is unconfirmed.
- **Device-specific:** measured on one iPad + Moonlight + host combination.
  Re-measure on other clients / networks.

## 8. Real-world recall and the window trade-off

Field use with **window = 250 ms**:
- Deliberate fast double-taps → **~90% trigger rate.**
- Natural (unconscious rhythm) double-taps → **~20–30% trigger rate.**

Natural-rhythm double-taps are slower than deliberate ones (~200–250 ms) and fall
outside a 250 ms window. The clean separation in §4② was a controlled-test result;
real rhythm has a wider, overlapping band. **Timing alone cannot fully separate
intended double-taps from natural re-taps.**

Recall over a later session (93 same-point pairs):

| window | recall |
|---|---|
| 200 | 1% |
| 250 | 54% |
| 300 | 75% |
| 350 | 76% |
| 400 | 76% |
| 500 | 79% |

Gap distribution: 69 pairs in 200–299, almost none in 300–399 (one at 328), re-taps
from 400+. Raising the window to 300 jumps recall 54→75%; beyond that, re-tap
false-positive risk grows while gains flatten (350+).

**Trade-off:** window ↑ = natural double-tap recall ↑ + re-tap false positives ↑.
There is no perfect window.

**Direction:**
1. **Natural-rhythm calibration** — measure the user's own double-tap rhythm and
   set a window that accepts that distribution (actual behavior, not a controlled
   test).
2. **Pass-through trigger** — false triggers become harmless (no input swallowed,
   outside click dismisses), so the window can be generous: higher recall, damage
   limited to "an occasional popup".
3. Window tuning alone cannot achieve recall and false-positive goals
   simultaneously → combine 1 + 2.

---

### Appendix: measurement method

- taplogger (`src/taplogger`): observe-only `WH_MOUSE_LL`, `GetTickCount64` ms
  reference. Every event passes through `CallNextHookEx` (zero input modification).
- Synthetic probe events excluded by coordinate/hold filters (probe points and
  hold > 25 ms).
- Cluster flag (logger): same point ≤2 px, down-to-down ≤ window (default 300,
  panel ±10 adjustment). An observation flag independent of the tap definition —
  not a gesture trigger.