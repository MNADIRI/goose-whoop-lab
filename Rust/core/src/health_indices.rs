//! Night signal features and composite health indices built only from WHOOP 5.0 data we receive
//! (R18 1 Hz: HR, R-R, gravity, dynamic accel, band state, skin temperature, motion flags; on-strap
//! SpO2 windows; daily load). References per function. Everything returns explicit JSON with the
//! components used; missing inputs give `null`, never 0.

use serde_json::{json, Value};

use crate::dsp::{detrend_linear, mean, median, percentile, periodogram, round, std_sample};
use crate::hrv_qc::{self, Beat};

/// One second of input, decoupled from the analysis' private types.
#[derive(Debug, Clone, Default)]
pub struct S1 {
    pub ts: i64,
    pub hr: f64,
    pub rr: Vec<f64>,
    pub grav: Option<[f64; 3]>,
    pub dyn_g: Option<f64>,
    pub skin: Option<f64>,
    pub sleep: bool,
    pub still_band: bool,
    pub steps: i64,
    /// strap motion flags (cardiac_flags bits 0/4/6)
    pub motion_flag: bool,
}

// ------------------------------------------------------------------ 1 Hz tachogram

/// Mean valid R-R per second (NaN when absent) from t0.
pub fn tachogram(s: &[S1]) -> (i64, Vec<f64>) {
    let Some(first) = s.first() else { return (0, vec![]) };
    let t0 = first.ts;
    let n = (s.last().unwrap().ts - t0 + 1).max(0) as usize;
    let mut x = vec![f64::NAN; n];
    for p in s {
        let v: Vec<f64> = p.rr.iter().copied().filter(|r| (300.0..=2000.0).contains(r)).collect();
        if let (Some(m), true) = (mean(&v), !p.motion_flag) {
            x[(p.ts - t0) as usize] = m;
        }
    }
    (t0, x)
}

/// Linear gap filling of a window when >= `min_cov` is present and no >25 % jump remains.
fn filled(w: &[f64], min_cov: f64) -> Option<Vec<f64>> {
    let known: Vec<(usize, f64)> = w.iter().copied().enumerate().filter(|(_, v)| v.is_finite()).collect();
    if (known.len() as f64) < min_cov * w.len() as f64 || known.len() < 2 {
        return None;
    }
    let mut out = vec![0.0; w.len()];
    let mut j = 0;
    for (i, o) in out.iter_mut().enumerate() {
        while j + 1 < known.len() && known[j + 1].0 <= i {
            j += 1;
        }
        *o = if i <= known[0].0 {
            known[0].1
        } else if j + 1 >= known.len() {
            known[known.len() - 1].1
        } else {
            let (x0, y0) = known[j];
            let (x1, y1) = known[j + 1];
            y0 + (y1 - y0) * (i - x0) as f64 / (x1 - x0) as f64
        };
    }
    if out.windows(2).any(|p| (p[1] - p[0]).abs() > 0.25 * p[0]) {
        return None;
    }
    Some(out)
}

fn band(power: &[f64], n: usize, lo: f64, hi: f64) -> f64 {
    power.iter().enumerate().filter(|(k, _)| { let f = *k as f64 / n as f64; f >= lo && f < hi }).map(|(_, p)| p).sum()
}

// ------------------------------------------------------------------ spectral HRV (Task Force 1996)

#[derive(Debug, Clone, Copy)]
pub struct SpecWin {
    pub t: i64,
    pub lf: f64,
    pub hf: f64,
    pub vlf: f64,
}

impl SpecWin {
    pub fn lf_hf(&self) -> f64 { self.lf / self.hf.max(1e-9) }
    pub fn hf_nu(&self) -> f64 { 100.0 * self.hf / (self.lf + self.hf).max(1e-9) }
}

/// 5-min windows (step 150 s) on the 1 Hz tachogram, >= 80 % coverage: VLF 0.0033-0.04,
/// LF 0.04-0.15, HF 0.15-0.4 Hz (ms^2, Hann periodogram). 1 Hz resolves HF up to the 0.5 Hz Nyquist.
pub fn spectral(s: &[S1], window: usize, step: usize) -> Vec<SpecWin> {
    let (t0, x) = tachogram(s);
    let mut out = Vec::new();
    let mut i = 0;
    while i + window <= x.len() {
        if let Some(mut w) = filled(&x[i..i + window], 0.8) {
            detrend_linear(&mut w);
            let p = periodogram(&w);
            // one-sided PSD of a Hann window (U = 0.375) integrated over bins of df = 1/N Hz
            let norm = 2.0 / ((window * window) as f64 * 0.375);
            out.push(SpecWin {
                t: t0 + i as i64,
                vlf: band(&p, window, 0.0033, 0.04) * norm,
                lf: band(&p, window, 0.04, 0.15) * norm,
                hf: band(&p, window, 0.15, 0.40) * norm,
            });
        }
        i += step;
    }
    out
}

pub fn spectral_summary(w: &[SpecWin]) -> Value {
    if w.is_empty() {
        return Value::Null;
    }
    let lf: Vec<f64> = w.iter().map(|x| x.lf).collect();
    let hf: Vec<f64> = w.iter().map(|x| x.hf).collect();
    let r: Vec<f64> = w.iter().map(SpecWin::lf_hf).collect();
    let nu: Vec<f64> = w.iter().map(SpecWin::hf_nu).collect();
    json!({
        "windows": w.len(),
        "lf_ms2": round(median(&lf).unwrap(), 0), "hf_ms2": round(median(&hf).unwrap(), 0),
        "ln_hf": round(median(&hf).unwrap().max(1.0).ln(), 2),
        "lf_hf": round(median(&r).unwrap(), 2), "hf_nu": round(median(&nu).unwrap(), 1),
        "series": w.iter().map(|x| json!([x.t, round(x.lf, 0), round(x.hf, 0)])).collect::<Vec<_>>(),
        "method": "Task Force 1996 bands on a 1 Hz R-R tachogram, 5-min windows",
    })
}

// ------------------------------------------------------------------ cardiopulmonary coupling

/// RR-only proxy of Thomas et al. 2005 CPC: without an independent respiration signal, coupling is
/// read from the tachogram spectrum of 512-s windows (step 256 s):
///   stable   (high-frequency coupling, NREM N2/N3): HF / (LF + VLF) >= 0.6 and >= 20 % of HF (calibrated on real 1 Hz nights: p90 ~0.6-0.7)
///            power in the dominant peak +- 1 bin (narrow respiratory peak);
///   unstable (low-frequency coupling, cyclic arousals / light N1-N2): LF / HF >= 2;
///   vlf      (REM / wake-like): VLF >= 50 % of total;
///   mixed    otherwise.
pub fn cpc(s: &[S1]) -> Vec<(i64, &'static str)> {
    let (t0, x) = tachogram(s);
    let n = 512;
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= x.len() {
        if let Some(mut w) = filled(&x[i..i + n], 0.7) {
            detrend_linear(&mut w);
            let p = periodogram(&w);
            let (vlf, lf, hf) = (band(&p, n, 0.0039, 0.01) + band(&p, n, 0.01, 0.04), band(&p, n, 0.04, 0.1), band(&p, n, 0.1, 0.4));
            let lf_wide = band(&p, n, 0.01, 0.1);
            let hf_bins: Vec<(usize, f64)> = p.iter().copied().enumerate().filter(|(k, _)| { let f = *k as f64 / n as f64; (0.1..0.4).contains(&f) }).collect();
            let peak = hf_bins.iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|(k, _)| *k).unwrap_or(0);
            let conc = hf_bins.iter().filter(|(k, _)| k.abs_diff(peak) <= 1).map(|(_, v)| v).sum::<f64>() / hf.max(1e-12);
            let total = vlf + lf + hf;
            let class = if hf / (lf_wide + band(&p, n, 0.0039, 0.01)).max(1e-12) >= 0.6 && conc >= 0.2 {
                "stable"
            } else if vlf / total.max(1e-12) >= 0.5 {
                "vlf"
            } else if lf / hf.max(1e-12) >= 2.0 {
                "unstable"
            } else {
                "mixed"
            };
            out.push((t0 + i as i64 + n as i64 / 2, class));
        }
        i += 256;
    }
    out
}

pub fn cpc_summary(c: &[(i64, &str)]) -> Value {
    if c.is_empty() {
        return Value::Null;
    }
    let share = |k: &str| round(100.0 * c.iter().filter(|x| x.1 == k).count() as f64 / c.len() as f64, 1);
    json!({
        "windows": c.len(),
        "stable_pct": share("stable"), "unstable_pct": share("unstable"), "vlf_pct": share("vlf"), "mixed_pct": share("mixed"),
        "series": c.iter().map(|(t, k)| json!([t, k])).collect::<Vec<_>>(),
        "method": "RR-only proxy of cardiopulmonary coupling (Thomas 2005): spectral concentration of the 1 Hz tachogram, 512-s windows",
    })
}

// ------------------------------------------------------------------ breathing regularity

/// Respiratory rate per 2-min window (RSA peak 0.13-0.45 Hz >= 3x band median) and its stability:
/// regularity = peak share of band power; variability = CV and successive differences of the rates.
pub fn breathing(s: &[S1]) -> (Vec<(i64, f64, f64)>, Value) {
    let (t0, x) = tachogram(s);
    let n = 120;
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= x.len() {
        if let Some(mut w) = filled(&x[i..i + n], 0.8) {
            detrend_linear(&mut w);
            let p = periodogram(&w);
            let bins: Vec<(usize, f64)> = p.iter().copied().enumerate().filter(|(k, _)| { let f = *k as f64 / n as f64; (0.13..=0.45).contains(&f) }).collect();
            let vals: Vec<f64> = bins.iter().map(|b| b.1).collect();
            if let (Some(med), Some(&(k, pk))) = (median(&vals), bins.iter().max_by(|a, b| a.1.total_cmp(&b.1))) {
                if pk >= 3.0 * med {
                    out.push((t0 + i as i64 + 60, k as f64 / n as f64 * 60.0, pk / vals.iter().sum::<f64>().max(1e-12)));
                }
            }
        }
        i += 60;
    }
    if out.len() < 5 {
        return (out, Value::Null);
    }
    let r: Vec<f64> = out.iter().map(|x| x.1).collect();
    let sd = std_sample(&r).unwrap_or(0.0);
    let m = mean(&r).unwrap();
    let succ: Vec<f64> = r.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let summary = json!({
        "windows": out.len(),
        "rate_cv_pct": round(100.0 * sd / m, 1),
        "rate_sd_rpm": round(sd, 2),
        "mean_abs_change_rpm": round(mean(&succ).unwrap_or(0.0), 2),
        "peak_share_median": round(median(&out.iter().map(|x| x.2).collect::<Vec<_>>()).unwrap(), 3),
        "irregular_windows_pct": round(100.0 * r.iter().filter(|v| (**v - median(&r).unwrap()).abs() > 3.0).count() as f64 / r.len() as f64, 1),
    });
    (out, summary)
}

// ------------------------------------------------------------------ nocturnal disturbances

/// Autonomic activations: HR rises >= 8 bpm above the median of the previous 30 s within 10 s,
/// 60 s refractory (cardiac arousal index, cf. Azarbarzin 2014 heart-rate response to arousals).
pub fn hr_surges(s: &[S1]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    let hr: Vec<f64> = s.iter().map(|x| x.hr).collect();
    for i in 30..s.len() {
        if out.last().is_some_and(|t| s[i].ts - t < 60) || hr[i] <= 0.0 {
            continue;
        }
        let base = median(&hr[i - 30..i - 10].iter().copied().filter(|v| *v > 0.0).collect::<Vec<_>>());
        let peak = hr[i - 10..=i].iter().copied().fold(0.0, f64::max);
        if let Some(b) = base {
            if peak - b >= 8.0 && s[i].sleep {
                out.push(s[i].ts);
            }
        }
    }
    out
}

/// Night respiratory-disturbance summary: on-strap SpO2 desaturation windows (>= 3 points below
/// the night's p90, i.e. the ODI-3 criterion applied to spot windows), HR surges per hour of sleep,
/// breathing irregularity. A screening indicator, not an AHI.
pub fn respiratory_disturbance(spo2_series: &[(i64, f64)], surges: usize, sleep_hours: f64, breathing: &Value) -> Value {
    let meds: Vec<f64> = spo2_series.iter().map(|x| x.1).collect();
    let base = percentile(&meds, 90.0);
    let desat: Vec<f64> = base.map(|b| meds.iter().copied().filter(|v| *v <= b - 3.0).collect()).unwrap_or_default();
    let surge_h = surges as f64 / sleep_hours.max(0.5);
    let desat_pct = (!meds.is_empty()).then(|| 100.0 * desat.len() as f64 / meds.len() as f64);
    let irregular = breathing["irregular_windows_pct"].as_f64();
    // composite 0-100: each component scaled to its own "clearly abnormal" level
    let parts: Vec<f64> = [desat_pct.map(|v| (v / 30.0).min(1.0)), Some((surge_h / 15.0).min(1.0)), irregular.map(|v| (v / 40.0).min(1.0))].into_iter().flatten().collect();
    json!({
        "index_0_100": (!parts.is_empty()).then(|| round(100.0 * parts.iter().sum::<f64>() / parts.len() as f64, 0)),
        "desat_windows": desat.len(), "spo2_windows": meds.len(), "desat_windows_pct": desat_pct.map(|v| round(v, 1)),
        "spo2_baseline_p90": base, "spo2_nadir_window": meds.iter().copied().reduce(f64::min),
        "hr_surges": surges, "hr_surges_per_h": round(surge_h, 1),
        "breathing_irregular_pct": irregular,
        "method": "ODI-3 on on-strap SpO2 windows + HR surges (>= 8 bpm) + breathing irregularity",
    })
}

// ------------------------------------------------------------------ movement

/// A second is "moving" when dynamic accel > 0.05 g, the strap motion flags are set, or gravity
/// moved > 30 mg. Episodes merge gaps <= 5 s. Posture change = >= 30 deg between consecutive 1-min
/// mean gravity vectors.
pub fn movement(s: &[S1]) -> Value {
    if s.len() < 600 {
        return Value::Null;
    }
    let mut moving = vec![false; s.len()];
    for i in 1..s.len() {
        let dg = match (s[i - 1].grav, s[i].grav) {
            (Some(a), Some(b)) => ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt(),
            _ => 0.0,
        };
        moving[i] = s[i].dyn_g.is_some_and(|d| d > 0.05) || s[i].motion_flag || dg > 0.03;
    }
    let mut episodes = 0;
    let mut last_move: Option<i64> = None;
    let mut long = 0;
    let mut cur_start: Option<i64> = None;
    for (i, m) in moving.iter().enumerate() {
        if *m {
            if last_move.is_none_or(|t| s[i].ts - t > 5) {
                episodes += 1;
                if let (Some(st), Some(lm)) = (cur_start, last_move) {
                    if lm - st >= 30 { long += 1; }
                }
                cur_start = Some(s[i].ts);
            }
            last_move = Some(s[i].ts);
        }
    }
    // posture changes
    let mut minutes: Vec<[f64; 3]> = Vec::new();
    for c in s.chunks(60) {
        let g: Vec<[f64; 3]> = c.iter().filter_map(|x| x.grav).collect();
        if g.len() >= 30 {
            minutes.push([0, 1, 2].map(|a| g.iter().map(|v| v[a]).sum::<f64>() / g.len() as f64));
        }
    }
    let angle = |a: &[f64; 3], b: &[f64; 3]| {
        let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let na = (a.iter().map(|v| v * v).sum::<f64>()).sqrt();
        let nb = (b.iter().map(|v| v * v).sum::<f64>()).sqrt();
        (d / (na * nb).max(1e-9)).clamp(-1.0, 1.0).acos().to_degrees()
    };
    let posture = minutes.windows(2).filter(|w| angle(&w[0], &w[1]) >= 30.0).count();
    let hours = (s.last().unwrap().ts - s[0].ts) as f64 / 3600.0;
    json!({
        "moving_pct": round(100.0 * moving.iter().filter(|m| **m).count() as f64 / s.len() as f64, 1),
        "episodes": episodes, "episodes_per_h": round(episodes as f64 / hours.max(0.1), 1),
        "long_episodes": long,
        "posture_changes": posture, "posture_changes_per_h": round(posture as f64 / hours.max(0.1), 1),
        "method": "dynamic accel > 0.05 g, strap motion flags or gravity change > 30 mg; posture = 30 deg between 1-min gravity means",
    })
}

/// Sedentary behaviour while awake: bouts of >= 30 min without steps and without movement
/// (interruptions <= 60 s tolerated), breaks per sedentary hour.
pub fn sedentary(s: &[S1]) -> Value {
    // 1-min resolution: a minute is active with >= 10 steps or mean dynamic accel > 0.15 g;
    // a sedentary bout = >= 30 consecutive inactive awake minutes (one active minute tolerated)
    let mut minutes: std::collections::BTreeMap<i64, (i64, i64, f64, usize, bool)> = Default::default();
    for x in s {
        let e = minutes.entry(x.ts / 60).or_insert((x.steps, x.steps, 0.0, 0, true));
        e.0 = e.0.min(x.steps);
        e.1 = e.1.max(x.steps);
        e.2 += x.dyn_g.unwrap_or(0.0);
        e.3 += 1;
        e.4 &= !x.sleep;
    }
    let awake: Vec<(i64, bool)> = minutes.iter().filter(|(_, v)| v.4 && v.3 >= 30).map(|(m, v)| (*m, (v.1 - v.0) >= 10 || v.2 / v.3 as f64 > 0.15)).collect();
    if awake.len() < 60 {
        return Value::Null;
    }
    let mut bouts: Vec<i64> = Vec::new();
    let (mut run, mut tolerated, mut prev): (i64, bool, Option<i64>) = (0, false, None);
    let close = |run: &mut i64, tolerated: &mut bool, bouts: &mut Vec<i64>| { if *run >= 30 { bouts.push(*run); } *run = 0; *tolerated = false; };
    for (m, active) in &awake {
        if prev.is_some_and(|p| m - p > 2) { close(&mut run, &mut tolerated, &mut bouts); }
        prev = Some(*m);
        if !active { run += 1; }
        else if !tolerated && run > 0 { tolerated = true; run += 1; }
        else { close(&mut run, &mut tolerated, &mut bouts); }
    }
    close(&mut run, &mut tolerated, &mut bouts);
    json!({
        "sedentary_bouts_30min": bouts.len(),
        "sedentary_in_bouts_min": bouts.iter().sum::<i64>(),
        "longest_bout_min": bouts.iter().max(),
        "active_minutes": awake.iter().filter(|a| a.1).count(),
        "awake_hours": round(awake.len() as f64 / 60.0, 1),
    })
}

// ------------------------------------------------------------------ morning HRV, skin temperature

/// HRV in the last 10 min of sleep before the final wake (still, motion flags removed): a
/// standardised "on waking" measurement, more comparable day to day than the whole-night mean.
pub fn morning_hrv(s: &[S1], wake_ts: i64) -> Value {
    let win: Vec<&S1> = s.iter().filter(|x| x.ts > wake_ts - 600 && x.ts <= wake_ts && !x.motion_flag).collect();
    let mut runs: Vec<Vec<Beat>> = Vec::new();
    let mut prev: Option<i64> = None;
    for x in win {
        if x.rr.is_empty() { continue; }
        if prev.is_none_or(|p| x.ts != p + 1 && x.ts != p) { runs.push(Vec::new()); }
        prev = Some(x.ts);
        runs.last_mut().unwrap().extend(x.rr.iter().map(|v| Beat { ts: x.ts, rr_ms: *v }));
    }
    let (segs, qc) = hrv_qc::clean(&runs);
    let d: Vec<f64> = segs.iter().flat_map(|s| s.windows(2).map(|p| p[1].rr_ms - p[0].rr_ms)).collect();
    if d.len() < 60 {
        return json!({"available": false, "successive_diffs": d.len()});
    }
    let rmssd = (d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64).sqrt();
    let nn: Vec<f64> = segs.iter().flatten().map(|b| b.rr_ms).collect();
    json!({"available": true, "rmssd_ms": round(rmssd, 1), "ln_rmssd": round(rmssd.ln(), 3),
           "hr_bpm": round(60_000.0 / mean(&nn).unwrap(), 1), "successive_diffs": d.len(), "artifact_pct": qc.artifact_pct,
           "window": "last 10 min of sleep before the final wake"})
}

/// Distal (wrist) skin warming at sleep onset: mean 20-40 min after onset minus mean of the
/// 15 min before going to bed (distal vasodilation precedes and accompanies sleep, Krauchi 1999).
pub fn skin_onset_warming(s: &[S1], bed: i64, onset: i64) -> Option<f64> {
    let before: Vec<f64> = s.iter().filter(|x| x.ts >= bed - 900 && x.ts < bed).filter_map(|x| x.skin).collect();
    let after: Vec<f64> = s.iter().filter(|x| x.ts >= onset + 1200 && x.ts < onset + 2400).filter_map(|x| x.skin).collect();
    Some(round(mean(&after)? - mean(&before)?, 2))
}

// ------------------------------------------------------------------ circadian rhythm

/// Single-component cosinor (24 h) by least squares on hourly means: y = M + A cos(2 pi (t - phi)/24).
pub fn cosinor(points: &[(f64, f64)]) -> Option<(f64, f64, f64)> {
    if points.len() < 12 {
        return None;
    }
    let w = 2.0 * std::f64::consts::PI / 24.0;
    let n = points.len() as f64;
    // normal equations for [M, b (cos), c (sin)]
    let (mut s1, mut sc, mut ss, mut scc, mut sss, mut scs, mut sy, mut syc, mut sys) = (n, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    s1 = s1.max(1.0);
    for &(h, y) in points {
        let (c, s) = ((w * h).cos(), (w * h).sin());
        sc += c; ss += s; scc += c * c; sss += s * s; scs += c * s; sy += y; syc += y * c; sys += y * s;
    }
    let m = [[s1, sc, ss], [sc, scc, scs], [ss, scs, sss]];
    let v = [sy, syc, sys];
    let det = |a: [[f64; 3]; 3]| a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0]) + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0]);
    let d = det(m);
    if d.abs() < 1e-9 {
        return None;
    }
    let solve = |col: usize| { let mut a = m; for r in 0..3 { a[r][col] = v[r]; } det(a) / d };
    let (mesor, b, c) = (solve(0), solve(1), solve(2));
    let amp = (b * b + c * c).sqrt();
    let acro = (c.atan2(b) / w).rem_euclid(24.0);
    Some((mesor, amp, acro))
}

/// Sleep Regularity Index (Phillips 2017): 200 x P(same sleep/wake state 24 h apart) - 100, on
/// 1-min epochs over consecutive days. 100 = perfectly regular.
pub fn sleep_regularity_index(minute_sleep: &std::collections::BTreeMap<i64, bool>) -> Option<f64> {
    let (mut same, mut total) = (0u64, 0u64);
    for (m, s) in minute_sleep {
        if let Some(s2) = minute_sleep.get(&(m + 1440)) {
            total += 1;
            if s == s2 { same += 1; }
        }
    }
    (total >= 720).then(|| round(200.0 * same as f64 / total as f64 - 100.0, 1))
}

// ------------------------------------------------------------------ fitness and load

/// VO2max (Uth et al. 2004): 15.3 x HRmax / HRrest (ml/kg/min), +-~10 % in the original study.
pub fn vo2max_uth(hrmax: f64, hrrest: f64) -> Option<f64> {
    (hrrest > 25.0 && hrmax > hrrest).then(|| round(15.3 * hrmax / hrrest, 1))
}

/// Banister fitness-fatigue: CTL (42 d) and ATL (7 d) exponentially weighted daily loads,
/// TSB = CTL - ATL ("form"). Series oldest first.
pub fn banister(daily: &[f64]) -> Vec<(f64, f64, f64)> {
    let (ka, kc) = (1.0 - (-1.0f64 / 7.0).exp(), 1.0 - (-1.0f64 / 42.0).exp());
    let (mut atl, mut ctl) = (0.0, 0.0);
    daily.iter().map(|l| { atl += ka * (l - atl); ctl += kc * (l - ctl); (ctl, atl, ctl - atl) }).collect()
}

// ------------------------------------------------------------------ composite indices

fn z(value: Option<f64>, history: &[f64], sd_floor: f64) -> Option<f64> {
    let v = value?;
    if history.len() < 3 {
        return None;
    }
    let m = mean(history)?;
    let sd = std_sample(history).unwrap_or(0.0).max(sd_floor);
    Some((v - m) / sd)
}

fn logistic(zc: f64, k: f64) -> f64 {
    100.0 / (1.0 + (-k * zc).exp())
}

fn composite(parts: &[(&str, f64, Option<f64>)], k: f64, offset: f64) -> Value {
    let avail: Vec<&(&str, f64, Option<f64>)> = parts.iter().filter(|p| p.2.is_some()).collect();
    let wsum: f64 = avail.iter().map(|p| p.1).sum();
    if avail.is_empty() || wsum < 0.3 {
        return json!({"score": null, "components": {}, "reason": "historique insuffisant"});
    }
    let zc: f64 = avail.iter().map(|p| p.1 * p.2.unwrap().clamp(-3.0, 3.0)).sum::<f64>() / wsum;
    let comp: serde_json::Map<String, Value> = parts.iter().map(|(n, w, v)| (n.to_string(), json!({"weight": w, "z": v.map(|x| round(x, 2))}))).collect();
    json!({"score": logistic(zc + offset, k).round(), "composite_z": round(zc, 2), "components": comp})
}

/// Per-day series assembled by the analysis, oldest first (one entry per day).
#[derive(Debug, Clone, Default)]
pub struct DaySeries {
    pub ln_rmssd: Option<f64>,
    pub rhr: Option<f64>,
    pub resp: Option<f64>,
    pub skin: Option<f64>,
    pub spo2: Option<f64>,
    pub asleep_min: Option<f64>,
    pub need_min: f64,
    pub trimp: f64,
    pub hrr60: Option<f64>,
    pub morning_ln_rmssd: Option<f64>,
}

/// Fatigue index 0-100 (high = fatigued), today = last element.
///   HRV: 7-day rolling mean of ln RMSSD vs the previous 30 days, in units of the smallest
///        worthwhile change (0.5 SD; Plews & Buchheit) -> below = fatigue;  weight 0.35
///   RHR: 7-day mean vs baseline (elevated = fatigue);                      weight 0.20
///   Load: ATL / CTL (Banister acute vs chronic), (ratio - 1) / 0.3;          weight 0.25
///   Sleep debt: 7-day deficit vs need, hours / 3;                           weight 0.20
pub fn fatigue(days: &[DaySeries]) -> Value {
    let n = days.len();
    if n == 0 {
        return Value::Null;
    }
    let last7 = &days[n.saturating_sub(7)..];
    let base = &days[n.saturating_sub(37)..n.saturating_sub(7)];
    let hrv7 = mean(&last7.iter().filter_map(|d| d.ln_rmssd).collect::<Vec<_>>());
    let hrv_base: Vec<f64> = base.iter().filter_map(|d| d.ln_rmssd).collect();
    let hrv_z = match (hrv7, mean(&hrv_base), std_sample(&hrv_base)) {
        (Some(h), Some(m), Some(sd)) if hrv_base.len() >= 5 => Some(-(h - m) / (0.5 * sd.max(0.05))),
        _ => None,
    };
    let rhr7 = mean(&last7.iter().filter_map(|d| d.rhr).collect::<Vec<_>>());
    let rhr_z = z(rhr7, &base.iter().filter_map(|d| d.rhr).collect::<Vec<_>>(), 1.5);
    let b = banister(&days.iter().map(|d| d.trimp).collect::<Vec<_>>());
    let (ctl, atl, tsb) = *b.last().unwrap();
    let load_z = (n >= 14 && ctl > 1.0).then(|| (atl / ctl - 1.0) / 0.3);
    let debt_h: f64 = last7.iter().filter_map(|d| d.asleep_min.map(|a| (d.need_min - a).max(0.0))).sum::<f64>() / 60.0;
    let debt_z = last7.iter().any(|d| d.asleep_min.is_some()).then(|| debt_h / 3.0 - 0.5);
    let cv = {
        let v: Vec<f64> = last7.iter().filter_map(|d| d.ln_rmssd).collect();
        (v.len() >= 5).then(|| round(100.0 * std_sample(&v).unwrap() / mean(&v).unwrap(), 1))
    };
    let mut out = composite(&[("hrv_7d_vs_swc", 0.35, hrv_z), ("rhr_7d", 0.20, rhr_z), ("acute_chronic_load", 0.25, load_z), ("sleep_debt", 0.20, debt_z)], 1.2, 0.0);
    out["ln_rmssd_7d"] = json!(hrv7.map(|v| round(v, 3)));
    out["ln_rmssd_cv_7d_pct"] = json!(cv);
    out["sleep_debt_7d_h"] = json!(round(debt_h, 1));
    out["banister"] = json!({"ctl": round(ctl, 1), "atl": round(atl, 1), "tsb": round(tsb, 1)});
    out["method"] = json!("HRV 7-d mean vs SWC (Plews/Buchheit), RHR 7-d, Banister ATL/CTL, 7-d sleep debt; logistic");
    out
}

/// Recovery 0-100 for the last night (high = recovered).
///   ln RMSSD night vs previous 30 nights (0.30) + morning ln RMSSD (0.10), RHR (-, 0.20),
///   respiration (-, 0.05), sleep performance = asleep / need (0.15), heart-rate recovery after the
///   last workout vs personal (0.10), Banister form TSB / CTL (0.10).
pub fn recovery_v2(days: &[DaySeries]) -> Value {
    let n = days.len();
    if n == 0 {
        return Value::Null;
    }
    let today = &days[n - 1];
    let hist = &days[n.saturating_sub(31)..n - 1];
    let col = |f: fn(&DaySeries) -> Option<f64>| hist.iter().filter_map(f).collect::<Vec<_>>();
    let hrv = z(today.ln_rmssd, &col(|d| d.ln_rmssd), 0.05);
    let morning = z(today.morning_ln_rmssd, &col(|d| d.morning_ln_rmssd), 0.08);
    let rhr = z(today.rhr, &col(|d| d.rhr), 1.5).map(|v| -v);
    let resp = z(today.resp, &col(|d| d.resp), 0.4).map(|v| -v);
    let perf = today.asleep_min.map(|a| ((a / today.need_min).min(1.1) - 0.85) / 0.12);
    let last_hrr = days.iter().rev().find_map(|d| d.hrr60);
    let hrr = z(last_hrr, &days.iter().filter_map(|d| d.hrr60).collect::<Vec<_>>(), 3.0);
    let b = banister(&days.iter().map(|d| d.trimp).collect::<Vec<_>>());
    let (ctl, _, tsb) = *b.last().unwrap();
    let form = (n >= 14 && ctl > 1.0).then(|| (tsb / ctl / 0.3).clamp(-3.0, 3.0));
    let mut out = composite(&[("hrv_night", 0.30, hrv), ("hrv_morning", 0.10, morning), ("resting_hr", 0.20, rhr), ("respiration", 0.05, resp),
        ("sleep_performance", 0.15, perf), ("hr_recovery_after_effort", 0.10, hrr), ("training_form", 0.10, form)], 1.6, 0.2);
    if hrv.is_none() {
        out["score"] = Value::Null;
        out["reason"] = json!(format!("Référence VFC en cours ({}/3 nuits)", col(|d| d.ln_rmssd).len()));
    }
    out["method"] = json!("z-scores vs personal baselines (HRV night + morning, RHR, respiration), sleep performance, HRR, Banister form; logistic, Z=0 -> 58");
    out
}

/// Early warning of illness or overreaching from the last night vs the previous <= 30 nights
/// (cf. Mishra 2020 RHR, Smarr 2020 temperature, Radin 2020): skin temp up, RHR up, HRV down,
/// respiration up, SpO2 down. Level: none / watch (>= 2 signals) / alert (>= 3, or temp + RHR).
pub fn early_warning(days: &[DaySeries], fatigue_score: Option<f64>) -> Value {
    let n = days.len();
    if n < 5 {
        return json!({"level": "calibrating", "nights_needed": 5usize.saturating_sub(n)});
    }
    let t = &days[n - 1];
    let hist = &days[n.saturating_sub(31)..n - 1];
    let col = |f: fn(&DaySeries) -> Option<f64>| hist.iter().filter_map(f).collect::<Vec<_>>();
    let dev = |v: Option<f64>, h: Vec<f64>, floor: f64| -> Option<(f64, f64)> {
        let (m, v) = (mean(&h)?, v?);
        (h.len() >= 4).then(|| (v - m, (v - m) / std_sample(&h).unwrap_or(0.0).max(floor)))
    };
    let mut signals = Vec::new();
    if let Some((d, zz)) = dev(t.skin, col(|d| d.skin), 0.2) { if d >= 0.5 && zz >= 1.5 { signals.push(json!({"signal": "skin_temp_up", "delta_c": round(d, 2)})); } }
    if let Some((d, zz)) = dev(t.rhr, col(|d| d.rhr), 1.5) { if d >= 3.0 && zz >= 1.5 { signals.push(json!({"signal": "resting_hr_up", "delta_bpm": round(d, 1)})); } }
    if let Some((d, zz)) = dev(t.ln_rmssd, col(|d| d.ln_rmssd), 0.08) { if zz <= -1.5 { signals.push(json!({"signal": "hrv_down", "delta_ln": round(d, 3)})); } }
    if let Some((d, zz)) = dev(t.resp, col(|d| d.resp), 0.4) { if d >= 1.0 && zz >= 1.5 { signals.push(json!({"signal": "respiration_up", "delta_rpm": round(d, 1)})); } }
    if let Some((d, _)) = dev(t.spo2, col(|d| d.spo2), 0.5) { if d <= -2.0 { signals.push(json!({"signal": "spo2_down", "delta_pct": round(d, 1)})); } }
    let names: Vec<&str> = signals.iter().filter_map(|s| s["signal"].as_str()).collect();
    let temp_rhr = names.contains(&"skin_temp_up") && names.contains(&"resting_hr_up");
    let level = if signals.len() >= 3 || temp_rhr { "alert" } else if signals.len() >= 2 { "watch" } else { "none" };
    // overreaching: fatigued + HRV suppressed 3 nights in a row
    let suppressed = days[n.saturating_sub(3)..].iter().all(|d| match (d.ln_rmssd, mean(&col(|x| x.ln_rmssd)), std_sample(&col(|x| x.ln_rmssd))) {
        (Some(v), Some(m), Some(sd)) => v < m - 0.5 * sd.max(0.05),
        _ => false,
    });
    json!({
        "level": level,
        "signals": signals,
        "overreaching_suspected": suppressed && fatigue_score.is_some_and(|f| f >= 65.0),
        "method": "last night vs previous nights: temp >= +0.5 C, RHR >= +3 bpm, ln RMSSD <= -1.5 SD, resp >= +1 rpm, SpO2 <= -2 %",
    })
}

/// Workout type from its own data (no pretrained HAR model can use 1 Hz summaries):
/// steps/min and cadence separate walking / running; elevated HR with few steps and moderate
/// sustained motion suggests cycling / rowing; intermittent high motion with few steps, strength.
pub fn workout_type(steps_per_min: f64, cadence_spm: Option<f64>, dyn_mean: f64, dyn_cv: f64, hr_pct_reserve: f64) -> &'static str {
    let cad = cadence_spm.unwrap_or(0.0);
    if steps_per_min >= 120.0 || cad >= 145.0 && steps_per_min >= 80.0 {
        "course"
    } else if steps_per_min >= 60.0 {
        if cad >= 115.0 || hr_pct_reserve >= 0.5 { "marche rapide" } else { "marche" }
    } else if steps_per_min < 15.0 && dyn_mean >= 0.05 && dyn_cv < 1.0 && hr_pct_reserve >= 0.4 {
        "vélo / rameur"
    } else if steps_per_min < 25.0 && dyn_cv >= 1.0 && hr_pct_reserve >= 0.3 {
        "renforcement"
    } else {
        "autre effort"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn night(f_resp: f64, amp: f64, n: usize) -> Vec<S1> {
        (0..n).map(|t| {
            let rr = 1000.0 + amp * (2.0 * PI * f_resp * t as f64).sin();
            S1 { ts: 1_000_000 + t as i64, hr: 60_000.0 / rr, rr: vec![rr], sleep: true, grav: Some([0.0, 0.6, -0.8]), dyn_g: Some(0.005), ..Default::default() }
        }).collect()
    }

    #[test]
    fn spectral_places_rsa_in_hf_and_cpc_reads_stable() {
        let s = night(0.25, 40.0, 1800);
        let w = spectral(&s, 300, 150);
        assert!(!w.is_empty());
        assert!(w[0].hf > 10.0 * w[0].lf, "{:?}", w[0]);
        let c = cpc(&s);
        assert!(c.iter().all(|x| x.1 == "stable"), "{c:?}");
        let (_, b) = breathing(&s);
        assert!((b["rate_sd_rpm"].as_f64().unwrap()) < 0.5);
    }

    #[test]
    fn slow_oscillation_is_unstable_coupling() {
        let s = night(0.05, 40.0, 1800);
        assert!(cpc(&s).iter().all(|x| x.1 == "unstable"));
    }

    #[test]
    fn cosinor_recovers_phase() {
        let pts: Vec<(f64, f64)> = (0..48).map(|h| (h as f64, 33.0 + 0.8 * (2.0 * PI * (h as f64 - 4.0) / 24.0).cos())).collect();
        let (m, a, phi) = cosinor(&pts).unwrap();
        assert!((m - 33.0).abs() < 1e-6 && (a - 0.8).abs() < 1e-6 && (phi - 4.0).abs() < 1e-6);
    }

    #[test]
    fn indices_need_history_and_react() {
        let mut days: Vec<DaySeries> = (0..20).map(|i| DaySeries { ln_rmssd: Some(4.0 + 0.02 * ((i % 3) as f64)), rhr: Some(50.0 + (i % 2) as f64), resp: Some(15.0), skin: Some(33.0 + 0.05 * (i % 2) as f64), spo2: Some(98.0), asleep_min: Some(470.0), need_min: 480.0, trimp: 40.0, ..Default::default() }).collect();
        let base = recovery_v2(&days);
        assert!(base["score"].as_f64().unwrap() > 40.0);
        days.push(DaySeries { ln_rmssd: Some(3.6), rhr: Some(57.0), resp: Some(17.0), skin: Some(34.0), spo2: Some(95.0), asleep_min: Some(300.0), need_min: 480.0, trimp: 40.0, ..Default::default() });
        let bad = recovery_v2(&days);
        assert!(bad["score"].as_f64().unwrap() < base["score"].as_f64().unwrap() - 20.0, "{bad}");
        let w = early_warning(&days, None);
        assert_eq!(w["level"], "alert", "{w}");
        assert!(fatigue(&days)["score"].as_f64().is_some());
        assert_eq!(vo2max_uth(190.0, 50.0), Some(58.1));
    }

    #[test]
    fn workout_types() {
        assert_eq!(workout_type(160.0, Some(165.0), 0.6, 0.4, 0.7), "course");
        assert_eq!(workout_type(5.0, None, 0.12, 0.5, 0.6), "vélo / rameur");
        assert_eq!(workout_type(100.0, Some(105.0), 0.2, 0.5, 0.3), "marche");
    }
}
