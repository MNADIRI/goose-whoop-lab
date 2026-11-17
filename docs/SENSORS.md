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

Triple-tap cancel (sport auto-start): taps are 3–12 g peaks only 1–2 samples wide (third tap of a
triple can be ~3 g); shaking, walking, gym and daily life give peaks >= 3 samples wide. Rule
(`sport::triple_taps`): three sharp peaks >= 2.5 g, 0.2–0.8 s apart, no other peak within 1 s
before or after. Checked on 5.3 h of stored IMU (gym, sessions, daily life): one match, a deliberate
tap during the 15 Nov alarm. The firmware DOUBLE_TAP event (14) is not used for this: it fired
dozens of times during the 11 Nov tennis match and the 13 Nov gym session.

Live IMU continuity (17 Nov review): the stream is only as good as the app's requests. On 15 Nov
an "off" queued behind a history sync landed after the next "on" (watch restarted 5 s later), so a
whole tennis watch had 0 s of IMU and the match (19:00–20:15) was never confirmed; on 17 Nov only
227 of the 600 s of the tap window had IMU frames (gaps of 20–36 s), so a cancel made in a gap is
lost. The app now builds each stream command from the state wanted when the strap is free, and
re-requests the stream after 20 s without a frame. Detection no longer depends on the IMU alone:
"mouvement soutenu" = 7 of the last 10 minutes with R18 dynamic acceleration >= 0.5 g and heart-rate
reserve >= 30 % (9–17 Nov history: the 11 and 15 Nov tennis and one active hour on 13 Nov, nothing
else). Periodic arm motion while stepping (> 1.2 steps/s) is not a strength set: on 17 Nov a
3-minute run gave three "pull-up" sets and opened a session.

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

## 4c. Optical audit, pulse wave and respiration (`ppg.rs`, 17 Nov)

**Channels** (1 673 s of R20 recorded 10–12 Nov, 25 samples / s / channel; PI on ADC codes here, absolute photocurrents and corrected PI in §4d):

| Channel | Light per unit of drive | PI | Class |
|---|---|---|---|
| LED 1 → PD 3 (block 0 A) | 73 | 1.1–1.2 % | green-like (strongly absorbed, very pulsatile) |
| LED 4 → PD 4 (block 0 B) | 15–23 | 1.5–1.8 % | green-like |
| LED 2 → PD 3 (block 4 A) | 594 (8×) | 0.2–0.3 % | red / infrared-like (penetrates, weakly pulsatile) |
| LED 2 → PD 1 (block 4 B) | — | 1.3 % | same LED 2 seen by photodiode 1 (see §4d) |
| blocks 3 A/B | — | — | ambient light |
| blocks 1 / 2 (LED 3 high current, LED 2) | — | — | SpO₂ slots, never sampled |

The R26 night bursts are **LED 1 → PD 3**: on the 40 s where R20 and R26 overlap the samples are
identical. With a single long-wavelength channel and the SpO₂ slots empty there is no simultaneous
red + infrared pair: continuous SpO₂ cannot be computed from what the strap sends (the strap's own
spot SpO₂ in R18 stays the source). Persistent R20 (command 153) is classed as a critical state
change and is not used.

**Pulse-wave template**: the PPG is band-limited (< 12.5 Hz), so each still burst is reconstructed
at 200 Hz (Lanczos a = 4); beats are aligned on the steepest upstroke (polarity: systole is steeper
than diastole) and median-averaged. 9–17 Nov nights: 690–960 beats, split-half r ≥ 0.9999.
Features: rise time 230–305 ms, systolic peak 170–245 ms after the steepest slope, a diastolic
(reflected) wave 210–260 ms later of almost the same height (reflection index 0.9–1.1), b/a
−0.30 to −0.46. These are relative night-to-night indices (they also depend on strap tightness,
temperature, arm position), not blood pressure.

**Respiration from the PPG**: per still burst, beat amplitude, baseline and interval series
(4 Hz), 9–30 rpm, kept when 2 of 3 agree within 2 rpm. The 1 Hz R-R tachogram shows that the
largest slow component is the ~0.1 Hz Mayer wave (5–8 rpm, blood-pressure regulation), hence the
lower band edge. Nights 9–17 Nov: 15.4–16.8 rpm from 23–31 bursts, vs 15.5–16.5 rpm from the R-R
method: two independent estimates within 1 rpm. In the day the same method runs on still bursts.

## 4d. Optical front end, reverse engineered (17 Nov)

Sources: 1 673 raw R20 payloads (2 128 B) of 10–12 Nov, R18 / R26 frames of the same seconds,
20 410 R26 windows of 9–17 Nov. The AFE read commands (40 LED drive, 42 TIA gain, 44 bias offset)
answer `UNSUPPORTED(3)` on fw 50.38.1 with or without a payload: everything below comes from the
frames, which carry their own configuration every second.

**R20 header** (payload offsets +8 = frame): `2f 14 81` = packet 47, layout 20, marker 0x81;
record index u32; timestamp u32; **u16 at frame 19 = sub-second phase of the first sample**
(1/32768 s, constant within a session: frames are exactly 1 s apart, so sample i of a frame is at
ts + sub/32768 + i/25); `04 00 19 00 00` (0x19 = 25 samples/s, 0x04 constant, unknown).

**Block = time slot, slot = photodiode.** Per block: count, then for slots A/B: LED u8 + drive
u16, photodiode u8 + range u32 + offset i16. The LEDs listed in a block fire together and both
photodiodes integrate them (block 4 slot B, "LED 4, drive 0", reads 0.59 µA against 0.01 µA of
ambient: it is LED 2's light from slot A).

| Field | Unit (evidence) |
|---|---|
| samples | 19-bit ADC codes: maximum exactly 524 287 (saturation, 0.06–0.3 % of green samples), negative when the offset exceeds the light |
| range | full-scale photocurrent in µA (32 or 16): codes × range / 2¹⁹ = µA |
| offset | DC cancellation current, 10 nA steps, applied from the next frame: on 12 Nov PD 4 saturated, the firmware set 800 / 1 600 and the next frame dropped by 132 000 codes = 8.06 µA for 800 |
| drive | LED current, 10 µA steps (3 350 = 33.5 mA, 12 750 = 127.5 mA) |

19-bit ADC with 16/32 µA ranges and ~128 mA LED drive match a Maxim MAX8614x-class front end
(inference, no part number in the data). Absolute photocurrent = codes × range / 2¹⁹ + offset × 0.01 µA.

| Block | LEDs (mA) | PD | Photocurrent | µA / mA | PI (absolute) | Role |
|---|---|---|---|---|---|---|
| 0 | LED 1 33.5 + LED 4 67 | 3 (A), 4 (B) | 23 / 17 µA | 0.23 / 0.17 | 0.76 / 0.70 % | **green** pulse (R26 = slot A) |
| 1 | LED 3 127.5, range 16 µA | 1, 2 | — | — | — | **red** (probable), SpO₂, never transmitted |
| 2 | LED 2 53 | 1, 2 | — | — | — | SpO₂ partner, never transmitted |
| 3 | none | 3, 4 | 0.01–0.02 µA | — | — | ambient + dark current |
| 4 | LED 2 2 | 3 (A), 1 (B) | 7.3 / 0.59 µA | 3.6 / 0.30 | 0.33 / 1.33 % | **infrared** (probable), contact sensing |

Wavelengths: LED 1 and 4 are green (strongly absorbed: 0.2 µA/mA; most pulsatile at the near
photodiode). LED 2 returns 16× more current per mA, has a third of the pulsatility and stays on at
2 mA all the time: the profile of an infrared LED used for skin contact (infrared penetrates and
returns the most light; a red LED returns less and pulses less than infrared at normal SpO₂). LED 3
only appears in the SpO₂ slot, driven hardest (127.5 mA) on the most sensitive range: the profile
of the red LED, which tissue absorbs more. Red vs infrared cannot be proven without LED 3 samples:
"probable" in the app. The deeper path to photodiode 1 gives LED 2 four times the pulsatility.

**R26** (76 B payload): time_word = the same sub-second phase as R20 (21 954 on 12 Nov);
motion f32; status u16 = bits 4–7 and 8–11 two codes fixed per burst (automatic gain choice, units
not resolvable from one R20 overlap), 0x1000 = saturated, 0x2000 = below zero, 0x4000 = near
saturation, bits 0–3 always 0; on-wrist u8; acceptance u8.

**R18 optical bytes**: 107 follows the absolute green photocurrent (r 0.70), 106 the PD 4 /
ambient level (r 0.5), 108–109 the logarithm of the pulse amplitude (r 0.60): compressed 1 Hz
summaries, superseded by R26 / R20. 74 follows the optical level (4 values), 77–80 constant
1/12/2/12, 83–103 always 0, 104 always 1, 105 unrelated to the optics. f32 at 113 (≈ −5, follows
heart rate loosely, r 0.62) is unresolved.

## 4e. Night optical windows (`night_optics.rs`, `NightOptics.swift`, test from 17 Nov)

Goal: find out whether the strap samples its SpO₂ slots (block 1 = LED 3 at 127.5 mA, block 2 = LED 2 at 53 mA, both on
photodiode 1) during sleep, and at what battery cost. Day captures (10 and 12 Nov, 1 673 s) always had them empty.

- **Trigger = the strap's own night mode.** R26 bursts repeat every 1 161 s (+15 s now and then) in its night
  configuration and every 1 240 s by day. Two consecutive gaps in 1 150–1 195 s, between 21 h and 10 h local: 98 % of
  the 2–6 h bursts from 9 to 17 Nov, none from 12 h to 22 h. No live stream is involved (a live stream may stop the
  night AFE switch, see STRAP_BEHAVIOR_AUDIT).
- **Windows.** 150 s of `TOGGLE_PERSISTENT_R20` (153) per burst period, so about 13 % of the night. The windows
  alternate: *aligned* (from 45 s before the predicted burst, does SpO₂ come with the 40 s burst?) and *between* (half a
  period later). The prediction is 1–2 periods ahead of the last synced burst, with an error ≤ 30 s.
- **Cost.** 150 R20 frames × 2 140 B ≈ 320 kB per window, about 6 MB per night in the strap's history, plus about 25 s
  of extra sync. The optical cost is probably zero if the AFE already runs these slots (they are configured in every
  frame).
- **Protocol.** 4 nights: with, without, with, without. Strap battery readings (≈ 1/h at night) give %/h per night; a
  rise ends the night (charging).
- **Safety.** The end of the open window is persisted, and an OFF is sent until acknowledged (5 tries, then closed).
  R20 frames more than 5 min after the last OFF trigger a new OFF. Nothing is switched on after the 4th night.
- **Report** (Indicateurs → Respiration et pouls → Fenêtres optiques de nuit). Per window: R20 seconds, SpO₂-slot
  seconds, burst overlap, level / pulsatility of green, red, infrared and contact channels, and the red/IR
  ratio of ratios (relative, uncalibrated). Per night: windows, minutes, SpO₂ slots present or not, battery %/h.

## 4b. Temperature (R18, 1 Hz)

Three thermometers per one-second R18 record, plus a battery reading in the strap events:

| Field | Offset | Scale | Worn, daytime | Night | Role (inferred) |
|---|---|---|---|---|---|
| `skin_temp` | 73, u16 | 0.01 °C | 29–34 °C | 31–33.6 °C | contact thermistor on the skin side |
| `aux_temp_1` | 69, i16 | 0.1 °C | 24–31 °C | 28–32 °C | outer side of the board: follows the air, fastest to heat |
| `aux_temp_2` | 71, i16 | 0.1 °C | 27–32 °C | 29–32 °C | inner side / battery: heats first when charging |
| event 63 u16@5 | — | 0.1 °C | — | — | battery temperature candidate (42.5 °C in the sauna, 37.3 °C charging) |

Worn normally heat flows from the wrist outwards: skin > aux 2 > aux 1 (aux 1 is 3–5 °C below the
skin). The decoder keeps aux values in 0–60 °C and skin in 5–45 °C (raw skin is also stored).

Observed episodes (11–15 Nov):

- **Dry sauna, 15 Nov 13:28–13:42**: aux 1 rises 2.3–2.4 °C/min from 28.6 to 45.4 °C (flat at 45.4
  for 3 min: possibly a ceiling), aux 2 to 42.8, skin to 40.8 °C (8 min ≥ 38 °C). aux 1 passes the
  skin 4 min after entry and stays above it (gradient inverted: the heat comes from the air). Heart
  rate 98 → 127 bpm with almost no movement. Exit: aux 1 falls 3.4 °C/min, back below the skin in 3 min.
- **Battery pack charging, 13 Nov 18:38–18:59** (CHARGING_ON … CHARGING_OFF, BATTERY_PACK_REMOVED):
  aux 2 leads (32.7 vs aux 1 29.6 °C one minute in), aux 1 reaches 43.1, skin 37.9 °C; heart rate
  stays at 53–60 bpm.

Sauna rule (`sport::heat`): minutes with aux 1 ≥ 38 °C, aux 1 ≥ skin + 1 °C and skin ≥ 34 °C, at least
5 such minutes (gaps ≤ 2 min), median heart rate over them ≥ awake rest + 20 bpm, and no charge /
battery-pack event within 10 min. Bounds: back from the first hot minute while aux 1 rises ≥ 1 °C/min
(entry), forward to the last minute before it falls ≥ 1 °C/min (exit). Checked on every recovered
R18 minute (9–15 Nov): the sauna is found, the charging spell is rejected by its heart rate alone. A
hot bath or a steam room gives the same picture and is reported as a sauna.

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
