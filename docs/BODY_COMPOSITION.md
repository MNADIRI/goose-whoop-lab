# Body composition

Occasional bioimpedance (BIA) measurements, stored locally next to the strap data, analysed as a
longitudinal dataset and joined with the continuous WHOOP physiology. Code: `Rust/core/src/body_comp.rs`
(engine), `GooseSwift/BodyComposition.swift` (store, HealthKit, coach), `GooseSwift/FormeBodyCompView.swift`
(Indicateurs card and detail screen). Fixture: `Rust/core/tests/fixtures/InBody-20261115.csv`.

## Data flow

```
InBody CSV (Files) ──┐
                     ├─> body_comp.import_* ─> body_comp_measurements (SQLite, goose.sqlite)
Santé (HealthKit) ───┘                                   │
                                                         ├─> body_comp.summary ─> Indicateurs > Composition
whoop_lab_days (daily strap analysis) ───────────────────┤                     └> coach tool get_body_composition
sport_sessions / sport_segments (training) ──────────────┘
body_comp_measurements ─> whoop_lab.analyze (day.energy: resting energy from measured fat-free mass)
```

## Import

- **InBody CSV**: header-driven (columns are matched by normalised name, so a reordered or longer
  export still works). Every cell is kept verbatim in `raw_json` (header, value) in file order;
  mapped columns go to `values_json` under canonical keys. `-` and empty cells are `null`, never 0.
  The date `YYYYMMDDHHMMSS` is wall-clock time: kept as `local_time` and converted with the phone's
  current UTC offset.
- **Id**: `inbody:<device>:<local_time>`. Re-importing the same row is a no-op; a row with the same id
  but different values (corrected export) replaces it and increments `revision`.
- **HealthKit**: body mass, body fat %, lean body mass (2 years). Samples within 10 min form one
  weighing (`healthkit:<uuids>`); a weighing within 10 min of an InBody CSV row is the same weighing
  (the InBody app writes to Health) and is skipped. Samples written by this app are ignored.
- **Other BIA devices**: add their header names to the `INBODY` table (or a sibling table) with the
  same canonical keys; nothing downstream depends on the device.
- Measurements can be excluded from trends (non-standard conditions) or deleted.

## Canonical fields

| Category | Keys |
|---|---|
| Anthropometric | `weight_kg`, `bmi`, `waist_cm`, `whr` |
| Composition | `smm_kg` (skeletal muscle), `slm_kg` (soft lean), `fat_mass_kg`, `pbf_pct`, `smi` |
| Segmental lean / fat | `lean_{right,left}_{arm,leg}_kg`, `lean_trunk_kg`, `lean_legs_kg`, `fat_*` |
| Hydration | `tbw_l`, `icw_l`, `ecw_l`, `ecw_ratio`, `ecw_ratio_{right_arm,…,left_leg}` |
| Metabolic | `bmr_device_kcal` |
| Physiological | `phase_angle_deg`, `bcm_kg`, `vfa_cm2` (estimated), `protein_kg`, `mineral_kg`, `bmc_kg` |
| Proprietary (kept, not analysed) | `inbody_score`, `inbody_age`, `visceral_fat_level`, `leg_muscle_level`, `balance_*` |

## Derived metrics (per measurement)

- Fat-free mass = weight − fat mass (HealthKit lean body mass when that is the source). Not the
  skeletal muscle mass, not the soft lean mass (which excludes bone mineral).
- FFMI = FFM / height², FMI = fat / height². Height: profile, else derived from the device BMI.
- Skeletal muscle / fat, skeletal muscle % of weight, left/right asymmetry (% of the pair mean).
- ECW/TBW, ICW/ECW, water / FFM (expected ≈ 0.73). Quality flags when ECW/TBW is outside the device
  reference 0.360–0.390 or water / FFM outside 0.69–0.77.
- BMR: device, Katch-McArdle (370 + 21.6 × FFM), Cunningham (500 + 22 × FFM), Mifflin-St Jeor
  (weight, height, age, sex). The InBody BMR is Katch-McArdle on the device's own FFM (15 Nov: 1894
  vs 1895 kcal), so agreement among the three FFM-based values is **not** a validation; only Mifflin
  is independent of the BIA.

## Longitudinal analysis

- **Measurement error**: assumed SEM per measurement (fat 0.6 kg, SMM 0.35 kg, FFM 0.5 kg, weight
  0.5 kg, PBF 0.8 pt, TBW 0.5 L; multi-frequency BIA test-retest plus day-to-day hydration). Minimal
  detectable change MDC95 = 1.96 × √2 × SEM. Pairwise changes below it are shown as "≈".
- **Previous comparable measurement**: same device/source, time of day within 2 h, ECW ratio within
  0.005; otherwise the issues are listed.
- **Recomposition** (≥ 3 measurements over ≥ 28 days): least-squares trend per variable, change over
  the span with a 95 % interval (noise = max(residual SD, SEM)); direction up/down only when the change
  exceeds the interval. Classes: muscle gain + fat loss, muscle gain + stable fat, fat loss with muscle
  preserved, possible muscle loss, mixed, indeterminate. A lean change mostly matched by water change
  (TBW / 0.73) is flagged as possibly hydration.
- **Segmental**: latest left/right comparison, change vs the first measurement, per-segment trends;
  an asymmetry is "persistent" only when ≥ 3 measurements show the same side larger by > 5 %.

## Strap integration

- **Context windows**: for each measurement, the 7/28/56/84 days strictly before it: sleep mean and
  SD, night RMSSD, resting HR, strain %, total / active kcal (days worn ≥ 20 h), sessions by type,
  training minutes, strength sessions, sets, reps, weekly strength consistency. Nothing after the
  measurement is used, and nothing is interpolated between measurements.
- **Adaptation**: muscle / fat / FFM change over 4/8/12 weeks (only when the measurements inside cover
  ≥ half the window), resistance volume (last 4 weeks vs the 4 before), per-interval change per week
  with its context, and Spearman associations across intervals (from 5 intervals; exploratory,
  observational, not causal). No composite score: with sparse BIA its error would exceed what it ranks.
- **Recovery**: the daily recovery score does not use body composition (no evidence of added
  predictive value); long-term adaptation and daily recovery stay separate.
- **Cardiometabolic trends**: observed points for PBF, fat mass, WHR, VFA, and monthly resting HR and
  RMSSD. Not a cardiovascular risk score.

## Energy expenditure (whoop_lab.analyze, ALGORITHM_VERSION 14)

Per day: resting = BMR × elapsed share of the day; active = Keytel 2005 above resting + 20 bpm minus
the resting share of those minutes (no double counting). BMR = Katch-McArdle on the FFM of the latest
usable measurement taken **before the end of that day** and at most 120 days old; otherwise
Mifflin-St Jeor on the profile (as before). The measurement's weight also feeds Keytel. Each day's
cache key includes the measurement used, so a new measurement recomputes the days after it and leaves
earlier days unchanged. `day.energy` reports `bmr_kcal`, `bmr_model`, `body_comp_id`, `weight_kg_used`.

## Needs more measurements

Trend classes, horizons, persistent asymmetry, associations and comparability-based deltas all need
repeated measurements (3+ over 28+ days for trends, 5+ intervals for associations). Best practice for
comparable BIA: same device, same time of day, fasted, bladder empty, no exercise or sauna beforehand.
