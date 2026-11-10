//! whoop-lab on-device analysis: WHOOP 5.0 R18 history -> per-day physiological results.
//!
//! This is the single source of the physiological values the app displays. It is a port of the
//! reference Python lab (`lab/analyze.py`, validated on real data); both must give the same values.
//!
//! Pipeline:
//!   decoded_frames (type 47, 124 B)  --sync_biometric_table-->  whoop_lab_biometric (1 row / second)
//!   whoop_lab_biometric              --analyze-->               whoop_lab_days (one JSON per local day)
//!
//! Each day is recomputed only when its input changed (row count / last second) or it is today or
//! yesterday, so a run costs one or two days of work.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::baselines::{self, Baseline};
use crate::hrv_qc::{self, Beat};
use crate::{optical, training, whoop5_store};
use crate::{protocol::parse_r18_history_fields, GooseError, GooseResult};

pub const RESULTS_SCHEMA: &str = "whoop-lab.results.v2";
const ALGORITHM_VERSION: &str = "6"; // 5: signed R26 base, optical gain state; 6: on-strap SpO2
const DAY: i64 = 86_400;
const EPOCH: i64 = 30;
const CK_WEIGHTS: [f64; 7] = [106.0, 54.0, 58.0, 76.0, 230.0, 74.0, 67.0];
const MIN_PLAUSIBLE_TS: i64 = 1_577_836_800;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    pub age: Option<f64>,
    pub sex: Option<String>,
    pub weight_kg: Option<f64>,
    pub height_cm: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnalyzeOptions {
    pub database_path: String,
    /// Seconds east of UTC for local day boundaries.
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
    #[serde(default)]
    pub profile: Profile,
}

// ------------------------------------------------------------------ storage

fn ensure_tables(conn: &Connection) -> GooseResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS whoop_lab_biometric (
            ts INTEGER PRIMARY KEY,
            hr INTEGER NOT NULL,
            rr TEXT NOT NULL,
            gx INTEGER, gy INTEGER, gz INTEGER,
            dyn INTEGER,
            steps INTEGER NOT NULL,
            activity TEXT,
            skin_centi INTEGER,
            band TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS whoop_lab_days (
            date_key TEXT PRIMARY KEY,
            input_count INTEGER NOT NULL,
            input_last_ts INTEGER NOT NULL,
            algorithm_version TEXT NOT NULL,
            result_json TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );
        CREATE TABLE IF NOT EXISTS whoop_lab_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        "#,
    )?;
    Ok(())
}

fn f32_at(payload: &[u8], offset: usize) -> Option<f32> {
    let bytes: [u8; 4] = payload.get(offset..offset + 4)?.try_into().ok()?;
    let value = f32::from_le_bytes(bytes);
    value.is_finite().then_some(value)
}

/// Copies new R18 records from decoded_frames into the typed 1 Hz table (cursor on rowid).
pub fn sync_biometric_table(conn: &Connection) -> GooseResult<usize> {
    ensure_tables(conn)?;
    let cursor: i64 = conn
        .query_row("SELECT value FROM whoop_lab_state WHERE key = 'decoded_rowid'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rows: Vec<(i64, String)> = {
        let mut st = conn.prepare(
            "SELECT rowid, payload_hex FROM decoded_frames WHERE rowid > ?1 AND packet_type = 47 AND raw_len = 124 ORDER BY rowid",
        )?;
        let mapped = st.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<Result<_, _>>()?
    };
    let max_rowid: i64 = conn.query_row("SELECT IFNULL(MAX(rowid), 0) FROM decoded_frames", [], |r| r.get(0))?;
    let tx = conn.unchecked_transaction()?;
    let mut inserted = 0;
    {
        let mut ins = tx.prepare_cached(
            "INSERT OR IGNORE INTO whoop_lab_biometric VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        for (_, payload_hex) in rows {
            let Ok(payload) = hex::decode(payload_hex.trim()) else { continue };
            let Some(fields) = parse_r18_history_fields(&payload) else { continue };
            let Some(ts) = payload.get(7..11).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64) else { continue };
            if ts < MIN_PLAUSIBLE_TS {
                continue;
            }
            let mg = |o: usize| f32_at(&payload, o).map(|v| (v * 1000.0).round() as i64);
            let rr = fields.rr_intervals_ms.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
            inserted += ins.execute(params![
                ts,
                fields.heart_rate_bpm,
                rr,
                mg(37),
                mg(41),
                mg(45),
                mg(33).filter(|v| (0..=8000).contains(v)),
                fields.step_count,
                fields.activity_class,
                fields.skin_temperature_centi_c,
                fields.band_state,
            ])?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO whoop_lab_state (key, value) VALUES ('decoded_rowid', ?1)",
        [max_rowid.max(cursor).to_string()],
    )?;
    tx.commit()?;
    Ok(inserted)
}

#[derive(Debug, Clone)]
struct Sample {
    ts: i64,
    hr: f64,
    rr: Vec<f64>,
    grav: Option<[f64; 3]>,
    dyn_g: Option<f64>,
    steps: i64,
    activity: Option<String>,
    skin: Option<f64>,
    band: String,
}

fn load_samples(conn: &Connection, start: i64, end: i64) -> GooseResult<Vec<Sample>> {
    let mut st = conn.prepare_cached(
        "SELECT ts, hr, rr, gx, gy, gz, dyn, steps, activity, skin_centi, band FROM whoop_lab_biometric WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = st.query_map(params![start, end], |r| {
        let rr: String = r.get(2)?;
        let g = (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?);
        Ok(Sample {
            ts: r.get(0)?,
            hr: r.get::<_, i64>(1)? as f64,
            rr: rr.split(',').filter_map(|v| v.parse().ok()).collect(),
            grav: match g {
                (Some(x), Some(y), Some(z)) => Some([x as f64 / 1000.0, y as f64 / 1000.0, z as f64 / 1000.0]),
                _ => None,
            },
            dyn_g: r.get::<_, Option<i64>>(6)?.map(|v| v as f64 / 1000.0),
            steps: r.get(7)?,
            activity: r.get(8)?,
            skin: r.get::<_, Option<i64>>(9)?.map(|v| v as f64 / 100.0),
            band: r.get(10)?,
        })
    })?;
    rows.collect::<Result<_, _>>().map_err(GooseError::from)
}

// ------------------------------------------------------------------ helpers

fn mean(v: &[f64]) -> Option<f64> {
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let rank = p / 100.0 * (v.len() - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    Some(v[lo] + (v[hi] - v[lo]) * (rank - lo as f64))
}

fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 50.0)
}

fn round(v: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (v * k).round() / k
}

fn std_sample(v: &[f64]) -> Option<f64> {
    if v.len() < 2 {
        return None;
    }
    let m = mean(v)?;
    Some((v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt())
}

// ------------------------------------------------------------------ HRV / respiration

/// Runs of consecutive device seconds carrying R-R, split at implausible values or >20 % jumps.
#[allow(dead_code)]
fn successive_segments(samples: &[Sample]) -> Vec<Vec<f64>> {
    let mut segs = Vec::new();
    let mut cur: Vec<f64> = Vec::new();
    let mut prev: Option<i64> = None;
    let flush = |cur: &mut Vec<f64>, segs: &mut Vec<Vec<f64>>| {
        if cur.len() >= 5 {
            segs.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for s in samples {
        if s.rr.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            if s.ts != p && s.ts != p + 1 {
                flush(&mut cur, &mut segs);
            }
        }
        prev = Some(s.ts);
        for &v in &s.rr {
            let plausible = (300.0..=2000.0).contains(&v);
            if !plausible || cur.last().is_some_and(|l| ((v - l) / l).abs() > 0.2) {
                flush(&mut cur, &mut segs);
                if !plausible {
                    continue;
                }
            }
            cur.push(v);
        }
    }
    flush(&mut cur, &mut segs);
    segs
}

/// Contiguous runs of beats (consecutive device seconds), uncleaned: hrv_qc does the cleaning.
fn beat_runs(samples: &[Sample]) -> Vec<Vec<Beat>> {
    let mut runs: Vec<Vec<Beat>> = Vec::new();
    let mut prev: Option<i64> = None;
    for s in samples {
        if s.rr.is_empty() {
            continue;
        }
        if prev.is_none_or(|p| s.ts != p && s.ts != p + 1) {
            runs.push(Vec::new());
        }
        prev = Some(s.ts);
        runs.last_mut().unwrap().extend(s.rr.iter().map(|v| Beat { ts: s.ts, rr_ms: *v }));
    }
    runs
}

/// HRV with QC; `null` when insufficient (the app decodes a present object as complete metrics).
fn hrv_with_qc(samples: &[Sample]) -> (Value, Value, Vec<Vec<Beat>>) {
    let (segs, qc) = hrv_qc::clean(&beat_runs(samples));
    let m = hrv_qc::time_domain(&segs, &qc);
    let qc_json = serde_json::to_value(&qc).unwrap_or(Value::Null);
    if m.get("rmssd_ms").is_some() { (m, qc_json, segs) } else { (Value::Null, qc_json, segs) }
}

#[allow(dead_code)]
fn hrv_metrics(segs: &[Vec<f64>]) -> Value {
    let diffs: Vec<f64> = segs.iter().flat_map(|s| s.windows(2).map(|w| w[1] - w[0])).collect();
    if diffs.len() < 100 {
        return Value::Null;
    }
    let nn: Vec<f64> = segs.iter().flatten().copied().collect();
    let rmssd = (diffs.iter().map(|d| d * d).sum::<f64>() / diffs.len() as f64).sqrt();
    json!({
        "rmssd_ms": round(rmssd, 1),
        "ln_rmssd": round(rmssd.ln(), 3),
        "sdnn_ms": round(std_sample(&nn).unwrap_or(0.0), 1),
        "pnn50_pct": round(100.0 * diffs.iter().filter(|d| d.abs() > 50.0).count() as f64 / diffs.len() as f64, 1),
        "mean_nn_ms": mean(&nn).unwrap_or(0.0).round(),
        "beats": nn.len(),
        "successive_diffs": diffs.len(),
    })
}

/// Respiratory sinus arrhythmia on a 1 Hz tachogram (mean R-R per second), 120 s windows with
/// >= 80 % coverage, linear detrend + Hann, dominant peak 0.13-0.45 Hz >= 3x the band median.
fn respiratory_rate(samples: &[Sample]) -> Value {
    let with_rr: Vec<&Sample> = samples.iter().filter(|s| s.rr.iter().any(|v| (300.0..=2000.0).contains(v))).collect();
    let (Some(first), Some(last)) = (with_rr.first(), with_rr.last()) else { return Value::Null };
    let t0 = first.ts;
    let n = (last.ts - t0 + 1) as usize;
    let mut x = vec![f64::NAN; n];
    for s in &with_rr {
        let vals: Vec<f64> = s.rr.iter().copied().filter(|v| (300.0..=2000.0).contains(v)).collect();
        x[(s.ts - t0) as usize] = mean(&vals).unwrap_or(f64::NAN);
    }
    let mut estimates = Vec::new();
    let mut w0 = 0;
    while w0 + 120 < n {
        let w = &x[w0..w0 + 120];
        w0 += 60;
        let known: Vec<(usize, f64)> = w.iter().copied().enumerate().filter(|(_, v)| v.is_finite()).collect();
        if (known.len() as f64) < 0.8 * 120.0 {
            continue;
        }
        let mut filled = vec![0.0; 120];
        for (i, slot) in filled.iter_mut().enumerate() {
            *slot = interp(&known, i as f64);
        }
        if filled.windows(2).any(|p| (p[1] - p[0]).abs() > 0.25 * p[0]) {
            continue;
        }
        detrend_linear(&mut filled);
        let power = hann_periodogram(&filled);
        // bins k/120 Hz; band 0.13-0.45 Hz
        let band: Vec<(usize, f64)> = (0..power.len()).filter(|k| { let f = *k as f64 / 120.0; (0.13..=0.45).contains(&f) }).map(|k| (k, power[k])).collect();
        let vals: Vec<f64> = band.iter().map(|(_, p)| *p).collect();
        let Some(med) = median(&vals) else { continue };
        let Some(&(k, peak)) = band.iter().max_by(|a, b| a.1.total_cmp(&b.1)) else { continue };
        if peak < 3.0 * med {
            continue;
        }
        estimates.push(k as f64 / 120.0 * 60.0);
    }
    if estimates.len() < 5 {
        return Value::Null;
    }
    json!({
        "rpm": round(median(&estimates).unwrap(), 1),
        "rpm_iqr": [round(percentile(&estimates, 25.0).unwrap(), 1), round(percentile(&estimates, 75.0).unwrap(), 1)],
        "windows": estimates.len(),
    })
}

fn interp(known: &[(usize, f64)], i: f64) -> f64 {
    match known.iter().position(|(k, _)| *k as f64 >= i) {
        Some(0) => known[0].1,
        Some(p) => {
            let (x0, y0) = known[p - 1];
            let (x1, y1) = known[p];
            y0 + (y1 - y0) * (i - x0 as f64) / (x1 as f64 - x0 as f64)
        }
        None => known.last().map(|k| k.1).unwrap_or(0.0),
    }
}

fn detrend_linear(x: &mut [f64]) {
    let n = x.len() as f64;
    let mx = (n - 1.0) / 2.0;
    let my = x.iter().sum::<f64>() / n;
    let mut sxy = 0.0;
    let mut sxx = 0.0;
    for (i, v) in x.iter().enumerate() {
        sxy += (i as f64 - mx) * (v - my);
        sxx += (i as f64 - mx).powi(2);
    }
    let slope = sxy / sxx;
    for (i, v) in x.iter_mut().enumerate() {
        *v -= my + slope * (i as f64 - mx);
    }
}

fn hann_periodogram(x: &[f64]) -> Vec<f64> {
    let n = x.len();
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

// ------------------------------------------------------------------ sleep

fn cole_kripke(counts: &[f64]) -> Vec<bool> {
    let a: Vec<f64> = counts.iter().map(|c| (c / 1000.0).clamp(0.0, 300.0)).collect();
    (0..a.len())
        .map(|i| {
            let mut si = 0.0;
            for (j, w) in CK_WEIGHTS.iter().enumerate() {
                let idx = i as i64 + j as i64 - 4;
                if idx >= 0 && (idx as usize) < a.len() {
                    si += w * a[idx as usize];
                }
            }
            0.001 * si < 1.0
        })
        .collect()
}

struct SleepPeriod {
    bed: usize,
    onset: usize,
    wake: usize,
}

fn sleep_period(s: &[Sample]) -> Option<SleepPeriod> {
    let idx: Vec<usize> = (0..s.len()).filter(|&i| s[i].band == "sleep").collect();
    if idx.len() < 1800 {
        return None;
    }
    let mut blocks = Vec::new();
    let mut start = idx[0];
    for w in idx.windows(2) {
        if s[w[1]].ts - s[w[0]].ts > 1200 {
            blocks.push((start, w[0]));
            start = w[1];
        }
    }
    blocks.push((start, *idx.last().unwrap()));
    let (onset, wake) = blocks.into_iter().max_by_key(|(a, b)| s[*b].ts - s[*a].ts)?;
    let mut bed = onset;
    while bed > 0 && (s[bed - 1].band == "still" || s[bed - 1].band == "sleep") && s[onset].ts - s[bed - 1].ts < 3 * 3600 {
        bed -= 1;
    }
    Some(SleepPeriod { bed, onset, wake })
}

fn stage_epochs(s: &[Sample], onset: usize, wake: usize) -> (Vec<&'static str>, i64) {
    let t0 = s[onset].ts;
    let n = ((s[wake].ts - t0) / EPOCH + 1) as usize;
    let mut counts = vec![0.0; n];
    let mut moving = vec![0.0; n];
    let mut seen: Vec<f64> = vec![0.0; n];
    let mut hr_sum = vec![0.0; n];
    let mut rr_by_epoch: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut prev_g: Option<[f64; 3]> = None;
    for smp in &s[onset..=wake] {
        let k = ((smp.ts - t0) / EPOCH) as usize;
        let dg = match (prev_g, smp.grav) {
            (Some(p), Some(g)) => ((g[0] - p[0]).powi(2) + (g[1] - p[1]).powi(2) + (g[2] - p[2]).powi(2)).sqrt(),
            _ => 0.0,
        };
        if smp.grav.is_some() {
            prev_g = smp.grav;
        }
        counts[k] += (dg - 0.01).max(0.0) * 1000.0;
        moving[k] += if dg > 0.01 { 1.0 } else { 0.0 };
        seen[k] += 1.0;
        hr_sum[k] += smp.hr;
        rr_by_epoch[k].extend(smp.rr.iter().copied());
    }
    let movefrac: Vec<f64> = (0..n).map(|k| moving[k] / seen[k].max(1.0)).collect();
    let hr: Vec<f64> = (0..n).map(|k| if seen[k] > 0.0 { hr_sum[k] / seen[k] } else { f64::NAN }).collect();
    let ck = cole_kripke(&counts);
    let loc_rmssd: Vec<f64> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(5);
            let hi = (i + 6).min(n);
            let w: Vec<f64> = rr_by_epoch[lo..hi].iter().flatten().copied().filter(|v| (300.0..=2000.0).contains(v)).collect();
            if w.len() < 20 {
                return f64::NAN;
            }
            let d: Vec<f64> = w.windows(2).filter(|p| (p[1] - p[0]).abs() < 0.2 * p[0]).map(|p| p[1] - p[0]).collect();
            if d.len() < 10 { f64::NAN } else { (d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64).sqrt() }
        })
        .collect();
    let hr_var: Vec<f64> = (0..n)
        .map(|i| {
            let w: Vec<f64> = hr[i.saturating_sub(10)..(i + 10).min(n)].iter().copied().filter(|v| v.is_finite()).collect();
            if w.is_empty() { f64::NAN } else { let m = mean(&w).unwrap(); (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / w.len() as f64).sqrt() }
        })
        .collect();
    let p_hr25 = percentile(&hr, 25.0).unwrap_or(0.0);
    let p_rm70 = percentile(&loc_rmssd, 70.0).unwrap_or(f64::INFINITY);
    let p_var65 = percentile(&hr_var, 65.0).unwrap_or(f64::INFINITY);
    let mut labels: Vec<&'static str> = (0..n)
        .map(|i| {
            if !ck[i] || movefrac[i] > 0.15 {
                return "wake";
            }
            let still = movefrac[i] <= 0.10;
            if still && (i as f64) < n as f64 * 0.5 && hr[i] <= p_hr25 && loc_rmssd[i] >= p_rm70 {
                "deep"
            } else if still && i > 120 && hr_var[i] >= p_var65 && hr[i] >= p_hr25 {
                "rem"
            } else {
                "light"
            }
        })
        .collect();
    for i in 1..n.saturating_sub(1) {
        if labels[i - 1] == labels[i + 1] && labels[i] != labels[i - 1] {
            labels[i] = labels[i - 1];
        }
    }
    (labels, t0)
}

fn baseline_of(prev: &[&Value], pick: impl Fn(&Value) -> Option<f64>, cfg: &baselines::MetricCfg) -> Option<Baseline> {
    let vals: Vec<Option<f64>> = prev.iter().map(|h| pick(h)).collect();
    baselines::fold(&vals, cfg)
}

/// On-strap SpO2 (R18 frame byte 82, found 10 Nov): during sleep the strap runs a 30 s spot
/// measurement on a fixed 20-minute clock (xx:16 / xx:36 / xx:56); each second of a window holds
/// either a saturation in % or a status code (2, 8, 16, 32, 128, 144, 160, 168 ...). Outside the
/// windows the byte is 0. Values 70..=100 are taken as SpO2 %, everything else as a status code.
pub fn spo2_summary(values: &[(i64, u8)]) -> Value {
    let mut windows: Vec<Vec<(i64, u8)>> = Vec::new();
    for &(t, v) in values.iter().filter(|(_, v)| *v != 0) {
        match windows.last_mut() {
            Some(w) if t - w.last().unwrap().0 <= 5 => w.push((t, v)),
            _ => windows.push(vec![(t, v)]),
        }
    }
    let per_window: Vec<(i64, f64, usize)> = windows
        .iter()
        .filter_map(|w| {
            let vals: Vec<f64> = w.iter().map(|(_, v)| f64::from(*v)).filter(|v| (70.0..=100.0).contains(v)).collect();
            // a window counts when at least 10 of its ~30 seconds carry a saturation
            (vals.len() >= 10).then(|| (w[0].0, median(&vals).unwrap(), vals.len()))
        })
        .collect();
    if per_window.is_empty() {
        return json!({"available": false, "windows": windows.len(), "status": "UNAVAILABLE"});
    }
    let meds: Vec<f64> = per_window.iter().map(|w| w.1).collect();
    json!({
        "available": true,
        "median_pct": round(median(&meds).unwrap(), 1),
        "min_window_pct": meds.iter().copied().fold(f64::INFINITY, f64::min),
        "windows": windows.len(),
        "valid_windows": per_window.len(),
        "windows_below_95": meds.iter().filter(|v| **v < 95.0).count(),
        "windows_below_90": meds.iter().filter(|v| **v < 90.0).count(),
        "series": per_window.iter().map(|w| json!([w.0, w.1, w.2])).collect::<Vec<_>>(),
        "source": "computed on the strap (R18 byte 82), 30 s windows every 20 min during sleep",
        "status": "DECODED_UNVALIDATED",
    })
}

fn analyze_night(s: &[Sample], history: &[Value], optical_windows: &[crate::whoop5_records::R26Record], spo2_values: &[(i64, u8)]) -> Value {
    let Some(sp) = sleep_period(s) else { return json!({ "available": false, "firmware_sleep_min": s.iter().filter(|x| x.band == "sleep").count() / 60 }) };
    let (stages, onset_ts) = stage_epochs(s, sp.onset, sp.wake);
    let count = |k: &str| (stages.iter().filter(|x| **x == k).count() as f64 * EPOCH as f64 / 60.0).round();
    let (wake_m, light, deep, rem) = (count("wake"), count("light"), count("deep"), count("rem"));
    let asleep = light + deep + rem;
    let in_bed = ((s[sp.wake].ts - s[sp.bed].ts) as f64 / 60.0).round();
    let night = &s[sp.onset..=sp.wake];
    let hr: Vec<f64> = night.iter().map(|x| x.hr).collect();
    let resting = if hr.len() > 300 {
        hr.windows(300).map(|w| w.iter().sum::<f64>() / 300.0).fold(f64::INFINITY, f64::min)
    } else {
        hr.iter().copied().fold(f64::INFINITY, f64::min)
    };
    let third = s[sp.onset].ts + 2 * (s[sp.wake].ts - s[sp.onset].ts) / 3;
    let last_third: Vec<Sample> = night.iter().filter(|x| x.ts >= third).cloned().collect();
    let skin_vals: Vec<f64> = night.iter().filter_map(|x| x.skin).collect();
    let skin = median(&skin_vals).map(|v| round(v, 2));
    let (hrv, hrv_qc_json, clean_segs) = hrv_with_qc(night);
    let (hrv_last, _, _) = hrv_with_qc(&last_third);
    let prev: Vec<&Value> = history.iter().filter(|h| h["available"] == true).collect();
    let prev_skin: Vec<f64> = prev.iter().filter_map(|h| h["skin_temp_c"].as_f64()).collect();
    let skin_delta = match (skin, prev_skin.len() >= 3) {
        (Some(v), true) => Some(round(v - mean(&prev_skin).unwrap(), 2)),
        _ => None,
    };
    let respiration = respiratory_rate(night);
    // personal baselines from previous nights (NOOP winsorised EWMA)
    let b_hrv = baseline_of(&prev, |h| h["hrv_night"]["rmssd_ms"].as_f64(), &baselines::HRV);
    let b_rhr = baseline_of(&prev, |h| h["resting_hr_bpm"].as_f64(), &baselines::RESTING_HR);
    let b_resp = baseline_of(&prev, |h| h["respiration"]["rpm"].as_f64(), &baselines::RESP);
    let b_skin = baseline_of(&prev, |h| h["skin_temp_c"].as_f64(), &baselines::SKIN_TEMP);
    // temperature: variability, linear trend over the night, hourly profile
    let skin_pts: Vec<(f64, f64)> = night.iter().filter_map(|x| x.skin.map(|t| ((x.ts - s[sp.onset].ts) as f64 / 3600.0, t))).collect();
    let skin_trend = (skin_pts.len() > 600).then(|| {
        let n = skin_pts.len() as f64;
        let (mx, my) = (skin_pts.iter().map(|p| p.0).sum::<f64>() / n, skin_pts.iter().map(|p| p.1).sum::<f64>() / n);
        let sxy: f64 = skin_pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let sxx: f64 = skin_pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        round(sxy / sxx.max(1e-9), 3)
    });
    let mut skin_hourly = serde_json::Map::new();
    for x in night.iter().filter(|x| x.skin.is_some()) {
        let h = ((x.ts - s[sp.onset].ts) / 3600).to_string();
        skin_hourly.entry(h).or_insert_with(|| json!([])).as_array_mut().unwrap().push(json!(x.skin));
    }
    for v in skin_hourly.values_mut() {
        let vals: Vec<f64> = v.as_array().unwrap().iter().filter_map(Value::as_f64).collect();
        *v = json!(median(&vals).map(|m| round(m, 2)));
    }
    // movement: share of 30 s epochs with gravity change > 10 mg
    let moved = night.windows(2).filter(|w| match (w[0].grav, w[1].grav) {
        (Some(a), Some(b)) => ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt() > 0.01,
        _ => false,
    }).count();
    let hr_by_ts: std::collections::HashMap<i64, f64> = night.iter().map(|x| (x.ts, x.hr)).collect();
    let night_optical: Vec<crate::whoop5_records::R26Record> = optical_windows.iter().filter(|w| w.ts >= s[sp.bed].ts && w.ts <= s[sp.wake].ts).cloned().collect();
    let optical = optical::summarize(&night_optical, &|t| hr_by_ts.get(&t).copied());
    let code: String = stages.iter().map(|x| match *x { "wake" => 'W', "rem" => 'R', "deep" => 'D', _ => 'L' }).collect();
    let rr_count: usize = night.iter().map(|x| x.rr.len()).sum();
    let mut out = json!({
        "available": true,
        "bed_time": s[sp.bed].ts, "sleep_onset": s[sp.onset].ts, "wake_time": s[sp.wake].ts,
        "time_in_bed_min": in_bed, "asleep_min": asleep,
        "latency_min": ((s[sp.onset].ts - s[sp.bed].ts) as f64 / 60.0).round(),
        "efficiency_pct": round(100.0 * asleep / in_bed.max(1.0), 1),
        "stages_min": {"wake": wake_m, "light": light, "deep": deep, "rem": rem},
        "awakenings": stages.windows(2).filter(|w| w[0] != "wake" && w[1] == "wake").count(),
        "hypnogram": code, "hypnogram_start": onset_ts,
        "resting_hr_bpm": round(resting, 1),
        "mean_sleep_hr_bpm": round(mean(&hr).unwrap_or(0.0), 1),
        "hrv_night": hrv,
        "hrv_last_third": hrv_last,
        "hrv_qc": hrv_qc_json,
        "hrv_rolling_5min": hrv_qc::rolling_rmssd(&clean_segs, 300, 30),
        "respiration": respiration,
        "skin_temp_c": skin,
        "skin_temp_delta_c": skin_delta,
        "skin_temp": {
            "kind": "skin (wrist), not core body temperature",
            "sd_c": std_sample(&skin_vals).map(|v| round(v, 2)),
            "trend_c_per_h": skin_trend,
            "hourly_median_c": skin_hourly,
            "baseline": baselines::deviation_json(skin, b_skin, 2),
            "status": if skin.is_some() { "DECODED_UNVALIDATED" } else { "UNAVAILABLE" },
        },
        "baselines": {
            "hrv_rmssd": baselines::deviation_json(hrv["rmssd_ms"].as_f64(), b_hrv, 1),
            "resting_hr": baselines::deviation_json(Some(round(resting, 1)), b_rhr, 1),
            "respiration": baselines::deviation_json(respiration["rpm"].as_f64(), b_resp, 1),
        },
        "sleep_detail": {
            "source": "inferred locally (band state + Cole-Kripke + HR/HRV rules); firmware band state shown separately",
            "firmware_sleep_min": night.iter().filter(|x| x.band == "sleep").count() / 60,
            "waso_min": wake_m,
            "fragmentation_per_h": round(stages.windows(2).filter(|w| w[0] != "wake" && w[1] == "wake").count() as f64 / (asleep / 60.0).max(0.1), 2),
            "movement_pct": round(100.0 * moved as f64 / night.len().max(1) as f64, 1),
            "nocturnal_hr": {"mean": round(mean(&hr).unwrap_or(0.0), 1), "min_5min": round(resting, 1), "p95": percentile(&hr, 95.0).map(|v| round(v, 1))},
            "stages_status": "EXPERIMENTAL (65-73 % epoch agreement ceiling without EEG)",
        },
        "optical": optical,
        "spo2": spo2_summary(&spo2_values.iter().copied().filter(|(t, _)| *t >= s[sp.bed].ts && *t <= s[sp.wake].ts).collect::<Vec<_>>()),
        "rr_coverage_pct": round(100.0 * rr_count as f64 / (hr.iter().sum::<f64>() / 60.0).max(1.0), 1),
    });
    out["recovery"] = baselines::recovery(&baselines::RecoveryInputs {
        rmssd: hrv["rmssd_ms"].as_f64(),
        resting_hr: Some(round(resting, 1)),
        resp: respiration["rpm"].as_f64(),
        efficiency_pct: out["efficiency_pct"].as_f64(),
        hrv_base: b_hrv,
        rhr_base: b_rhr,
        resp_base: b_resp,
    });
    out
}

#[allow(dead_code)]
fn recovery(night: &Value, prev: &[&Value]) -> Value {
    let hist: Vec<&&Value> = prev.iter().filter(|p| p["hrv_night"].is_object()).collect();
    let (Some(ln), Some(rhr)) = (night["hrv_night"]["ln_rmssd"].as_f64(), night["resting_hr_bpm"].as_f64()) else {
        return json!({ "score": null, "reason": "VFC insuffisante cette nuit" });
    };
    if hist.len() < 3 {
        return json!({ "score": null, "reason": format!("Référence en cours ({}/3 nuits)", hist.len()) });
    }
    let z = |cur: f64, vals: Vec<f64>| {
        let m = mean(&vals).unwrap();
        let sd = std_sample(&vals).filter(|v| *v > 1e-6).unwrap_or(1e-6);
        (cur - m) / sd
    };
    let zh = z(ln, hist.iter().filter_map(|p| p["hrv_night"]["ln_rmssd"].as_f64()).collect());
    let zr = -z(rhr, hist.iter().filter_map(|p| p["resting_hr_bpm"].as_f64()).collect());
    let composite = 0.6 * zh + 0.4 * zr;
    json!({
        "score": (100.0 / (1.0 + (-1.2 * composite).exp())).round(),
        "z_ln_rmssd": round(zh, 2), "z_resting_hr": round(zr, 2), "baseline_nights": hist.len(),
    })
}

// ------------------------------------------------------------------ day

fn analyze_day(s: &[Sample], day_start: i64, now: i64, profile: &Profile, resting_hr: Option<f64>) -> Value {
    let covered_h = s.len() as f64 / 3600.0;
    if s.len() < 600 {
        return json!({ "coverage_h": round(covered_h, 1) });
    }
    let mut steps = 0i64;
    for w in s.windows(2) {
        let d = w[1].steps - w[0].steps;
        if d > 0 && d < 300 && w[1].ts - w[0].ts <= 60 {
            steps += d;
        }
    }
    let age = profile.age.unwrap_or(30.0);
    let hr_max_obs = s.iter().map(|x| x.hr).fold(0.0, f64::max);
    let hrmax = (208.0 - 0.7 * age).max(hr_max_obs);
    let mut zone_sec = [0.0f64; 6];
    for x in s {
        let p = x.hr / hrmax;
        let z = [0.5, 0.6, 0.7, 0.8, 0.9].iter().filter(|t| p >= **t).count();
        zone_sec[z] += 1.0;
    }
    let zone_min: Vec<f64> = (1..=5).map(|z| round(zone_sec[z] / 60.0, 1)).collect();
    let trimp: f64 = (1..=5).map(|z| z as f64 * zone_sec[z] / 60.0).sum();
    let hrs: Vec<f64> = s.iter().map(|x| x.hr).collect();
    let rhr = resting_hr.unwrap_or_else(|| percentile(&hrs, 5.0).unwrap_or(60.0));
    let reserve = (hrmax - rhr).max(1.0);
    let mvpa = s.iter().filter(|x| (x.hr - rhr) / reserve >= 0.4).count() as f64 / 60.0;
    // energy: resting = Mifflin-St Jeor prorated over the elapsed part of the day; active = Keytel
    // (HR > resting + 20) minus the resting share of those minutes.
    let w = profile.weight_kg.unwrap_or(70.0);
    let h = profile.height_cm.unwrap_or(175.0);
    let male = profile.sex.as_deref() != Some("female");
    let bmr = 10.0 * w + 6.25 * h - 5.0 * age + if male { 5.0 } else { -161.0 };
    // Day starts at midnight, unless the strap was not worn before (first data > 3 h after midnight,
    // e.g. the first day): then the resting share starts with the data.
    let span_start = if s[0].ts - day_start > 3 * 3600 { s[0].ts } else { day_start };
    let elapsed = ((now.min(day_start + DAY) - span_start) as f64 / DAY as f64).clamp(0.0, 1.0);
    let resting_kcal = bmr * elapsed;
    let active: Vec<f64> = hrs.iter().copied().filter(|v| *v > rhr + 20.0).collect();
    let keytel: f64 = active
        .iter()
        .map(|hr| if male { (-55.0969 + 0.6309 * hr + 0.1988 * w + 0.2017 * age) / 4.184 } else { (-20.4022 + 0.4472 * hr - 0.1263 * w + 0.074 * age) / 4.184 })
        .map(|k| k.max(0.0))
        .sum::<f64>()
        / 60.0;
    let active_kcal = (keytel - bmr / 1440.0 * active.len() as f64 / 60.0).max(0.0);
    let mut hourly = serde_json::Map::new();
    for hour in 0..24 {
        let (h0, h1) = (day_start + hour * 3600, day_start + (hour + 1) * 3600);
        let seg: Vec<&Sample> = s.iter().filter(|x| x.ts >= h0 && x.ts < h1).collect();
        if seg.len() > 60 {
            let st: i64 = seg.windows(2).map(|w| w[1].steps - w[0].steps).filter(|d| *d > 0 && *d < 300).sum();
            hourly.insert(hour.to_string(), json!({
                "hr": round(seg.iter().map(|x| x.hr).sum::<f64>() / seg.len() as f64, 1),
                "steps": st,
            }));
        }
    }
    json!({
        "coverage_h": round(covered_h, 1),
        "steps": steps,
        "walk_min": round(s.iter().filter(|x| x.activity.as_deref() == Some("walk")).count() as f64 / 60.0, 1),
        "run_min": round(s.iter().filter(|x| x.activity.as_deref() == Some("run")).count() as f64 / 60.0, 1),
        "mvpa_min": round(mvpa, 1),
        "hr_zone_min": {"z1": zone_min[0], "z2": zone_min[1], "z3": zone_min[2], "z4": zone_min[3], "z5": zone_min[4]},
        "edwards_trimp": round(trimp, 1),
        "strain_0_21": round(21.0 * (1.0 - (-trimp / 250.0).exp()), 1),
        "hrmax_used": hrmax.round(),
        "energy": {"resting_kcal": resting_kcal.round(), "active_kcal": active_kcal.round(), "total_kcal": (resting_kcal + active_kcal).round(),
                   "profile_defaults_used": profile.weight_kg.is_none() || profile.age.is_none()},
        "hr_mean": round(mean(&hrs).unwrap_or(0.0), 1),
        "hr_min": hrs.iter().copied().fold(f64::INFINITY, f64::min),
        "hr_max": hr_max_obs,
        "hourly": hourly,
        "stress": stress(s, rhr, hrmax),
        "age_default_used": profile.age.is_none(),
        "training": training_block(s, rhr, age, profile),
    })
}

fn training_block(s: &[Sample], rhr: f64, age: f64, profile: &Profile) -> Value {
    let hrs: Vec<f64> = s.iter().map(|x| x.hr).filter(|h| *h > 0.0).collect();
    let (hrmax, source) = training::hrmax_used(age, &hrs);
    let female = profile.sex.as_deref() == Some("female");
    let triples: Vec<(i64, f64, Option<f64>)> = s.iter().map(|x| (x.ts, x.hr, x.dyn_g)).collect();
    let mut w = training::workouts(&triples, rhr, hrmax, female);
    if let Some(sessions) = w["sessions"].as_array_mut() {
        for sess in sessions {
            let (a, b) = (sess["start"].as_i64().unwrap_or(0), sess["end"].as_i64().unwrap_or(0));
            let hr: Vec<f64> = s.iter().filter(|x| x.ts >= a && x.ts <= b).map(|x| x.hr).collect();
            sess["cardiac_drift_pct"] = json!(training::cardiac_drift(&hr));
            sess["hr_recovery"] = training::hr_recovery(&triples, b);
        }
    }
    json!({
        "hrmax_source": source,
        "load_karvonen": training::load(&hrs, rhr, hrmax, female),
        "workouts": w,
    })
}

fn stress(s: &[Sample], rhr: f64, hrmax: f64) -> Value {
    let reserve = (hrmax - rhr).max(1.0);
    let calm: Vec<&Sample> = s
        .iter()
        .filter(|x| x.band != "sleep" && x.activity.as_deref() == Some("still") && x.dyn_g.unwrap_or(1.0) < 0.03)
        .collect();
    if calm.len() < 600 {
        return Value::Null;
    }
    let t0 = s[0].ts;
    let mut buckets: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for x in calm {
        buckets.entry((x.ts - t0) / 300).or_default().push(((x.hr - rhr) / (0.4 * reserve)).clamp(0.0, 1.0) * 100.0);
    }
    let scores: Vec<f64> = buckets.values().filter(|v| v.len() >= 120).filter_map(|v| mean(v)).collect();
    if scores.is_empty() {
        return Value::Null;
    }
    let band = |lo: f64, hi: f64| scores.iter().filter(|v| **v > lo && **v <= hi).count() as f64 * 5.0;
    json!({
        "mean": mean(&scores).unwrap().round(), "max": scores.iter().copied().fold(0.0, f64::max).round(),
        "minutes": {"repos": band(-1.0, 25.0), "bas": band(25.0, 50.0), "moyen": band(50.0, 75.0), "haut": band(75.0, 101.0)},
        "buckets": scores.len(),
    })
}

fn training_load(days: &[Value]) -> Value {
    let series: Vec<(String, f64)> = days
        .iter()
        .filter(|d| d["day"]["steps"].is_number())
        .map(|d| (d["date"].as_str().unwrap_or("").to_string(), d["day"]["edwards_trimp"].as_f64().unwrap_or(0.0)))
        .collect();
    if series.is_empty() {
        return Value::Null;
    }
    let vals: Vec<f64> = series.iter().map(|(_, v)| *v).collect();
    let acute: f64 = vals.iter().rev().take(7).sum();
    let n28 = vals.len().min(28);
    let chronic = vals.iter().rev().take(28).sum::<f64>() / n28 as f64 * 7.0;
    let acwr = (vals.len() >= 21 && chronic > 0.0).then(|| round(acute / chronic, 2));
    json!({
        "acute_7d": round(acute, 1), "chronic_28d_weekly": round(chronic, 1), "acwr": acwr, "days": vals.len(),
        "foster": training::foster(&vals),
        "daily": series.iter().rev().take(28).rev().map(|(d, v)| json!({"date": d, "trimp": round(*v, 1)})).collect::<Vec<_>>(),
    })
}

// ------------------------------------------------------------------ provenance

/// Source, method, unit and status of every value in the results (see lab/METRICS.md).
fn provenance() -> Value {
    let p = |src: &str, method: &str, unit: &str, status: &str| json!({"source": src, "method": method, "unit": unit, "status": status, "algorithm_version": ALGORITHM_VERSION});
    json!({
        "night.bed_time/sleep_onset/wake_time": p("R18 band state + gravity", "longest firmware sleep block (gaps <= 20 min), extended back over still", "unix s", "INFERRED"),
        "night.stages_min": p("R18 HR, R-R, gravity", "Cole-Kripke (te Lindert) + transparent HR/HRV rules", "min", "EXPERIMENTAL"),
        "night.resting_hr_bpm": p("R18 HR", "lowest 5-min rolling mean during sleep", "bpm", "DECODED_UNVALIDATED"),
        "night.hrv_night": p("R18 R-R", "range + Malik 20 % + local-median 20 % QC, successive beats only, Task Force time domain", "ms", "EXPERIMENTAL"),
        "night.respiration": p("R18 R-R", "RSA: 1 Hz tachogram, 120 s Hann periodogram, 0.13-0.45 Hz peak", "breaths/min", "INFERRED"),
        "night.skin_temp_c": p("R18 skin temperature (raw/100)", "night median; skin, not core", "degC", "DECODED_UNVALIDATED"),
        "night.recovery": p("night HRV, resting HR, respiration, efficiency", "NOOP RecoveryScorer port (winsorised EWMA baselines, logistic)", "0-100", "EXPERIMENTAL"),
        "night.optical": p("R26 compact optical windows", "burst reconstruction, 0.5-4 Hz band-pass, ACF pulse rate checked against R18 HR", "bpm / ADC code", "EXPERIMENTAL"),
        "day.steps": p("R18 step counter", "sum of positive increments < 300/s", "steps", "DECODED_UNVALIDATED"),
        "day.edwards_trimp": p("R18 HR", "Edwards zones on %HRmax (Tanaka)", "a.u.", "EXPERIMENTAL"),
        "day.training": p("R18 HR + dynamic accel", "Karvonen %HRR, Edwards/Banister TRIMP, NOOP workout detector", "a.u.", "EXPERIMENTAL"),
        "day.stress": p("R18 HR, activity, dynamic accel", "HR elevation at rest vs 40 % HR reserve, 5-min buckets", "0-100", "EXPERIMENTAL"),
        "day.energy": p("R18 HR + profile", "Mifflin-St Jeor resting + Keytel 2005 active", "kcal", "EXPERIMENTAL"),
        "load": p("daily Edwards TRIMP", "7 d acute, 28 d chronic, ACWR after 21 d, Foster monotony", "a.u.", "EXPERIMENTAL"),
        "night.spo2": p("R18 byte 82 (on-strap SpO2)", "per-window median of the % values (70-100), status codes discarded", "%", "DECODED_UNVALIDATED"),
    })
}

// ------------------------------------------------------------------ driver

fn date_key(local_midnight: i64, offset: i64) -> String {
    let days = (local_midnight + offset).div_euclid(DAY);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Runs the whole pipeline and returns all days (first day with data -> today), most recent last.
pub fn analyze(options: &AnalyzeOptions) -> GooseResult<Value> {
    let conn = Connection::open(&options.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    sync_biometric_table(&conn)?;
    let layer_b = whoop5_store::ingest(&conn)?;
    let off = options.utc_offset_seconds;
    let now = options.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    let (first, last, total): (Option<i64>, Option<i64>, i64) =
        conn.query_row("SELECT MIN(ts), MAX(ts), COUNT(*) FROM whoop_lab_biometric", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(json!({ "schema": RESULTS_SCHEMA, "generated": now, "days": [], "load": null, "device": {"records": 0} }));
    };
    let midnight = |ts: i64| (ts + off).div_euclid(DAY) * DAY - off;
    let mut day_start = midnight(first);
    let today_start = midnight(now.max(last));
    let mut days: Vec<Value> = Vec::new();
    let mut nights_hist: Vec<Value> = Vec::new();
    while day_start <= today_start {
        let key = date_key(day_start, off);
        // inputs: the night ending this morning (18:00 previous day -> 14:00) and the calendar day
        let (n0, n1) = (day_start - 6 * 3600, day_start + 14 * 3600);
        let (d0, d1) = (day_start, day_start + DAY);
        let (cnt, last_ts): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), IFNULL(MAX(ts), 0) FROM whoop_lab_biometric WHERE ts >= ?1 AND ts < ?2",
            params![n0, d1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let cached: Option<(i64, i64, String, String)> = conn
            .query_row("SELECT input_count, input_last_ts, algorithm_version, result_json FROM whoop_lab_days WHERE date_key = ?1", [&key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let reuse = cached.as_ref().is_some_and(|(c, l, v, _)| *c == cnt && *l == last_ts && v == ALGORITHM_VERSION) && day_start < today_start;
        let result: Value = if let (true, Some((_, _, _, j))) = (reuse, &cached) {
            serde_json::from_str(j).unwrap_or(Value::Null)
        } else {
            let night_samples = load_samples(&conn, n0, n1)?;
            let optical_windows = whoop5_store::load_r26(&conn, n0, n1)?;
            let spo2_values: Vec<(i64, u8)> = {
                let mut st = conn.prepare_cached("SELECT ts, sleep_adjacent_raw FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2 AND sleep_adjacent_raw > 0 ORDER BY ts")?;
                let rows = st.query_map(params![n0, n1], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<Result<_, _>>()?
            };
            let night = analyze_night(&night_samples, &nights_hist, &optical_windows, &spo2_values);
            let day_samples = load_samples(&conn, d0, d1)?;
            let rhr = night["resting_hr_bpm"].as_f64();
            let day = analyze_day(&day_samples, d0, now, &options.profile, rhr);
            let value = json!({ "date": key, "night": night, "day": day });
            conn.execute(
                "INSERT OR REPLACE INTO whoop_lab_days (date_key, input_count, input_last_ts, algorithm_version, result_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, cnt, last_ts, ALGORITHM_VERSION, value.to_string()],
            )?;
            value
        };
        if result["night"]["available"] == true {
            let mut n = result["night"].clone();
            n.as_object_mut().map(|o| o.remove("hypnogram"));
            nights_hist.push(n);
            if nights_hist.len() > 30 {
                nights_hist.remove(0);
            }
        }
        days.push(result);
        day_start += DAY;
    }
    let load = training_load(&days);
    // sleep regularity: SD of onset / wake clock times over the last 7 nights (minutes)
    let clock = |v: &Value| v.as_i64().map(|t| (((t + off).rem_euclid(DAY) + DAY / 2) % DAY) as f64 / 60.0);
    let recent: Vec<&Value> = days.iter().rev().filter(|d| d["night"]["available"] == true).take(7).map(|d| &d["night"]).collect();
    let onsets: Vec<f64> = recent.iter().filter_map(|n| clock(&n["sleep_onset"])).collect();
    let wakes: Vec<f64> = recent.iter().filter_map(|n| clock(&n["wake_time"])).collect();
    let regularity = json!({
        "nights": recent.len(),
        "onset_sd_min": std_sample(&onsets).map(|v| round(v, 0)),
        "wake_sd_min": std_sample(&wakes).map(|v| round(v, 0)),
        "status": if recent.len() >= 3 { "EXPERIMENTAL" } else { "UNAVAILABLE" },
    });
    Ok(json!({
        "schema": RESULTS_SCHEMA,
        "generated": now,
        "first_day": days.first().map(|d| d["date"].clone()),
        "days": days,
        "load": load,
        "sleep_regularity": regularity,
        "layer_b_ingest": serde_json::to_value(&layer_b).unwrap_or(Value::Null),
        "provenance": provenance(),
        "device": {"records": total, "first": first, "last": last,
                   "coverage_pct": round(100.0 * total as f64 / ((last - first + 1) as f64).max(1.0), 1)},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spo2_windows_from_real_sequences() {
        // two real windows (10 Nov 01:36 and 02:56): one clean, one mostly status codes
        let a = [16u8, 48, 32, 32, 99, 100, 99, 99, 99, 99, 99, 100, 99, 100, 100, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 100];
        let b = [128u8, 128, 128, 144, 32, 32, 32, 88, 86, 87, 88, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 32, 160, 87, 90, 32, 32];
        let mut v: Vec<(i64, u8)> = a.iter().enumerate().map(|(i, x)| (1000 + i as i64, *x)).collect();
        v.extend(b.iter().enumerate().map(|(i, x)| (2200 + i as i64, *x)));
        let s = spo2_summary(&v);
        assert_eq!(s["windows"], 2);
        assert_eq!(s["valid_windows"], 1); // the second has only 6 saturation seconds
        assert_eq!(s["median_pct"], 99.0);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(date_key(1_794_178_800, 3600), "2026-11-09");
    }

    #[test]
    fn cole_kripke_isolated_turn_stays_sleep() {
        let mut counts = vec![0.0; 20];
        counts[10] = 2000.0;
        assert!(cole_kripke(&counts).iter().all(|s| *s));
        let busy = vec![5000.0; 20];
        assert!(cole_kripke(&busy).iter().all(|s| !*s));
    }

    #[test]
    fn periodogram_finds_breathing_peak() {
        // 0.25 Hz oscillation (15 breaths/min) on a 1 Hz grid
        let mut x: Vec<f64> = (0..120).map(|i| 1000.0 + 40.0 * (2.0 * PI * 0.25 * i as f64).sin()).collect();
        detrend_linear(&mut x);
        let p = hann_periodogram(&x);
        let k = (16..=54).max_by(|a, b| p[*a].total_cmp(&p[*b])).unwrap();
        assert_eq!(k, 30);
    }
}

#[cfg(test)]
mod real_data {
    /// GOOSE_DB=/path/to/copy.sqlite cargo test --release --lib real_db -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_db() {
        let Ok(db) = std::env::var("GOOSE_DB") else { return };
        let t = std::time::Instant::now();
        let v = super::analyze(&super::AnalyzeOptions { database_path: db.clone(), utc_offset_seconds: 3600, now_unix: None, profile: Default::default() }).unwrap();
        println!("first run {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let _ = super::analyze(&super::AnalyzeOptions { database_path: db.clone(), utc_offset_seconds: 3600, now_unix: None, profile: Default::default() }).unwrap();
        println!("cached run {:?}", t.elapsed());
        std::fs::write(format!("{db}.results.json"), serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        println!("{}", serde_json::to_string_pretty(&crate::whoop5_store::inventory(&conn).unwrap()).unwrap());
    }
}
