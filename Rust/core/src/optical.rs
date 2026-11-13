//! Optical (PPG) processing for WHOOP 5.0 R26 compact windows.
//!
//! What is established from real captures (firmware 50.38.1): one R26 window per device second,
//! 25 reconstructed samples per window, consecutive windows continuous (the jump from the last
//! sample of one window to the next base equals a typical intra-window step), recorded in bursts of
//! ~40 s every ~20 min. The sampling rate is therefore ESTIMATED as samples / elapsed seconds per
//! burst, never assumed. The wavelength of the channel is unknown: it is an unidentified optical
//! channel. Nothing here estimates SpO2, blood pressure or perfusion index.
//!
//! Per burst: continuity check -> linear detrend -> 0.5-4 Hz zero-phase band-pass -> pulse rate by
//! autocorrelation (polarity independent) -> pulse peaks on the polarity that matches it -> pulse
//! intervals, amplitudes, rise time, amplitude variability, spectral SNR -> agreement with the R18
//! heart rate of the same seconds (internal consistency only, not external validation).

use serde_json::{json, Value};

use crate::dsp::{autocorr, bandpass, band_power, detrend_linear, find_peaks, mean, median, periodogram, round, std_sample};
use crate::whoop5_records::{R26Record, Status};

pub const SAMPLES_PER_WINDOW: usize = 25;
pub const MOTION_LIMIT: f64 = 0.20;

#[derive(Debug, Clone)]
pub struct Burst<'a> {
    pub windows: Vec<&'a R26Record>,
}

/// Groups windows (sorted by ts) into bursts of consecutive seconds.
pub fn bursts(windows: &[R26Record]) -> Vec<Burst<'_>> {
    let mut out: Vec<Burst> = Vec::new();
    for w in windows {
        match out.last_mut() {
            Some(b) if b.windows.last().is_some_and(|l| w.ts == l.ts + 1 && w.status_raw == l.status_raw) => b.windows.push(w),
            Some(b) if b.windows.last().is_some_and(|l| w.ts == l.ts) => {} // duplicate second
            _ => out.push(Burst { windows: vec![w] }),
        }
    }
    out
}

/// Ratio of the boundary jump (last sample -> next base) to the typical intra-window step.
fn continuity(b: &Burst) -> Option<f64> {
    let intra: Vec<f64> = b.windows.iter().flat_map(|w| w.deltas.iter().map(|d| f64::from(*d).abs())).collect();
    let joins: Vec<f64> = b.windows.windows(2).map(|p| (p[1].samples[0] - p[0].samples[24]).abs() as f64).collect();
    Some(median(&joins)? / median(&intra)?.max(1.0))
}

/// Analyses one burst. `hr_at` returns the R18 heart rate for a device second, if any.
pub fn analyze_burst(b: &Burst, hr_at: &dyn Fn(i64) -> Option<f64>) -> Value {
    let n_win = b.windows.len();
    let start = b.windows[0].ts;
    let elapsed = (b.windows[n_win - 1].ts - start + 1) as f64;
    let fs = (n_win * SAMPLES_PER_WINDOW) as f64 / elapsed;
    let motion: Vec<f64> = b.windows.iter().filter_map(|w| w.motion.map(f64::from)).collect();
    let moving = motion.iter().filter(|m| **m > MOTION_LIMIT).count();
    let accepted = b.windows.iter().filter(|w| w.acceptance_raw != 0).count();
    let base = json!({
        "start": start, "windows": n_win, "fs_hz_estimated": round(fs, 2),
        "continuity_ratio": continuity(b).map(|v| round(v, 2)),
        "motion_median": median(&motion).map(|v| round(v, 3)),
        "motion_windows": moving, "accepted_windows": accepted,
        // LED / AFE setting candidate: changes with the DC level, so amplitudes are only comparable
        // between bursts that share it
        "status_raw": b.windows[0].status_raw,
        "dc_median": median(&b.windows.iter().map(|w| w.samples[0] as f64).collect::<Vec<_>>()).map(|v| v.round()),
    });
    if n_win < 8 {
        return merge(base, json!({"status": Status::Unavailable, "reason": "burst shorter than 8 s"}));
    }
    let mut x: Vec<f64> = b.windows.iter().flat_map(|w| w.samples.iter().map(|v| *v as f64)).collect();
    detrend_linear(&mut x);
    let y = bandpass(&x, fs, 0.5, 4.0);
    let y = &y[(fs as usize).min(y.len() / 4)..y.len() - (fs as usize).min(y.len() / 4)]; // drop filter edges
    // pulse rate from autocorrelation, lags for 40-200 bpm
    let (lmin, lmax) = ((fs * 60.0 / 200.0).floor() as usize, (fs * 60.0 / 40.0).ceil() as usize);
    let ac = autocorr(y, lmax);
    let best = (lmin.max(1)..ac.len()).filter(|l| *l > 0 && *l + 1 < ac.len() && ac[*l] >= ac[*l - 1] && ac[*l] >= ac[*l + 1]).max_by(|a, b| ac[*a].total_cmp(&ac[*b]));
    let Some(lag) = best else { return merge(base, json!({"status": Status::Invalid, "reason": "no periodicity"})) };
    let pr_acf = 60.0 * fs / lag as f64;
    // spectral SNR: power within +-0.2 Hz of the pulse frequency (and its harmonic) / 0.5-4 Hz
    let pw = periodogram(y);
    let f0 = pr_acf / 60.0;
    let total = band_power(&pw, fs, y.len(), 0.5, 4.0).max(1e-12);
    let signal = band_power(&pw, fs, y.len(), f0 - 0.2, f0 + 0.2) + band_power(&pw, fs, y.len(), 2.0 * f0 - 0.2, 2.0 * f0 + 0.2);
    let snr = signal / total;
    // peaks on both polarities, keep the one whose median interval matches the ACF period
    let sd = std_sample(y).unwrap_or(0.0);
    let pick = |sig: &[f64]| find_peaks(sig, (0.6 * lag as f64) as usize, 0.5 * sd);
    let neg: Vec<f64> = y.iter().map(|v| -v).collect();
    let (peaks_pos, peaks_neg) = (pick(y), pick(&neg));
    let score = |p: &[usize]| {
        let iv: Vec<f64> = p.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
        median(&iv).map(|m| (m - lag as f64).abs()).unwrap_or(f64::INFINITY)
    };
    let (peaks, sig, polarity) = if score(&peaks_neg) < score(&peaks_pos) { (peaks_neg, &neg[..], "inverted") } else { (peaks_pos, y, "direct") };
    let ibi_ms: Vec<f64> = peaks.windows(2).map(|w| (w[1] - w[0]) as f64 / fs * 1000.0).filter(|v| (300.0..=1500.0).contains(v)).collect();
    // amplitude: peak minus preceding trough; rise time trough -> peak
    let mut amps = Vec::new();
    let mut rise_ms = Vec::new();
    for w in peaks.windows(2) {
        let (a, bpk) = (w[0], w[1]);
        if let Some((ti, tv)) = sig[a..bpk].iter().enumerate().min_by(|p, q| p.1.total_cmp(q.1)) {
            amps.push(sig[bpk] - tv);
            rise_ms.push((bpk - (a + ti)) as f64 / fs * 1000.0);
        }
    }
    let pr_peaks = median(&ibi_ms).map(|m| 60_000.0 / m);
    let hr_ref: Vec<f64> = b.windows.iter().filter_map(|w| hr_at(w.ts)).filter(|h| *h > 0.0).collect();
    let hr_med = median(&hr_ref);
    let err = match (pr_peaks.or(Some(pr_acf)), hr_med) {
        (Some(p), Some(h)) => Some(p - h),
        _ => None,
    };
    let clean = moving == 0 && motion.len() == n_win && accepted == n_win
        && continuity(b).is_some_and(|r| r <= 3.0) && snr >= 0.5;
    let status = match err {
        Some(e) if e.abs() <= 5.0 && clean => Status::Experimental,
        _ if clean => Status::Experimental,
        _ => Status::Invalid,
    };
    let amp_mean = mean(&amps);
    merge(base, json!({
        "polarity": polarity,
        "pulse_rate_acf_bpm": round(pr_acf, 1),
        "pulse_rate_peaks_bpm": pr_peaks.map(|v| round(v, 1)),
        "r18_hr_median_bpm": hr_med,
        "pulse_rate_minus_hr_bpm": err.map(|v| round(v, 1)),
        "pulses": peaks.len(),
        "ibi_ms": ibi_ms.iter().map(|v| v.round()).collect::<Vec<_>>(),
        "ibi_sdnn_ms": std_sample(&ibi_ms).map(|v| round(v, 1)),
        "amplitude_median": median(&amps).map(|v| round(v, 1)),
        "amplitude_cv": match (amp_mean, std_sample(&amps)) { (Some(m), Some(s)) if m > 0.0 => Some(round(s / m, 3)), _ => None },
        "rise_time_ms_median": median(&rise_ms).map(|v| v.round()),
        "spectral_peak_power_fraction": round(snr.clamp(0.0, 1.0), 3),
        "agreement_with_firmware": err.map(|e| e.abs() <= 5.0),
        "external_validation": false,
        "acf_peak": round(ac[lag], 3),
        "status": status,
    }))
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(o), Value::Object(m)) = (a.as_object_mut(), b) {
        o.extend(m);
    }
    a
}

/// Summary over all bursts of a period (e.g. a night).
pub fn summarize(windows: &[R26Record], hr_at: &dyn Fn(i64) -> Option<f64>) -> Value {
    let bs = bursts(windows);
    if bs.is_empty() {
        return json!({"status": Status::Unavailable, "windows": 0});
    }
    let results: Vec<Value> = bs.iter().map(|b| analyze_burst(b, hr_at)).collect();
    let errs: Vec<f64> = results.iter().filter_map(|r| r["pulse_rate_minus_hr_bpm"].as_f64()).collect();
    let fs: Vec<f64> = results.iter().filter_map(|r| r["fs_hz_estimated"].as_f64()).collect();
    let usable = results.iter().filter(|r| r["status"] == "EXPERIMENTAL").count();
    let amps: Vec<f64> = results.iter().filter(|r| r["status"] == "EXPERIMENTAL").filter_map(|r| r["amplitude_median"].as_f64()).collect();
    json!({
        "windows": windows.len(),
        "bursts": bs.len(),
        "fs_hz_estimated_median": median(&fs).map(|v| round(v, 2)),
        "accepted_window_pct": round(100.0 * windows.iter().filter(|w| w.acceptance_raw != 0).count() as f64 / windows.len() as f64, 1),
        "validated_bursts": 0,
        "usable_bursts": usable,
        "pulse_rate_vs_hr_mae_bpm": (!errs.is_empty()).then(|| round(errs.iter().map(|e| e.abs()).sum::<f64>() / errs.len() as f64, 1)),
        "pulse_amplitude_median": median(&amps).map(|v| round(v, 1)),
        "status": if usable > 0 { Status::Experimental } else { Status::Unavailable },
        "per_burst": results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn synth(seconds: usize, bpm: f64, inverted: bool) -> Vec<R26Record> {
        let fs = 25.0;
        (0..seconds)
            .map(|s| {
                let samples: Vec<i64> = (0..25)
                    .map(|i| {
                        let t = (s * 25 + i) as f64 / fs;
                        // pulse-like: fundamental + harmonic + slow drift
                        let v = 800.0 * (2.0 * PI * bpm / 60.0 * t).sin() + 250.0 * (4.0 * PI * bpm / 60.0 * t).sin() + 30.0 * t;
                        300_000 + if inverted { -v } else { v } as i64
                    })
                    .collect();
                R26Record {
                    ts: 1_794_000_000 + s as i64,
                    record_index: s as u32,
                    time_word: 0,
                    burst: 1,
                    base: samples[0] as i32,
                    deltas: samples.windows(2).map(|w| (w[1] - w[0]) as i16).collect(),
                    samples,
                    motion: Some(0.05),
                    status_raw: 0,
                    on_wrist_raw: 1,
                    acceptance_raw: 1,
                }
            })
            .collect()
    }

    #[test]
    fn gain_changes_split_bursts_and_missing_quality_rejects() {
        let mut windows = synth(40, 66.0, false);
        for w in &mut windows[20..] { w.status_raw = 1; }
        assert_eq!(bursts(&windows).len(), 2);
        for w in &mut windows { w.motion = None; }
        let result = summarize(&windows, &|_| Some(66.0));
        assert_eq!(result["usable_bursts"], 0);
        assert_eq!(result["validated_bursts"], 0);
    }

    #[test]
    fn recovers_pulse_rate_and_validates_against_hr() {
        for inverted in [false, true] {
            let w = synth(40, 66.0, inverted);
            let s = summarize(&w, &|_| Some(66.0));
            let b = &s["per_burst"][0];
            assert_eq!(b["fs_hz_estimated"], 25.0);
            let pr = b["pulse_rate_peaks_bpm"].as_f64().unwrap();
            assert!((pr - 66.0).abs() < 2.0, "pr {pr}");
            assert_eq!(b["status"], "EXPERIMENTAL", "{b}");
            assert_eq!(b["agreement_with_firmware"], true);
            assert_eq!(b["external_validation"], false);
            assert!(b["continuity_ratio"].as_f64().unwrap() < 3.0);
        }
    }

    #[test]
    fn disagreement_with_hr_is_not_validated() {
        let w = synth(40, 66.0, false);
        let s = summarize(&w, &|_| Some(110.0));
        assert_ne!(s["per_burst"][0]["status"], "VALIDATED");
    }
}
