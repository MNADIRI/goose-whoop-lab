//! Cardiovascular load with every intermediate exposed (ported/adapted from NOOP StrainScorer and
//! WorkoutDetector; references: Karvonen 1957, Edwards 1993, Banister 1991, Tanaka 2001,
//! Foster 1998, Gabbett 2016).
//!
//! - HRmax: Tanaka 208 - 0.7 x age, raised to the observed 99.5th percentile when >= 600 samples.
//! - %HRR = (HR - resting) / (HRmax - resting), clamped 0..1 (Karvonen).
//! - Edwards %HRR TRIMP: minutes x zone weight (1..5 at 50/60/70/80/90 %HRR).
//! - Banister TRIMP: minutes x x x 0.64 e^(b x), b = 1.92 men / 1.67 women.
//! - NOOP strain: 21 x ln(TRIMP + 1) / ln(7201) on the Edwards %HRR TRIMP.
//! - Foster: monotony = mean / SD of the last 7 daily loads, strain = weekly load x monotony.
//! - Workouts: >= 5 min where HR > resting + 15 bpm AND smoothed motion > 0.20 g-ish intensity,
//!   gaps <= 150 s merged (NOOP WorkoutDetector thresholds; motion here = R18 dynamic accel).

use serde_json::{json, Value};

use crate::dsp::{mean, percentile, round, std_sample};

pub fn tanaka_hrmax(age: f64) -> f64 {
    208.0 - 0.7 * age
}

pub fn hrmax_used(age: f64, hr: &[f64]) -> (f64, &'static str) {
    let t = tanaka_hrmax(age);
    match (hr.len() >= 600).then(|| percentile(hr, 99.5)).flatten() {
        Some(obs) if obs > t => (obs, "observed_p99.5"),
        _ => (t, "tanaka"),
    }
}

/// `hr` sampled at 1 Hz (one value per second).
pub fn load(hr: &[f64], resting: f64, hrmax: f64, female: bool) -> Value {
    let reserve = (hrmax - resting).max(1.0);
    let b = if female { 1.67 } else { 1.92 };
    let mut zone_s = [0.0f64; 6];
    let mut banister = 0.0;
    for &h in hr {
        let x = ((h - resting) / reserve).clamp(0.0, 1.0);
        let z = [0.5, 0.6, 0.7, 0.8, 0.9].iter().filter(|t| x >= **t).count();
        zone_s[z] += 1.0;
        banister += x * 0.64 * (b * x).exp() / 60.0;
    }
    let edwards_hrr: f64 = (1..=5).map(|z| z as f64 * zone_s[z] / 60.0).sum();
    json!({
        "resting_hr": round(resting, 1),
        "hrmax": hrmax.round(),
        "hr_reserve": round(reserve, 1),
        "hrr_zone_min": {"z1": round(zone_s[1] / 60.0, 1), "z2": round(zone_s[2] / 60.0, 1), "z3": round(zone_s[3] / 60.0, 1), "z4": round(zone_s[4] / 60.0, 1), "z5": round(zone_s[5] / 60.0, 1)},
        "edwards_hrr_trimp": round(edwards_hrr, 1),
        "banister_trimp": round(banister, 1),
        "banister_b": b,
        "noop_strain_0_21": round(21.0 * (edwards_hrr + 1.0).ln() / 7201f64.ln(), 1),
        "status": "EXPERIMENTAL",
    })
}

pub fn foster(daily: &[f64]) -> Value {
    let last: Vec<f64> = daily.iter().rev().take(7).copied().collect();
    if last.len() < 7 {
        return json!({"status": "UNAVAILABLE", "days": last.len(), "reason": "7 jours nécessaires"});
    }
    let m = mean(&last).unwrap_or(0.0);
    let sd = std_sample(&last).unwrap_or(0.0);
    let monotony = if sd > 1e-9 { m / sd } else { f64::NAN };
    let weekly: f64 = last.iter().sum();
    json!({
        "weekly_load": round(weekly, 1),
        "mean_daily": round(m, 1),
        "sd_daily": round(sd, 1),
        "monotony": monotony.is_finite().then(|| round(monotony, 2)),
        "strain": monotony.is_finite().then(|| round(weekly * monotony, 1)),
        "status": "EXPERIMENTAL",
    })
}

/// Detects workouts on 1 Hz samples (ts, hr, dyn accel g).
pub fn workouts(samples: &[(i64, f64, Option<f64>)], resting: f64, hrmax: f64, female: bool) -> Value {
    const MIN_S: i64 = 300;
    const MERGE_GAP_S: i64 = 150;
    // 10 s moving mean of dynamic acceleration
    let dyn_s: Vec<f64> = (0..samples.len())
        .map(|i| {
            let lo = i.saturating_sub(5);
            let hi = (i + 5).min(samples.len());
            let v: Vec<f64> = samples[lo..hi].iter().filter_map(|s| s.2).collect();
            mean(&v).unwrap_or(0.0)
        })
        .collect();
    let active: Vec<bool> = samples.iter().zip(&dyn_s).map(|(s, d)| s.1 > resting + 15.0 && *d > 0.20).collect();
    let mut bouts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < samples.len() {
        if !active[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < samples.len() && active[i] {
            i += 1;
        }
        let end = i - 1;
        match bouts.last_mut() {
            Some(last) if samples[start].0 - samples[last.1].0 <= MERGE_GAP_S => last.1 = end,
            _ => bouts.push((start, end)),
        }
    }
    let out: Vec<Value> = bouts
        .into_iter()
        .filter(|(a, b)| samples[*b].0 - samples[*a].0 >= MIN_S)
        .map(|(a, b)| {
            let hr: Vec<f64> = samples[a..=b].iter().map(|s| s.1).collect();
            json!({
                "start": samples[a].0, "end": samples[b].0,
                "duration_min": round((samples[b].0 - samples[a].0) as f64 / 60.0, 1),
                "hr_mean": round(mean(&hr).unwrap_or(0.0), 1),
                "hr_max": hr.iter().copied().fold(0.0, f64::max),
                "load": load(&hr, resting, hrmax, female),
            })
        })
        .collect();
    json!({"count": out.len(), "sessions": out, "status": "EXPERIMENTAL",
           "method": "HR > resting + 15 bpm and 10 s mean dynamic accel > 0.20 g for >= 5 min, gaps <= 150 s merged"})
}

/// Heart-rate recovery after a workout: drop at 60 s and 120 s after its end.
pub fn hr_recovery(samples: &[(i64, f64, Option<f64>)], end_ts: i64) -> Value {
    let at = |t: i64| samples.iter().find(|s| s.0 >= t && s.0 <= t + 5).map(|s| s.1);
    match (at(end_ts), at(end_ts + 60), at(end_ts + 120)) {
        (Some(h0), Some(h1), h2) => json!({"hr_end": h0, "drop_60s": h0 - h1, "drop_120s": h2.map(|v| h0 - v), "status": "EXPERIMENTAL"}),
        _ => json!({"status": "UNAVAILABLE"}),
    }
}

/// Cardiac drift: % change of mean HR between the first and second half of a steady bout.
pub fn cardiac_drift(hr: &[f64]) -> Option<f64> {
    if hr.len() < 600 {
        return None;
    }
    let h = hr.len() / 2;
    let (a, b) = (mean(&hr[..h])?, mean(&hr[h..])?);
    Some(round(100.0 * (b / a - 1.0), 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_zones_and_trimps() {
        // 10 min at 75 % HRR with resting 50, hrmax 190 -> HR 155
        let hr = vec![155.0; 600];
        let l = load(&hr, 50.0, 190.0, false);
        assert_eq!(l["hrr_zone_min"]["z3"], 10.0);
        assert_eq!(l["edwards_hrr_trimp"], 30.0);
        let expected = 10.0 * 0.75 * 0.64 * (1.92f64 * 0.75).exp();
        assert!((l["banister_trimp"].as_f64().unwrap() - expected).abs() < 0.2);
    }

    #[test]
    fn foster_and_workouts() {
        assert_eq!(foster(&[10.0; 3])["status"], "UNAVAILABLE");
        let f = foster(&[10.0, 20.0, 10.0, 20.0, 10.0, 20.0, 10.0]);
        assert!(f["monotony"].as_f64().unwrap() > 1.0);
        let mut s: Vec<(i64, f64, Option<f64>)> = (0..1200).map(|t| (t, 60.0, Some(0.01))).collect();
        for x in s.iter_mut().skip(200).take(600) {
            x.1 = 140.0;
            x.2 = Some(0.5);
        }
        let w = workouts(&s, 55.0, 190.0, false);
        assert_eq!(w["count"], 1);
        assert!((w["sessions"][0]["duration_min"].as_f64().unwrap() - 10.0).abs() < 0.3);
    }
}
