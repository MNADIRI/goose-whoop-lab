//! Small, dependency-free signal primitives shared by the optical, IMU and HRV modules.
//!
//! Kept local on purpose: the core is built for iOS as a static library and these few routines
//! (biquad Butterworth, zero-phase filtering, peaks, autocorrelation, periodogram) are short and
//! testable. Formulas follow the RBJ Audio-EQ cookbook (bilinear transform, Q = 1/sqrt 2).

use std::f64::consts::PI;

pub fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

pub fn std_sample(v: &[f64]) -> Option<f64> {
    if v.len() < 2 {
        return None;
    }
    let m = mean(v)?;
    Some((v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt())
}

pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let rank = p / 100.0 * (v.len() - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    Some(v[lo] + (v[hi] - v[lo]) * (rank - lo as f64))
}

pub fn median(v: &[f64]) -> Option<f64> {
    percentile(v, 50.0)
}

pub fn round(v: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (v * k).round() / k
}

#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0] }
    }
    pub fn lowpass(fs: f64, fc: f64) -> Self {
        let w = 2.0 * PI * fc / fs;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let c = w.cos();
        Self::new((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }
    pub fn highpass(fs: f64, fc: f64) -> Self {
        let w = 2.0 * PI * fc / fs;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let c = w.cos();
        Self::new((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }
    pub fn run(&self, x: &[f64]) -> Vec<f64> {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        // start in steady state on the first value to limit the edge transient
        if let Some(&first) = x.first() {
            let dc_gain = (self.b[0] + self.b[1] + self.b[2]) / (1.0 + self.a[0] + self.a[1]);
            x1 = first;
            x2 = first;
            y1 = first * dc_gain;
            y2 = y1;
        }
        x.iter()
            .map(|&v| {
                let y = self.b[0] * v + self.b[1] * x1 + self.b[2] * x2 - self.a[0] * y1 - self.a[1] * y2;
                x2 = x1;
                x1 = v;
                y2 = y1;
                y1 = y;
                y
            })
            .collect()
    }
}

/// Zero-phase (forward + backward) application of a cascade of biquads.
pub fn filtfilt(stages: &[Biquad], x: &[f64]) -> Vec<f64> {
    let mut y = x.to_vec();
    for s in stages {
        y = s.run(&y);
        y.reverse();
        y = s.run(&y);
        y.reverse();
    }
    y
}

/// 2nd-order high-pass + 2nd-order low-pass, run forward and backward (4th order effective).
pub fn bandpass(x: &[f64], fs: f64, lo: f64, hi: f64) -> Vec<f64> {
    filtfilt(&[Biquad::highpass(fs, lo), Biquad::lowpass(fs, hi)], x)
}

pub fn detrend_linear(x: &mut [f64]) {
    let n = x.len() as f64;
    if x.len() < 2 {
        return;
    }
    let mx = (n - 1.0) / 2.0;
    let my = x.iter().sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        sxy += (i as f64 - mx) * (v - my);
        sxx += (i as f64 - mx).powi(2);
    }
    let slope = sxy / sxx;
    for (i, v) in x.iter_mut().enumerate() {
        *v -= my + slope * (i as f64 - mx);
    }
}

/// Local maxima at least `min_distance` samples apart whose prominence is >= `min_prominence`.
pub fn find_peaks(x: &[f64], min_distance: usize, min_prominence: f64) -> Vec<usize> {
    let mut cand: Vec<usize> = (1..x.len().saturating_sub(1)).filter(|&i| x[i] > x[i - 1] && x[i] >= x[i + 1]).collect();
    // prominence: height above the higher of the two minima on each side up to a taller peak
    cand.retain(|&i| {
        let left = x[..i].iter().rev().take_while(|v| **v <= x[i]).fold(x[i], |m, v| m.min(*v));
        let right = x[i + 1..].iter().take_while(|v| **v <= x[i]).fold(x[i], |m, v| m.min(*v));
        x[i] - left.max(right) >= min_prominence
    });
    // enforce distance, keeping the taller peak
    let mut order = cand.clone();
    order.sort_by(|a, b| x[*b].total_cmp(&x[*a]));
    let mut kept: Vec<usize> = Vec::new();
    for i in order {
        if kept.iter().all(|k| k.abs_diff(i) >= min_distance) {
            kept.push(i);
        }
    }
    kept.sort_unstable();
    kept
}

/// Normalised autocorrelation for lags 0..=max_lag.
pub fn autocorr(x: &[f64], max_lag: usize) -> Vec<f64> {
    let m = mean(x).unwrap_or(0.0);
    let d: Vec<f64> = x.iter().map(|v| v - m).collect();
    let e: f64 = d.iter().map(|v| v * v).sum();
    if e <= 0.0 {
        return vec![0.0; max_lag + 1];
    }
    (0..=max_lag.min(x.len().saturating_sub(1))).map(|l| d[..d.len() - l].iter().zip(&d[l..]).map(|(a, b)| a * b).sum::<f64>() / e).collect()
}

/// Hann-windowed periodogram, bins k * fs / n for k in 0..=n/2.
pub fn periodogram(x: &[f64]) -> Vec<f64> {
    let n = x.len();
    if n < 4 {
        return Vec::new();
    }
    let w: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos()).collect();
    let m = x.iter().zip(&w).map(|(a, b)| a * b).sum::<f64>() / w.iter().sum::<f64>();
    (0..=n / 2)
        .map(|k| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, (v, wi)) in x.iter().zip(&w).enumerate() {
                let a = 2.0 * PI * k as f64 * i as f64 / n as f64;
                let y = (v - m) * wi;
                re += y * a.cos();
                im -= y * a.sin();
            }
            re * re + im * im
        })
        .collect()
}

/// Sum of periodogram power between lo and hi Hz.
pub fn band_power(power: &[f64], fs: f64, n: usize, lo: f64, hi: f64) -> f64 {
    power.iter().enumerate().filter(|(k, _)| (lo..=hi).contains(&(*k as f64 * fs / n as f64))).map(|(_, p)| p).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(fs: f64, f: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * PI * f * i as f64 / fs).sin()).collect()
    }

    #[test]
    fn bandpass_keeps_pulse_band_and_removes_drift() {
        let fs = 25.0;
        let x: Vec<f64> = sine(fs, 1.2, 500).iter().enumerate().map(|(i, v)| v + 0.05 * i as f64).collect();
        let y = bandpass(&x, fs, 0.5, 4.0);
        let mid = &y[100..400];
        let amp = mid.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!((0.8..1.2).contains(&amp), "amp {amp}");
        assert!(mean(mid).unwrap().abs() < 0.05);
    }

    #[test]
    fn peaks_respect_distance_and_prominence() {
        let x = sine(25.0, 1.0, 250);
        let p = find_peaks(&x, 10, 0.5);
        assert_eq!(p.len(), 10);
        assert!(p.windows(2).all(|w| w[1] - w[0] == 25));
    }

    #[test]
    fn autocorr_finds_period() {
        let x = sine(25.0, 1.25, 250);
        let ac = autocorr(&x, 40);
        let lag = (10..=40).max_by(|a, b| ac[*a].total_cmp(&ac[*b])).unwrap();
        assert_eq!(lag, 20);
    }
}
