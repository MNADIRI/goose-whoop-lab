//! Personal baselines and the recovery composite, ported from NOOP (StrandAnalytics Baselines /
//! RecoveryScorer, itself from its Python server). Every intermediate is returned.
//!
//! Baseline = winsorised EWMA: centre half-life 14 nights, spread (EWMA of |deviation|) half-life
//! 21 nights, floored per metric; values outside physiological bounds are skipped, values beyond
//! 5 spreads are seen but not folded once seeded, others are clamped to +-3 spreads before folding.
//! Status: calibrating (< 4 valid nights), provisional (4-13), trusted (>= 14), stale (> 14 missing).
//! Robust z = (value - baseline) / (1.253 x spread).
//!
//! Recovery = logistic(1.6 x (Z + 0.2)) x 100 with Z the weight-renormalised sum of
//! 0.60 z(RMSSD) - 0.20 z(resting HR) - 0.05 z(respiration) + 0.15 z(sleep efficiency vs 85 %, /12 %).
//! Z = 0 gives ~58 %. No score until the HRV baseline is at least provisional.

use serde::Serialize;
use serde_json::{json, Value};

use crate::dsp::round;

pub const WINSOR_K: f64 = 3.0;
pub const HARD_OUTLIER_K: f64 = 5.0;
pub const MIN_NIGHTS_SEED: usize = 4;
pub const MIN_NIGHTS_TRUST: usize = 14;
pub const STALE_NIGHTS: usize = 14;

#[derive(Debug, Clone, Copy)]
pub struct MetricCfg {
    pub min: f64,
    pub max: f64,
    pub floor_spread: f64,
    pub half_life_centre: f64,
    pub half_life_spread: f64,
}

pub const HRV: MetricCfg = MetricCfg { min: 5.0, max: 250.0, floor_spread: 5.0, half_life_centre: 14.0, half_life_spread: 21.0 };
pub const RESTING_HR: MetricCfg = MetricCfg { min: 30.0, max: 120.0, floor_spread: 2.0, half_life_centre: 14.0, half_life_spread: 21.0 };
pub const RESP: MetricCfg = MetricCfg { min: 4.0, max: 40.0, floor_spread: 0.5, half_life_centre: 14.0, half_life_spread: 21.0 };
pub const SKIN_TEMP: MetricCfg = MetricCfg { min: 20.0, max: 42.0, floor_spread: 0.3, half_life_centre: 14.0, half_life_spread: 21.0 };

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Baseline {
    pub centre: f64,
    pub spread: f64,
    pub n_valid: usize,
    pub nights_since_update: usize,
    pub status: &'static str,
}

fn lambda(half_life: f64) -> f64 {
    1.0 - 0.5f64.powf(1.0 / half_life)
}

fn status(n_valid: usize, since: usize) -> &'static str {
    if since > STALE_NIGHTS && n_valid >= MIN_NIGHTS_SEED {
        "stale"
    } else if n_valid < MIN_NIGHTS_SEED {
        "calibrating"
    } else if n_valid < MIN_NIGHTS_TRUST {
        "provisional"
    } else {
        "trusted"
    }
}

pub fn update(state: Option<Baseline>, value: Option<f64>, cfg: &MetricCfg) -> Baseline {
    let valid = value.filter(|v| (cfg.min..=cfg.max).contains(v));
    let Some(s) = state.filter(|s| s.n_valid > 0) else {
        return match valid {
            Some(v) => Baseline { centre: v, spread: cfg.floor_spread, n_valid: 1, nights_since_update: 0, status: "calibrating" },
            None => Baseline { centre: (cfg.min + cfg.max) / 2.0, spread: cfg.floor_spread, n_valid: 0, nights_since_update: state.map_or(1, |s| s.nights_since_update + 1), status: "calibrating" },
        };
    };
    let Some(v) = valid else {
        let m = s.nights_since_update + 1;
        return Baseline { nights_since_update: m, status: status(s.n_valid, m), ..s };
    };
    if s.n_valid >= MIN_NIGHTS_SEED && (v - s.centre).abs() > HARD_OUTLIER_K * s.spread {
        return Baseline { nights_since_update: 0, status: status(s.n_valid, 0), ..s };
    }
    let clamped = v.clamp(s.centre - WINSOR_K * s.spread, s.centre + WINSOR_K * s.spread);
    let (lb, ls) = (lambda(cfg.half_life_centre), lambda(cfg.half_life_spread));
    let centre = lb * clamped + (1.0 - lb) * s.centre;
    let spread = (ls * (v - centre).abs() + (1.0 - ls) * s.spread).max(cfg.floor_spread);
    Baseline { centre, spread, n_valid: s.n_valid + 1, nights_since_update: 0, status: status(s.n_valid + 1, 0) }
}

/// Folds previous nights (oldest first; None = missing night).
pub fn fold(values: &[Option<f64>], cfg: &MetricCfg) -> Option<Baseline> {
    values.iter().fold(None, |st, v| Some(update(st, *v, cfg)))
}

pub fn z(value: f64, b: &Baseline) -> f64 {
    (value - b.centre) / (1.253 * b.spread)
}

pub fn deviation_json(value: Option<f64>, b: Option<Baseline>, digits: i32) -> Value {
    match (value, b) {
        (Some(v), Some(b)) if b.n_valid > 0 => json!({
            "value": v, "baseline": round(b.centre, digits), "spread": round(b.spread, digits + 1),
            "delta": round(v - b.centre, digits), "pct": round(100.0 * (v / b.centre - 1.0), 1),
            "z": round(z(v, &b), 2), "nights": b.n_valid, "status": b.status,
        }),
        (_, Some(b)) => json!({"value": value, "nights": b.n_valid, "status": b.status}),
        _ => json!({"value": value, "nights": 0, "status": "calibrating"}),
    }
}

pub const W_HRV: f64 = 0.60;
pub const W_RHR: f64 = 0.20;
pub const W_RESP: f64 = 0.05;
pub const W_SLEEP: f64 = 0.15;
pub const LOGISTIC_K: f64 = 1.6;
pub const LOGISTIC_Z0: f64 = -0.20;

pub struct RecoveryInputs {
    pub rmssd: Option<f64>,
    pub resting_hr: Option<f64>,
    pub resp: Option<f64>,
    pub efficiency_pct: Option<f64>,
    pub hrv_base: Option<Baseline>,
    pub rhr_base: Option<Baseline>,
    pub resp_base: Option<Baseline>,
}

pub fn recovery(i: &RecoveryInputs) -> Value {
    let hrv_ready = i.hrv_base.is_some_and(|b| b.n_valid >= MIN_NIGHTS_SEED);
    let mut terms: Vec<(&str, f64, f64, Value)> = Vec::new(); // name, weight, signed z, detail
    if let (Some(v), Some(b)) = (i.rmssd, i.hrv_base.filter(|b| b.n_valid >= MIN_NIGHTS_SEED)) {
        terms.push(("hrv_rmssd", W_HRV, z(v, &b), deviation_json(Some(v), Some(b), 1)));
    }
    if let (Some(v), Some(b)) = (i.resting_hr, i.rhr_base.filter(|b| b.n_valid >= MIN_NIGHTS_SEED)) {
        terms.push(("resting_hr", W_RHR, -z(v, &b), deviation_json(Some(v), Some(b), 1)));
    }
    if let (Some(v), Some(b)) = (i.resp, i.resp_base.filter(|b| b.n_valid >= MIN_NIGHTS_SEED)) {
        terms.push(("respiration", W_RESP, -z(v, &b), deviation_json(Some(v), Some(b), 1)));
    }
    if let Some(e) = i.efficiency_pct {
        terms.push(("sleep_efficiency", W_SLEEP, (e / 100.0 - 0.85) / 0.12, json!({"value": e, "centre": 85.0, "scale": 12.0})));
    }
    let nights = i.hrv_base.map_or(0, |b| b.n_valid);
    if !hrv_ready || i.rmssd.is_none() {
        return json!({
            "score": null, "status": "UNAVAILABLE",
            "reason": if i.rmssd.is_none() { "VFC insuffisante cette nuit".to_string() } else { format!("Référence en cours ({nights}/{MIN_NIGHTS_SEED} nuits)") },
            "baseline_nights": nights,
        });
    }
    let wsum: f64 = terms.iter().map(|t| t.1).sum();
    let zc: f64 = terms.iter().map(|t| t.1 * t.2).sum::<f64>() / wsum;
    let score = 100.0 / (1.0 + (-LOGISTIC_K * (zc - LOGISTIC_Z0)).exp());
    let comp: serde_json::Map<String, Value> = terms
        .iter()
        .map(|(n, w, zz, d)| (n.to_string(), json!({"weight": w, "weight_renormalised": round(w / wsum, 3), "signed_z": round(*zz, 2), "contribution": round(w / wsum * zz, 3), "detail": d})))
        .collect();
    json!({
        "score": score.round(),
        "composite_z": round(zc, 3),
        "band": if score < 34.0 { "red" } else if score < 67.0 { "yellow" } else { "green" },
        "components": comp,
        "baseline_nights": nights,
        "baseline_status": i.hrv_base.map(|b| b.status),
        "z_ln_rmssd": terms.iter().find(|t| t.0 == "hrv_rmssd").map(|t| round(t.2, 2)),
        "z_resting_hr": terms.iter().find(|t| t.0 == "resting_hr").map(|t| round(t.2, 2)),
        "method": "NOOP RecoveryScorer port: winsorised-EWMA baselines, weights 0.60/0.20/0.05/0.15, logistic k=1.6 z0=-0.2",
        "status": "EXPERIMENTAL",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_statuses_and_outlier_gate() {
        let vals: Vec<Option<f64>> = vec![Some(50.0), Some(52.0), None, Some(48.0), Some(51.0)];
        let b = fold(&vals, &HRV).unwrap();
        assert_eq!(b.n_valid, 4);
        assert_eq!(b.status, "provisional");
        let after = update(Some(b), Some(240.0), &HRV); // > 5 spreads: seen, not folded
        assert_eq!(after.centre, b.centre);
        assert_eq!(update(Some(b), Some(1.0), &HRV).nights_since_update, 1); // implausible: skipped
    }

    #[test]
    fn recovery_needs_baseline_and_exposes_components() {
        let base = fold(&[Some(50.0); 5], &HRV);
        let rhr = fold(&[Some(50.0); 5], &RESTING_HR);
        let r = recovery(&RecoveryInputs { rmssd: Some(50.0), resting_hr: Some(50.0), resp: None, efficiency_pct: Some(85.0), hrv_base: base, rhr_base: rhr, resp_base: None });
        assert_eq!(r["score"], 58.0);
        assert_eq!(r["components"]["hrv_rmssd"]["weight_renormalised"], 0.632);
        let cold = recovery(&RecoveryInputs { rmssd: Some(50.0), resting_hr: None, resp: None, efficiency_pct: None, hrv_base: fold(&[Some(50.0); 2], &HRV), rhr_base: None, resp_base: None });
        assert!(cold["score"].is_null());
    }
}
