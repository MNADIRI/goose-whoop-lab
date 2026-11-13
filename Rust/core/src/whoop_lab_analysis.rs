//! whoop-lab on-device analysis: WHOOP 5.0 R18 history -> per-day physiological results.
//!
//! This is the single source of the physiological values the app displays. It is a port of the
//! historical Python lab (`lab/analyze.py`). Version 12 changes data-quality policies (unknown
//! epochs, contiguous resting HR, no CPC stage relabelling): Python is no longer a parity oracle.
//!
//! Pipeline:
//!   decoded_frames (type 47, 124 B)  --sync_biometric_table-->  whoop_lab_biometric (1 row / second)
//!   whoop_lab_biometric              --analyze-->               whoop_lab_days (one JSON per local day)
//!
//! Each day is recomputed only when its input changed (row count / last second), the analysis
//! context changed (algorithm, decoder, profile, UTC offset) or it is today or yesterday.

use std::collections::BTreeMap;
use std::f64::consts::PI;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::baselines::{self, Baseline};
use crate::hrv_qc::{self, Beat};
use crate::health_indices::{self as hi, S1};
use crate::{optical, training, whoop5_store};
use crate::{protocol::parse_r18_history_fields, GooseError, GooseResult};

pub const RESULTS_SCHEMA: &str = "whoop-lab.results.v2";
const ALGORITHM_VERSION: &str = "12"; // 12: unknown epochs + coverage, shared RR QC for stages, no CPC relabelling, contiguous resting HR, no SpO2 screening score, context-aware cache; 11: Banister seeded, 28-day calibration; 10: composite sleep score, awake resting vitals, strain %
const DAY: i64 = 86_400;
const EPOCH: i64 = 30;
/// Minimum share of scored epochs between sleep onset and wake for a night to be scored.
const SLEEP_MIN_COVERAGE: f64 = 0.9;
const CK_WEIGHTS: [f64; 7] = [106.0, 54.0, 58.0, 76.0, 230.0, 74.0, 67.0];
const MIN_PLAUSIBLE_TS: i64 = 1_577_836_800;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    // Read the cursor, rows and high watermark in one snapshot; no concurrent insert can be skipped.
    let tx = conn.unchecked_transaction()?;
    let cursor: i64 = conn
        .query_row("SELECT value FROM whoop_lab_state WHERE key = 'decoded_rowid'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let rows: Vec<(i64, String)> = {
        let mut st = conn.prepare(
            "SELECT rowid, payload_hex FROM decoded_frames WHERE rowid > ?1 AND packet_type = 47 AND raw_len = 124 AND header_crc_valid = 1 AND payload_crc_valid = 1 ORDER BY rowid",
        )?;
        let mapped = st.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<Result<_, _>>()?
    };
    let max_rowid: i64 = conn.query_row("SELECT IFNULL(MAX(rowid), 0) FROM decoded_frames", [], |r| r.get(0))?;
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
    // Score full epochs only; a partial final epoch is not a sensor dropout.
    let n = ((s[wake].ts - t0 + 1) / EPOCH) as usize;
    let mut counts = vec![0.0; n];
    let mut moving = vec![0.0; n];
    let mut seen: Vec<f64> = vec![0.0; n];
    let mut hr_sum = vec![0.0; n];
    let mut motion_seen = vec![0usize; n];
    let mut rr_by_epoch: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut prev_g: Option<[f64; 3]> = None;
    for smp in &s[onset..=wake] {
        let k = ((smp.ts - t0) / EPOCH) as usize;
        if k >= n { continue; }
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
        if smp.grav.is_some() { motion_seen[k] += 1; }
        rr_by_epoch[k].extend(smp.rr.iter().copied());
    }
    let movefrac: Vec<f64> = (0..n).map(|k| moving[k] / seen[k].max(1.0)).collect();
    let hr: Vec<f64> = (0..n).map(|k| if seen[k] > 0.0 { hr_sum[k] / seen[k] } else { f64::NAN }).collect();
    let ck = cole_kripke(&counts);
    // Local RMSSD (±5 epochs) for staging. The whole-night shared QC (hrv_qc) was tried here but
    // its 100-difference minimum and segment splitting blank most 5.5-min windows on R18 data and
    // remove nearly all N3 on real nights; the light 20 % successive-difference filter is kept.
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
            if seen[i] < 24.0 || motion_seen[i] < 24 { return "unknown"; }
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
        if labels[i - 1] != "unknown" && labels[i] != "unknown" && labels[i + 1] != "unknown"
            && labels[i - 1] == labels[i + 1] && labels[i] != labels[i - 1] {
            labels[i] = labels[i - 1];
        }
    }
    (labels, t0)
}

fn baseline_of(prev: &[&Value], pick: impl Fn(&Value) -> Option<f64>, cfg: &baselines::MetricCfg) -> Option<Baseline> {
    let vals: Vec<Option<f64>> = prev.iter().map(|h| pick(h)).collect();
    baselines::fold(&vals, cfg)
}

/// Reverse-engineered per-second R18 fields (10 Nov, see docs/OPTICAL.md), joined by timestamp.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extra {
    pub cardiac_flags: u8,
    /// rr_adjacent high byte: firmware resting-HR candidate, 0 when active
    pub fw_rest_hr: u8,
    pub cadence_raw: u16,
    pub opt_amp: u8,
    pub aux_temp: Option<f64>,
    pub hr_quality: u8,
}

/// cardiac_flags bits 0, 4, 6: set while moving, R-R rarely valid -> beats in these seconds are skipped
pub const MOTION_FLAGS: u8 = 0b0101_0001;

pub fn load_extras(conn: &Connection, from: i64, to: i64) -> GooseResult<std::collections::HashMap<i64, Extra>> {
    let mut st = conn.prepare_cached(
        "SELECT ts, cardiac_flags_raw, rr_adjacent_raw, cadence_raw, optical_amplitude_a, aux_temp2_c, hr_quality_raw FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2",
    )?;
    let rows = st.query_map(params![from, to], |r| {
        let ra: i64 = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
        Ok((r.get::<_, i64>(0)?, Extra {
            cardiac_flags: r.get::<_, Option<i64>>(1)?.unwrap_or(0) as u8,
            fw_rest_hr: (ra >> 8) as u8,
            cadence_raw: r.get::<_, Option<i64>>(3)?.unwrap_or(0) as u16,
            opt_amp: r.get::<_, Option<i64>>(4)?.unwrap_or(0) as u8,
            aux_temp: r.get(5)?,
            hr_quality: r.get::<_, Option<i64>>(6)?.unwrap_or(0) as u8,
        }))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn to_s1(s: &[Sample], extras: &std::collections::HashMap<i64, Extra>) -> Vec<S1> {
    s.iter()
        .map(|x| S1 {
            ts: x.ts, hr: x.hr, rr: x.rr.clone(), grav: x.grav, dyn_g: x.dyn_g, skin: x.skin,
            sleep: x.band == "sleep", still_band: x.band == "still", steps: x.steps,
            motion_flag: extras.get(&x.ts).is_some_and(|e| e.cardiac_flags & MOTION_FLAGS != 0),
        })
        .collect()
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

fn resting_five_minute_hr(samples: &[Sample]) -> Option<f64> {
    samples.windows(300).filter(|w| w.windows(2).all(|p| p[1].ts == p[0].ts + 1)
        && w.iter().all(|s| s.hr.is_finite() && s.hr > 0.0))
        .map(|w| w.iter().map(|s| s.hr).sum::<f64>() / 300.0).reduce(f64::min)
}

fn analyze_night(s: &[Sample], history: &[Value], optical_windows: &[crate::whoop5_records::R26Record], spo2_values: &[(i64, u8)], extras: &std::collections::HashMap<i64, Extra>) -> Value {
    let Some(sp) = sleep_period(s) else { return json!({ "available": false, "firmware_sleep_min": s.iter().filter(|x| x.band == "sleep").count() / 60 }) };
    let (mut stages, onset_ts) = stage_epochs(s, sp.onset, sp.wake);
    let s1_night = to_s1(&s[sp.onset..=sp.wake], extras);
    let cpc_w = hi::cpc(&s1_night);
    let rule_counts = ["wake", "light", "deep", "rem"].map(|k| (stages.iter().filter(|x| **x == k).count() as f64 * EPOCH as f64 / 60.0).round());
    // CPC refinement (RR-only approximation of Thomas 2005): stable coupling in the first 2/3 of
    // the night supports N3, very-low-frequency coupling after the first hour supports REM,
    // unstable / VLF coupling contradicts N3. Without it the HR/HRV rules alone find almost no N3
    // on real nights (0-14 min vs 9-59 min), which is physiologically implausible. Epochs without
    // data stay unknown.
    let n_ep = stages.len();
    for (k, st) in stages.iter_mut().enumerate() {
        let t = onset_ts + k as i64 * EPOCH;
        let Some((_, class)) = cpc_w.iter().find(|(c, _)| (t - c).abs() <= 128) else { continue };
        let frac = k as f64 / n_ep.max(1) as f64;
        match (*st, *class) {
            ("light", "stable") if frac < 0.66 => *st = "deep",
            ("deep", "unstable") | ("deep", "vlf") => *st = "light",
            ("light", "vlf") if k > 120 => *st = "rem",
            _ => {}
        }
    }
    let count = |k: &str| (stages.iter().filter(|x| **x == k).count() as f64 * EPOCH as f64 / 60.0).round();
    let (wake_m, light, deep, rem) = (count("wake"), count("light"), count("deep"), count("rem"));
    let asleep = light + deep + rem;
    let unknown_m = count("unknown");
    let scored = stages.iter().filter(|x| **x != "unknown").count();
    let coverage = scored as f64 / stages.len().max(1) as f64;
    // Short capture gaps are excluded from the stage minutes; below 90 % coverage the night is a
    // lower bound and no score is derived from it (an interruption is not a short night).
    let complete = coverage >= SLEEP_MIN_COVERAGE;
    let in_bed = ((s[sp.wake].ts - s[sp.bed].ts) as f64 / 60.0).round();
    let night = &s[sp.onset..=sp.wake];
    let hr: Vec<f64> = night.iter().map(|x| x.hr).filter(|v| v.is_finite() && *v > 0.0).collect();
    let spec = hi::spectral(&s1_night, 300, 150);
    let by_stage = |stage: &str| -> Value {
        let sel: Vec<&hi::SpecWin> = spec.iter().filter(|w| {
            let k = ((w.t + 150 - onset_ts) / EPOCH).max(0) as usize;
            stages.get(k) == Some(&stage)
        }).collect();
        let hf: Vec<f64> = sel.iter().map(|w| w.hf).collect();
        let r: Vec<f64> = sel.iter().map(|w| w.lf_hf()).collect();
        json!({"windows": sel.len(), "hf_ms2": median(&hf).map(|v| round(v, 0)), "lf_hf": median(&r).map(|v| round(v, 2))})
    };
    let (_, breathing) = hi::breathing(&s1_night);
    let surges = hi::hr_surges(&s1_night);
    let s1_bed = to_s1(&s[sp.bed..=sp.wake], extras);
    // NaN is encoded as null by serde_json; never substitute a single-sample minimum.
    let resting = resting_five_minute_hr(night).unwrap_or(f64::NAN);
    let third = s[sp.onset].ts + 2 * (s[sp.wake].ts - s[sp.onset].ts) / 3;
    let last_third: Vec<Sample> = night.iter().filter(|x| x.ts >= third).cloned().collect();
    let skin_vals: Vec<f64> = night.iter().filter_map(|x| x.skin).collect();
    let skin = median(&skin_vals).map(|v| round(v, 2));
    // quality mask: drop R-R of seconds the strap flags as moving (cardiac_flags bits 0/4/6)
    let masked: Vec<Sample> = night
        .iter()
        .map(|x| {
            let mut y = x.clone();
            if extras.get(&x.ts).is_some_and(|e| e.cardiac_flags & MOTION_FLAGS != 0) {
                y.rr.clear();
            }
            y
        })
        .collect();
    let masked_secs = night.len() - masked.iter().zip(night).filter(|(a, b)| a.rr.len() == b.rr.len()).count();
    let (hrv, hrv_qc_json, clean_segs) = hrv_with_qc(&masked);
    let (hrv_unmasked, _, _) = hrv_with_qc(night);
    // firmware-side signals over the sleep period
    let ex: Vec<&Extra> = night.iter().filter_map(|x| extras.get(&x.ts)).collect();
    let fw_rest: Vec<f64> = ex.iter().map(|e| f64::from(e.fw_rest_hr)).filter(|v| *v > 0.0).collect();
    let still_amp: Vec<f64> = night.iter().filter(|x| x.dyn_g.is_some_and(|d| d < 0.02)).filter_map(|x| extras.get(&x.ts)).map(|e| f64::from(e.opt_amp)).filter(|v| *v > 0.0).collect();
    let aux: Vec<f64> = ex.iter().filter_map(|e| e.aux_temp).collect();
    let conf: Vec<f64> = ex.iter().map(|e| f64::from(e.hr_quality)).collect();
    let prev_amp: Vec<f64> = history.iter().filter_map(|h| h["perfusion"]["optical_amplitude_median"].as_f64()).collect();
    let amp_med = median(&still_amp);
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
    let code: String = stages.iter().map(|x| match *x { "wake" => 'W', "rem" => 'R', "deep" => 'D', "light" => 'L', _ => 'U' }).collect();
    let rr_count: usize = night.iter().map(|x| x.rr.len()).sum();
    let mut out = json!({
        "available": true,
        "bed_time": s[sp.bed].ts, "sleep_onset": s[sp.onset].ts, "wake_time": s[sp.wake].ts,
        "time_in_bed_min": in_bed, "asleep_min": asleep,
        "latency_min": ((s[sp.onset].ts - s[sp.bed].ts) as f64 / 60.0).round(),
        "efficiency_pct": complete.then(|| round(100.0 * asleep / (in_bed - unknown_m).max(1.0), 1)),
        "unknown_min": unknown_m,
        "unscored_boundary_seconds": (s[sp.wake].ts - onset_ts + 1).rem_euclid(EPOCH),
        "sleep_coverage_pct": round(100.0 * coverage, 1),
        "sleep_duration_status": if complete { "COMPLETE" } else { "INCOMPLETE_LOWER_BOUND" },
        "stages_min": {"wake": wake_m, "light": light, "deep": deep, "rem": rem},
        "awakenings": stages.windows(2).filter(|w| matches!(w[0], "light" | "deep" | "rem") && w[1] == "wake").count(),
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
            "resting_hr": baselines::deviation_json(resting.is_finite().then(|| round(resting, 1)), b_rhr, 1),
            "respiration": baselines::deviation_json(respiration["rpm"].as_f64(), b_resp, 1),
        },
        "sleep_detail": {
            "source": "inferred locally (band state + Cole-Kripke + HR/HRV rules); firmware band state shown separately",
            "firmware_sleep_min": night.iter().filter(|x| x.band == "sleep").count() / 60,
            "waso_min": wake_m,
            "fragmentation_per_h": round(stages.windows(2).filter(|w| matches!(w[0], "light" | "deep" | "rem") && w[1] == "wake").count() as f64 / (asleep / 60.0).max(0.1), 2),
            "movement_pct": round(100.0 * moved as f64 / night.len().max(1) as f64, 1),
            "nocturnal_hr": {"mean": round(mean(&hr).unwrap_or(0.0), 1), "min_5min": round(resting, 1), "p95": percentile(&hr, 95.0).map(|v| round(v, 1))},
            "stages_status": "EXPERIMENTAL; not validated against PSG",
        },
        "optical": optical,
        "stages_rule_v1_min": {"wake": rule_counts[0], "light": rule_counts[1], "deep": rule_counts[2], "rem": rule_counts[3]},
        "hrv_spectral": {"night": hi::spectral_summary(&spec), "deep": by_stage("deep"), "light": by_stage("light"), "rem": by_stage("rem")},
        "cpc": hi::cpc_summary(&cpc_w),
        "breathing_regularity": breathing,
        "movement": hi::movement(&s1_bed),
        "morning_hrv": hi::morning_hrv(&s1_night, s[sp.wake].ts),
        "skin_onset_warming_c": hi::skin_onset_warming(&to_s1(s, extras), s[sp.bed].ts, s[sp.onset].ts),
        "spo2": spo2_summary(&spo2_values.iter().copied().filter(|(t, _)| *t >= s[sp.bed].ts && *t <= s[sp.wake].ts).collect::<Vec<_>>()),
        "rr_coverage_pct": round(100.0 * rr_count as f64 / (hr.iter().sum::<f64>() / 60.0).max(1.0), 1),
        "quality_mask": {
            "method": "R-R ignored in seconds with cardiac_flags bits 0/4/6 (strap motion flags)",
            "masked_seconds_pct": round(100.0 * masked_secs as f64 / night.len().max(1) as f64, 1),
            "rmssd_without_mask_ms": hrv_unmasked["rmssd_ms"].clone(),
            "hr_confidence_median": median(&conf),
        },
        "firmware_resting_hr": {
            "median_bpm": median(&fw_rest).map(|v| round(v, 1)),
            "p10_bpm": percentile(&fw_rest, 10.0).map(|v| round(v, 1)),
            "minus_ours_bpm": median(&fw_rest).map(|v| round(v - resting, 1)),
            "source": "R18 rr_adjacent high byte (0 when active)",
            "status": "DECODED_UNVALIDATED",
        },
        "perfusion": {
            "optical_amplitude_median": amp_med,
            "vs_previous_nights_pct": match (amp_med, prev_amp.len() >= 3) { (Some(a), true) => Some(round(100.0 * (a / mean(&prev_amp).unwrap() - 1.0), 1)), _ => None },
            "source": "R18 optical amplitude, still seconds of the sleep period (per-second, relative units)",
            "status": "EXPERIMENTAL",
        },
        "environment_temp": {
            "board_temp_median_c": median(&aux).map(|v| round(v, 2)),
            "skin_minus_board_c": match (skin, median(&aux)) { (Some(a), Some(b)) => Some(round(a - b, 2)), _ => None },
            "source": "R18 aux temperature 2 (board / under-cover environment)",
            "status": "DECODED_UNVALIDATED",
        },
    });
    let spo2_series: Vec<(i64, f64)> = out["spo2"]["series"].as_array().map(|a| a.iter().filter_map(|x| Some((x[0].as_i64()?, x[1].as_f64()?))).collect()).unwrap_or_default();
    out["respiratory_disturbance"] = hi::respiratory_disturbance(&spo2_series, surges.len(), asleep / 60.0, &out["breathing_regularity"]);
    out["sleep_score"] = sleep_score(&out, &prev, SLEEP_NEED_MIN);
    out["recovery"] = baselines::recovery(&baselines::RecoveryInputs {
        rmssd: hrv["rmssd_ms"].as_f64(),
        resting_hr: resting.is_finite().then(|| round(resting, 1)),
        resp: respiration["rpm"].as_f64(),
        efficiency_pct: out["efficiency_pct"].as_f64(),
        hrv_base: b_hrv,
        rhr_base: b_rhr,
        resp_base: b_resp,
    });
    if !complete {
        out["recovery"] = json!({"score": null, "reason": "incomplete sleep coverage"});
    }
    out
}

/// Default adult sleep need (NSF: 7-9 h for 18-64 y, Hirshkowitz 2015; AASM/SRS >= 7 h).
pub const SLEEP_NEED_MIN: f64 = 480.0;

/// Linear 0-100 between a "poor" and a "good" threshold (either direction).
fn ramp(v: f64, poor: f64, good: f64) -> f64 {
    (100.0 * (v - poor) / (good - poor)).clamp(0.0, 100.0)
}

/// Composite sleep score, 0-100, built on the RU-SATED dimensions (Buysse 2014) we can measure:
/// - duration vs need (40 %): Hirshkowitz 2015 (NSF), Watson 2015 (AASM/SRS) — 100 at >= need,
///   0 at <= half of it;
/// - continuity (30 %): NSF sleep-quality indicators (Ohayon 2017): efficiency >= 85 % good (65 %
///   poor), latency <= 30 min good (>= 60 min poor), WASO <= 20 min good (>= 60 min poor);
/// - regularity (15 %): mid-sleep shift vs the median of the previous 7 nights, <= 30 min good,
///   >= 2 h poor (Phillips 2017 SRI, Windred 2024: regularity predicts outcomes beyond duration;
///   social jet lag, Wittmann 2006);
/// - architecture (15 %, low weight: staging without EEG is ~65-73 % accurate): deep >= 15 % and
///   REM >= 20 % good, deep <= 5 % and REM <= 10 % poor (Ohayon 2004 norms, Ohayon 2017).
/// Missing components are dropped and the remaining weights renormalised.
fn sleep_score(n: &Value, prev: &[&Value], need_min: f64) -> Value {
    if n["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND" {
        return json!({"score": null, "need_min": need_min, "components": {}, "reason": "incomplete sleep coverage"});
    }
    let asleep = n["asleep_min"].as_f64();
    let mut comps: Vec<(&str, f64, Option<f64>, Value)> = Vec::new();
    comps.push(("duration", 0.40, asleep.map(|a| ramp(a / need_min, 0.5, 1.0)), json!({"asleep_min": asleep, "need_min": need_min})));
    let eff = n["efficiency_pct"].as_f64();
    let lat = n["latency_min"].as_f64();
    let waso = n["sleep_detail"]["waso_min"].as_f64();
    let cont: Vec<f64> = [eff.map(|e| ramp(e, 65.0, 85.0)), lat.map(|l| ramp(l, 60.0, 30.0)), waso.map(|w| ramp(w, 60.0, 20.0))].into_iter().flatten().collect();
    comps.push(("continuity", 0.30, mean(&cont), json!({"efficiency_pct": eff, "latency_min": lat, "waso_min": waso})));
    let mid = |v: &Value| Some((v["sleep_onset"].as_i64()? + v["wake_time"].as_i64()?) / 2);
    let wrap = |d: i64| ((d % DAY) + DAY + DAY / 2) % DAY - DAY / 2;
    let shift = mid(n).and_then(|m| {
        let devs: Vec<f64> = prev.iter().rev().take(7).filter_map(|p| mid(p)).map(|pm| wrap(m - pm) as f64 / 60.0).collect();
        (devs.len() >= 3).then(|| median(&devs).unwrap().abs())
    });
    comps.push(("regularity", 0.15, shift.map(|m| ramp(m, 120.0, 30.0)), json!({"midpoint_shift_min": shift.map(|v| v.round())})));
    let st = &n["stages_min"];
    let arch = asleep.filter(|a| *a > 60.0).and_then(|a| {
        let deep = 100.0 * st["deep"].as_f64()? / a;
        let rem = 100.0 * st["rem"].as_f64()? / a;
        Some((deep, rem, (ramp(deep, 5.0, 15.0) + ramp(rem, 10.0, 20.0)) / 2.0))
    });
    comps.push(("architecture", 0.15, arch.map(|x| x.2), json!({"deep_pct": arch.map(|x| round(x.0, 1)), "rem_pct": arch.map(|x| round(x.1, 1))})));
    let wsum: f64 = comps.iter().filter(|c| c.2.is_some()).map(|c| c.1).sum();
    let score = (wsum > 0.0 && comps[0].2.is_some()).then(|| comps.iter().filter_map(|c| c.2.map(|v| v * c.1)).sum::<f64>() / wsum);
    let mut components = serde_json::Map::new();
    for (name, w, v, detail) in comps {
        components.insert(name.into(), json!({"weight": w, "score": v.map(|x| x.round()), "detail": detail}));
    }
    json!({
        "score": score.map(|v| v.round()),
        "need_min": need_min,
        "components": components,
        "method": "RU-SATED dimensions: duration vs need 40 %, continuity (efficiency, latency, WASO; NSF/Ohayon 2017) 30 %, regularity (mid-sleep shift) 15 %, architecture (deep/REM share) 15 %",
        "status": "EXPERIMENTAL",
    })
}

/// Awake resting vitals: 5-min windows after the morning wake, awake (band != sleep), still for
/// >= 90 % of seconds (dynamic acceleration < 0.03 g), R-R dropped on strap motion flags.
/// Comparable from day to day; not comparable to the night (posture, digestion, caffeine...).
/// SpO2 is only computed by the strap during sleep, so it has no daytime value.
fn day_rest_vitals(s: &[Sample], after: i64, extras: &std::collections::HashMap<i64, Extra>) -> Value {
    let awake: Vec<&Sample> = s.iter().filter(|x| x.ts > after && x.band != "sleep" && x.hr > 0.0).collect();
    let mut wins: BTreeMap<i64, Vec<&Sample>> = BTreeMap::new();
    for x in &awake {
        wins.entry(x.ts / 300).or_default().push(x);
    }
    let still: Vec<Vec<&Sample>> = wins
        .into_values()
        .filter(|w| w.len() >= 270 && w.iter().filter(|x| x.dyn_g.is_some_and(|d| d < 0.03)).count() as f64 >= 0.9 * w.len() as f64)
        .collect();
    let skin: Vec<f64> = awake.iter().filter_map(|x| x.skin).collect();
    let skin_med = median(&skin).map(|v| round(v, 2));
    if still.len() < 3 {
        return json!({"available": false, "still_windows": still.len(), "skin_temp_c": skin_med});
    }
    let hr_means: Vec<f64> = still.iter().map(|w| w.iter().map(|x| x.hr).sum::<f64>() / w.len() as f64).collect();
    let samples: Vec<Sample> = still
        .iter()
        .flatten()
        .map(|x| {
            let mut y = (*x).clone();
            if extras.get(&x.ts).is_some_and(|e| e.cardiac_flags & MOTION_FLAGS != 0) {
                y.rr.clear();
            }
            y
        })
        .collect();
    let (hrv, _, _) = hrv_with_qc(&samples);
    let resp = respiratory_rate(&samples);
    json!({
        "available": true,
        "still_windows": still.len(),
        "still_min": still.len() * 5,
        "resting_hr_bpm": percentile(&hr_means, 10.0).map(|v| round(v, 1)),
        "hr_still_median_bpm": median(&hr_means).map(|v| round(v, 1)),
        "rmssd_ms": hrv["rmssd_ms"].clone(),
        "respiration_rpm": resp["rpm"].clone(),
        "skin_temp_c": skin_med,
        "method": "awake 5-min windows, >= 90 % still (< 0.03 g), after wake; HRV/respiration on R-R without strap motion flags",
        "status": "EXPERIMENTAL",
    })
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

/// Cadence from the R18 step-period field, self-calibrated on the step counter: k = median over
/// walking minutes of counted steps/min x raw / 60 (5 ms ticks ~ 200 by default, 196.6 measured).
fn cadence(s: &[Sample], extras: &std::collections::HashMap<i64, Extra>) -> Value {
    let walking = |x: &Sample| matches!(x.activity.as_deref(), Some("walk") | Some("run"));
    let mut ks = Vec::new();
    let mut minute: BTreeMap<i64, Vec<&Sample>> = BTreeMap::new();
    for x in s {
        minute.entry(x.ts / 60).or_default().push(x);
    }
    for secs in minute.values() {
        let w: Vec<&&Sample> = secs.iter().filter(|x| walking(x)).collect();
        if secs.len() < 55 || w.len() < 40 {
            continue;
        }
        let steps: i64 = secs.windows(2).map(|p| p[1].steps - p[0].steps).filter(|d| *d > 0 && *d < 300).sum();
        let raws: Vec<f64> = w.iter().filter_map(|x| extras.get(&x.ts)).map(|e| f64::from(e.cadence_raw)).filter(|v| *v > 20.0).collect();
        if steps >= 40 {
            if let Some(r) = median(&raws) {
                ks.push(steps as f64 * 60.0 / w.len() as f64 * r / 60.0);
            }
        }
    }
    let (k, src) = if ks.len() >= 5 { (median(&ks).unwrap(), "self-calibrated on the step counter") } else { (200.0, "default 5 ms tick (196.6 measured on 8 min)") };
    let spm: Vec<f64> = s.iter().filter(|x| walking(x)).filter_map(|x| extras.get(&x.ts)).map(|e| f64::from(e.cadence_raw)).filter(|v| *v > 20.0).map(|r| 60.0 * k / r).collect();
    if spm.len() < 60 {
        return json!({"available": false, "status": "UNAVAILABLE"});
    }
    json!({
        "available": true,
        "median_spm": round(median(&spm).unwrap(), 0),
        "p90_spm": round(percentile(&spm, 90.0).unwrap(), 0),
        "walking_seconds": spm.len(),
        "calibration": {"k": round(k, 1), "minutes": ks.len(), "source": src},
        "status": "EXPERIMENTAL",
    })
}

fn analyze_day(s: &[Sample], day_start: i64, now: i64, profile: &Profile, resting_hr: Option<f64>, extras: &std::collections::HashMap<i64, Extra>) -> Value {
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
        // same saturating curve as a share of the day's capacity: 100 % is unreachable, 50 % ~ 1 h hard
        "strain_pct": round(100.0 * (1.0 - (-trimp / 250.0).exp()), 0),
        "hrmax_used": hrmax.round(),
        "energy": {"resting_kcal": resting_kcal.round(), "active_kcal": active_kcal.round(), "total_kcal": (resting_kcal + active_kcal).round(),
                   "profile_defaults_used": profile.weight_kg.is_none() || profile.age.is_none()},
        "hr_mean": round(mean(&hrs).unwrap_or(0.0), 1),
        "hr_min": hrs.iter().copied().fold(f64::INFINITY, f64::min),
        "hr_max": hr_max_obs,
        "hourly": hourly,
        "stress": stress(s, rhr, hrmax),
        "age_default_used": profile.age.is_none(),
        "training": training_block(s, rhr, age, profile, extras),
        "sedentary": hi::sedentary(&to_s1(s, extras)),
        "cadence": cadence(s, extras),
    })
}

fn training_block(s: &[Sample], rhr: f64, age: f64, profile: &Profile, extras: &std::collections::HashMap<i64, Extra>) -> Value {
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
            let part: Vec<&Sample> = s.iter().filter(|x| x.ts >= a && x.ts <= b).collect();
            let minutes = ((b - a) as f64 / 60.0).max(1.0);
            let steps: i64 = part.windows(2).map(|p| p[1].steps - p[0].steps).filter(|d| *d > 0 && *d < 300).sum();
            let cad: Vec<f64> = part.iter().filter_map(|x| extras.get(&x.ts)).map(|e| f64::from(e.cadence_raw)).filter(|v| *v > 20.0).map(|r| 60.0 * 200.0 / r).collect();
            let dyns: Vec<f64> = part.iter().filter_map(|x| x.dyn_g).collect();
            let dm = mean(&dyns).unwrap_or(0.0);
            let dcv = std_sample(&dyns).unwrap_or(0.0) / dm.max(1e-6);
            let hrr = (mean(&hr).unwrap_or(rhr) - rhr) / (hrmax - rhr).max(1.0);
            sess["type"] = json!(hi::workout_type(steps as f64 / minutes, median(&cad), dm, dcv, hrr));
            sess["steps_per_min"] = json!(round(steps as f64 / minutes, 0));
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

/// Last 7 days: 24-h cosinor of wrist skin temperature (peak = night) and of HR (trough = night),
/// sleep midpoint, Sleep Regularity Index on 1-min band states.
fn circadian_rhythm(conn: &Connection, off: i64, now: i64) -> GooseResult<Value> {
    let from = now - 7 * DAY;
    let mut st = conn.prepare("SELECT ts / 3600, AVG(skin_centi) / 100.0, AVG(hr), SUM(band = 'sleep') FROM whoop_lab_biometric WHERE ts >= ?1 GROUP BY 1 HAVING COUNT(*) > 1800")?;
    let rows: Vec<(i64, Option<f64>, f64)> = st.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    let local_h = |h: i64| ((h * 3600 + off).rem_euclid(DAY)) as f64 / 3600.0;
    let skin: Vec<(f64, f64)> = rows.iter().filter_map(|(h, s, _)| s.map(|v| (local_h(*h), v))).collect();
    let hr: Vec<(f64, f64)> = rows.iter().map(|(h, _, v)| (local_h(*h), *v)).collect();
    let mut sm = conn.prepare("SELECT ts / 60, MAX(band = 'sleep') FROM whoop_lab_biometric WHERE ts >= ?1 GROUP BY 1")?;
    let minutes: std::collections::BTreeMap<i64, bool> = sm.query_map([from], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? == 1)))?.collect::<Result<_, _>>()?;
    let cos = |p: &[(f64, f64)]| hi::cosinor(p).map(|(m, a, phi)| json!({"mesor": round(m, 2), "amplitude": round(a, 2), "acrophase_h": round(phi, 1)}));
    Ok(json!({
        "hours": rows.len(),
        "skin_temp": cos(&skin),
        "heart_rate": cos(&hr).map(|mut v| { let a = v["acrophase_h"].as_f64().unwrap(); v["trough_h"] = json!(round((a + 12.0).rem_euclid(24.0), 1)); v }),
        "sleep_regularity_index": hi::sleep_regularity_index(&minutes),
        "method": "single-component 24-h cosinor on hourly means (last 7 days); SRI (Phillips 2017) on 1-min firmware sleep states",
    }))
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
        "night.quality_mask": p("R18 cardiac_flags bits 0/4/6", "R-R of seconds flagged as moving are dropped before HRV", "", "EXPERIMENTAL"),
        "night.firmware_resting_hr": p("R18 rr_adjacent high byte", "median / p10 during sleep, compared with ours", "bpm", "DECODED_UNVALIDATED"),
        "night.perfusion": p("R18 optical amplitude", "median over still sleep seconds, vs previous nights", "a.u.", "EXPERIMENTAL"),
        "night.environment_temp": p("R18 aux temperature 2", "night median, skin minus board gradient", "degC", "DECODED_UNVALIDATED"),
        "day.cadence": p("R18 step-period field", "60 x k / raw, k self-calibrated on the step counter", "steps/min", "EXPERIMENTAL"),
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
/// Cache key: algorithm version + everything outside the per-day input that changes a day's
/// result (decoder version, profile, UTC offset). New samples are caught by the per-day row count
/// and last timestamp, so a sync only recomputes the days it touched.
fn cache_context(conn: &Connection, options: &AnalyzeOptions) -> GooseResult<String> {
    let decoder: Option<String> = conn.query_row("SELECT value FROM whoop5_state WHERE key = 'decoder_version'", [], |r| r.get(0)).optional().unwrap_or(None);
    let context = json!({"decoder": decoder, "profile": options.profile, "utc_offset_seconds": options.utc_offset_seconds});
    let digest = Sha256::digest(context.to_string().as_bytes());
    Ok(format!("{}:{}", ALGORITHM_VERSION, digest.iter().take(6).map(|b| format!("{b:02x}")).collect::<String>()))
}

pub fn analyze(options: &AnalyzeOptions) -> GooseResult<Value> {
    let conn = Connection::open(&options.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode = WAL;")?;
    sync_biometric_table(&conn)?;
    let layer_b = whoop5_store::ingest(&conn)?;
    let cache_version = cache_context(&conn, options)?;
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
        let reuse = cached.as_ref().is_some_and(|(c, l, v, _)| *c == cnt && *l == last_ts && v == &cache_version) && day_start < today_start;
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
            let extras = load_extras(&conn, n0, d1)?;
            let night = analyze_night(&night_samples, &nights_hist, &optical_windows, &spo2_values, &extras);
            let day_samples = load_samples(&conn, d0, d1)?;
            let rhr = night["resting_hr_bpm"].as_f64();
            let mut day = analyze_day(&day_samples, d0, now, &options.profile, rhr, &extras);
            if day.is_object() {
                let after = night["wake_time"].as_i64().unwrap_or(d0);
                day["rest_vitals"] = day_rest_vitals(&day_samples, after, &extras);
            }
            let value = json!({ "date": key, "night": night, "day": day });
            conn.execute(
                "INSERT OR REPLACE INTO whoop_lab_days (date_key, input_count, input_last_ts, algorithm_version, result_json) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, cnt, last_ts, cache_version, value.to_string()],
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
    // composite indices, computed for every day from the days up to it
    let series: Vec<hi::DaySeries> = days.iter().map(|d| {
        let n = &d["night"];
        let hrr = d["day"]["training"]["workouts"]["sessions"].as_array().and_then(|a| a.iter().filter_map(|s| s["hr_recovery"]["drop_60s"].as_f64()).reduce(f64::max));
        hi::DaySeries {
            ln_rmssd: n["hrv_night"]["ln_rmssd"].as_f64(), rhr: n["resting_hr_bpm"].as_f64(), resp: n["respiration"]["rpm"].as_f64(),
            skin: n["skin_temp_c"].as_f64(), spo2: n["spo2"]["median_pct"].as_f64(), asleep_min: if n["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND" { None } else { n["asleep_min"].as_f64() },
            need_min: 480.0, trimp: d["day"]["edwards_trimp"].as_f64().unwrap_or(0.0), hrr60: hrr,
            morning_ln_rmssd: n["morning_hrv"]["ln_rmssd"].as_f64(),
        }
    }).collect();
    let age = options.profile.age.unwrap_or(30.0);
    let rhrs: Vec<f64> = series.iter().filter_map(|x| x.rhr).collect();
    let day_max: Vec<Option<f64>> = days.iter().map(|d| d["day"]["hr_max"].as_f64()).collect();
    for i in 0..days.len() {
        let upto = &series[..=i];
        let fat = hi::fatigue(upto);
        let rhr7 = median(&upto.iter().rev().filter_map(|x| x.rhr).take(7).collect::<Vec<_>>());
        let hrmax = (208.0 - 0.7 * age).max(day_max.iter().take(i + 1).filter_map(|v| *v).fold(0.0, f64::max));
        days[i]["indices"] = json!({
            "fatigue": fat.clone(),
            "recovery": if days[i]["night"]["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND" {
                json!({"score": null, "reason": "incomplete sleep coverage"})
            } else { hi::recovery_v2(upto) },
            "early_warning": hi::early_warning(upto, fat["score"].as_f64()),
            "vo2max": {"ml_kg_min": rhr7.and_then(|r| hi::vo2max_uth(hrmax, r)), "hrmax_used": hrmax.round(), "resting_hr_7d": rhr7,
                       "method": "Uth 2004: 15.3 x HRmax / HRrest; HRmax = max(Tanaka, observed)"},
        });
    }
    let _ = rhrs;
    let circadian = circadian_rhythm(&conn, off, now)?;
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
        "circadian": circadian,
        "layer_b_ingest": serde_json::to_value(&layer_b).unwrap_or(Value::Null),
        "provenance": provenance(),
        "device": {"records": total, "first": first, "last": last,
                   "coverage_pct": round(100.0 * total as f64 / ((last - first + 1) as f64).max(1.0), 1)},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleep_samples() -> Vec<Sample> {
        (0..300).map(|ts| Sample { ts, hr: 60.0, rr: vec![1000.0],
            grav: Some([0.0, 0.0, 1.0]), dyn_g: Some(0.0), steps: 0,
            activity: None, skin: Some(33.0), band: "sleep".into() }).collect()
    }

    #[test]
    fn resting_hr_requires_contiguous_five_minutes() {
        let mut samples = sleep_samples();
        assert_eq!(resting_five_minute_hr(&samples), Some(60.0));
        for s in &mut samples[150..] { s.ts += 30; }
        assert!(resting_five_minute_hr(&samples).is_none());
    }

    #[test]
    fn missing_sleep_epochs_are_unknown_and_not_smoothed() {
        let samples: Vec<_> = sleep_samples().into_iter().filter(|s| !(120..150).contains(&s.ts)).collect();
        let (labels, _) = stage_epochs(&samples, 0, samples.len() - 1);
        assert_eq!(labels[4], "unknown");
        assert_ne!(labels[3], "unknown");
        assert_ne!(labels[5], "unknown");
    }

    #[test]
    fn missing_motion_cannot_establish_sleep() {
        let mut samples = sleep_samples();
        for sample in &mut samples[60..90] { sample.grav = None; }
        assert_eq!(stage_epochs(&samples, 0, samples.len() - 1).0[2], "unknown");
    }

    #[test]
    fn cache_key_tracks_profile_and_offset_but_not_new_samples() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_tables(&conn).unwrap();
        whoop5_store::ensure_tables(&conn).unwrap();
        let mut options = AnalyzeOptions { database_path: String::new(), utc_offset_seconds: 3600,
            now_unix: None, profile: Profile::default() };
        let a = cache_context(&conn, &options).unwrap();
        assert!(a.starts_with(ALGORITHM_VERSION));
        // new samples are handled by the per-day count / last timestamp, not by the context key
        conn.execute("INSERT INTO whoop_lab_biometric VALUES (1,60,'1000',0,0,1000,0,0,NULL,3300,'sleep')", []).unwrap();
        assert_eq!(a, cache_context(&conn, &options).unwrap());
        options.profile.age = Some(40.0);
        let b = cache_context(&conn, &options).unwrap();
        assert_ne!(a, b);
        options.utc_offset_seconds = 0;
        assert_ne!(b, cache_context(&conn, &options).unwrap());
    }

    #[test]
    fn short_gap_keeps_night_scored_long_gap_does_not() {
        // 2 h of sleep; a 2-min hole stays COMPLETE (unknown minutes excluded), a 30-min hole does not
        let full: Vec<Sample> = (0..7200).map(|ts| Sample { ts, hr: 60.0, rr: vec![1000.0],
            grav: Some([0.0, 0.0, 1.0]), dyn_g: Some(0.0), steps: 0, activity: None, skin: Some(33.0), band: "sleep".into() }).collect();
        for (hole, complete) in [(120, true), (1800, false)] {
            let s: Vec<_> = full.iter().filter(|x| !(3000..3000 + hole).contains(&x.ts)).cloned().collect();
            let (labels, _) = stage_epochs(&s, 0, s.len() - 1);
            let cov = labels.iter().filter(|l| **l != "unknown").count() as f64 / labels.len() as f64;
            assert_eq!(cov >= SLEEP_MIN_COVERAGE, complete, "hole {hole}: coverage {cov}");
        }
    }

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
