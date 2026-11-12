# WHOOP 5.0 sensors — reference (fw 50.38.1)

Source: guided sensor session of 12 Nov 2026 (Alarm tab → "Session capteurs complète",
`SensorSession` in GooseSwift/SmartWake.swift): 7 phases, 1 987 frames, every frame stored with its
phase/step in `Documents/sensor_session.tsv`. Complements docs/OPTICAL.md (optical feasibility),
docs/STRAP_BEHAVIOR_AUDIT.md (day/night behaviour) and the field catalogue in
Rust/core/src/whoop5_records.rs.

## 1. What the strap sends, and when

| Situation | Frames |
|---|---|
| No command, between syncs (60 s observed) | **nothing** — the strap only transmits on request or during a history sync |
| History sync (every 6 min by default, 5/2 min/45 s when smart wake is armed) | R18 (124 B) 1/s of history, R26 (88 B), events (48), metadata (49), console (50) |
| `TOGGLE_REALTIME_HR` (cmd 3, payload `[1]`) | packet 40, 1 frame/s |
| `TOGGLE_IMU_MODE` (cmd 106, `[1,1]`) | packet 43 layout 21 (R21, 1 244 B), 1 frame/s |
| `ENABLE_OPTICAL_DATA` + `TOGGLE_OPTICAL_MODE` (cmds 107 + 108, `[1,1]`) | packet 43 layout 20 (R20, 2 140 B), 1 frame/s **and** R20 written to history |
| `TOGGLE_PERSISTENT_R20` / `R21` (cmds 153 / 154) | R20 / R21 written to on-strap history, 1 record/s, fetched by the next sync (packet 47) |
| `SEND_R10_R11_REALTIME` (cmd 63) | refused (result 3) |

All streams together (HR + IMU + optical) hold 1 frame/s each over BLE (~3.4 kB/s).
Command response (packet 36): byte 10 = command, 11 = sequence, 12 = result (1 = OK, 3 = refused).

### Stopping the raw recorders — order matters
On-strap R20/R21 recording continued after `153/154 off` was acknowledged when optical mode had been
switched off *before* `153 on`. Sending the full stop set in this order stops everything
immediately (verified: last record 10:28:57, stop at 10:29:00, R18 unaffected):
`153 off → 108 off → 107 off → 154 off → 106 off → 63 off → 3 off`
(`SensorStreamCommandKind.stopPhysiologyCapture`, used by the session end and by the
"Arrêter les flux de test" button). Left running, the recorders store ~290 MB/day on the strap,
lengthen every sync and keep the LEDs at 25 Hz.

## 2. Live heart rate (packet 40)

Frame offsets: ts u32 @10 · word u16 @14 (constant within a stream, not a time) · HR u8 @16 ·
R-R count u8 @17 · R-R u16[] @18 (1/1024 s ticks) · byte 26 = 1 (flag).

- Arrival − strap timestamp ≈ 4.0 s (constant).
- 0–2 R-R per frame; at rest ~100 % of beats are delivered; R-R spread at rest 42 ms SD (the strap's
  own beat detector is far cleaner than naive peak picking on R20, 185 ms SD).
- **R-R is withheld during motion** (walking: 2 R-R in 30 s) while HR keeps updating (57 → 77 bpm).
- Slow breathing (8/min): R-R 720 → 1 460 ms within one breath — genuine respiratory sinus
  arrhythmia. Breath hold: HR 59 → 63 → 55 bpm (vagal response).
- Artefacts are mostly **split beats**: two intervals summing to one normal beat
  (655 + 576, 325 + 854 in a 1 150 ms rhythm).

→ Since analysis v9, `hrv_qc` merges split beats and uses an adaptive artefact threshold
(5.2 × local quartile deviation, 20–45 %) instead of a fixed 20 % (Lipponen & Tarvainen 2019).

## 3. IMU (R21, 1 244 B)

Offsets: marker @10 (0x80) · record index u32 @11 · ts u32 @15 + u16 @19 (1/32768 s) ·
accel count u16 @24 · accel x/y/z i16[100] planar @28/228/428 · gyro count u16 @630 ·
gyro x/y/z i16[100] @640/840/1040. Constants: byte 26 = 3, u16 @632 = 517 (likely range/ODR codes).

| | Accelerometer | Gyroscope |
|---|---|---|
| Rate | 100 Hz (100 samples/frame, frames 1.000 s apart) | 100 Hz |
| Scale | 4 096 LSB/g (|a| still = 1.001 g) | 16.38 LSB/(°/s); checked against gravity rotation, ratio 1.01 |
| Range | ±8 g, saturates at ±32 767 (taps, shakes) | ±2 000 °/s |
| Noise at rest | 2–4 mg SD | 0.2–0.6 °/s SD, bias ≈ 1 °/s on X |
| Quirk | — | first ~4 samples after IMU start = −32 768 (sentinel; held to next valid since v9) |

Gesture signatures: walking 1.70 Hz in |a| (= 102 steps/min); double tap = two 5–6 g spikes 0.32 s
apart; triple tap = three 6–10 g spikes ~0.45 s apart; flat wrist → gravity on Y/Z, walking arm →
gravity on X.

## 4. Optical (R20, 2 140 B)

Five blocks at frame offsets 26/448/870/1292/1714; each: count u8, two slots (A/B) with source
(LED) u8, drive u16, detector (photodiode) u8, range u32, offset i16, then i32[50] samples.
Samples are continuous across frames (boundary step = within-frame step) → **25 Hz per channel**.

| Block | Config (live, daytime) | Level | Pulse | Role |
|---|---|---|---|---|
| 0 A | LED 1, drive 3 350, PD 3 | ~315 k | clear, PI 0.5 % | main PPG (green-like) |
| 0 B | LED 4, drive 6 700, PD 4 | ~439 k | clear, PI 0.46 % | main PPG, 2nd LED/PD pair |
| 1 | LED 3, drive 12 750, range 16, count 0 | — | — | reserved (high-current: SpO₂ red/IR) |
| 2 | LED 2, drive 5 300, offsets 1 600/800, count 0 | — | — | reserved (SpO₂) |
| 3 A/B | drive 0, PD 3/4 | ~300 | none | ambient light (subtract) |
| 4 A | LED 2, drive 200, PD 3 | ~97 k | weak, PI 0.1 % | 2nd wavelength, low current |
| 4 B | PD 1 | ~9.7 k | yes, PI 0.4 % | 3rd photodiode (other distance) |

The DC offset of block 0 toggles 0/800/1 600 between frames (offset DAC tracking).

Responses: firm press → AC ×10, DC +22 %; clenched fist → AC ×10–20, DC −30 % and stays lower after
(contact / venous volume change). PPG pulse rate at rest 55.9 bpm vs strap HR 57.6 bpm; while
walking the PPG spectral peak is wrong (46 bpm vs 67 bpm) → raw PPG needs accelerometer-referenced
motion cancellation before use in motion.

## 5. On-strap history (sync)

R18 1/s always (124 B). R20 1/s while optical mode or `153` is on; R21 1/s while `154` is on;
fetched without duplicates (unique timestamps = seconds). An R26 window (88 B, 25 samples, burst 3,
status 2960) appeared only while all live streams ran together.

## 6. Consequences for the pipeline

1. HRV: split-beat merge + adaptive threshold (done, analysis v9; real nights: last-third RMSSD
   53.0 → 56.2 ms on 11 Nov, 26 beats recovered).
2. IMU: start-up sentinel handled at decode (done); accel/gyro scales now marked validated.
3. Raw PPG: usable at rest (perfusion, waveform, two wavelengths, three photodiodes); in motion it
   needs IMU-based artefact removal; the strap's own R-R is the reference beat source.
