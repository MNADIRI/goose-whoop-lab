# Sport Intelligence Engine

Rust: `Rust/core/src/sport.rs` (bridge `sport.*`) · Swift: `GooseSwift/Sport.swift` (controller + Sport tab) ·
offline: `lab/sport/` · sensor facts: docs/SENSORS.md.

## 1. Inputs and acquisition (tiered)

| Tier | Stream | Rate | When | Role |
|---|---|---|---|---|
| 1 | R18 history (HR, steps, firmware still/walk/run, cadence, dynamic accel, gravity) | 1 Hz | always (strap memory), reaches the phone at each sync | detection, pre-activity buffer, backdating |
| 2 | packet 40 live HR + R-R | 1 Hz | `watch` → end of post-buffer | real-time HR (no sync lag) |
| 2 | R21 live IMU (accel ±8 g, gyro ±2000 °/s) | 100 Hz | `watch` → end of post-buffer | strokes, sets, reps, phases, exercise signature |

- Sync interval: 300 s at rest, **120 s** while `watch`/`active`/`ending` (R18 lag ≤ 2 min).
- Streams: `3 [1]` + `106 [1,1]` on; `106 [1,0]` then `3 [0]` off. Never touched while the smart wake is armed.
- Stored as received: `sport_live` (ts, hr, rr, arrival), `sport_imu` (record ts = u32 + u16/32768, arrival, full
  CRC-checked frame). Strap clock for every event; phone arrival kept for latency studies. Raw frames also go to the
  whoop-lab raw export as before.
- Buffers: pre 120 s (R18 always exists; IMU only from `watch`), post 180 s (streams stay on through `ending`).

## 2. Online detection (`sport.update`, every 30 s and after each sync)

Per-minute features → minute score (logistic on %HR reserve + motion, `minute_score`):
- %HRR = (HR − rest)/(HRmax − rest); rest = p5 of sleep HR (7 d), HRmax = max(Tanaka, observed p99.98, 30 d).
- motion = max(steps, R18 dynamic accel, IMU dynamic accel, gravity-vector change).
- ≥ 120 steps/min or firmware "run" → active.

Heart rate + motion only open the **watch** (IMU on). A session needs the **signature of a sport** (`signature`), because
an active life is full of short efforts (stairs, running to the kitchen / the office) — 11 Nov 16:53 was one:
- running: ≥ 3 of the last 5 minutes at ≥ 140 steps/min or firmware "run";
- tennis: ≥ 3 isolated strokes in the last 5 minutes (IMU);
- strength: ≥ 2 periodic sets of ≥ 4 reps in the last 10 minutes (IMU);
- sustained: ≥ 8 of the last 10 minutes at ≥ 50 % HRR (other sports, days without IMU).
Replay 10–12 Nov (R18 only): 24 watch episodes, 23 dropped, 1 session = the tennis match (11:57–13:35, tennis 0.90).

State machine: `idle → watch` (any minute > 0.35 in the last 3) · `watch → active` (≥ 3 active of 6 **and** a sport
signature; start backdated to the first active minute, gaps ≤ 2 min, minus pre-buffer) · `watch → idle` after 8 quiet
minutes or 20 min without signature (logged with the signature values as a negative example) · `active → ending`
after 6 min quiet (rests between sets / games) · `→ ended` after 10 min quiet (end = last active minute + post-buffer) ·
sessions shorter than 8 min are discarded (stairs, a sprint to the bus). Manual start/stop overrides everything and is a
label.

## 3. Analysis (`sport.analyze`, at the end of a session and after a type correction)

- **Strokes (tennis)**: isolated swing — gyro peak > 600 °/s, acceleration > 3 g within 150 ms, no rival peak within
  ±0.5 s (rejects shaking and strap taps: 0 false strokes on the sensor session), ≥ 0.8 s apart; exchanges = strokes < 25 s apart.
- **Sets / rests (strength)**: 25 Hz energy (gyro + 150 × dynamic accel), threshold 2.5 × p20 (≥ 30), gaps < 3 s merged,
  sets 8–180 s, rest = gap between sets.
- **Repetitions**: most periodic of {vertical dynamic accel, PCA-1 accel, PCA-1 gyro}; period = first autocorrelation peak
  ≥ 85 % of the best in 0.8–8 s; periodic if r ≥ 0.4 and ≥ 2.5 periods; reps = peak count (distance ≥ 0.6 T) or
  duration / T when they disagree by > 30 %. Confidence = r.
- **Phases**: vertical velocity (integrated, high-passed over one period) → up / down / hold time per repetition;
  concentric = up by default, switchable per set ("concentrique en descente": pull-downs…).
- **Exercise**: per-set signature (wrist orientation vs gravity, movement axis vs gravity, tempo, rotation share) →
  nearest human-labelled set (confidence exp(−2d), shown when ≥ 0.5). No guess before the first labelled example.
- **Session type**: feature vector `FEATURES` (16 values) → active personal model if installed, else `rules-v1`.
  Probabilities and the model version are stored with the session.

## 4. Human validation (Sport tab)

- Live card: current session, live guess, HR, reps/strokes of the last 10 min, sensors 1 Hz / 100 Hz, stop button;
  idle: auto-detection switch + one-tap manual start (Course / Tennis / Musculation / Autre).
- **À vérifier**: finished unlabelled sessions, **least confident first**; one tap confirms ("Oui, tennis"), one tap
  corrects (other type), one tap rejects ("Pas une séance").
- Session detail: HR curve with sets/exchanges shaded, probabilities, per-set exercise menu (+ new exercise), reps − / +,
  ✓ / ✕, concentric direction.

## 5. Personal dataset and learning

Tables: `sport_sessions` (prediction, probabilities, features, engine + model version, label, label status),
`sport_segments` (sets / exchanges with predictions and labels; labelled segments are never re-derived),
`sport_annotations` (append-only: old / new / predicted value, confidence, model version, source),
`sport_models` (versioned, one active), `sport_acquisition` (stream on/off log, incl. unconfirmed `watch` = negatives).

Export after every session (and on demand) to `whoop-lab/sport/` (iCloud folder) or `Documents/sport_dataset`:
`sessions.jsonl`, `segments.jsonl`, `annotations.jsonl`, `models.jsonl`, `acquisition.jsonl`, `README.json`, and per
session `sessions/<id>/{imu.csv, imu_frames.jsonl, r18.csv, live_hr.csv}` with buffers (written once, immutable).

Learning loop: `lab/sport/train.py <dataset>` → leave-one-session-out vs rules-v1 → `models/<version>.json`; the app
installs the newest file at its next export (`sport.install_model`), later predictions carry that version.

## 6. HARNet (OxWearables ssl-wearables) and model choices

- HARNet-10: ResNet pretrained (self-supervised) on 700 000 person-days of UK Biobank wrist accelerometry; input
  3 × 300 (10 s at 30 Hz, accel in g); open weights. Best candidate for window embeddings with few personal labels.
  Accel only: gyro features are added alongside (tennis and strength depend on rotation).
- `lab/sport/harnet_eval.py` compares HARNet embeddings, handcrafted accel+gyro features and both, leave-one-group-out,
  plus parameters and CPU time per window, on the sensor session (scripted gestures) or on the labelled dataset.
  Run with `--repo <local clone of OxWearables/ssl-wearables>` (weights are in the repository).
- First run (12 Nov, sensor session, 15 windows of scripted gestures — plumbing check, not a benchmark): 131 weight
  tensors loaded, 10.5 M parameters, 1 024-d embedding, 5.4 ms / window on a Mac CPU; leave-one-step-out 1-NN:
  handcrafted 0.73, HARNet 0.67, HARNet + gyro 0.67 (3 of 5 classes have a single window, so they can never be right).
  The real comparison needs ~10 validated sessions per sport.
- Licence: Oxford academic, non-commercial use only (personal research is fine; no redistribution or product use).
- Why not now on the phone: no labels yet; rules-v1 + personal logistic model need none of the 10 M-parameter network.
  Planned once ≥ 10 labelled sessions per class: HARNet embeddings → personal head, converted to Core ML, run only on
  `active` windows.
- Strength exercise models in the literature (CNN on wrist IMU, ~99 % on known exercise sets) all need per-exercise
  training data — the labelled sets collected here are exactly that.
