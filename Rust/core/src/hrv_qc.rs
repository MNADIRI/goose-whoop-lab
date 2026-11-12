//! R-R quality control and time-domain HRV with explicit rejection statistics.
//!
//! WHOOP 5.0 R18 carries 0-4 R-R values per second, ~75 % of beats at night. Two beats are only
//! "successive" when they come from consecutive device seconds, so the input is a list of
//! contiguous runs. Each run is cleaned in four passes and split wherever a beat is removed, so
//! that a difference is never taken across a gap:
//!   0. split-beat merge: two consecutive intervals, one < 60 % of the local reference (median of
//!      +-5 neighbours), whose sum is within 15 % of it, are one beat detected twice -> summed
//!      (seen on the live stream, e.g. 655 + 576 ms in a 1150 ms rhythm);
//!   1. range: 300-2000 ms (20-200 bpm);
//!   2. Malik: |RR - previous accepted| / previous > threshold (Malik 1989; ectopic / missed beat);
//!   3. local median (NOOP HRVAnalyzer): |RR - median of +-2 neighbours| / median > threshold.
//! The threshold adapts to the local beat-to-beat variability (Lipponen & Tarvainen 2019: 5.2 x the
//! quartile deviation of the relative differences over +-45 beats), floored at 20 % and capped at
//! 45 %: deep respiratory sinus arrhythmia (slow breathing: 720 -> 1460 ms within one breath) is
//! physiology, not artefact, and a fixed 20 % rule removed up to 20 % of those beats.
//! Metrics are only produced with >= MIN_DIFFS successive differences, and always carry the counts.

use serde::Serialize;
use serde_json::{json, Value};

use crate::dsp::{mean, median, percentile, round, std_sample};
use crate::whoop5_records::Status;

pub const RR_MIN_MS: f64 = 300.0;
pub const RR_MAX_MS: f64 = 2000.0;
pub const MALIK_THRESHOLD: f64 = 0.20;
pub const MEDIAN_RADIUS: usize = 2;
pub const MEDIAN_THRESHOLD: f64 = 0.20;
pub const MIN_DIFFS: usize = 100;
pub const MIN_SEGMENT_BEATS: usize = 5;
pub const ADAPTIVE_K: f64 = 5.2;
pub const ADAPTIVE_RADIUS: usize = 45;
pub const THRESHOLD_CAP: f64 = 0.45;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Beat {
    pub ts: i64,
    pub rr_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RrQc {
    pub n_input: usize,
    pub merged_split_beats: usize,
    pub rejected_range: usize,
    pub rejected_malik: usize,
    pub rejected_median: usize,
    pub rejected_short_segment: usize,
    pub n_accepted: usize,
    pub n_successive_diffs: usize,
    pub segments: usize,
    /// accepted / input, including beats lost only because their run was shorter than 5 beats
    pub acceptance_pct: f64,
    /// (range + Malik + median rejections) / input: the artefact rate proper
    pub artifact_pct: f64,
    /// good: artefacts <= 5 % and >= 300 diffs; fair: >= MIN_DIFFS diffs; insufficient otherwise.
    pub grade: &'static str,
}

fn split_push(cur: &mut Vec<Beat>, out: &mut Vec<Vec<Beat>>, short: &mut usize) {
    if cur.len() >= MIN_SEGMENT_BEATS {
        out.push(std::mem::take(cur));
    } else {
        *short += cur.len();
        cur.clear();
    }
}

fn merge_split_beats(run: &[Beat], merged: &mut usize) -> Vec<Beat> {
    let mut out: Vec<Beat> = Vec::with_capacity(run.len());
    let mut i = 0;
    while i < run.len() {
        if i + 1 < run.len() {
            let (a, b) = (run[i].rr_ms, run[i + 1].rr_ms);
            let lo = i.saturating_sub(5);
            let hi = (i + 6).min(run.len() - 1);
            let neigh: Vec<f64> = (lo..=hi).filter(|j| *j != i && *j != i + 1).map(|j| run[j].rr_ms).filter(|v| (RR_MIN_MS..=RR_MAX_MS).contains(v)).collect();
            if neigh.len() >= 4 {
                let reference = median(&neigh).unwrap_or(0.0);
                if reference > 0.0 && a.min(b) < 0.6 * reference && ((a + b - reference) / reference).abs() <= 0.15 {
                    out.push(Beat { ts: run[i + 1].ts, rr_ms: a + b });
                    *merged += 1;
                    i += 2;
                    continue;
                }
            }
        }
        out.push(run[i]);
        i += 1;
    }
    out
}

/// Per-beat artefact threshold from the local spread of relative successive differences.
fn adaptive_thresholds(run: &[Beat]) -> Vec<f64> {
    let d: Vec<f64> = run.windows(2).map(|w| (w[1].rr_ms - w[0].rr_ms) / w[0].rr_ms).filter(|v| v.is_finite()).collect();
    (0..run.len())
        .map(|i| {
            if d.len() < 10 {
                return MALIK_THRESHOLD;
            }
            let c = i.min(d.len() - 1);
            let lo = c.saturating_sub(ADAPTIVE_RADIUS);
            let hi = (c + ADAPTIVE_RADIUS).min(d.len() - 1);
            let w = &d[lo..=hi];
            let qd = (percentile(w, 75.0).unwrap_or(0.0) - percentile(w, 25.0).unwrap_or(0.0)) / 2.0;
            (ADAPTIVE_K * qd).clamp(MALIK_THRESHOLD, THRESHOLD_CAP)
        })
        .collect()
}

/// Cleans contiguous runs of beats; returns the clean segments and the rejection statistics.
pub fn clean(runs: &[Vec<Beat>]) -> (Vec<Vec<Beat>>, RrQc) {
    let mut qc = RrQc::default();
    let mut stage1: Vec<(Vec<Beat>, Vec<f64>)> = Vec::new();
    let mut short = 0;
    for raw in runs {
        qc.n_input += raw.len();
        let run = merge_split_beats(raw, &mut qc.merged_split_beats);
        let in_range: Vec<Beat> = run.iter().copied().filter(|b| (RR_MIN_MS..=RR_MAX_MS).contains(&b.rr_ms)).collect();
        let th_all = adaptive_thresholds(&in_range);
        let mut k = 0;
        let mut cur: Vec<Beat> = Vec::new();
        let mut cur_th: Vec<f64> = Vec::new();
        let flush = |cur: &mut Vec<Beat>, cur_th: &mut Vec<f64>, out: &mut Vec<(Vec<Beat>, Vec<f64>)>, short: &mut usize| {
            if cur.len() >= MIN_SEGMENT_BEATS {
                out.push((std::mem::take(cur), std::mem::take(cur_th)));
            } else {
                *short += cur.len();
                cur.clear();
                cur_th.clear();
            }
        };
        for b in &run {
            if !(RR_MIN_MS..=RR_MAX_MS).contains(&b.rr_ms) {
                qc.rejected_range += 1;
                flush(&mut cur, &mut cur_th, &mut stage1, &mut short);
                continue;
            }
            let th = th_all.get(k).copied().unwrap_or(MALIK_THRESHOLD);
            k += 1;
            if let Some(prev) = cur.last() {
                if ((b.rr_ms - prev.rr_ms) / prev.rr_ms).abs() > th {
                    qc.rejected_malik += 1;
                    flush(&mut cur, &mut cur_th, &mut stage1, &mut short);
                    continue;
                }
            }
            cur.push(*b);
            cur_th.push(th);
        }
        flush(&mut cur, &mut cur_th, &mut stage1, &mut short);
    }
    let mut out = Vec::new();
    for (seg, th) in stage1 {
        let mut cur: Vec<Beat> = Vec::new();
        for i in 0..seg.len() {
            let lo = i.saturating_sub(MEDIAN_RADIUS);
            let hi = (i + MEDIAN_RADIUS).min(seg.len() - 1);
            let neigh: Vec<f64> = (lo..=hi).filter(|j| *j != i).map(|j| seg[j].rr_ms).collect();
            let med = median(&neigh).unwrap_or(seg[i].rr_ms);
            if neigh.len() >= 2 && ((seg[i].rr_ms - med) / med).abs() > th[i].max(MEDIAN_THRESHOLD) {
                qc.rejected_median += 1;
                split_push(&mut cur, &mut out, &mut short);
                continue;
            }
            cur.push(seg[i]);
        }
        split_push(&mut cur, &mut out, &mut short);
    }
    qc.rejected_short_segment = short;
    qc.n_accepted = out.iter().map(Vec::len).sum();
    qc.n_successive_diffs = out.iter().map(|s| s.len() - 1).sum();
    qc.segments = out.len();
    qc.acceptance_pct = if qc.n_input > 0 { round(100.0 * qc.n_accepted as f64 / qc.n_input as f64, 1) } else { 0.0 };
    let artifacts = qc.rejected_range + qc.rejected_malik + qc.rejected_median;
    qc.artifact_pct = if qc.n_input > 0 { round(100.0 * artifacts as f64 / qc.n_input as f64, 2) } else { 0.0 };
    qc.grade = if qc.n_successive_diffs >= 300 && qc.artifact_pct <= 5.0 {
        "good"
    } else if qc.n_successive_diffs >= MIN_DIFFS {
        "fair"
    } else {
        "insufficient"
    };
    (out, qc)
}

fn diffs(segs: &[Vec<Beat>]) -> Vec<f64> {
    segs.iter().flat_map(|s| s.windows(2).map(|w| w[1].rr_ms - w[0].rr_ms)).collect()
}

/// Time-domain HRV (Task Force 1996) on clean segments. Null metrics below MIN_DIFFS.
pub fn time_domain(segs: &[Vec<Beat>], qc: &RrQc) -> Value {
    let d = diffs(segs);
    let nn: Vec<f64> = segs.iter().flatten().map(|b| b.rr_ms).collect();
    if d.len() < MIN_DIFFS {
        return json!({"status": Status::Unavailable, "qc": qc});
    }
    let rmssd = (d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64).sqrt();
    let mean_nn = mean(&nn).unwrap_or(0.0);
    json!({
        "rmssd_ms": round(rmssd, 1),
        "ln_rmssd": round(rmssd.ln(), 3),
        "sdnn_ms": round(std_sample(&nn).unwrap_or(0.0), 1),
        "sdsd_ms": round(std_sample(&d).unwrap_or(0.0), 1),
        "pnn50_pct": round(100.0 * d.iter().filter(|x| x.abs() > 50.0).count() as f64 / d.len() as f64, 1),
        "pnn20_pct": round(100.0 * d.iter().filter(|x| x.abs() > 20.0).count() as f64 / d.len() as f64, 1),
        "mean_nn_ms": mean_nn.round(),
        "mean_hr_from_nn_bpm": round(60_000.0 / mean_nn.max(1.0), 1),
        "beats": nn.len(),
        "successive_diffs": d.len(),
        "status": Status::Experimental,
        "qc": qc,
    })
}

/// RMSSD in consecutive windows (default 5 min) for rolling / nocturnal profiles.
pub fn rolling_rmssd(segs: &[Vec<Beat>], window_s: i64, min_diffs: usize) -> Value {
    let Some(t0) = segs.iter().flatten().map(|b| b.ts).min() else { return Value::Null };
    let mut buckets: std::collections::BTreeMap<i64, Vec<f64>> = Default::default();
    for s in segs {
        for w in s.windows(2) {
            buckets.entry((w[1].ts - t0) / window_s).or_default().push(w[1].rr_ms - w[0].rr_ms);
        }
    }
    let series: Vec<(i64, f64, usize)> = buckets
        .iter()
        .filter(|(_, d)| d.len() >= min_diffs)
        .map(|(k, d)| (t0 + k * window_s, (d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64).sqrt(), d.len()))
        .collect();
    if series.is_empty() {
        return Value::Null;
    }
    let vals: Vec<f64> = series.iter().map(|s| s.1).collect();
    json!({
        "window_s": window_s,
        "windows": series.len(),
        "median_ms": round(median(&vals).unwrap(), 1),
        "p10_ms": round(percentile(&vals, 10.0).unwrap(), 1),
        "p90_ms": round(percentile(&vals, 90.0).unwrap(), 1),
        "series": series.iter().map(|(t, v, n)| json!([t, round(*v, 1), n])).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(vals: &[f64]) -> Vec<Beat> {
        vals.iter().enumerate().map(|(i, v)| Beat { ts: i as i64, rr_ms: *v }).collect()
    }

    #[test]
    fn rejects_ectopic_and_out_of_range_with_counts() {
        let mut v: Vec<f64> = (0..400).map(|i| 1000.0 + if i % 2 == 0 { 20.0 } else { -20.0 }).collect();
        v[100] = 600.0; // premature beat
        v[200] = 2500.0; // out of range
        let (segs, qc) = clean(&[run(&v)]);
        assert_eq!(qc.n_input, 400);
        assert_eq!(qc.rejected_range, 1);
        assert_eq!(qc.rejected_malik, 1);
        assert_eq!(qc.n_accepted, 398);
        assert_eq!(qc.grade, "good");
        let m = time_domain(&segs, &qc);
        assert_eq!(m["rmssd_ms"], 40.0);
        assert_eq!(m["mean_nn_ms"], 1000.0);
        // no difference is taken across a removed beat
        assert_eq!(m["successive_diffs"], 398 - segs.len());
    }

    #[test]
    fn keeps_deep_sinus_arrhythmia_and_merges_split_beats() {
        // real live stream, 12 Nov sensor session: slow breathing (8 /min), then a rest run with
        // two split beats (655 + 576, 325 + 854)
        let slow = [1230., 1250., 1123., 958., 833., 811., 833., 1001., 1260., 1370., 1367., 1271., 1179., 1119., 1050., 946., 817., 777., 719., 743., 795., 1457., 1295., 1252., 1276., 1238., 1108., 968., 879., 814., 774., 923., 1085., 1117., 1140., 1101., 975., 875., 834., 866., 947., 1004., 1032., 1030., 1015., 906., 843., 756., 755., 774., 972., 1197., 1188., 1213., 1049., 917., 838., 788.];
        let (_, qc) = clean(&[run(&slow)]);
        assert!(qc.rejected_malik + qc.rejected_median <= 3, "{qc:?}");
        let rest = [1125., 1159., 1122., 1103., 1134., 1141., 1138., 1172., 1101., 1295., 1346., 1233., 655., 576., 1196., 1099., 1097., 1194., 1096., 1184., 1188., 1185., 1220., 1164., 1181., 1173., 1113., 1139., 1181., 1148., 1189., 1197., 1071., 1094., 1096., 1035., 1119., 1142., 1120., 1229., 1151., 1093., 325., 854., 1029., 1036., 1096., 1145., 1211., 1257., 1227.];
        let (segs, qc) = clean(&[run(&rest)]);
        assert_eq!(qc.merged_split_beats, 2, "{qc:?}");
        assert!(segs.iter().flatten().all(|b| b.rr_ms > 900.0));
        // a stable rhythm keeps the strict 20 % rule
        let mut v: Vec<f64> = (0..200).map(|i| 1000.0 + if i % 2 == 0 { 10.0 } else { -10.0 }).collect();
        v[100] = 1260.0;
        let (_, qc) = clean(&[run(&v)]);
        assert_eq!(qc.rejected_malik + qc.rejected_median, 1);
    }

    #[test]
    fn too_few_beats_is_unavailable_not_zero() {
        let (segs, qc) = clean(&[run(&[1000.0; 50])]);
        let m = time_domain(&segs, &qc);
        assert_eq!(m["status"], "UNAVAILABLE");
        assert!(m.get("rmssd_ms").is_none());
        assert_eq!(qc.grade, "insufficient");
    }
}
