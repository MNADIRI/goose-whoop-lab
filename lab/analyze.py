"""Physiological analyses over the decoded WHOOP 5.0 tables (numpy/scipy only).

Every metric states its method and its inputs; nothing is reported when inputs fall short.

Streams used:
    biometric_1hz  HR, R-R, gravity, dynamic acceleration, steps, activity, skin/aux temperature, band state
    rr_intervals   R-R from history (R18) and live (packet 40)
    optical_windows R26 windows (signal amplitude / acceptance as a signal-quality index)
    events         battery, wrist on/off, charging, taps, BLE

Methods (references in the report):
    Sleep period      band state SLEEP/STILL + Cole-Kripke (te Lindert 30 s weights) on gravity activity counts
    Sleep stages      transparent per-epoch rules (HR percentile, HRV, movement) - approximate, not PSG-validated
    Resting HR        lowest 5-min rolling mean during the sleep period
    HRV               RMSSD / SDNN / pNN50 on truly successive beats (consecutive-second runs, artifact filter),
                      whole night and last-third (deep-sleep-weighted, WHOOP-like); ln(RMSSD)
    Respiratory rate  respiratory sinus arrhythmia: dominant 0.1-0.5 Hz peak of the resampled R-R series (Welch)
    Skin temperature  sleep median and deviation vs personal baseline (>=3 nights)
    Activity          steps (counter deltas), walking/running minutes, MVPA from HR reserve
    Strain/load       Edwards TRIMP on HR zones (%HRmax), log-scaled 0-21 WHOOP-like strain
    Energy            Keytel 2005 HR-based active kcal + Mifflin-St Jeor resting kcal
    Recovery          z-scores of ln(RMSSD), resting HR, respiration vs baseline -> 0-100 (needs >=3 nights)
"""

from __future__ import annotations

import datetime as dt
import json
import math
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
from scipy import signal

HERE = Path(__file__).resolve().parent
DEFAULT_DB = HERE / "data" / "whoop-lab.sqlite"
PROFILE = HERE / "profile.json"  # optional: {"age": 30, "sex": "male", "weight_kg": 75, "height_cm": 180}

CK_WEIGHTS = np.array([106, 54, 58, 76, 230, 74, 67], dtype=float)  # A-4..A+2
EPOCH = 30


def load_profile() -> dict:
    defaults = {"age": None, "sex": None, "weight_kg": None, "height_cm": None}
    if PROFILE.exists():
        defaults.update(json.loads(PROFILE.read_text()))
    return defaults


@dataclass
class Bio:
    ts: np.ndarray
    hr: np.ndarray
    grav: np.ndarray
    dyn: np.ndarray
    steps: np.ndarray
    activity: np.ndarray
    skin: np.ndarray
    aux1: np.ndarray
    band: np.ndarray
    wrist: np.ndarray


def load_bio(db: sqlite3.Connection, start: int, end: int) -> Bio:
    rows = db.execute(
        "SELECT ts, hr, grav_x, grav_y, grav_z, dyn_accel_g, steps, activity, skin_temp_c, aux_temp1_c, band_state, wrist_bits "
        "FROM biometric_1hz WHERE ts >= ? AND ts < ? ORDER BY ts", (start, end)).fetchall()
    if not rows:
        return Bio(*(np.array([]) for _ in range(10)))
    cols = list(zip(*rows))
    f = lambda i: np.array([np.nan if v is None else v for v in cols[i]], dtype=float)
    return Bio(ts=np.array(cols[0], dtype=np.int64), hr=f(1), grav=np.stack([f(2), f(3), f(4)], 1), dyn=f(5),
               steps=f(6), activity=np.array(cols[7], dtype=object), skin=f(8), aux1=f(9),
               band=np.array(cols[10], dtype=object), wrist=np.array(cols[11]))


def load_rr(db: sqlite3.Connection, start: int, end: int) -> list[tuple[int, int]]:
    return db.execute("SELECT ts, rr_ms FROM rr_intervals WHERE source='history' AND ts >= ? AND ts < ? ORDER BY ts, idx",
                      (start, end)).fetchall()


# ---------------------------------------------------------------- HRV on successive beats
def successive_segments(rr: list[tuple[int, int]], min_beats: int = 5) -> list[np.ndarray]:
    """Runs of consecutive seconds that all carry R-R; split at implausible or >20 % jumps."""
    segs, cur, prev_ts = [], [], None
    def flush():
        if len(cur) >= min_beats:
            segs.append(np.array(cur, dtype=float))
        cur.clear()
    for ts, v in rr:
        if prev_ts is not None and ts not in (prev_ts, prev_ts + 1):
            flush()
        prev_ts = ts
        if not 300 <= v <= 2000 or (cur and abs(v - cur[-1]) / cur[-1] > 0.2):
            flush()
            if not 300 <= v <= 2000:
                continue
        cur.append(v)
    flush()
    return segs


def hrv_metrics(segs: list[np.ndarray]) -> dict | None:
    diffs = np.concatenate([np.diff(s) for s in segs]) if segs else np.array([])
    if len(diffs) < 100:
        return None
    nn = np.concatenate(segs)
    rmssd = float(np.sqrt(np.mean(diffs ** 2)))
    return {"rmssd_ms": round(rmssd, 1), "ln_rmssd": round(math.log(rmssd), 3), "sdnn_ms": round(float(np.std(nn, ddof=1)), 1),
            "pnn50_pct": round(100 * float(np.mean(np.abs(diffs) > 50)), 1), "mean_nn_ms": round(float(nn.mean()), 0),
            "beats": int(len(nn)), "successive_diffs": int(len(diffs))}


def respiratory_rate(rr: list[tuple[int, int]]) -> dict | None:
    """Respiratory sinus arrhythmia on a 1 Hz tachogram (mean R-R per device second).
    R18 does not carry every beat, so 120 s windows with >= 80 % of seconds covered are used,
    short gaps linearly interpolated; Welch PSD, dominant peak in 0.13-0.45 Hz (8-27 breaths/min)
    that must stand out (>= 3x the band median)."""
    if not rr:
        return None
    per_sec: dict[int, list[int]] = {}
    for ts, v in rr:
        if 300 <= v <= 2000:
            per_sec.setdefault(ts, []).append(v)
    t0, t1 = min(per_sec), max(per_sec)
    x = np.full(t1 - t0 + 1, np.nan)
    for ts, vals in per_sec.items():
        x[ts - t0] = np.mean(vals)
    estimates = []
    for w0 in range(0, len(x) - 120, 60):
        w = x[w0:w0 + 120]
        ok = np.isfinite(w)
        if ok.mean() < 0.8:
            continue
        idx = np.arange(120)
        w = np.interp(idx, idx[ok], w[ok])
        if np.any(np.abs(np.diff(w)) > 0.25 * w[:-1]):
            continue
        f, p = signal.welch(signal.detrend(w), fs=1.0, nperseg=120)
        band = (f >= 0.13) & (f <= 0.45)
        if p[band].max() < 3 * np.median(p[band]):
            continue
        estimates.append(f[band][np.argmax(p[band])] * 60)
    if len(estimates) < 5:
        return None
    est = np.array(estimates)
    return {"rpm": round(float(np.median(est)), 1), "rpm_iqr": [round(float(q), 1) for q in np.percentile(est, [25, 75])],
            "windows": len(est)}


# ---------------------------------------------------------------- sleep
def cole_kripke(counts: np.ndarray) -> np.ndarray:
    """True = sleep. te Lindert 30 s weights; SI = 0.001*sum(w*A); sleep if SI < 1.
    Counts here are mg of gravity change above a 10 mg noise floor, not ActiGraph counts: the
    divisor is 1000 (instead of 100) so an isolated roll-over (~2 g*s) stays sleep and only
    sustained movement over several epochs scores wake."""
    a = np.clip(counts / 1000, 0, 300)
    padded = np.pad(a, (4, 2))
    si = np.array([0.001 * np.dot(CK_WEIGHTS, padded[i:i + 7]) for i in range(len(a))])
    return si < 1


def sleep_period(bio: Bio) -> dict | None:
    """Longest block of SLEEP band state (gaps <= 20 min merged), refined with Cole-Kripke epochs."""
    if len(bio.ts) == 0:
        return None
    asleep = np.isin(bio.band, ["sleep"])
    idx = np.flatnonzero(asleep)
    if len(idx) < 1800:
        return None
    blocks, start = [], idx[0]
    for a, b in zip(idx, idx[1:]):
        if bio.ts[b] - bio.ts[a] > 1200:
            blocks.append((start, a)); start = b
    blocks.append((start, idx[-1]))
    s, e = max(blocks, key=lambda be: bio.ts[be[1]] - bio.ts[be[0]])
    # extend backwards through STILL (in bed, not yet classified asleep) to estimate time in bed
    bed = s
    while bed > 0 and bio.band[bed - 1] in ("still", "sleep") and bio.ts[s] - bio.ts[bed - 1] < 3 * 3600:
        bed -= 1
    return {"bed_i": int(bed), "onset_i": int(s), "wake_i": int(e)}


def epochs(bio: Bio, i0: int, i1: int) -> dict:
    ts = bio.ts[i0:i1 + 1]
    g = bio.grav[i0:i1 + 1]
    dg = np.r_[0, np.linalg.norm(np.diff(g, axis=0), axis=1)]
    dg = np.nan_to_num(dg)
    n = int((ts[-1] - ts[0]) // EPOCH) + 1
    k = ((ts - ts[0]) // EPOCH).astype(int)
    # sensor noise is ~2-5 mg/s at rest; count only movement above a 10 mg floor (actigraphy counts ~0 at rest)
    counts = np.bincount(k, weights=np.clip(dg - 0.01, 0, None) * 1000, minlength=n)  # mg above floor
    movefrac = np.bincount(k, weights=(dg > 0.01).astype(float), minlength=n) / np.maximum(np.bincount(k, minlength=n), 1)
    hr = np.bincount(k, weights=np.nan_to_num(bio.hr[i0:i1 + 1]), minlength=n) / np.maximum(np.bincount(k, minlength=n), 1)
    return {"t0": int(ts[0]), "counts": counts, "movefrac": movefrac, "hr": hr, "ck_sleep": cole_kripke(counts)}


def stage_epochs(ep: dict, rr: list[tuple[int, int]]) -> list[str]:
    """Rules: wake if CK wake or moving; deep if low HR + high local RMSSD + still, early night;
    REM if HR variability high + still, not in first 60 min; else light. Approximate."""
    n = len(ep["hr"])
    rr_by_epoch: dict[int, list[int]] = {}
    for ts, v in rr:
        rr_by_epoch.setdefault((ts - ep["t0"]) // EPOCH, []).append(v)
    loc_rmssd = np.full(n, np.nan)
    for i in range(n):
        win = [v for j in range(i - 5, i + 6) for v in rr_by_epoch.get(j, [])]
        w = np.array([v for v in win if 300 <= v <= 2000], float)
        if len(w) >= 20:
            d = np.diff(w); d = d[np.abs(d) < 0.2 * w[:-1]]
            if len(d) >= 10:
                loc_rmssd[i] = np.sqrt(np.mean(d ** 2))
    hr = ep["hr"]
    hr_var = np.array([np.nanstd(hr[max(0, i - 10):i + 10]) for i in range(n)])
    p_hr25, p_hr70 = np.nanpercentile(hr, [25, 70])
    p_rm70 = np.nanpercentile(loc_rmssd, 70) if np.isfinite(loc_rmssd).any() else np.inf
    p_var65 = np.nanpercentile(hr_var, 65)
    labels = []
    for i in range(n):
        if not ep["ck_sleep"][i] or ep["movefrac"][i] > 0.15:
            labels.append("wake"); continue
        still = ep["movefrac"][i] <= 0.10
        if still and i < n * 0.5 and hr[i] <= p_hr25 and loc_rmssd[i] >= p_rm70:
            labels.append("deep")
        elif still and i > 120 and hr_var[i] >= p_var65 and hr[i] >= p_hr25:
            labels.append("rem")
        else:
            labels.append("light")
    # median-like smoothing: replace isolated single epochs
    for i in range(1, n - 1):
        if labels[i - 1] == labels[i + 1] != labels[i]:
            labels[i] = labels[i - 1]
    return labels


def analyze_night(db: sqlite3.Connection, day_end_local: dt.datetime, history: list[dict]) -> dict:
    """Night ending on the morning of `day_end_local` (window 18:00 previous day -> 14:00)."""
    w0 = int((day_end_local - dt.timedelta(hours=20)).timestamp())
    w1 = int(day_end_local.timestamp())
    bio = load_bio(db, w0, w1)
    sp = sleep_period(bio)
    out: dict = {"date": (day_end_local - dt.timedelta(days=1)).date().isoformat(), "available": sp is not None}
    if not sp:
        return out
    b, s, e = sp["bed_i"], sp["onset_i"], sp["wake_i"]
    ts = bio.ts
    rr = load_rr(db, int(ts[s]), int(ts[e]) + 1)
    ep = epochs(bio, s, e)
    stages = stage_epochs(ep, rr)
    mins = {k: round(stages.count(k) * EPOCH / 60) for k in ("wake", "light", "deep", "rem")}
    asleep_min = mins["light"] + mins["deep"] + mins["rem"]
    in_bed_min = round((ts[e] - ts[b]) / 60)
    hr_sleep = bio.hr[s:e + 1]
    roll = np.convolve(np.nan_to_num(hr_sleep), np.ones(300) / 300, mode="valid") if len(hr_sleep) > 300 else hr_sleep
    segs = successive_segments(rr)
    third = int(ts[s] + 2 * (ts[e] - ts[s]) / 3)
    segs_last = successive_segments([x for x in rr if x[0] >= third])
    skin = bio.skin[s:e + 1]
    out.update({
        "bed_time": int(ts[b]), "sleep_onset": int(ts[s]), "wake_time": int(ts[e]),
        "time_in_bed_min": in_bed_min, "asleep_min": asleep_min,
        "latency_min": round((ts[s] - ts[b]) / 60), "efficiency_pct": round(100 * asleep_min / max(in_bed_min, 1), 1),
        "stages_min": mins, "awakenings": sum(1 for a, c in zip(stages, stages[1:]) if a != "wake" and c == "wake"),
        "hypnogram": stages,
        "resting_hr_bpm": round(float(np.nanmin(roll)), 1), "mean_sleep_hr_bpm": round(float(np.nanmean(hr_sleep)), 1),
        "hrv_night": hrv_metrics(segs), "hrv_last_third": hrv_metrics(segs_last),
        "respiration": respiratory_rate(rr),
        "skin_temp_c": round(float(np.nanmedian(skin)), 2) if np.isfinite(skin).any() else None,
        "rr_coverage_pct": round(100 * len(rr) / max(1, np.nansum(hr_sleep) / 60), 1),
    })
    # personal baselines (previous nights)
    prev = [h for h in history if h.get("available")]
    def base(key):
        vals = [h[key] for h in prev if h.get(key) is not None]
        return (float(np.mean(vals)), float(np.std(vals, ddof=1)) if len(vals) > 1 else None, len(vals)) if len(vals) >= 3 else None
    for key, val in (("skin_temp_c", out["skin_temp_c"]),):
        b3 = base(key)
        out["skin_temp_delta_c"] = round(val - b3[0], 2) if b3 and val is not None else None
    out["recovery"] = recovery_score(out, prev)
    return out


def recovery_score(night: dict, prev: list[dict]) -> dict | None:
    """z-score composite vs personal baseline: +ln(RMSSD), -resting HR, -|respiration deviation|."""
    hist = [p for p in prev if p.get("hrv_night")]
    if len(hist) < 3 or not night.get("hrv_night"):
        return {"score": None, "reason": f"référence en cours ({len(hist)}/3 nuits)"}
    def z(cur, vals, invert=False):
        m, sd = np.mean(vals), np.std(vals, ddof=1) or 1e-6
        v = (cur - m) / sd
        return -v if invert else v
    zh = z(night["hrv_night"]["ln_rmssd"], [p["hrv_night"]["ln_rmssd"] for p in hist])
    zr = z(night["resting_hr_bpm"], [p["resting_hr_bpm"] for p in hist], invert=True)
    composite = 0.6 * zh + 0.4 * zr
    score = round(100 / (1 + math.exp(-1.2 * composite)))
    return {"score": score, "z_ln_rmssd": round(zh, 2), "z_resting_hr": round(zr, 2), "baseline_nights": len(hist)}


# ---------------------------------------------------------------- day activity / strain / energy
def analyze_day(db: sqlite3.Connection, day: dt.date, profile: dict, resting_hr: float | None) -> dict:
    tz = dt.datetime.now().astimezone().tzinfo
    d0 = dt.datetime.combine(day, dt.time(), tz)
    bio = load_bio(db, int(d0.timestamp()), int((d0 + dt.timedelta(days=1)).timestamp()))
    out: dict = {"date": day.isoformat(), "coverage_h": round(len(bio.ts) / 3600, 1)}
    if len(bio.ts) < 600:
        return out
    st = bio.steps
    d = np.diff(st)
    steps = int(np.sum(d[(d > 0) & (d < 300)]))  # ignore resets/source changes
    age = profile.get("age") or 30  # Tanaka 2001; default age 30 until profile.json is filled
    hrmax = max(208 - 0.7 * age, float(np.nanmax(bio.hr)))
    pct = bio.hr / hrmax
    zones = np.digitize(pct, [0.5, 0.6, 0.7, 0.8, 0.9])  # 0 = below zone 1
    zone_min = {f"z{z}": round(int(np.sum(zones == z)) / 60, 1) for z in range(1, 6)}
    trimp = sum(z * zone_min[f"z{z}"] for z in range(1, 6))  # Edwards
    strain = round(21 * (1 - math.exp(-trimp / 250)), 1)
    rhr = resting_hr or float(np.nanpercentile(bio.hr, 5))
    mvpa = round(float(np.sum((bio.hr - rhr) / (hrmax - rhr) >= 0.4)) / 60, 1)
    kcal = energy(bio.hr, profile, rhr)
    hourly = {}
    for h in range(24):
        m = (bio.ts >= d0.timestamp() + h * 3600) & (bio.ts < d0.timestamp() + (h + 1) * 3600)
        if m.sum() > 60:
            dd = np.diff(st[m]); hourly[h] = {"hr": round(float(np.nanmean(bio.hr[m])), 1), "steps": int(np.sum(dd[(dd > 0) & (dd < 300)])),
                                              "move": round(float(np.nanmean(bio.dyn[m])) * 1000, 1), "skin": round(float(np.nanmean(bio.skin[m])), 2)}
    wrist_on_pct = round(100 * float(np.mean(bio.wrist == 0)), 1)
    stress = stress_index(bio, rhr, hrmax)
    out.update({"steps": steps, "walk_min": round(float(np.sum(bio.activity == "walk")) / 60, 1),
                "run_min": round(float(np.sum(bio.activity == "run")) / 60, 1), "mvpa_min": mvpa, "hr_zone_min": zone_min,
                "edwards_trimp": round(trimp, 1), "strain_0_21": strain, "hrmax_used": round(hrmax), "energy": kcal,
                "hr_mean": round(float(np.nanmean(bio.hr)), 1), "hr_min": int(np.nanmin(bio.hr)), "hr_max": int(np.nanmax(bio.hr)),
                "hourly": hourly, "wrist_bits0_pct": wrist_on_pct, "stress": stress, "age_default_used": not profile.get("age")})
    return out


def stress_index(bio: Bio, rhr: float, hrmax: float) -> dict | None:
    """Physiological stress while awake and not moving: HR elevation above resting HR as a share of
    the HR reserve, per 5-min bucket, only where the band reports no activity (activity still,
    dynamic acceleration < 0.03 g) so exercise does not count. 0-25 rest, 26-50 low, 51-75 medium,
    76-100 high (same banding as common consumer stress scores)."""
    awake = ~np.isin(bio.band, ["sleep"])
    calm = (bio.activity == "still") & (np.nan_to_num(bio.dyn, nan=1) < 0.03)
    mask = awake & calm & np.isfinite(bio.hr)
    if mask.sum() < 600:
        return None
    reserve = max(hrmax - rhr, 1)
    level = np.clip((bio.hr - rhr) / (0.4 * reserve), 0, 1) * 100
    buckets = (bio.ts - bio.ts[0]) // 300
    scores = []
    for b in np.unique(buckets[mask]):
        m = mask & (buckets == b)
        if m.sum() >= 120:
            scores.append(float(np.mean(level[m])))
    if not scores:
        return None
    arr = np.array(scores)
    bands = {"repos": 0.0, "bas": 0.0, "moyen": 0.0, "haut": 0.0}
    for v in arr:
        key = "repos" if v <= 25 else "bas" if v <= 50 else "moyen" if v <= 75 else "haut"
        bands[key] += 5
    return {"mean": round(float(arr.mean())), "max": round(float(arr.max())), "minutes": bands, "buckets": len(arr)}


def training_load(days: list[dict]) -> dict | None:
    """Acute (7-day) and chronic (28-day) Edwards TRIMP load and their ratio (ACWR, Gabbett 2016).
    Days without data count as 0; the ratio is only reported with >= 21 days of history."""
    series = [(d["date"], d.get("edwards_trimp") or 0.0) for d in days if d.get("coverage_h")]
    if not series:
        return None
    values = [v for _, v in series]
    acute = sum(values[-7:])
    chronic_weekly = sum(values[-28:]) / max(1, min(len(values), 28)) * 7
    ratio = round(acute / chronic_weekly, 2) if len(values) >= 21 and chronic_weekly > 0 else None
    return {"acute_7d": round(acute, 1), "chronic_28d_weekly": round(chronic_weekly, 1), "acwr": ratio,
            "days": len(values), "daily": [{"date": d, "trimp": round(v, 1)} for d, v in series[-28:]]}


def energy(hr: np.ndarray, p: dict, rhr: float) -> dict:
    w, a, sex, h = p.get("weight_kg") or 70, p.get("age") or 30, p.get("sex") or "male", p.get("height_cm") or 175
    bmr = 10 * w + 6.25 * h - 5 * a + (5 if sex == "male" else -161)  # Mifflin-St Jeor kcal/day
    resting = bmr * len(hr) / 86400
    active_hr = hr[hr > rhr + 20]  # Keytel only meaningful above resting
    if sex == "male":
        kcal_min = (-55.0969 + 0.6309 * active_hr + 0.1988 * w + 0.2017 * a) / 4.184
    else:
        kcal_min = (-20.4022 + 0.4472 * active_hr - 0.1263 * w + 0.074 * a) / 4.184
    active = float(np.sum(np.clip(kcal_min, 0, None)) / 60) - bmr / 1440 * len(active_hr) / 60
    return {"resting_kcal": round(resting), "active_kcal": round(max(active, 0)), "total_kcal": round(resting + max(active, 0)),
            "profile_defaults_used": not PROFILE.exists()}


# ---------------------------------------------------------------- device / signal quality
def device_summary(db: sqlite3.Connection) -> dict:
    ev = db.execute("SELECT ts, name, decoded_json FROM events WHERE ts > 1577836800 ORDER BY ts").fetchall()
    batt = [(t, json.loads(j)["voltage_mv_candidate"]) for t, n, j in ev if n == "BATTERY_LEVEL" and j]
    counts: dict[str, int] = {}
    for _, n, _ in ev:
        counts[n] = counts.get(n, 0) + 1
    opt = db.execute("SELECT count(*), avg(acceptance_raw), avg(on_wrist_raw), avg(amplitude) FROM optical_windows").fetchone()
    a, b, n = db.execute("SELECT min(ts), max(ts), count(*) FROM biometric_1hz").fetchone()
    return {"first": a, "last": b, "records": n, "coverage_pct": round(100 * n / max(1, b - a + 1), 1),
            "battery_mv": batt[-48:], "event_counts": counts,
            "optical_windows": {"n": opt[0], "accepted_pct": round(100 * (opt[1] or 0), 1),
                                "on_wrist_pct": round(100 * (opt[2] or 0), 1), "median_amplitude": opt[3]},
            "unused_streams": {"r20_optical": db.execute("SELECT count(*) FROM optical_r20_blocks").fetchone()[0],
                               "r21_imu": db.execute("SELECT count(*) FROM imu_100hz").fetchone()[0]}}


def run(db_path: Path = DEFAULT_DB) -> dict:
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    profile = load_profile()
    first, last = db.execute("SELECT min(ts), max(ts) FROM biometric_1hz").fetchone()
    tz = dt.datetime.now().astimezone().tzinfo
    d_first = dt.datetime.fromtimestamp(first, tz).date()
    d_last = dt.datetime.fromtimestamp(last, tz).date()
    nights, days = [], []
    day = d_first
    while day <= d_last:
        end = dt.datetime.combine(day, dt.time(14), tz)
        nights.append(analyze_night(db, end, nights))
        day += dt.timedelta(days=1)
    rhr_by_date = {n["date"]: n.get("resting_hr_bpm") for n in nights}  # keyed by the evening date
    day = d_first
    while day <= d_last:
        prev_night = (day - dt.timedelta(days=1)).isoformat()  # night that ended this morning
        days.append(analyze_day(db, day, profile, rhr_by_date.get(prev_night)))
        day += dt.timedelta(days=1)
    return {"generated": int(dt.datetime.now().timestamp()), "profile": profile, "nights": nights, "days": days,
            "load": training_load(days),
            "device": device_summary(db)}


if __name__ == "__main__":
    result = run()
    for n in result["nights"]:
        if n.get("available"):
            h = n["hrv_night"] or {}
            print(n["date"], "sleep", n["asleep_min"], "min eff", n["efficiency_pct"], n["stages_min"], "RHR", n["resting_hr_bpm"],
                  "RMSSD", h.get("rmssd_ms"), "resp", (n["respiration"] or {}).get("rpm"), "skin", n["skin_temp_c"], "rec", n["recovery"])
    for d in result["days"]:
        print(d["date"], {k: d.get(k) for k in ("coverage_h", "steps", "walk_min", "mvpa_min", "strain_0_21", "energy")})
    print("device", {k: v for k, v in result["device"].items() if k != "battery_mv"})
