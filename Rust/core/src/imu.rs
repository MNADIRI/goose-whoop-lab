//! Six-axis IMU processing for WHOOP 5.0 R21 records.
//!
//! Raw int16 axes are preserved upstream (whoop5_signal_imu). Here they are converted with the
//! tentative scales (accel raw/4096 g, gyro raw*2000/32768 deg/s; verified by NOOP on the 4.0 raw
//! stream only) and the accelerometer scale is re-checked on every dataset: during still periods
//! |a| must be 1 g, so the median raw magnitude while still gives the implied LSB/g.
//! The sampling rate is estimated from record timestamps (samples / elapsed time), not assumed.
//!
//! Features per window: gravity (0.3 Hz low-pass per axis), dynamic acceleration (a - g), ENMO,
//! jerk, gyro magnitude, angular acceleration, tremor band (4-12 Hz) share, steps (peaks of the
//! 0.5-3 Hz band-passed |a|), cadence, gait regularity (autocorrelation at the step and stride lag,
//! Moe-Nilssen & Helbostad 2004) and an activity class.

use serde_json::{json, Value};

use crate::dsp::{autocorr, band_power, bandpass, filtfilt, find_peaks, mean, median, periodogram, round, Biquad};
use crate::whoop5_records::{R21Record, Status, ACCEL_LSB_PER_G, GYRO_DPS_PER_LSB};

#[derive(Debug, Clone, Default)]
pub struct ImuSeries {
    pub t: Vec<f64>,
    /// raw counts, kept to re-derive scale
    pub accel_raw: [Vec<f64>; 3],
    pub gyro_raw: [Vec<f64>; 3],
    pub fs_hz: f64,
    pub gaps: usize,
}

/// Concatenates records (sorted by ts) into one series; sample times are spread over each
/// record's span (next record ts - this ts, or 1 s for the last one).
pub fn series(records: &[R21Record]) -> ImuSeries {
    let mut s = ImuSeries::default();
    let mut rates = Vec::new();
    for (i, r) in records.iter().enumerate() {
        let n = r.accel_raw[0].len().min(r.accel_raw[1].len()).min(r.accel_raw[2].len());
        if n == 0 {
            continue;
        }
        let span = records.get(i + 1).map(|nx| nx.ts - r.ts).filter(|d| *d > 0.0 && *d < 2.0);
        if span.is_none() && i + 1 < records.len() {
            s.gaps += 1;
        }
        let span = span.unwrap_or(1.0);
        rates.push(n as f64 / span);
        for k in 0..n {
            s.t.push(r.ts + span * k as f64 / n as f64);
            for a in 0..3 {
                s.accel_raw[a].push(f64::from(r.accel_raw[a][k]));
                s.gyro_raw[a].push(r.gyro_raw[a].get(k).map(|v| f64::from(*v)).unwrap_or(f64::NAN));
            }
        }
    }
    s.fs_hz = median(&rates).unwrap_or(0.0);
    s
}

fn magnitude(x: &[Vec<f64>; 3], i: usize) -> f64 {
    (x[0][i].powi(2) + x[1][i].powi(2) + x[2][i].powi(2)).sqrt()
}

/// Implied accelerometer LSB/g from still samples (|a| should equal 1 g).
pub fn accel_scale_check(s: &ImuSeries) -> Value {
    let n = s.t.len();
    if n < 200 {
        return json!({"status": Status::Unavailable});
    }
    let mag: Vec<f64> = (0..n).map(|i| magnitude(&s.accel_raw, i)).collect();
    // still = 1 s neighbourhood with small spread
    let w = (s.fs_hz.max(1.0)) as usize;
    let still: Vec<f64> = (w..n.saturating_sub(w))
        .filter(|&i| {
            let seg = &mag[i - w / 2..i + w / 2];
            let m = mean(seg).unwrap_or(0.0);
            seg.iter().all(|v| (v - m).abs() < 0.02 * m.max(1.0))
        })
        .map(|i| mag[i])
        .collect();
    let Some(implied) = median(&still) else { return json!({"status": Status::Unavailable, "reason": "no still period"}) };
    let rel = implied / ACCEL_LSB_PER_G - 1.0;
    json!({
        "implied_lsb_per_g": round(implied, 1),
        "assumed_lsb_per_g": ACCEL_LSB_PER_G,
        "relative_error": round(rel, 4),
        "still_samples": still.len(),
        "status": if rel.abs() <= 0.05 { Status::Validated } else { Status::Invalid },
    })
}

/// Features over consecutive windows of `window_s` seconds.
pub fn features(s: &ImuSeries, window_s: f64) -> Value {
    let fs = s.fs_hz;
    let n = s.t.len();
    if fs < 10.0 || n < (fs * window_s) as usize {
        return json!({"status": Status::Unavailable, "fs_hz_estimated": round(fs, 1), "samples": n});
    }
    let acc: [Vec<f64>; 3] = s.accel_raw.clone().map(|a| a.iter().map(|v| v / ACCEL_LSB_PER_G).collect());
    let gyr: [Vec<f64>; 3] = s.gyro_raw.clone().map(|a| a.iter().map(|v| v * GYRO_DPS_PER_LSB).collect());
    let lp = Biquad::lowpass(fs, 0.3);
    let grav: [Vec<f64>; 3] = acc.clone().map(|a| filtfilt(&[lp], &a));
    let mag: Vec<f64> = (0..n).map(|i| magnitude(&acc, i)).collect();
    let dynm: Vec<f64> = (0..n).map(|i| ((acc[0][i] - grav[0][i]).powi(2) + (acc[1][i] - grav[1][i]).powi(2) + (acc[2][i] - grav[2][i]).powi(2)).sqrt()).collect();
    let gmag: Vec<f64> = (0..n).map(|i| magnitude(&gyr, i)).collect();
    let step_sig = bandpass(&mag, fs, 0.5, 3.0);
    let wlen = (fs * window_s) as usize;
    let mut windows = Vec::new();
    let mut total_steps = 0usize;
    let mut i0 = 0;
    while i0 + wlen <= n {
        let r = i0..i0 + wlen;
        let rms = |v: &[f64]| (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt();
        let dyn_rms = rms(&dynm[r.clone()]);
        let enmo = mean(&mag[r.clone()].iter().map(|m| (m - 1.0).max(0.0)).collect::<Vec<_>>()).unwrap_or(0.0);
        let jerk: Vec<f64> = mag[r.clone()].windows(2).map(|w| (w[1] - w[0]) * fs).collect();
        let gseg: Vec<f64> = gmag[r.clone()].iter().copied().filter(|v| v.is_finite()).collect();
        let ang_acc: Vec<f64> = gseg.windows(2).map(|w| (w[1] - w[0]) * fs).collect();
        let pw = periodogram(&dynm[r.clone()]);
        let tremor = band_power(&pw, fs, wlen, 4.0, 12.0) / band_power(&pw, fs, wlen, 0.3, fs / 2.0).max(1e-12);
        let seg = &step_sig[r.clone()];
        let peaks = find_peaks(seg, (0.25 * fs) as usize, 0.05);
        let moving = dyn_rms >= 0.05 && peaks.len() >= 3;
        let steps = if moving { peaks.len() } else { 0 };
        total_steps += steps;
        let cadence = steps as f64 * 60.0 / window_s;
        let (step_reg, stride_reg) = if moving {
            let iv: Vec<f64> = peaks.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
            let lag = median(&iv).unwrap_or(0.0) as usize;
            let ac = autocorr(seg, 2 * lag + 1);
            (ac.get(lag).copied(), ac.get(2 * lag).copied())
        } else {
            (None, None)
        };
        let gyro_mean = mean(&gseg).unwrap_or(0.0);
        let class = if dyn_rms < 0.02 && gyro_mean < 10.0 {
            "still"
        } else if moving && cadence >= 140.0 || dyn_rms > 0.6 {
            "run"
        } else if moving && (60.0..140.0).contains(&cadence) {
            "walk"
        } else {
            "active"
        };
        windows.push(json!({
            "t": round(s.t[i0], 2),
            "class": class,
            "dyn_rms_g": round(dyn_rms, 4),
            "enmo_g": round(enmo, 4),
            "jerk_rms_g_s": round(rms(&jerk), 3),
            "gyro_mean_dps": round(gyro_mean, 2),
            "angular_acc_rms_dps2": round(if ang_acc.is_empty() { 0.0 } else { rms(&ang_acc) }, 1),
            "tremor_4_12hz_share": round(tremor, 3),
            "steps": steps,
            "cadence_spm": round(cadence, 1),
            "step_regularity": step_reg.map(|v| round(v, 3)),
            "stride_regularity": stride_reg.map(|v| round(v, 3)),
            "gravity_g": [round(mean(&grav[0][r.clone()]).unwrap_or(0.0), 3), round(mean(&grav[1][r.clone()]).unwrap_or(0.0), 3), round(mean(&grav[2][r]).unwrap_or(0.0), 3)],
        }));
        i0 += wlen;
    }
    let count = |c: &str| windows.iter().filter(|w| w["class"] == c).count() as f64 * window_s / 60.0;
    json!({
        "fs_hz_estimated": round(fs, 2),
        "samples": n,
        "gaps": s.gaps,
        "window_s": window_s,
        "steps": total_steps,
        "minutes": {"still": round(count("still"), 1), "walk": round(count("walk"), 1), "run": round(count("run"), 1), "active": round(count("active"), 1)},
        "status": Status::Experimental,
        "windows": windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn records(seconds: usize, step_hz: f64, amp_g: f64) -> Vec<R21Record> {
        (0..seconds)
            .map(|s| {
                let mut a: [Vec<i16>; 3] = Default::default();
                let mut g: [Vec<i16>; 3] = Default::default();
                for k in 0..100 {
                    let t = s as f64 + k as f64 / 100.0;
                    let bounce = amp_g * (2.0 * PI * step_hz * t).sin();
                    a[0].push(0);
                    a[1].push(0);
                    a[2].push(((1.0 + bounce) * 4096.0) as i16);
                    g[0].push(0);
                    g[1].push(0);
                    g[2].push(if amp_g > 0.0 { 300 } else { 0 });
                }
                R21Record { ts: 1_794_000_000.0 + s as f64, record_index: s as u32, marker_raw: 0, accel_count_raw: 100, gyro_count_raw: 100, accel_raw: a, gyro_raw: g, warnings: vec![] }
            })
            .collect()
    }

    #[test]
    fn still_data_validates_scale_and_is_still() {
        let s = series(&records(30, 0.0, 0.0));
        assert_eq!(s.fs_hz, 100.0);
        let chk = accel_scale_check(&s);
        assert_eq!(chk["status"], "VALIDATED");
        let f = features(&s, 10.0);
        assert_eq!(f["steps"], 0);
        assert_eq!(f["minutes"]["still"], 0.5);
    }

    #[test]
    fn walking_counts_steps_and_cadence() {
        let s = series(&records(30, 1.8, 0.25));
        let f = features(&s, 10.0);
        let steps = f["steps"].as_u64().unwrap();
        assert!((50..=56).contains(&steps), "steps {steps}");
        assert_eq!(f["windows"][1]["class"], "walk");
        assert!(f["windows"][1]["step_regularity"].as_f64().unwrap() > 0.8);
    }
}
