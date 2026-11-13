//! Sport Intelligence Engine: activity detection, adaptive acquisition, classification, sets and
//! repetitions, personal dataset with human labels.
//!
//! Sensing is tiered (see docs/SPORT.md):
//!   tier 1  R18, 1 Hz, always recorded on the strap, reaches the phone at each history sync
//!           (HR, steps, firmware still/walk/run, dynamic acceleration, gravity vector);
//!   tier 2  live packet 40 (HR + R-R) and live R21 IMU (100 Hz accel + gyro), switched on by the
//!           phone from the moment a session becomes plausible ("watch") until the post-buffer ends.
//! The online state machine (`update`) runs on per-minute features: idle -> watch -> active ->
//! ending -> idle. Start/end are dated on the strap's own timestamps, so a session is backdated
//! to its first active minute (pre-buffer from R18, always available). `analyze` classifies a
//! session (rules v1, or an installed personal model) and, from the IMU, finds strokes (tennis),
//! sets / rests / repetitions / movement phases (strength) and exercises (nearest labelled set).
//! Every prediction carries ENGINE_VERSION + model version; every human answer is appended to
//! sport_annotations, never overwritten.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dsp::{mean, median, percentile, round};
use crate::GooseResult;

pub const ENGINE_VERSION: &str = "sport-3"; // 3: neutral-palm walking excluded, strength signature = 3 named sets + cardiac load; 2: repetition-based sets
pub const RULES_MODEL: &str = "rules-v1";
pub const CLASSES: [&str; 4] = ["course", "tennis", "musculation", "autre"];
/// Session feature vector (order is the contract with lab/sport/train.py and installed models).
pub const FEATURES: [&str; 16] = [
    "dur_min", "hrr_mean", "hrr_p90", "steps_pm", "cad_spm", "run_frac", "walk_frac", "dyn_mean", "dyn_cv", "grav_var",
    "active_alt", "imu_cov", "strokes_pm", "sets_per_10min", "periodic_frac", "gyro_p95",
];
const PRE_BUFFER_S: i64 = 120;
const POST_BUFFER_S: i64 = 180;
const IMU_FS: f64 = 100.0;

pub fn ensure_tables(conn: &Connection) -> GooseResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sport_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS sport_live (ts INTEGER PRIMARY KEY, hr REAL, rr TEXT NOT NULL, arrival REAL);
        CREATE TABLE IF NOT EXISTS sport_imu (ts REAL PRIMARY KEY, arrival REAL, frame BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS sport_sessions (
            id TEXT PRIMARY KEY, start_ts INTEGER NOT NULL, end_ts INTEGER, status TEXT NOT NULL,
            source TEXT NOT NULL, predicted_type TEXT, probs_json TEXT, confidence REAL,
            features_json TEXT, summary_json TEXT, engine_version TEXT NOT NULL, model_version TEXT,
            label_type TEXT, label_status TEXT NOT NULL DEFAULT 'pending', analyzed_at INTEGER,
            created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sport_segments (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, start_ts REAL NOT NULL, end_ts REAL NOT NULL,
            reps_pred INTEGER, reps_label INTEGER, exercise_pred TEXT, exercise_conf REAL, exercise_label TEXT,
            confidence REAL, detail_json TEXT, label_status TEXT NOT NULL DEFAULT 'pending', engine_version TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS sport_segments_session ON sport_segments (session_id, start_ts);
        CREATE TABLE IF NOT EXISTS sport_annotations (
            id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, target TEXT NOT NULL, target_id TEXT NOT NULL,
            field TEXT NOT NULL, old_value TEXT, new_value TEXT, predicted_value TEXT, confidence REAL,
            model_version TEXT, engine_version TEXT NOT NULL, source TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sport_models (
            version TEXT PRIMARY KEY, kind TEXT NOT NULL, created_at INTEGER NOT NULL, active INTEGER NOT NULL DEFAULT 0,
            params_json TEXT NOT NULL, metrics_json TEXT
        );
        CREATE TABLE IF NOT EXISTS sport_acquisition (
            id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, stream TEXT NOT NULL, start_ts INTEGER NOT NULL,
            end_ts INTEGER, reason TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn state_get(conn: &Connection, key: &str) -> GooseResult<Option<Value>> {
    let v: Option<String> = conn.query_row("SELECT value FROM sport_state WHERE key = ?1", [key], |r| r.get(0)).optional()?;
    Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
}

fn state_set(conn: &Connection, key: &str, v: &Value) -> GooseResult<()> {
    conn.execute("INSERT OR REPLACE INTO sport_state (key, value) VALUES (?1, ?2)", params![key, v.to_string()])?;
    Ok(())
}

// ------------------------------------------------------------------ per-second / per-minute data

#[derive(Debug, Clone, Default)]
pub struct Sec {
    pub ts: i64,
    pub hr: Option<f64>,
    pub steps: Option<i64>,
    pub cadence_raw: Option<f64>,
    pub dyn_g: Option<f64>,
    pub grav: Option<[f64; 3]>,
    pub activity: Option<String>,
    pub sleep: bool,
}

fn load_secs(conn: &Connection, from: i64, to: i64) -> GooseResult<BTreeMap<i64, Sec>> {
    let mut out: BTreeMap<i64, Sec> = BTreeMap::new();
    let has_r18: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='whoop5_r18'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if has_r18 {
        let mut st = conn.prepare_cached(
            "SELECT ts, hr, steps, cadence_raw, dyn_accel_g, grav_x_g, grav_y_g, grav_z_g, activity, band_state FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2",
        )?;
        let rows = st.query_map(params![from, to], |r| {
            let gx: Option<f64> = r.get(5)?;
            let gy: Option<f64> = r.get(6)?;
            let gz: Option<f64> = r.get(7)?;
            let hr: i64 = r.get(1)?;
            Ok(Sec {
                ts: r.get(0)?,
                hr: (hr > 25).then_some(hr as f64),
                steps: r.get(2)?,
                cadence_raw: r.get::<_, Option<i64>>(3)?.map(|v| v as f64).filter(|v| *v > 20.0),
                dyn_g: r.get::<_, Option<f64>>(4)?.filter(|v| v.is_finite() && *v >= 0.0 && *v < 16.0),
                grav: match (gx, gy, gz) {
                    (Some(x), Some(y), Some(z)) => Some([x, y, z]),
                    _ => None,
                },
                activity: r.get(8)?,
                sleep: r.get::<_, Option<String>>(9)?.as_deref() == Some("sleep"),
            })
        })?;
        for s in rows {
            let s = s?;
            out.insert(s.ts, s);
        }
    }
    // live HR fills seconds the history has not delivered yet
    let mut st = conn.prepare_cached("SELECT ts, hr FROM sport_live WHERE ts >= ?1 AND ts < ?2")?;
    for row in st.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?)))? {
        let (ts, hr) = row?;
        let e = out.entry(ts).or_insert_with(|| Sec { ts, ..Default::default() });
        if e.hr.is_none() {
            e.hr = hr.filter(|v| *v > 25.0);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct Minute {
    pub t: i64,
    pub n: usize,
    pub hr: Option<f64>,
    pub steps: f64,
    pub cad_spm: Option<f64>,
    pub dyn_mean: Option<f64>,
    pub dyn_p90: Option<f64>,
    pub grav_var: f64,
    pub run_frac: f64,
    pub walk_frac: f64,
    pub sleep_frac: f64,
    pub imu_dyn: Option<f64>,
}

fn minutes(secs: &BTreeMap<i64, Sec>, imu_dyn: &BTreeMap<i64, f64>) -> Vec<Minute> {
    let mut by: BTreeMap<i64, Vec<&Sec>> = BTreeMap::new();
    for s in secs.values() {
        by.entry(s.ts.div_euclid(60)).or_default().push(s);
    }
    by.into_iter()
        .map(|(m, v)| {
            let hr: Vec<f64> = v.iter().filter_map(|s| s.hr).collect();
            let dyn_: Vec<f64> = v.iter().filter_map(|s| s.dyn_g).collect();
            let steps: i64 = v.windows(2).filter_map(|p| Some((p[1].steps? - p[0].steps?).rem_euclid(65536))).filter(|d| *d < 300).sum();
            let cad: Vec<f64> = v.iter().filter(|s| matches!(s.activity.as_deref(), Some("walk") | Some("run"))).filter_map(|s| s.cadence_raw).map(|r| 60.0 * 196.6 / r).filter(|c| (40.0..260.0).contains(c)).collect();
            let gv: f64 = v.windows(2).filter_map(|p| Some(((p[1].grav?[0] - p[0].grav?[0]).powi(2) + (p[1].grav?[1] - p[0].grav?[1]).powi(2) + (p[1].grav?[2] - p[0].grav?[2]).powi(2)).sqrt())).sum();
            let n = v.len().max(1) as f64;
            Minute {
                t: m * 60,
                n: v.len(),
                hr: mean(&hr),
                steps: steps as f64,
                cad_spm: median(&cad),
                dyn_mean: mean(&dyn_),
                dyn_p90: percentile(&dyn_, 90.0),
                grav_var: gv,
                run_frac: v.iter().filter(|s| s.activity.as_deref() == Some("run")).count() as f64 / n,
                walk_frac: v.iter().filter(|s| s.activity.as_deref() == Some("walk")).count() as f64 / n,
                sleep_frac: v.iter().filter(|s| s.sleep).count() as f64 / n,
                imu_dyn: imu_dyn.get(&m).copied(),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub rest: f64,
    pub hrmax: f64,
}

impl Profile {
    fn hrr(&self, hr: f64) -> f64 {
        ((hr - self.rest) / (self.hrmax - self.rest).max(20.0)).clamp(-0.5, 1.5)
    }
}

fn profile(conn: &Connection, now: i64, age: Option<f64>) -> GooseResult<Profile> {
    if let Some(p) = state_get(conn, "profile")? {
        if p["v"].as_i64() == Some(2) && p["computed_at"].as_i64().unwrap_or(0) > now - 86_400 && p["age"].as_f64() == age {
            return Ok(Profile { rest: p["rest"].as_f64().unwrap_or(55.0), hrmax: p["hrmax"].as_f64().unwrap_or(190.0) });
        }
    }
    let mut rest = 55.0;
    let mut awake: Option<f64> = None;
    let mut observed_max = 0.0;
    let has_r18: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='whoop5_r18'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if has_r18 {
        let sleep_hr: Vec<f64> = conn
            .prepare("SELECT hr FROM whoop5_r18 WHERE ts > ?1 AND band_state = 'sleep' AND hr > 30")?
            .query_map([now - 7 * 86_400], |r| r.get::<_, i64>(0).map(|v| v as f64))?
            .collect::<Result<_, _>>()?;
        if sleep_hr.len() > 3600 {
            rest = percentile(&sleep_hr, 5.0).unwrap_or(rest);
        }
        // activity is judged against the awake, still heart rate (sleep HR would make any walk an
        // effort: 11 Nov replay, sleep p5 = 44 bpm vs ~60 awake)
        let awake_hr: Vec<f64> = conn
            .prepare("SELECT hr FROM whoop5_r18 WHERE ts > ?1 AND band_state != 'sleep' AND dyn_accel_g < 0.03 AND hr > 30")?
            .query_map([now - 7 * 86_400], |r| r.get::<_, i64>(0).map(|v| v as f64))?
            .collect::<Result<_, _>>()?;
        if awake_hr.len() > 7200 {
            awake = percentile(&awake_hr, 10.0);
        }
        let all_hr: Vec<f64> = conn
            .prepare("SELECT hr FROM whoop5_r18 WHERE ts > ?1 AND hr > 30")?
            .query_map([now - 30 * 86_400], |r| r.get::<_, i64>(0).map(|v| v as f64))?
            .collect::<Result<_, _>>()?;
        observed_max = percentile(&all_hr, 99.98).unwrap_or(0.0);
    }
    let tanaka = 208.0 - 0.7 * age.unwrap_or(30.0);
    let p = Profile { rest: awake.unwrap_or(rest + 12.0).max(rest), hrmax: tanaka.max(observed_max) };
    state_set(conn, "profile", &json!({"rest": p.rest, "sleep_rest": rest, "hrmax": p.hrmax, "age": age, "computed_at": now, "v": 2}))?;
    Ok(p)
}

/// Activity evidence of one minute in [0, 1]; > 0.5 = active minute.
pub fn minute_score(m: &Minute, p: &Profile) -> f64 {
    if m.n < 10 || m.sleep_frac > 0.5 {
        return 0.0;
    }
    let hrr = m.hr.map(|h| p.hrr(h)).unwrap_or(0.0);
    let motion = ((m.steps - 30.0) / 70.0)
        // R18 dynamic acceleration: ~0.2-0.35 g walking, 0.6-0.8 g tennis (11 Nov)
        .max((m.dyn_mean.unwrap_or(0.0) - 0.10) / 0.40)
        .max((m.imu_dyn.unwrap_or(0.0) - 0.05) / 0.15)
        .max((m.grav_var - 3.0) / 10.0)
        .clamp(0.0, 1.0);
    if m.steps >= 120.0 || m.run_frac > 0.5 {
        return 0.95;
    }
    // heart-rate reserve is the main evidence (walking to the bus has motion but low HRR)
    let x = 10.0 * (hrr - 0.33) + 1.0 * (motion - 0.3);
    1.0 / (1.0 + (-x).exp())
}

// ------------------------------------------------------------------ IMU

#[derive(Debug, Default, Clone)]
pub struct Imu {
    pub t: Vec<f64>,
    pub a: [Vec<f64>; 3],
    pub g: [Vec<f64>; 3],
}

impl Imu {
    pub fn len(&self) -> usize {
        self.t.len()
    }
    pub fn is_empty(&self) -> bool {
        self.t.is_empty()
    }
    fn push_record(&mut self, r: &crate::whoop5_records::R21Record) {
        let n = r.accel_raw[0].len().min(r.gyro_raw[0].len());
        for i in 0..n {
            self.t.push(r.ts + i as f64 / IMU_FS);
            for k in 0..3 {
                self.a[k].push(f64::from(r.accel_raw[k][i]) / crate::whoop5_records::ACCEL_LSB_PER_G);
                self.g[k].push(f64::from(r.gyro_raw[k][i]) * crate::whoop5_records::GYRO_DPS_PER_LSB);
            }
        }
    }
}

pub fn load_imu(conn: &Connection, from: f64, to: f64) -> GooseResult<Imu> {
    let mut recs: BTreeMap<i64, crate::whoop5_records::R21Record> = BTreeMap::new();
    let mut st = conn.prepare_cached("SELECT frame FROM sport_imu WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
    for f in st.query_map(params![from, to], |r| r.get::<_, Vec<u8>>(0))? {
        let f = f?;
        if f.len() < crate::whoop5_records::HEADER_LEN + 4 {
            continue;
        }
        if let Some(r) = crate::whoop5_records::decode_r21(&f[crate::whoop5_records::HEADER_LEN..f.len() - 4]) {
            recs.insert((r.ts * 1000.0) as i64, r);
        }
    }
    // the same records may also sit in Layer B (history R21, or live frames ingested there)
    if let Ok(more) = crate::whoop5_store::load_r21(conn, from, to) {
        for r in more {
            recs.entry((r.ts * 1000.0) as i64).or_insert(r);
        }
    }
    let mut imu = Imu::default();
    for r in recs.values() {
        imu.push_record(r);
    }
    Ok(imu)
}

/// Strap step-counter increments per second (the counter also counts arm repetitions, so it only
/// helps to tell walking — arm hanging, steps — from exercises done in place).
pub fn steps_per_second(secs: &BTreeMap<i64, Sec>) -> BTreeMap<i64, f64> {
    let v: Vec<&Sec> = secs.values().collect();
    v.windows(2)
        .filter(|p| p[1].ts - p[0].ts == 1)
        .filter_map(|p| Some((p[1].ts, ((p[1].steps? - p[0].steps?).rem_euclid(65536)) as f64)))
        .filter(|(_, d)| *d < 10.0)
        .collect()
}

/// Mean dynamic acceleration (| |a| - 1 g |) per minute, for the online detector.
fn imu_dyn_per_minute(imu: &Imu) -> BTreeMap<i64, f64> {
    let mut acc: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    for i in 0..imu.len() {
        let m = (imu.a[0][i].powi(2) + imu.a[1][i].powi(2) + imu.a[2][i].powi(2)).sqrt();
        let e = acc.entry((imu.t[i] as i64).div_euclid(60)).or_insert((0.0, 0));
        e.0 += (m - 1.0).abs();
        e.1 += 1;
    }
    acc.into_iter().filter(|(_, v)| v.1 >= 1000).map(|(k, v)| (k, v.0 / v.1 as f64)).collect()
}

fn moving_avg(x: &[f64], w: usize) -> Vec<f64> {
    if x.is_empty() || w <= 1 {
        return x.to_vec();
    }
    let mut c = vec![0.0; x.len() + 1];
    for i in 0..x.len() {
        c[i + 1] = c[i] + x[i];
    }
    (0..x.len())
        .map(|i| {
            let lo = i.saturating_sub(w / 2);
            let hi = (i + w / 2 + 1).min(x.len());
            (c[hi] - c[lo]) / (hi - lo) as f64
        })
        .collect()
}

/// 25 Hz working copy: accel (g), gyro (dps), gravity (1 s low-pass), dynamic accel.
#[derive(Debug, Default)]
struct Work {
    t: Vec<f64>,
    a: [Vec<f64>; 3],
    g: [Vec<f64>; 3],
    grav: [Vec<f64>; 3],
}

fn work25(imu: &Imu) -> Work {
    let mut w = Work::default();
    let mut i = 0;
    while i + 4 <= imu.len() {
        if imu.t[i + 3] - imu.t[i] > 0.1 {
            i += 1;
            continue;
        }
        w.t.push(imu.t[i]);
        for k in 0..3 {
            w.a[k].push(imu.a[k][i..i + 4].iter().sum::<f64>() / 4.0);
            w.g[k].push(imu.g[k][i..i + 4].iter().sum::<f64>() / 4.0);
        }
        i += 4;
    }
    for k in 0..3 {
        w.grav[k] = moving_avg(&w.a[k], 25);
    }
    w
}

fn gyro_mag(imu: &Imu, i: usize) -> f64 {
    (imu.g[0][i].powi(2) + imu.g[1][i].powi(2) + imu.g[2][i].powi(2)).sqrt()
}

/// Tennis strokes: an isolated swing. Gyro magnitude peak > 600 deg/s with an acceleration > 3 g
/// within 150 ms, at least 0.8 s from the previous stroke, and no other swing peak (> half its
/// amplitude) within +-0.5 s: shaking the hand (4-6 Hz oscillation) and double/triple taps on the
/// strap (sensor session 12 Nov: 11 and 3 false strokes with a plain threshold) are rejected.
pub fn strokes(imu: &Imu) -> Vec<(f64, f64, f64)> {
    let n = imu.len();
    if n < 3 {
        return Vec::new();
    }
    let gm: Vec<f64> = moving_avg(&(0..n).map(|i| gyro_mag(imu, i)).collect::<Vec<_>>(), 3);
    let am: Vec<f64> = (0..n).map(|i| (imu.a[0][i].powi(2) + imu.a[1][i].powi(2) + imu.a[2][i].powi(2)).sqrt()).collect();
    let peaks: Vec<usize> = (1..n - 1).filter(|&i| gm[i] > 200.0 && gm[i] >= gm[i - 1] && gm[i] > gm[i + 1]).collect();
    let mut out: Vec<(f64, f64, f64)> = Vec::new();
    for (k, &i) in peaks.iter().enumerate() {
        if gm[i] <= 600.0 {
            continue;
        }
        let lo = i.saturating_sub(15);
        let hi = (i + 15).min(n - 1);
        let amax = am[lo..=hi].iter().copied().fold(0.0, f64::max);
        if amax <= 3.0 {
            continue;
        }
        let rivals = peaks[k.saturating_sub(20)..(k + 20).min(peaks.len())]
            .iter()
            .filter(|&&j| j != i && (imu.t[j] - imu.t[i]).abs() <= 0.5 && (imu.t[j] - imu.t[i]).abs() >= 0.08 && gm[j] > 0.5 * gm[i])
            .count();
        if rivals > 0 {
            continue;
        }
        match out.last_mut() {
            Some(last) if imu.t[i] - last.0 < 0.8 => {
                if gm[i] > last.1 {
                    *last = (imu.t[i], gm[i], amax);
                }
            }
            _ => out.push((imu.t[i], gm[i], amax)),
        }
    }
    out
}

fn pca1(x: &[[f64; 3]]) -> [f64; 3] {
    let n = x.len().max(1) as f64;
    let m: [f64; 3] = [0, 1, 2].map(|k| x.iter().map(|v| v[k]).sum::<f64>() / n);
    let mut c = [[0.0; 3]; 3];
    for v in x {
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += (v[i] - m[i]) * (v[j] - m[j]) / n;
            }
        }
    }
    let mut e = [0.577, 0.577, 0.577];
    for _ in 0..50 {
        let y = [0, 1, 2].map(|i| c[i][0] * e[0] + c[i][1] * e[1] + c[i][2] * e[2]);
        let norm = (y[0] * y[0] + y[1] * y[1] + y[2] * y[2]).sqrt();
        if norm < 1e-12 {
            break;
        }
        e = y.map(|v| v / norm);
    }
    e
}

fn autocorr_peak(x: &[f64], fs: f64, min_s: f64, max_s: f64) -> Option<(f64, f64)> {
    let n = x.len();
    if n < 8 {
        return None;
    }
    // linear detrend: a slow orientation sweep must not look periodic at every lag
    let nf = n as f64;
    let mx = (nf - 1.0) / 2.0;
    let my = mean(x)?;
    let sxy: f64 = x.iter().enumerate().map(|(i, v)| (i as f64 - mx) * (v - my)).sum();
    let sxx: f64 = (0..n).map(|i| (i as f64 - mx).powi(2)).sum();
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let y: Vec<f64> = x.iter().enumerate().map(|(i, v)| v - my - slope * (i as f64 - mx)).collect();
    let var: f64 = y.iter().map(|v| v * v).sum();
    if var <= 1e-12 {
        return None;
    }
    let lo = ((min_s * fs) as usize).max(2);
    let hi = ((max_s * fs) as usize).min(n / 2);
    if hi <= lo + 2 {
        return None;
    }
    // Pearson correlation per lag (normalised on the overlap): a slow movement in a short window
    // is not penalised against a fast one
    let r: Vec<(usize, f64)> = (lo - 1..=hi + 1)
        .map(|l| {
            let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
            for i in 0..n - l {
                sab += y[i] * y[i + l];
                saa += y[i] * y[i];
                sbb += y[i + l] * y[i + l];
            }
            (l, sab / (saa * sbb).sqrt().max(1e-12))
        })
        .collect();
    let _ = var;
    // interior local maxima only (a maximum at the edge of the range is not a period)
    let loc: Vec<(usize, f64)> = r.windows(3).filter(|w| w[1].1 >= w[0].1 && w[1].1 > w[2].1).map(|w| w[1]).collect();
    let best = loc.iter().map(|v| v.1).fold(f64::MIN, f64::max);
    if best <= 0.0 {
        return None;
    }
    // first local maximum reaching 85 % of the best: avoids locking on 2x the true period
    let pick = loc.iter().find(|v| v.1 >= 0.85 * best)?;
    Some((pick.0 as f64 / fs, pick.1))
}

#[derive(Debug, Clone)]
pub struct SetResult {
    pub start: f64,
    pub end: f64,
    pub periodic: bool,
    pub reps: usize,
    pub period_s: Option<f64>,
    pub confidence: f64,
    pub up_s: Option<f64>,
    pub down_s: Option<f64>,
    pub hold_s: Option<f64>,
    pub features: Vec<f64>,
    /// orientation change between the first and the last repetition (transition when large)
    pub net_turn: f64,
    /// strap steps per second during the set (walking when high with the arm hanging)
    pub spm: f64,
}

/// Strength sets: bouts of movement energy separated by rests >= 15 s, then repetitions from the
/// autocorrelation of the most periodic axis, and up / down / hold phases from vertical velocity.
pub fn sets_and_reps(imu: &Imu) -> Vec<SetResult> {
    sets_and_reps_with(imu, &BTreeMap::new(), &|_| None)
}

fn sign_norm(v: [f64; 3]) -> [f64; 3] {
    let k = (0..3).max_by(|a, b| v[*a].abs().total_cmp(&v[*b].abs())).unwrap_or(0);
    if v[k] < 0.0 { v.map(|x| -x) } else { v }
}

/// Number of shape dimensions (orientation, movement axes, rotation share) at the head of
/// SetResult::features; then tempo, rotation and velocity amplitudes.
pub const SHAPE_DIMS: usize = 14;

/// Strength sets from repetitions, not from movement energy (12 Nov gym session: walking between
/// exercises kept the energy high and welded 20 sets into a few long "efforts"):
///   1. 8 s windows every second: dominant period (0.8-4 s) and autocorrelation of the most periodic
///      of {vertical dynamic accel, PCA-1 accel, PCA-1 gyro}, mean wrist orientation, energy;
///   2. a window repeats when r >= 0.45, energy >= 15 and it is not walking (strap steps >= 1/s with
///      the arm hanging and a 0.8-1.8 s swing);
///   3. consecutive repeating windows with the same orientation (< 0.4) and period (± 30 %) form a set
///      (gaps of up to 2 windows bridged); a change of orientation or tempo starts the next exercise;
///   4. per set: period re-estimated on the set (bounded by the user's labelled period if any),
///      peaks counted, start / end at the first / last repetition;
///   5. rejected: < 5 repetitions, a net orientation change (a transition, not a set), overlaps.
pub fn sets_and_reps_with(imu: &Imu, steps: &BTreeMap<i64, f64>, period_hint: &dyn Fn(&[f64]) -> Option<f64>) -> Vec<SetResult> {
    let w = work25(imu);
    let n = w.t.len();
    let fs = 25.0;
    let win = 200usize;
    let hop = 25usize;
    if n < win + hop {
        return Vec::new();
    }
    let dynv: Vec<[f64; 3]> = (0..n).map(|i| [0, 1, 2].map(|k| w.a[k][i] - w.grav[k][i])).collect();
    let unit = |i: usize| -> [f64; 3] {
        let g = [w.grav[0][i], w.grav[1][i], w.grav[2][i]];
        let m = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt().max(1e-6);
        g.map(|v| v / m)
    };
    struct Win { s: usize, g: [f64; 3], per: f64, k: usize, rep: bool }
    let mut wins: Vec<Win> = Vec::new();
    let mut s0 = 0;
    while s0 + win <= n {
        // windows never span a data gap
        if w.t[s0 + win - 1] - w.t[s0] > win as f64 / fs + 0.5 {
            s0 += hop;
            continue;
        }
        let mut gm = [0.0; 3];
        for i in s0..s0 + win {
            let u = unit(i);
            for k in 0..3 {
                gm[k] += u[k] / win as f64;
            }
        }
        let gn = (gm[0] * gm[0] + gm[1] * gm[1] + gm[2] * gm[2]).sqrt().max(1e-6);
        let gm = gm.map(|v| v / gn);
        let d = &dynv[s0..s0 + win];
        let ax = pca1(d);
        let gyr: Vec<[f64; 3]> = (s0..s0 + win).map(|i| [w.g[0][i], w.g[1][i], w.g[2][i]]).collect();
        let gx = pca1(&gyr);
        let sigs = [
            d.iter().map(|v| v[0] * gm[0] + v[1] * gm[1] + v[2] * gm[2]).collect::<Vec<f64>>(),
            d.iter().map(|v| v[0] * ax[0] + v[1] * ax[1] + v[2] * ax[2]).collect(),
            gyr.iter().map(|v| v[0] * gx[0] + v[1] * gx[1] + v[2] * gx[2]).collect(),
        ];
        let found: Vec<Option<(f64, f64)>> = sigs.iter().map(|sg| autocorr_peak(&moving_avg(sg, 3), fs, 0.8, 4.0)).collect();
        let mut best = (0.0, 0.0, 0usize);
        for (k, f) in found.iter().enumerate() {
            if let Some((p, r)) = f {
                if *r > best.1 {
                    best = (*p, *r, k);
                }
            }
        }
        best = prefer_rotation_period(best, found[2]);
        let gyro_rms = (gyr.iter().map(|v| v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sum::<f64>() / win as f64).sqrt();
        let acc_rms = (d.iter().map(|v| v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sum::<f64>() / win as f64).sqrt();
        let energy = gyro_rms + 150.0 * acc_rms;
        let tc = w.t[s0 + win / 2] as i64;
        let spm = (-4..4).map(|q| steps.get(&(tc + q)).copied().unwrap_or(0.0)).sum::<f64>() / 8.0;
        // walking: steps, arm hanging (x) with the palm side out (z > 0; a curl has z < 0), arm swing
        let walking = spm >= 1.0 && gm[0] >= 0.6 && gm[2] > -0.1 && best.0 < 1.8;
        wins.push(Win { s: s0, g: gm, per: best.0, k: best.2, rep: best.1 >= 0.45 && energy >= 15.0 && !walking });
        s0 += hop;
    }
    // grow sets over repeating windows of the same orientation and tempo
    let mut cand: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < wins.len() {
        if !wins[i].rep {
            i += 1;
            continue;
        }
        let (mut last, mut j, mut gap) = (i, i, 0);
        while j + 1 < wins.len() {
            let members: Vec<&Win> = wins[i..=last].iter().filter(|x| x.rep).collect();
            let gref = [0, 1, 2].map(|k| members.iter().map(|x| x.g[k]).sum::<f64>() / members.len() as f64);
            let pref = median(&members.iter().map(|x| x.per).collect::<Vec<_>>()).unwrap_or(1.0);
            let nx = &wins[j + 1];
            let dg = ((nx.g[0] - gref[0]).powi(2) + (nx.g[1] - gref[1]).powi(2) + (nx.g[2] - gref[2]).powi(2)).sqrt();
            if nx.s > wins[j].s + hop {
                break; // data gap
            }
            if nx.rep && dg < 0.4 && (nx.per - pref).abs() < 0.3 * pref {
                last = j + 1;
                gap = 0;
            } else if gap < 2 && dg < 0.4 {
                gap += 1;
            } else {
                break;
            }
            j += 1;
        }
        cand.push((i, last));
        i = last + 1;
    }
    let mut out: Vec<SetResult> = Vec::new();
    for (a, b) in cand {
        let members: Vec<&Win> = wins[a..=b].iter().filter(|x| x.rep).collect();
        let pw = median(&members.iter().map(|x| x.per).collect::<Vec<_>>()).unwrap_or(1.0);
        let ks = [0, 1, 2].map(|k| members.iter().filter(|x| x.k == k).count());
        let kbest = (0..3).max_by_key(|k| ks[*k]).unwrap_or(0);
        let (s, e_idx) = (wins[a].s, (wins[b].s + win - 1).min(n - 1));
        // the set's own orientation: mean over the centres of its repeating windows
        let gref = {
            let mut g = [0.0; 3];
            for m in &members {
                for k in 0..3 {
                    g[k] += m.g[k];
                }
            }
            let n = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt().max(1e-6);
            g.map(|v| v / n)
        };
        if let Some(set) = describe_set(&w, &dynv, s, e_idx, pw, kbest, gref, period_hint) {
            out.push(set);
        }
    }
    for x in out.iter_mut() {
        let (a, b) = (x.start as i64, x.end as i64);
        x.spm = (a..=b).map(|t| steps.get(&t).copied().unwrap_or(0.0)).sum::<f64>() / ((b - a + 1) as f64);
    }
    // a set is a stable repetition: a net change of wrist orientation is a transition
    out.retain(|x| x.reps >= 5 && x.net_turn < 0.5);
    out.retain(|x| !(x.spm >= 1.0 && x.features[0] >= 0.6 && x.features[2] > -0.1 && x.period_s.unwrap_or(9.0) < 1.8));
    // a set lasts and repeats cleanly; an unnamed movement must be even cleaner (12 Nov: most of the
    // remaining false sets were 8 s, r < 0.6 bursts between exercises)
    out.retain(|x| x.end - x.start >= 9.0 && x.confidence >= 0.5 && (movement_family(x).is_some() || (x.confidence >= 0.6 && x.reps >= 8)));
    // overlapping candidates: keep the best (autocorrelation x duration)
    out.sort_by(|p, q| (q.confidence * (q.end - q.start)).total_cmp(&(p.confidence * (p.end - p.start))));
    let mut keep: Vec<SetResult> = Vec::new();
    for x in out {
        if keep.iter().all(|k| (x.end.min(k.end) - x.start.max(k.start)) < 0.3 * (x.end - x.start)) {
            keep.push(x);
        }
    }
    keep.sort_by(|p, q| p.start.total_cmp(&q.start));
    keep
}

/// Acceleration of a back-and-forth movement often peaks twice per repetition (a curl: up, then
/// down), the rotation rate does not: when the gyro sees a clean period twice as long, it is the
/// repetition.
fn prefer_rotation_period(best: (f64, f64, usize), gyro: Option<(f64, f64)>) -> (f64, f64, usize) {
    match gyro {
        Some((pg, rg)) if best.2 != 2 && best.0 > 0.0 && rg >= 0.6 && (pg / best.0 - 2.0).abs() < 0.3 => (pg, rg, 2),
        _ => best,
    }
}

/// Period, repetitions, phases and signature of one set (indices in the 25 Hz work arrays).
fn describe_set(w: &Work, dynv: &[[f64; 3]], s: usize, e_idx: usize, window_period: f64, kbest: usize, gref: [f64; 3], period_hint: &dyn Fn(&[f64]) -> Option<f64>) -> Option<SetResult> {
    let fs = 25.0;
    let dur = w.t[e_idx] - w.t[s];
    // unit gravity around a sample (± 0.3 s), relative index
    let at = |i: usize| -> [f64; 3] {
        let lo = (s + i).saturating_sub(8);
        let hi = (s + i + 8).min(e_idx);
        let mut g = [0.0; 3];
        for j in lo..=hi {
            for k in 0..3 {
                g[k] += w.grav[k][j];
            }
        }
        let m = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt().max(1e-6);
        g.map(|v| v / m)
    };
    let off = |g: [f64; 3]| ((g[0] - gref[0]).powi(2) + (g[1] - gref[1]).powi(2) + (g[2] - gref[2]).powi(2)).sqrt();
        let seg_dyn = &dynv[s..=e_idx];
        let gmean = [0, 1, 2].map(|k| w.grav[k][s..=e_idx].iter().sum::<f64>() / (e_idx - s + 1) as f64);
        let gn = (gmean[0].powi(2) + gmean[1].powi(2) + gmean[2].powi(2)).sqrt().max(1e-6);
        let gu = gmean.map(|v| v / gn);
        let vert: Vec<f64> = seg_dyn.iter().map(|v| v[0] * gu[0] + v[1] * gu[1] + v[2] * gu[2]).collect();
        let ax = pca1(seg_dyn);
        let pa: Vec<f64> = seg_dyn.iter().map(|v| v[0] * ax[0] + v[1] * ax[1] + v[2] * ax[2]).collect();
        let gyr: Vec<[f64; 3]> = (s..=e_idx).map(|i| [w.g[0][i], w.g[1][i], w.g[2][i]]).collect();
        let gx = pca1(&gyr);
        let pg: Vec<f64> = gyr.iter().map(|v| v[0] * gx[0] + v[1] * gx[1] + v[2] * gx[2]).collect();
        // shape of the movement, independent of tempo: wrist orientation and its spread, movement
        // axis (accel) and rotation axis (gyro) in the strap frame, vertical share, rotation share
        let nn = (e_idx - s + 1) as f64;
        let gsd = [0, 1, 2].map(|k| {
            let m = gu[k];
            ((s..=e_idx).map(|i| {
                let g = [w.grav[0][i], w.grav[1][i], w.grav[2][i]];
                let n = (g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt().max(1e-6);
                (g[k] / n - m).powi(2)
            }).sum::<f64>() / nn).sqrt()
        });
        let axn = sign_norm(ax);
        let gxn = sign_norm(gx);
        let gyro_rms0 = (gyr.iter().map(|v| v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sum::<f64>() / gyr.len() as f64).sqrt();
        let acc_rms0 = (seg_dyn.iter().map(|v| v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sum::<f64>() / seg_dyn.len() as f64).sqrt();
        let shape: Vec<f64> = vec![
            gu[0], gu[1], gu[2], 2.0 * gsd[0], 2.0 * gsd[1], 2.0 * gsd[2], axn[0], axn[1], axn[2], gxn[0], gxn[1], gxn[2],
            (ax[0] * gu[0] + ax[1] * gu[1] + ax[2] * gu[2]).abs(), (gyro_rms0 / (acc_rms0 * 300.0).max(1e-3)).min(5.0) / 2.0,
        ];
        // the user's period for this movement if known, else the windows' period (± 30 %)
        let (lo_s, hi_s) = match period_hint(&shape) {
            Some(t) => ((0.75 * t).max(0.6), (1.4 * t).min(10.0)),
            None => ((0.7 * window_period).max(0.6), 1.35 * window_period),
        };
        let sigs = [moving_avg(&vert, 5), moving_avg(&pa, 5), moving_avg(&pg, 5)];
        let mut best: Option<(f64, f64, Vec<f64>)> = None;
        for (k, sig) in sigs.iter().enumerate() {
            if let Some((p, r)) = autocorr_peak(sig, fs, lo_s, hi_s.min(dur / 2.0)) {
                // the windows' dominant signal wins ties
                let score = r + if k == kbest { 0.05 } else { 0.0 };
                if best.as_ref().is_none_or(|b| score > b.1) {
                    best = Some((p, score, sig.clone()));
                }
            }
        }
        let (period, r, sig) = best?;
        let r = (r - 0.05).max(r - if r > 0.05 { 0.05 } else { 0.0 });
        let periodic = r >= 0.4 && dur / period >= 2.5;
        let mut first_last = (0usize, sig.len().saturating_sub(1));
        let mut inner = first_last;
        let reps = if periodic {
            let base = moving_avg(&sig, (period * fs) as usize);
            let sig: Vec<f64> = sig.iter().zip(&base).map(|(a, b)| a - b).collect();
            let m = 0.0;
            let sd = (sig.iter().map(|v| v * v).sum::<f64>() / sig.len() as f64).sqrt();
            let dist = (0.6 * period * fs) as usize;
            let mut peaks: Vec<usize> = Vec::new();
            for i in 1..sig.len() - 1 {
                if sig[i] > m + 0.3 * sd && sig[i] >= sig[i - 1] && sig[i] >= sig[i + 1] {
                    match peaks.last() {
                        Some(&l) if i - l < dist => {
                            if sig[i] > sig[l] {
                                *peaks.last_mut().unwrap() = i;
                            }
                        }
                        _ => peaks.push(i),
                    }
                }
            }
            // keep the longest run of repetitions done in the set's orientation: the 8 s windows
            // overhang into the transition to the next exercise
            let mut runs: Vec<Vec<usize>> = vec![Vec::new()];
            for &pk in &peaks {
                if off(at(pk)) < 0.4 {
                    runs.last_mut().unwrap().push(pk);
                } else if !runs.last().unwrap().is_empty() {
                    runs.push(Vec::new());
                }
            }
            peaks = runs.into_iter().max_by_key(|r| r.len()).unwrap_or_default();
            if let (Some(f), Some(l)) = (peaks.first(), peaks.last()) {
                first_last = (*f, *l);
            }
            // orientation is compared one repetition inside the set (getting into position is
            // not part of it)
            if peaks.len() >= 4 {
                inner = (peaks[1], peaks[peaks.len() - 2]);
            } else {
                inner = first_last;
            }
            let active = if peaks.len() >= 2 { (peaks[peaks.len() - 1] - peaks[0]) as f64 / fs + period } else { dur };
            let by_period = (active / period).round() as usize;
            if (peaks.len() as f64 - by_period as f64).abs() <= 0.3 * by_period as f64 { peaks.len() } else { by_period }
        } else {
            0
        };
        // vertical velocity (high-passed over one period) -> up / down / hold time per repetition
        let (mut up_s, mut down_s, mut hold_s) = (None, None, None);
        if periodic {
            let mut v = vec![0.0; vert.len()];
            for i in 1..vert.len() {
                v[i] = v[i - 1] + 9.81 * vert[i] / fs;
            }
            let trend = moving_avg(&v, (period * fs) as usize);
            let vh: Vec<f64> = v.iter().zip(&trend).map(|(a, b)| a - b).collect();
            let sd = (vh.iter().map(|x| x * x).sum::<f64>() / vh.len() as f64).sqrt();
            let eps = 0.25 * sd;
            let nn = vh.len() as f64;
            let fu = vh.iter().filter(|x| **x > eps).count() as f64 / nn;
            let fd = vh.iter().filter(|x| **x < -eps).count() as f64 / nn;
            up_s = Some(round(fu * period, 2));
            down_s = Some(round(fd * period, 2));
            hold_s = Some(round((1.0 - fu - fd).max(0.0) * period, 2));
        }
        // amplitudes per repetition: rotation (deg) about the main gyro axis, vertical velocity
        let (mut rot_amp, mut vel_amp) = (0.0, 0.0);
        if periodic {
            let mut ang = vec![0.0; pg.len()];
            for i in 1..pg.len() {
                ang[i] = ang[i - 1] + pg[i] / fs;
            }
            let tr = moving_avg(&ang, (period * fs) as usize);
            let hp: Vec<f64> = ang.iter().zip(&tr).map(|(a, b)| a - b).collect();
            rot_amp = percentile(&hp, 95.0).unwrap_or(0.0) - percentile(&hp, 5.0).unwrap_or(0.0);
            let mut v = vec![0.0; vert.len()];
            for i in 1..vert.len() {
                v[i] = v[i - 1] + 9.81 * vert[i] / fs;
            }
            let tv = moving_avg(&v, (period * fs) as usize);
            let hv: Vec<f64> = v.iter().zip(&tv).map(|(a, b)| a - b).collect();
            vel_amp = percentile(&hv, 95.0).unwrap_or(0.0) - percentile(&hv, 5.0).unwrap_or(0.0);
        }
        let mut features = shape.clone();
        features.extend([(period.max(0.3)).ln() / 1.5, (rot_amp / 90.0).min(3.0), vel_amp.min(2.0)]);
        // net orientation change between the first and the last repetition (same phase)
        let (g0, g1) = (at(inner.0), at(inner.1));
        let net_turn = ((g0[0] - g1[0]).powi(2) + (g0[1] - g1[1]).powi(2) + (g0[2] - g1[2]).powi(2)).sqrt();
        let (st, en) = if periodic {
            ((w.t[s + first_last.0] - period / 2.0).max(w.t[s]), (w.t[s + first_last.1] + period / 2.0).min(w.t[e_idx]))
        } else {
            (w.t[s], w.t[e_idx])
        };
        Some(SetResult {
            start: st,
            end: en,
            net_turn,
            spm: 0.0,
            periodic,
            reps,
            period_s: periodic.then_some(round(period, 2)),
            confidence: round(r.clamp(0.0, 1.0), 3),
            up_s,
            down_s,
            hold_s,
            features,
        })
}

// ------------------------------------------------------------------ session analysis

fn softmax(s: &[f64]) -> Vec<f64> {
    let m = s.iter().copied().fold(f64::MIN, f64::max);
    let e: Vec<f64> = s.iter().map(|v| (v - m).exp()).collect();
    let z: f64 = e.iter().sum();
    e.iter().map(|v| v / z).collect()
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Rules v1 (no labels yet). Scores are log-odds-like; the softmax gives the probabilities.
pub fn rules_v1(f: &BTreeMap<&str, f64>) -> Vec<f64> {
    let g = |k: &str| f.get(k).copied().unwrap_or(0.0);
    let imu = g("imu_cov") >= 0.3;
    let run = 3.0 * clamp01((g("steps_pm") - 100.0) / 50.0) + 2.0 * g("run_frac") + 1.0 * clamp01((g("cad_spm") - 140.0) / 30.0) - if imu { 1.5 * clamp01(g("strokes_pm") / 3.0) } else { 0.0 };
    let tennis = if imu {
        3.0 * clamp01(g("strokes_pm") / 3.0) + 1.0 * clamp01((g("steps_pm") - 15.0) / 30.0) * clamp01((110.0 - g("steps_pm")) / 30.0) - 1.0 * g("periodic_frac")
    } else {
        // without IMU a brisk walk has the same steps: require the intensity of a match
        (1.5 * clamp01((g("steps_pm") - 20.0) / 30.0) * clamp01((130.0 - g("steps_pm")) / 30.0) + 1.0 * clamp01((g("dyn_cv") - 0.4) / 0.6) + 1.0 * clamp01((g("dyn_mean") - 0.3) / 0.3))
            * clamp01((g("hrr_mean") - 0.3) / 0.25)
    };
    let strength = if imu {
        2.0 * clamp01(g("sets_per_10min") / 2.5) + 1.5 * g("periodic_frac") + 1.0 * clamp01((25.0 - g("steps_pm")) / 25.0) - 1.0 * clamp01(g("strokes_pm") / 3.0)
    } else {
        // sets are done standing: few steps; a walk alternates too, so steps and intensity gate it
        (1.5 * clamp01((25.0 - g("steps_pm")) / 25.0) + 1.0 * clamp01(g("active_alt") / 0.4) + 0.7 * clamp01((g("grav_var") - 4.0) / 8.0))
            * clamp01((45.0 - g("steps_pm")) / 20.0)
            * clamp01((g("hrr_mean") - 0.25) / 0.2)
    };
    // a walk (firmware "walk" most of the time, moderate heart rate) is "autre"
    let other = 1.2 + 1.5 * clamp01((g("walk_frac") - 0.4) / 0.3) * clamp01((0.5 - g("hrr_mean")) / 0.15);
    softmax(&[1.6 * run, 1.6 * tennis, 1.6 * strength, other])
}

fn model_predict(params_json: &Value, f: &BTreeMap<&str, f64>) -> Option<Vec<f64>> {
    let names: Vec<String> = serde_json::from_value(params_json.get("features")?.clone()).ok()?;
    let mu: Vec<f64> = serde_json::from_value(params_json.get("mean")?.clone()).ok()?;
    let sd: Vec<f64> = serde_json::from_value(params_json.get("std")?.clone()).ok()?;
    let w: Vec<Vec<f64>> = serde_json::from_value(params_json.get("weights")?.clone()).ok()?;
    let b: Vec<f64> = serde_json::from_value(params_json.get("bias")?.clone()).ok()?;
    let classes: Vec<String> = serde_json::from_value(params_json.get("classes")?.clone()).ok()?;
    let x: Vec<f64> = names.iter().enumerate().map(|(i, n)| (f.get(n.as_str()).copied().unwrap_or(0.0) - mu[i]) / sd[i].max(1e-9)).collect();
    let z: Vec<f64> = w.iter().zip(&b).map(|(row, bi)| row.iter().zip(&x).map(|(a, c)| a * c).sum::<f64>() + bi).collect();
    let p = softmax(&z);
    Some(CLASSES.iter().map(|c| classes.iter().position(|k| k == c).map(|i| p[i]).unwrap_or(0.0)).collect())
}

fn active_model(conn: &Connection) -> GooseResult<Option<(String, Value)>> {
    let r: Option<(String, String)> = conn
        .query_row("SELECT version, params_json FROM sport_models WHERE active = 1 ORDER BY created_at DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(r.and_then(|(v, p)| serde_json::from_str(&p).ok().map(|p| (v, p))))
}

pub fn session_features(mins: &[Minute], p: &Profile, imu: &Imu, sets: &[SetResult], strokes_n: usize, start: i64, end: i64) -> BTreeMap<&'static str, f64> {
    let act: Vec<&Minute> = mins.iter().filter(|m| minute_score(m, p) > 0.5).collect();
    let base: Vec<&Minute> = if act.len() >= 3 { act.clone() } else { mins.iter().collect() };
    let dur_min = ((end - start) as f64 / 60.0).max(1.0);
    let hrr: Vec<f64> = base.iter().filter_map(|m| m.hr.map(|h| p.hrr(h))).collect();
    let dyn_: Vec<f64> = base.iter().filter_map(|m| m.dyn_mean).collect();
    let dyn_mean = mean(&dyn_).unwrap_or(0.0);
    let dyn_sd = if dyn_.len() > 1 { (dyn_.iter().map(|v| (v - dyn_mean).powi(2)).sum::<f64>() / dyn_.len() as f64).sqrt() } else { 0.0 };
    let flags: Vec<bool> = mins.iter().map(|m| minute_score(m, p) > 0.5).collect();
    let alt = if flags.len() > 2 { flags.windows(2).filter(|w| w[0] != w[1]).count() as f64 / (flags.len() - 1) as f64 } else { 0.0 };
    let imu_cov = (imu.len() as f64 / IMU_FS / (end - start).max(1) as f64).min(1.0);
    let gyro: Vec<f64> = (0..imu.len()).step_by(10).map(|i| gyro_mag(imu, i)).collect();
    let set_time: f64 = sets.iter().map(|s| s.end - s.start).sum();
    let per_time: f64 = sets.iter().filter(|s| s.periodic).map(|s| s.end - s.start).sum();
    let mut f = BTreeMap::new();
    f.insert("dur_min", round(dur_min, 1));
    f.insert("hrr_mean", round(mean(&hrr).unwrap_or(0.0), 3));
    f.insert("hrr_p90", round(percentile(&hrr, 90.0).unwrap_or(0.0), 3));
    f.insert("steps_pm", round(mean(&base.iter().map(|m| m.steps).collect::<Vec<_>>()).unwrap_or(0.0), 1));
    f.insert("cad_spm", round(median(&base.iter().filter_map(|m| m.cad_spm).collect::<Vec<_>>()).unwrap_or(0.0), 0));
    f.insert("run_frac", round(mean(&base.iter().map(|m| m.run_frac).collect::<Vec<_>>()).unwrap_or(0.0), 3));
    f.insert("walk_frac", round(mean(&base.iter().map(|m| m.walk_frac).collect::<Vec<_>>()).unwrap_or(0.0), 3));
    f.insert("dyn_mean", round(dyn_mean, 4));
    f.insert("dyn_cv", round(if dyn_mean > 0.0 { dyn_sd / dyn_mean } else { 0.0 }, 3));
    f.insert("grav_var", round(mean(&base.iter().map(|m| m.grav_var).collect::<Vec<_>>()).unwrap_or(0.0), 2));
    f.insert("active_alt", round(alt, 3));
    f.insert("imu_cov", round(imu_cov, 3));
    let imu_min = (imu.len() as f64 / IMU_FS / 60.0).max(1e-6);
    f.insert("strokes_pm", if imu.is_empty() { 0.0 } else { round(strokes_n as f64 / imu_min, 2) });
    f.insert("sets_per_10min", if imu.is_empty() { 0.0 } else { round(sets.iter().filter(|s| s.periodic).count() as f64 / imu_min * 10.0, 2) });
    f.insert("periodic_frac", if set_time > 0.0 { round(per_time / set_time, 3) } else { 0.0 });
    f.insert("gyro_p95", round(percentile(&gyro, 95.0).unwrap_or(0.0), 0));
    f
}

/// One labelled set of the personal exercise library: label, signature, period, labelled reps.
#[derive(Debug, Clone)]
pub struct LibSet {
    pub label: String,
    pub features: Vec<f64>,
    pub period: Option<f64>,
}

fn exercise_library(conn: &Connection) -> GooseResult<Vec<LibSet>> {
    let mut st = conn.prepare("SELECT exercise_label, detail_json, reps_label, start_ts, end_ts FROM sport_segments WHERE kind = 'set' AND exercise_label IS NOT NULL AND label_status IN ('confirmed','corrected')")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, f64>(3)?, r.get::<_, f64>(4)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (label, d, reps, s, e) = row?;
        let v: Value = d.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        let f: Option<Vec<f64>> = serde_json::from_value(v["features"].clone()).ok();
        // the user's rep count is the truth for the period of that exercise
        let period = reps.filter(|r| *r >= 2).map(|r| (e - s) / r as f64).or_else(|| v["period_s"].as_f64());
        if let Some(f) = f {
            out.push(LibSet { label, features: f, period });
        }
    }
    Ok(out)
}

fn dist(a: &[f64], b: &[f64], dims: usize) -> f64 {
    a.iter().zip(b).take(dims).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
}

/// Few-shot exercise recognition: distance-weighted vote of the 3 nearest labelled sets
/// (prototype / metric approach — the personal library is the training set). Confidence falls
/// with the distance to the closest example; unseen movements stay unnamed.
pub fn recognize_exercise(lib: &[LibSet], f: &[f64]) -> Option<(String, f64)> {
    let mut d: Vec<(&LibSet, f64)> = lib.iter().filter(|l| l.features.len() == f.len()).map(|l| (l, dist(&l.features, f, f.len()))).collect();
    if d.is_empty() {
        return None;
    }
    d.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut votes: BTreeMap<&str, f64> = BTreeMap::new();
    for (l, x) in d.iter().take(3) {
        *votes.entry(l.label.as_str()).or_default() += 1.0 / (x + 0.05);
    }
    let total: f64 = votes.values().sum();
    let (label, v) = votes.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let conf = (v / total) * (-1.2 * d[0].1).exp();
    Some((label.to_string(), round(conf, 3)))
}

/// Name guess before the user has labelled anything, from the wrist orientation (strap x axis
/// along the forearm, towards the hand: +1 arm hanging, -1 arm overhead) and the tempo. Shown with
/// a question mark; the first validation replaces it with the personal library.
pub fn movement_family(s: &SetResult) -> Option<&'static str> {
    let (x, y, z) = (s.features[0], s.features[1], s.features[2]);
    let t = s.period_s?;
    if x <= -0.85 && t < 2.0 {
        Some("Tractions")
    } else if x <= -0.6 && t >= 2.0 {
        Some("Squats / thrusters")
    } else if (0.2..=0.75).contains(&x) && y >= 0.6 && z < 0.0 {
        Some("Curls biceps")
    } else if x >= 0.85 && s.spm < 0.8 && t < 1.1 {
        Some("Pompes")
    } else if x >= 0.85 && s.spm < 0.8 && t < 2.2 {
        Some("Dips")
    } else if x >= 0.85 && t >= 2.2 {
        Some("Squats bulgares / fentes")
    } else {
        None
    }
}

/// Expected period for a movement shape: the closest labelled set of similar shape.
fn period_for_shape(lib: &[LibSet], shape: &[f64]) -> Option<f64> {
    lib.iter()
        .filter(|l| l.features.len() >= SHAPE_DIMS && l.period.is_some())
        .map(|l| (l, dist(&l.features, shape, SHAPE_DIMS)))
        .filter(|(_, d)| *d < 0.6)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .and_then(|(l, _)| l.period)
}

/// Groups the sets of one session by exercise (sets of the same exercise are usually
/// consecutive): one label from the user then names the whole group.
pub fn cluster_sets(sets: &[&SetResult]) -> Vec<usize> {
    let mut centroids: Vec<(Vec<f64>, usize)> = Vec::new();
    let mut out = Vec::new();
    let mut prev: Option<usize> = None;
    for s in sets {
        let f = &s.features;
        let best = centroids.iter().enumerate().map(|(k, (c, _))| (k, dist(c, f, f.len().min(c.len())))).min_by(|a, b| a.1.total_cmp(&b.1));
        let k = match (best, prev) {
            (Some((k, d)), Some(p)) if k == p && d < 1.1 => k,
            (Some((k, d)), _) if d < 0.8 => k,
            _ => {
                centroids.push((f.clone(), 0));
                centroids.len() - 1
            }
        };
        let (c, n) = &mut centroids[k];
        *n += 1;
        for (ci, fi) in c.iter_mut().zip(f) {
            *ci += (fi - *ci) / *n as f64;
        }
        out.push(k);
        prev = Some(k);
    }
    out
}

/// Full analysis of one session: features, classification, segments. Human-labelled segments are
/// never replaced; unlabelled ones are re-derived.
pub fn analyze_session(conn: &Connection, id: &str, age: Option<f64>) -> GooseResult<Value> {
    let (start, end, source): (i64, Option<i64>, String) =
        conn.query_row("SELECT start_ts, end_ts, source FROM sport_sessions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let now = now_s();
    let end = end.unwrap_or(now);
    let p = profile(conn, now, age)?;
    let imu = load_imu(conn, (start - PRE_BUFFER_S) as f64, (end + POST_BUFFER_S) as f64)?;
    let secs = load_secs(conn, start, end + 1)?;
    let mins = minutes(&secs, &imu_dyn_per_minute(&imu));
    let st = strokes(&imu);
    let lib = exercise_library(conn)?;
    let steps = steps_per_second(&load_secs(conn, start - PRE_BUFFER_S, end + POST_BUFFER_S)?);
    let sets = sets_and_reps_with(&imu, &steps, &|shape| period_for_shape(&lib, shape));
    let f = session_features(&mins, &p, &imu, &sets, st.len(), start, end);
    let (model_version, probs) = match active_model(conn)? {
        Some((v, params_json)) => match model_predict(&params_json, &f) {
            Some(pr) => (v, pr),
            None => (RULES_MODEL.to_string(), rules_v1(&f)),
        },
        None => (RULES_MODEL.to_string(), rules_v1(&f)),
    };
    let best = probs.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap_or(3);
    let predicted = CLASSES[best];
    let hr: Vec<f64> = mins.iter().filter_map(|m| m.hr).collect();
    let summary = json!({
        "duration_min": f["dur_min"], "hr_mean": mean(&hr).map(|v| round(v, 0)), "hr_max": hr.iter().copied().fold(None, |a: Option<f64>, b| Some(a.map_or(b, |x| x.max(b)))),
        "imu_minutes": round(imu.len() as f64 / IMU_FS / 60.0, 1), "strokes": st.len(),
        "sets": sets.iter().filter(|s| s.periodic).count(), "reps": sets.iter().filter(|s| s.periodic).map(|s| s.reps).sum::<usize>(),
        "exercise_groups": cluster_sets(&sets.iter().filter(|s| s.periodic).collect::<Vec<_>>()).iter().copied().max().map(|m| m + 1).unwrap_or(0),
        "cadence_spm": f["cad_spm"], "profile": {"rest_hr": round(p.rest, 0), "hr_max": round(p.hrmax, 0)},
        "hr_series": mins.iter().filter_map(|m| m.hr.map(|h| json!([m.t, round(h, 0)]))).collect::<Vec<_>>(),
    });
    let feats: Value = json!(FEATURES.iter().map(|k| (k.to_string(), json!(f.get(k).copied().unwrap_or(0.0)))).collect::<serde_json::Map<_, _>>());
    conn.execute(
        "UPDATE sport_sessions SET predicted_type = ?2, probs_json = ?3, confidence = ?4, features_json = ?5, summary_json = ?6, model_version = ?7, engine_version = ?8, analyzed_at = ?9, updated_at = ?9 WHERE id = ?1",
        params![id, predicted, json!(CLASSES.iter().zip(&probs).map(|(c, p)| (c.to_string(), json!(round(*p, 3)))).collect::<serde_json::Map<_, _>>()).to_string(), round(probs[best], 3), feats.to_string(), summary.to_string(), model_version, ENGINE_VERSION, now],
    )?;
    let labelled: i64 = conn.query_row("SELECT COUNT(*) FROM sport_segments WHERE session_id = ?1 AND label_status != 'pending'", [id], |r| r.get(0))?;
    if labelled == 0 {
        conn.execute("DELETE FROM sport_segments WHERE session_id = ?1", [id])?;
        let label_type: Option<String> = conn.query_row("SELECT label_type FROM sport_sessions WHERE id = ?1", [id], |r| r.get(0))?;
        let kind_of = label_type.as_deref().unwrap_or(predicted);
        let mut ins = conn.prepare("INSERT INTO sport_segments (id, session_id, kind, start_ts, end_ts, reps_pred, exercise_pred, exercise_conf, confidence, detail_json, engine_version) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?;
        if kind_of == "musculation" || (kind_of == "autre" && !sets.is_empty() && source == "manual") {
            let periodic: Vec<&SetResult> = sets.iter().filter(|s| s.periodic).collect();
            let clusters = cluster_sets(&periodic);
            let mut ci = 0;
            for (k, s) in sets.iter().enumerate() {
                let ex = if s.periodic { recognize_exercise(&lib, &s.features).or_else(|| movement_family(s).map(|f| (f.to_string(), 0.3))) } else { None };
                let cluster = if s.periodic { ci += 1; Some(clusters[ci - 1]) } else { None };
                let detail = json!({"periodic": s.periodic, "period_s": s.period_s, "up_s": s.up_s, "down_s": s.down_s, "hold_s": s.hold_s, "features": s.features, "cluster": cluster});
                ins.execute(params![format!("{id}-s{k}"), id, if s.periodic { "set" } else { "effort" }, s.start, s.end, s.periodic.then_some(s.reps as i64), ex.as_ref().filter(|e| e.1 >= 0.3).map(|e| e.0.clone()), ex.as_ref().map(|e| e.1), s.confidence, detail.to_string(), ENGINE_VERSION])?;
            }
        } else if kind_of == "tennis" && !st.is_empty() {
            // rallies / games: strokes closer than 25 s belong to the same exchange block
            let mut blocks: Vec<(f64, f64, usize)> = Vec::new();
            for s in &st {
                match blocks.last_mut() {
                    Some(b) if s.0 - b.1 < 25.0 => {
                        b.1 = s.0;
                        b.2 += 1;
                    }
                    _ => blocks.push((s.0, s.0, 1)),
                }
            }
            for (k, b) in blocks.iter().enumerate() {
                let peak: Vec<f64> = st.iter().filter(|s| s.0 >= b.0 && s.0 <= b.1).map(|s| s.1).collect();
                ins.execute(params![format!("{id}-r{k}"), id, "exchange", b.0, b.1, b.2 as i64, Option::<String>::None, Option::<f64>::None, 1.0, json!({"strokes": b.2, "gyro_peak_median_dps": median(&peak).map(|v| round(v, 0))}).to_string(), ENGINE_VERSION])?;
            }
        }
    }
    Ok(json!({"id": id, "predicted_type": predicted, "probs": CLASSES.iter().zip(&probs).map(|(c, p)| (c.to_string(), json!(round(*p, 3)))).collect::<serde_json::Map<_, _>>(), "model_version": model_version, "features": feats, "summary": summary}))
}

/// Tightens an auto session to its effort core: the approach walk and the walk home keep the heart
/// rate up (EPOC) but are not the sport. Core = minutes above 0.5; effort minute = HRR >= 60 % of
/// the core's p75 HRR and motion (dynamic accel or steps) >= 40-50 % of the core's p75.
fn refine_bounds(conn: &Connection, p: &Profile, s: i64, e: i64) -> GooseResult<(i64, i64)> {
    let secs = load_secs(conn, s, e + 1)?;
    let mins = minutes(&secs, &BTreeMap::new());
    let core: Vec<&Minute> = mins.iter().filter(|m| minute_score(m, p) > 0.5).collect();
    if core.len() < 5 {
        return Ok((s, e));
    }
    let q = |v: Vec<f64>| percentile(&v, 75.0).unwrap_or(0.0);
    let hrr75 = q(core.iter().filter_map(|m| m.hr.map(|h| p.hrr(h))).collect());
    let dyn75 = q(core.iter().filter_map(|m| m.dyn_mean).collect());
    let st75 = q(core.iter().map(|m| m.steps).collect());
    let effort = |m: &Minute| {
        m.hr.map(|h| p.hrr(h)).unwrap_or(0.0) >= 0.6 * hrr75
            // steps only count for running cadences: walking off the court keeps 50-60 steps/min
            && (m.dyn_mean.unwrap_or(0.0) >= 0.5 * dyn75 || (st75 >= 140.0 && m.steps >= 0.8 * st75) || m.run_frac > 0.5)
    };
    let flags: Vec<bool> = mins.iter().map(effort).collect();
    // first / last effort minute that starts / ends a run of >= 3 effort minutes out of 4
    let dense = |i: usize| flags[i..(i + 4).min(flags.len())].iter().filter(|f| **f).count() >= 3;
    let first = (0..flags.len()).find(|&i| flags[i] && dense(i));
    let last = (0..flags.len()).rev().find(|&i| flags[i] && dense(i.saturating_sub(3)));
    match (first, last) {
        (Some(a), Some(b)) if b > a => Ok(((mins[a].t - PRE_BUFFER_S).max(s), (mins[b].t + 60 + POST_BUFFER_S).min(e))),
        _ => Ok((s, e)),
    }
}

fn imu_window(imu: &Imu, from: f64) -> Imu {
    let k = imu.t.partition_point(|t| *t < from);
    Imu { t: imu.t[k..].to_vec(), a: [0, 1, 2].map(|i| imu.a[i][k..].to_vec()), g: [0, 1, 2].map(|i| imu.g[i][k..].to_vec()) }
}

/// A session is only opened on the signature of a sport, never on heart rate and motion alone
/// (an active life has many short efforts: stairs, running to the kitchen or to the office —
/// 11 Nov 16:53 replay). Any one of:
///   running     >= 3 of the last 5 minutes at >= 140 steps/min (or firmware "run");
///   tennis      >= 3 isolated strokes in the last 5 minutes (IMU);
///   strength    >= 2 periodic sets in the last 10 minutes (IMU);
///   sustained   >= 8 of the last 10 minutes at >= 50 % heart-rate reserve (other sports, and
///               days without IMU): daily-life bursts do not last that long.
pub fn signature(mins: &[Minute], p: &Profile, imu: &Imu, steps: &BTreeMap<i64, f64>, now: i64) -> Value {
    let last = |k: usize| -> Vec<&Minute> { mins.iter().rev().take(k).collect() };
    let run_min = last(5).iter().filter(|m| m.steps >= 140.0 || m.run_frac > 0.5).count();
    let sustained = last(10).iter().filter(|m| m.n >= 30 && m.hr.map(|h| p.hrr(h)).unwrap_or(0.0) >= 0.5).count();
    let w5 = imu_window(imu, (now - 300) as f64);
    let w10 = imu_window(imu, (now - 600) as f64);
    let imu_s = w10.len() as f64 / IMU_FS;
    let strokes5 = strokes(&w5).len();
    // strength: named movements only (an unnamed periodic burst is usually walking or fidgeting),
    // and the heart has to work (13 Nov: 3 "sets" of walking at HR 88 opened a session)
    let sets10 = sets_and_reps_with(&w10, steps, &|_| None).iter().filter(|s| s.periodic && s.reps >= 6 && movement_family(s).is_some()).count();
    let hrr10: Vec<f64> = last(10).iter().filter_map(|m| m.hr.map(|h| p.hrr(h))).collect();
    let load = percentile(&hrr10, 75.0).unwrap_or(0.0);
    let kind = if run_min >= 3 {
        Some("course")
    } else if strokes5 >= 3 {
        Some("tennis")
    } else if sets10 >= 3 && load >= 0.35 {
        Some("musculation")
    } else if sustained >= 8 {
        Some("effort soutenu")
    } else {
        None
    };
    json!({"confirmed": kind.is_some(), "by": kind, "run_minutes": run_min, "strokes_5min": strokes5, "sets_10min": sets10, "hrr_p75_10min": round(load, 2), "sustained_minutes": sustained, "imu_s_10min": round(imu_s, 0)})
}

// ------------------------------------------------------------------ online engine

#[derive(Debug, Deserialize)]
pub struct UpdateArgs {
    pub database_path: String,
    #[serde(default)]
    pub now_unix: Option<i64>,
    /// live packet-40 seconds since the last call: [ts, hr, [rr_ms...]]
    #[serde(default)]
    pub live: Vec<(i64, f64, Vec<f64>)>,
    /// live R21 frames since the last call: [arrival_unix, frame_hex]
    #[serde(default)]
    pub imu: Vec<(f64, String)>,
    #[serde(default)]
    pub age: Option<f64>,
    /// streams currently on (as Swift believes): echoed into the acquisition log
    #[serde(default)]
    pub capturing: bool,
    /// detection paused (smart wake owns the streams, user disabled it)
    #[serde(default)]
    pub paused: bool,
    /// replay only: R18 seconds newer than now - lag are not visible yet (history arrives by sync)
    #[serde(default)]
    pub history_lag_s: Option<i64>,
    /// replay / backfill: no stream is really switched, nothing goes to the acquisition log
    #[serde(default)]
    pub replay: bool,
}

fn store_live(conn: &Connection, args: &UpdateArgs, arrival: f64) -> GooseResult<(usize, usize)> {
    let tx = conn.unchecked_transaction()?;
    let (mut nl, mut ni) = (0, 0);
    {
        let mut ins = tx.prepare_cached("INSERT OR REPLACE INTO sport_live (ts, hr, rr, arrival) VALUES (?1, ?2, ?3, ?4)")?;
        for (ts, hr, rr) in &args.live {
            ins.execute(params![ts, (*hr > 0.0).then_some(*hr), rr.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(","), arrival])?;
            nl += 1;
        }
        let mut imu = tx.prepare_cached("INSERT OR REPLACE INTO sport_imu (ts, arrival, frame) VALUES (?1, ?2, ?3)")?;
        for (arr, hex_frame) in &args.imu {
            let Ok(frame) = hex::decode(hex_frame.trim()) else { continue };
            let Ok(parsed) = crate::protocol::parse_frame(crate::protocol::DeviceType::Goose, &frame) else { continue };
            if !(parsed.header_crc_valid && parsed.payload_crc_valid) || frame.len() != crate::whoop5_records::R21_FRAME_LEN {
                continue;
            }
            let ts = f64::from(u32::from_le_bytes([frame[15], frame[16], frame[17], frame[18]])) + f64::from(u16::from_le_bytes([frame[19], frame[20]])) / 32768.0;
            imu.execute(params![ts, arr, frame])?;
            ni += 1;
        }
    }
    tx.commit()?;
    Ok((nl, ni))
}

fn new_session(conn: &Connection, start: i64, source: &str, label: Option<&str>, now: i64) -> GooseResult<String> {
    // an auto session that ended less than 5 min before this start is resumed, never duplicated
    if source == "auto" {
        let prev: Option<String> = conn
            .query_row("SELECT id FROM sport_sessions WHERE source = 'auto' AND status IN ('ended','active') AND label_status = 'pending' AND COALESCE(end_ts, ?1) >= ?2 ORDER BY start_ts DESC LIMIT 1", params![now, start - 300], |r| r.get(0))
            .optional()?;
        if let Some(id) = prev {
            conn.execute("DELETE FROM sport_segments WHERE session_id = ?1", [&id])?;
            conn.execute("UPDATE sport_sessions SET status = 'active', end_ts = NULL, updated_at = ?2 WHERE id = ?1", params![id, now])?;
            return Ok(id);
        }
    }
    let id = format!("sp-{start}");
    conn.execute(
        "INSERT OR IGNORE INTO sport_sessions (id, start_ts, status, source, engine_version, label_type, label_status, created_at, updated_at) VALUES (?1, ?2, 'active', ?3, ?4, ?5, ?6, ?7, ?7)",
        params![id, start, source, ENGINE_VERSION, label, if label.is_some() { "manual" } else { "pending" }, now],
    )?;
    Ok(id)
}

fn log_acq(conn: &Connection, session: Option<&str>, on: bool, now: i64, reason: &str) -> GooseResult<()> {
    if on {
        conn.execute("INSERT INTO sport_acquisition (session_id, stream, start_ts, reason) VALUES (?1, 'imu+hr', ?2, ?3)", params![session, now, reason])?;
    } else {
        conn.execute("UPDATE sport_acquisition SET end_ts = ?1 WHERE end_ts IS NULL", [now])?;
    }
    Ok(())
}

/// One online step. Returns what the phone should do: streams on/off, sync interval, UI state.
pub fn update(args: &UpdateArgs) -> GooseResult<Value> {
    let t0 = std::time::Instant::now();
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_tables(&conn)?;
    let now = args.now_unix.unwrap_or_else(now_s);
    let stored = store_live(&conn, args, now as f64)?;
    let _ = crate::whoop5_store::ingest(&conn);
    let mut st = state_get(&conn, "engine")?.unwrap_or_else(|| json!({"state": "idle"}));
    let state = st["state"].as_str().unwrap_or("idle").to_string();
    let p = profile(&conn, now, args.age)?;
    let from = now - 40 * 60;
    let imu = load_imu(&conn, from as f64, (now + 1) as f64)?;
    let mut secs = load_secs(&conn, from, now + 1)?;
    if let Some(lag) = args.history_lag_s {
        secs.retain(|t, _| *t <= now - lag);
    }
    let mins = minutes(&secs, &imu_dyn_per_minute(&imu));
    let after = st["after"].as_i64().unwrap_or(0);
    let scores: Vec<(i64, f64)> = mins.iter().filter(|m| state != "idle" && state != "watch" || m.t >= after).map(|m| (m.t, minute_score(m, &p))).collect();
    let data_until = secs.keys().next_back().copied().unwrap_or(0);
    let recent = |k: usize| -> Vec<f64> { scores.iter().rev().take(k).map(|s| s.1).collect() };
    let active_recent = |k: usize| recent(k).iter().filter(|v| **v > 0.5).count();
    let manual = st["manual"].as_bool().unwrap_or(false);
    let session_id = st["session_id"].as_str().map(String::from);
    let mut next = state.clone();
    let mut event: Option<&str> = None;
    if args.paused && !manual {
        if state != "idle" && state != "active" && state != "ending" {
            next = "idle".into();
        }
    } else {
        match state.as_str() {
            "idle" => {
                // speculative capture on the first sign (HR reserve or sustained motion)
                if now >= st["cooldown_until"].as_i64().unwrap_or(0) && recent(3).iter().any(|v| *v > 0.35) && data_until > now - 15 * 60 {
                    next = "watch".into();
                    st["watch_since"] = json!(now);
                    event = Some("watch");
                }
            }
            "watch" => {
                let sig = signature(&mins, &p, &imu, &steps_per_second(&secs), now);
                st["signature"] = sig.clone();
                let quiet_watch = recent(8).iter().all(|v| *v <= 0.35);
                if active_recent(6) >= 3 && sig["confirmed"].as_bool() == Some(true) {
                    // backdate to the first active minute of this run (gaps <= 2 min tolerated)
                    let mut start = scores.last().map(|s| s.0).unwrap_or(now);
                    let mut gap = 0;
                    for s in scores.iter().rev() {
                        if s.1 > 0.5 {
                            start = s.0;
                            gap = 0;
                        } else {
                            gap += 1;
                            if gap > 2 {
                                break;
                            }
                        }
                    }
                    let id = new_session(&conn, start - PRE_BUFFER_S, "auto", None, now)?;
                    st["session_id"] = json!(id);
                    st["active_start"] = json!(start);
                    next = "active".into();
                    event = Some("start");
                } else if quiet_watch || now - st["watch_since"].as_i64().unwrap_or(now) > 20 * 60 {
                    // daily-life bursts (stairs, running to the kitchen): no sport signature
                    if !args.replay { conn.execute("UPDATE sport_acquisition SET reason = ?1 WHERE end_ts IS NULL", [format!("watch_dropped {sig}")])?; }
                    next = "idle".into();
                    event = Some("watch_dropped");
                }
            }
            "active" | "ending" => {
                let last_active = scores.iter().rev().find(|s| s.1 > 0.5).map(|s| s.0).unwrap_or(st["last_active"].as_i64().unwrap_or(now));
                st["last_active"] = json!(last_active.max(st["last_active"].as_i64().unwrap_or(0)));
                let quiet = now - st["last_active"].as_i64().unwrap_or(now) - 60;
                let too_long = now - st["active_start"].as_i64().unwrap_or(now) > 4 * 3600;
                if manual {
                    next = "active".into();
                    if too_long {
                        next = "ended".into();
                    }
                } else if quiet >= 10 * 60 || too_long {
                    next = "ended".into();
                } else if quiet >= 6 * 60 {
                    next = "ending".into();
                } else {
                    next = "active".into();
                }
                if next == "ended" {
                    let end = st["last_active"].as_i64().unwrap_or(now) + 60 + POST_BUFFER_S;
                    if let Some(id) = &session_id {
                        conn.execute("UPDATE sport_sessions SET end_ts = ?2, status = 'ended', updated_at = ?3 WHERE id = ?1", params![id, end.min(now), now])?;
                    }
                    event = Some("end");
                }
            }
            _ => next = "idle".into(),
        }
    }
    let mut analysis = Value::Null;
    if next == "ended" {
        if let Some(id) = &session_id {
            // discard bouts too short to be a session (stairs, a sprint to the bus)
            let (s, e): (i64, i64) = conn.query_row("SELECT start_ts, COALESCE(end_ts, ?2) FROM sport_sessions WHERE id = ?1", params![id, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let (s, e) = if manual { (s, e) } else { refine_bounds(&conn, &p, s, e)? };
            if !manual {
                conn.execute("UPDATE sport_sessions SET start_ts = ?2, end_ts = ?3 WHERE id = ?1", params![id, s, e])?;
            }
            if !manual && e - s < 8 * 60 + PRE_BUFFER_S + POST_BUFFER_S {
                conn.execute("UPDATE sport_sessions SET status = 'discarded', label_status = 'auto_discarded', updated_at = ?2 WHERE id = ?1", params![id, now])?;
            } else {
                analysis = analyze_session(&conn, id, args.age)?;
            }
        }
        // minutes of the session just closed cannot open the next one
        st = json!({"state": "idle", "after": st["last_active"].as_i64().unwrap_or(now) + 120});
        next = "idle".into();
    }
    if next == "watch" && state == "idle" && !args.replay {
        log_acq(&conn, None, true, now, "watch")?;
    }
    if next == "idle" && state != "idle" && !args.replay {
        log_acq(&conn, None, false, now, "stop")?;
    }
    st["state"] = json!(next);
    state_set(&conn, "engine", &st)?;
    let want = matches!(next.as_str(), "watch" | "active" | "ending");
    let live_session = st["session_id"].as_str().map(String::from);
    let current = match &live_session {
        Some(id) if next != "idle" => {
            let (start, label): (i64, Option<String>) = conn.query_row("SELECT start_ts, label_type FROM sport_sessions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            // a cheap live guess from the last 10 minutes
            let recent_imu = load_imu(&conn, (now - 600) as f64, (now + 1) as f64)?;
            let rs = sets_and_reps_with(&recent_imu, &steps_per_second(&secs), &|_| None);
            let rk = strokes(&recent_imu);
            let f = session_features(&mins, &p, &recent_imu, &rs, rk.len(), start.max(now - 600), now);
            let pr = rules_v1(&f);
            let b = pr.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap_or(3);
            json!({"id": id, "start_ts": start, "label_type": label, "guess": CLASSES[b], "guess_p": round(pr[b], 2), "reps_last10": rs.iter().filter(|s| s.periodic).map(|s| s.reps).sum::<usize>(), "sets_last10": rs.iter().filter(|s| s.periodic).count(), "strokes_last10": rk.len()})
        }
        _ => Value::Null,
    };
    Ok(json!({
        "state": next, "event": event, "want_streams": want, "sync_interval_s": if want { 120 } else { 300 },
        "data_until": data_until, "lag_s": now - data_until, "minute_scores": scores.iter().rev().take(10).map(|s| json!([s.0, round(s.1, 2)])).collect::<Vec<_>>(),
        "current": current, "analysis": analysis, "signature": st["signature"].clone(), "stored": {"live": stored.0, "imu": stored.1},
        "profile": {"rest": round(p.rest, 0), "hrmax": round(p.hrmax, 0)}, "engine_version": ENGINE_VERSION,
        "latency_ms": round(t0.elapsed().as_secs_f64() * 1000.0, 1),
    }))
}

// ------------------------------------------------------------------ manual sessions, labels, listing, dataset

#[derive(Debug, Deserialize)]
pub struct ManualArgs {
    pub database_path: String,
    /// "start" or "stop"
    pub action: String,
    #[serde(default)]
    pub sport: Option<String>,
    #[serde(default)]
    pub now_unix: Option<i64>,
    #[serde(default)]
    pub age: Option<f64>,
}

pub fn manual(args: &ManualArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let now = args.now_unix.unwrap_or_else(now_s);
    let mut st = state_get(&conn, "engine")?.unwrap_or_else(|| json!({"state": "idle"}));
    match args.action.as_str() {
        "start" => {
            let sport = args.sport.clone().unwrap_or_else(|| "autre".into());
            let id = match st["session_id"].as_str().filter(|_| st["state"] != "idle") {
                // an auto session already running becomes the labelled one
                Some(id) => {
                    conn.execute("UPDATE sport_sessions SET label_type = ?2, label_status = 'manual', source = 'auto+manual', updated_at = ?3 WHERE id = ?1", params![id, sport, now])?;
                    annotate_row(&conn, "session", id, "type", None, Some(&sport), "manual_start")?;
                    id.to_string()
                }
                None => {
                    let id = new_session(&conn, now, "manual", Some(&sport), now)?;
                    annotate_row(&conn, "session", &id, "type", None, Some(&sport), "manual_start")?;
                    log_acq(&conn, Some(&id), true, now, "manual")?;
                    id
                }
            };
            st = json!({"state": "active", "session_id": id, "active_start": now, "last_active": now, "manual": true});
            state_set(&conn, "engine", &st)?;
            Ok(json!({"state": "active", "session_id": id, "want_streams": true}))
        }
        "stop" => {
            let Some(id) = st["session_id"].as_str().map(String::from) else { return Ok(json!({"state": "idle"})) };
            conn.execute("UPDATE sport_sessions SET end_ts = ?2, status = 'ended', updated_at = ?2 WHERE id = ?1", params![id, now])?;
            log_acq(&conn, Some(&id), false, now, "manual_stop")?;
            state_set(&conn, "engine", &json!({"state": "idle", "cooldown_until": now + 300}))?;
            let a = analyze_session(&conn, &id, args.age)?;
            Ok(json!({"state": "idle", "session_id": id, "want_streams": false, "analysis": a}))
        }
        other => Err(crate::GooseError::Message(format!("unknown action {other}"))),
    }
}

fn annotate_row(conn: &Connection, target: &str, id: &str, field: &str, old: Option<&str>, new: Option<&str>, source: &str) -> GooseResult<()> {
    let (pred, conf, mv): (Option<String>, Option<f64>, Option<String>) = if target == "session" {
        conn.query_row("SELECT predicted_type, confidence, model_version FROM sport_sessions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.unwrap_or((None, None, None))
    } else {
        conn.query_row("SELECT CASE WHEN ?2 = 'reps' THEN CAST(reps_pred AS TEXT) ELSE exercise_pred END, confidence, engine_version FROM sport_segments WHERE id = ?1", params![id, field], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?.unwrap_or((None, None, None))
    };
    conn.execute(
        "INSERT INTO sport_annotations (ts, target, target_id, field, old_value, new_value, predicted_value, confidence, model_version, engine_version, source) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![now_s(), target, id, field, old, new, pred, conf, mv, ENGINE_VERSION, source],
    )?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct AnnotateArgs {
    pub database_path: String,
    /// "session" or "segment"
    pub target: String,
    pub id: String,
    /// session: "confirm" | "type" | "reject"; segment: "confirm" | "reps" | "exercise" | "reject" | "concentric"
    pub field: String,
    #[serde(default)]
    pub value: Option<String>,
}

pub fn annotate(args: &AnnotateArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let now = now_s();
    let v = args.value.as_deref();
    match (args.target.as_str(), args.field.as_str()) {
        ("session", "confirm") => {
            let old: Option<String> = conn.query_row("SELECT label_type FROM sport_sessions WHERE id = ?1", [&args.id], |r| r.get(0))?;
            let pred: Option<String> = conn.query_row("SELECT predicted_type FROM sport_sessions WHERE id = ?1", [&args.id], |r| r.get(0))?;
            let label = old.clone().or(pred);
            conn.execute("UPDATE sport_sessions SET label_type = ?2, label_status = 'confirmed', updated_at = ?3 WHERE id = ?1", params![args.id, label, now])?;
            annotate_row(&conn, "session", &args.id, "type", old.as_deref(), label.as_deref(), "confirm")?;
        }
        ("session", "type") => {
            let old: Option<String> = conn.query_row("SELECT label_type FROM sport_sessions WHERE id = ?1", [&args.id], |r| r.get(0))?;
            conn.execute("UPDATE sport_sessions SET label_type = ?2, label_status = 'corrected', updated_at = ?3 WHERE id = ?1", params![args.id, v, now])?;
            annotate_row(&conn, "session", &args.id, "type", old.as_deref(), v, "correct")?;
            // segments depend on the type: re-derive them unless some were labelled already
            analyze_session(&conn, &args.id, None)?;
        }
        ("session", "reject") => {
            conn.execute("UPDATE sport_sessions SET label_type = 'aucune', label_status = 'rejected', updated_at = ?2 WHERE id = ?1", params![args.id, now])?;
            annotate_row(&conn, "session", &args.id, "type", None, Some("aucune"), "reject")?;
        }
        ("segment", "confirm") => {
            conn.execute("UPDATE sport_segments SET reps_label = COALESCE(reps_label, reps_pred), exercise_label = COALESCE(exercise_label, exercise_pred), label_status = 'confirmed' WHERE id = ?1", [&args.id])?;
            annotate_row(&conn, "segment", &args.id, "confirm", None, Some("ok"), "confirm")?;
        }
        ("segment", "reps") => {
            let n: Option<i64> = v.and_then(|s| s.parse().ok());
            conn.execute("UPDATE sport_segments SET reps_label = ?2, label_status = 'corrected' WHERE id = ?1", params![args.id, n])?;
            annotate_row(&conn, "segment", &args.id, "reps", None, v, "correct")?;
        }
        ("segment", "exercise") => {
            conn.execute("UPDATE sport_segments SET exercise_label = ?2, label_status = 'corrected' WHERE id = ?1", params![args.id, v])?;
            annotate_row(&conn, "segment", &args.id, "exercise", None, v, "correct")?;
            // the other sets of the same group in this session get it as a prediction
            let (sid, d): (String, Option<String>) = conn.query_row("SELECT session_id, detail_json FROM sport_segments WHERE id = ?1", [&args.id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let cluster = d.and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v["cluster"].as_i64());
            if let Some(c) = cluster {
                let mut st = conn.prepare("SELECT id, detail_json FROM sport_segments WHERE session_id = ?1 AND id != ?2 AND label_status = 'pending'")?;
                let others: Vec<(String, Option<String>)> = st.query_map(params![sid, args.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
                for (oid, od) in others {
                    if od.and_then(|s| serde_json::from_str::<Value>(&s).ok()).and_then(|v| v["cluster"].as_i64()) == Some(c) {
                        conn.execute("UPDATE sport_segments SET exercise_pred = ?2, exercise_conf = 0.9 WHERE id = ?1", params![oid, v])?;
                    }
                }
            }
        }
        ("segment", "concentric") => {
            // "up" (default) or "down": which vertical direction is the concentric phase
            let d: Option<String> = conn.query_row("SELECT detail_json FROM sport_segments WHERE id = ?1", [&args.id], |r| r.get(0))?;
            let mut d: Value = d.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
            d["concentric"] = json!(v.unwrap_or("up"));
            conn.execute("UPDATE sport_segments SET detail_json = ?2 WHERE id = ?1", params![args.id, d.to_string()])?;
            annotate_row(&conn, "segment", &args.id, "concentric", None, v, "correct")?;
        }
        ("segment", "reject") => {
            conn.execute("UPDATE sport_segments SET label_status = 'rejected' WHERE id = ?1", [&args.id])?;
            annotate_row(&conn, "segment", &args.id, "reject", None, Some("rejected"), "reject")?;
        }
        (t, f) => return Err(crate::GooseError::Message(format!("unknown annotation {t}.{f}"))),
    }
    Ok(json!({"ok": true}))
}

#[derive(Debug, Deserialize)]
pub struct ReplayArgs {
    pub database_path: String,
    pub from_ts: i64,
    pub to_ts: i64,
    #[serde(default)]
    pub age: Option<f64>,
}

/// Offline replay of the online engine over past history (R18 + any IMU stored), one step per
/// minute, with the history lag of the real app (sync every 300 s at rest, 120 s while a session is
/// plausible). Run it on a copy of the database: it rewrites the engine state and adds sessions.
pub fn replay(args: &ReplayArgs) -> GooseResult<Value> {
    {
        let conn = Connection::open(&args.database_path)?;
        ensure_tables(&conn)?;
        state_set(&conn, "engine", &json!({"state": "idle"}))?;
    }
    let mut events = Vec::new();
    let mut timeline = Vec::new();
    let mut state = "idle".to_string();
    let mut last_sync = args.from_ts;
    for now in (args.from_ts..=args.to_ts).step_by(60) {
        let every = if state == "idle" { 300 } else { 120 };
        if now - last_sync >= every {
            last_sync = now;
        }
        let r = update(&UpdateArgs {
            database_path: args.database_path.clone(),
            now_unix: Some(now),
            live: vec![],
            imu: vec![],
            age: args.age,
            capturing: state != "idle",
            paused: false,
            history_lag_s: Some(now - last_sync + 20),
            replay: true,
        })?;
        state = r["state"].as_str().unwrap_or("idle").to_string();
        if let Some(e) = r["event"].as_str() {
            events.push(json!({"ts": now, "event": e, "state": state, "data_until": r["data_until"]}));
        }
        if let Some(sc) = r["minute_scores"].as_array().and_then(|a| a.first()) {
            timeline.push(json!([now, state, sc[1]]));
        }
    }
    let conn = Connection::open(&args.database_path)?;
    let sessions: Vec<Value> = conn
        .prepare("SELECT id, start_ts, end_ts, status, source, predicted_type, probs_json, confidence, summary_json, model_version, label_type, label_status FROM sport_sessions WHERE start_ts >= ?1 AND start_ts <= ?2 ORDER BY start_ts")?
        .query_map(params![args.from_ts - 3600, args.to_ts], row_session)?
        .collect::<Result<_, _>>()?;
    let features: Vec<Value> = conn
        .prepare("SELECT id, features_json FROM sport_sessions WHERE start_ts >= ?1 AND start_ts <= ?2 ORDER BY start_ts")?
        .query_map(params![args.from_ts - 3600, args.to_ts], |r| Ok(json!({"id": r.get::<_, String>(0)?, "features": r.get::<_, Option<String>>(1)?.and_then(|s| serde_json::from_str::<Value>(&s).ok())})))?
        .collect::<Result<_, _>>()?;
    Ok(json!({"events": events, "sessions": sessions, "features": features, "timeline": timeline}))
}

#[derive(Debug, Deserialize)]
pub struct SeedLabel {
    pub ts: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct BackfillArgs {
    pub database_path: String,
    #[serde(default)]
    pub days: Option<i64>,
    #[serde(default)]
    pub now_unix: Option<i64>,
    #[serde(default)]
    pub age: Option<f64>,
    /// labels given by the user outside the app: the session containing ts gets this type, validated
    #[serde(default)]
    pub labels: Vec<SeedLabel>,
}

/// Retrospective detection on the phone's own history (sessions done before the engine existed or
/// while the app was not running). Day by day, only around candidate minutes, with the same online
/// engine and sync lag as `replay`; the live engine state is saved and restored. Incremental:
/// resumes where the previous backfill stopped.
pub fn backfill(args: &BackfillArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_tables(&conn)?;
    let _ = crate::whoop5_store::ingest(&conn);
    let now = args.now_unix.unwrap_or_else(now_s);
    let saved = state_get(&conn, "engine")?;
    let done_until = state_get(&conn, "backfill")?.and_then(|v| v["done_until"].as_i64()).unwrap_or(0);
    let to = now - 3600;
    let from = done_until.max(now - args.days.unwrap_or(14) * 86_400);
    let p = profile(&conn, to, args.age)?;
    let mut windows: Vec<(i64, i64)> = Vec::new();
    let mut day = from;
    while day < to {
        let secs = load_secs(&conn, day, (day + 86_400).min(to))?;
        for m in minutes(&secs, &BTreeMap::new()) {
            if minute_score(&m, &p) > 0.35 {
                match windows.last_mut() {
                    Some(w) if m.t - w.1 <= 30 * 60 => w.1 = m.t,
                    _ => windows.push((m.t, m.t)),
                }
            }
        }
        day += 86_400;
    }
    drop(conn);
    let mut replayed = 0;
    for (a, b) in &windows {
        let (a, b) = ((a - 20 * 60).max(from), (b + 40 * 60).min(to));
        let c = Connection::open(&args.database_path)?;
        let overlaps: i64 = c.query_row("SELECT COUNT(*) FROM sport_sessions WHERE start_ts <= ?2 AND COALESCE(end_ts, start_ts) >= ?1", params![a, b], |r| r.get(0))?;
        if overlaps > 0 {
            continue;
        }
        state_set(&c, "engine", &json!({"state": "idle"}))?;
        drop(c);
        let mut state = "idle".to_string();
        let mut last_sync = a;
        for t in (a..=b).step_by(60) {
            if t - last_sync >= if state == "idle" { 300 } else { 120 } {
                last_sync = t;
            }
            let r = update(&UpdateArgs { database_path: args.database_path.clone(), now_unix: Some(t), live: vec![], imu: vec![], age: args.age, capturing: false, paused: false, history_lag_s: Some(t - last_sync + 20), replay: true })?;
            state = r["state"].as_str().unwrap_or("idle").to_string();
        }
        // a session still open at the end of its window is closed by the next steps
        let mut t = b;
        while state != "idle" && t < b + 3 * 3600 && t < to {
            t += 60;
            let r = update(&UpdateArgs { database_path: args.database_path.clone(), now_unix: Some(t), live: vec![], imu: vec![], age: args.age, capturing: false, paused: false, history_lag_s: Some(20), replay: true })?;
            state = r["state"].as_str().unwrap_or("idle").to_string();
        }
        replayed += 1;
    }
    let conn = Connection::open(&args.database_path)?;
    state_set(&conn, "engine", &saved.unwrap_or_else(|| json!({"state": "idle"})))?;
    state_set(&conn, "backfill", &json!({"done_until": to, "at": now}))?;
    let mut labelled = 0;
    for l in &args.labels {
        let id: Option<String> = conn
            .query_row("SELECT id FROM sport_sessions WHERE start_ts <= ?1 AND COALESCE(end_ts, start_ts) >= ?1 AND status != 'discarded' ORDER BY start_ts DESC LIMIT 1", [l.ts], |r| r.get(0))
            .optional()?;
        if let Some(id) = id {
            let old: Option<String> = conn.query_row("SELECT label_type FROM sport_sessions WHERE id = ?1", [&id], |r| r.get(0))?;
            if old.as_deref() != Some(l.kind.as_str()) {
                conn.execute("UPDATE sport_sessions SET label_type = ?2, label_status = 'confirmed', updated_at = ?3 WHERE id = ?1", params![id, l.kind, now])?;
                annotate_row(&conn, "session", &id, "type", old.as_deref(), Some(&l.kind), "user_outside_app")?;
                labelled += 1;
            }
        }
    }
    let sessions: Vec<Value> = conn
        .prepare("SELECT id, start_ts, end_ts, status, source, predicted_type, probs_json, confidence, summary_json, model_version, label_type, label_status FROM sport_sessions WHERE start_ts >= ?1 AND start_ts <= ?2 ORDER BY start_ts")?
        .query_map(params![from - 3600, to], row_session)?
        .collect::<Result<_, _>>()?;
    Ok(json!({"from": from, "to": to, "windows": windows.len(), "replayed": replayed, "labelled": labelled, "sessions": sessions}))
}

#[derive(Debug, Deserialize)]
pub struct DeleteArgs {
    pub database_path: String,
    pub id: String,
}

/// Removes a session started by mistake (and its segments). The deletion itself is kept in
/// sport_annotations so the dataset history stays auditable; the exported folder is removed at
/// the next export. Deleting the running session stops it.
pub fn delete_session(args: &DeleteArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let now = now_s();
    let row: Option<(String, Option<String>)> = conn.query_row("SELECT COALESCE(label_type, predicted_type, ''), source FROM sport_sessions WHERE id = ?1", [&args.id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    let Some((ty, source)) = row else { return Ok(json!({"ok": false, "reason": "not_found"})) };
    conn.execute(
        "INSERT INTO sport_annotations (ts, target, target_id, field, old_value, new_value, engine_version, source) VALUES (?1, 'session', ?2, 'deleted', ?3, NULL, ?4, ?5)",
        params![now, args.id, ty, ENGINE_VERSION, source.unwrap_or_default()],
    )?;
    conn.execute("DELETE FROM sport_segments WHERE session_id = ?1", [&args.id])?;
    conn.execute("DELETE FROM sport_sessions WHERE id = ?1", [&args.id])?;
    conn.execute("UPDATE sport_acquisition SET session_id = NULL WHERE session_id = ?1", [&args.id])?;
    let mut stopped = false;
    if let Some(st) = state_get(&conn, "engine")? {
        if st["session_id"].as_str() == Some(args.id.as_str()) {
            state_set(&conn, "engine", &json!({"state": "idle", "cooldown_until": now + 300}))?;
            stopped = true;
        }
    }
    Ok(json!({"ok": true, "stopped": stopped}))
}

#[derive(Debug, Deserialize)]
pub struct ListArgs {
    pub database_path: String,
    #[serde(default)]
    pub days: Option<i64>,
}

fn row_session(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let parse = |s: Option<String>| s.and_then(|v| serde_json::from_str::<Value>(&v).ok()).unwrap_or(Value::Null);
    Ok(json!({
        "id": r.get::<_, String>(0)?, "start_ts": r.get::<_, i64>(1)?, "end_ts": r.get::<_, Option<i64>>(2)?, "status": r.get::<_, String>(3)?,
        "source": r.get::<_, String>(4)?, "predicted_type": r.get::<_, Option<String>>(5)?, "probs": parse(r.get(6)?), "confidence": r.get::<_, Option<f64>>(7)?,
        "summary": parse(r.get(8)?), "model_version": r.get::<_, Option<String>>(9)?, "label_type": r.get::<_, Option<String>>(10)?, "label_status": r.get::<_, String>(11)?,
    }))
}

pub fn list(args: &ListArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    // a newer engine re-analyses finished sessions once (human-labelled segments are kept)
    let stale: Vec<String> = conn
        .prepare("SELECT id FROM sport_sessions WHERE status = 'ended' AND engine_version != ?1")?
        .query_map([ENGINE_VERSION], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let reanalyzed = stale.len();
    for id in stale {
        let _ = analyze_session(&conn, &id, None);
    }
    let since = now_s() - args.days.unwrap_or(30) * 86_400;
    let mut st = conn.prepare(
        "SELECT id, start_ts, end_ts, status, source, predicted_type, probs_json, confidence, summary_json, model_version, label_type, label_status FROM sport_sessions WHERE start_ts >= ?1 AND status != 'discarded' ORDER BY start_ts DESC",
    )?;
    let mut sessions: Vec<Value> = st.query_map([since], row_session)?.collect::<Result<_, _>>()?;
    let mut seg = conn.prepare("SELECT id, kind, start_ts, end_ts, reps_pred, reps_label, exercise_pred, exercise_conf, exercise_label, confidence, detail_json, label_status FROM sport_segments WHERE session_id = ?1 ORDER BY start_ts")?;
    for s in sessions.iter_mut() {
        let id = s["id"].as_str().unwrap_or("").to_string();
        let segs: Vec<Value> = seg
            .query_map([&id], |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "start_ts": r.get::<_, f64>(2)?, "end_ts": r.get::<_, f64>(3)?,
                    "reps_pred": r.get::<_, Option<i64>>(4)?, "reps_label": r.get::<_, Option<i64>>(5)?, "exercise_pred": r.get::<_, Option<String>>(6)?,
                    "exercise_conf": r.get::<_, Option<f64>>(7)?, "exercise_label": r.get::<_, Option<String>>(8)?, "confidence": r.get::<_, Option<f64>>(9)?,
                    "detail": r.get::<_, Option<String>>(10)?.and_then(|v| serde_json::from_str::<Value>(&v).ok()).unwrap_or(Value::Null), "label_status": r.get::<_, String>(11)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        s["segments"] = json!(segs);
    }
    // review queue: unlabelled finished sessions, least confident first
    let mut queue: Vec<&Value> = sessions.iter().filter(|s| s["label_status"] == "pending" && s["status"] == "ended").collect();
    queue.sort_by(|a, b| a["confidence"].as_f64().unwrap_or(0.0).total_cmp(&b["confidence"].as_f64().unwrap_or(0.0)));
    let queue_ids: Vec<Value> = queue.iter().map(|s| s["id"].clone()).collect();
    let exercises: Vec<String> = conn.prepare("SELECT exercise_label FROM sport_segments WHERE exercise_label IS NOT NULL GROUP BY exercise_label ORDER BY COUNT(*) DESC")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let engine = state_get(&conn, "engine")?.unwrap_or(Value::Null);
    let counts: Value = json!({
        "labelled_sessions": conn.query_row("SELECT COUNT(*) FROM sport_sessions WHERE label_status IN ('confirmed','corrected','manual','rejected')", [], |r| r.get::<_, i64>(0))?,
        "labelled_sets": conn.query_row("SELECT COUNT(*) FROM sport_segments WHERE label_status IN ('confirmed','corrected')", [], |r| r.get::<_, i64>(0))?,
        "annotations": conn.query_row("SELECT COUNT(*) FROM sport_annotations", [], |r| r.get::<_, i64>(0))?,
    });
    Ok(json!({"sessions": sessions, "review_queue": queue_ids, "exercises": exercises, "engine": engine, "dataset": counts, "engine_version": ENGINE_VERSION, "reanalyzed": reanalyzed}))
}

#[derive(Debug, Deserialize)]
pub struct DatasetArgs {
    pub database_path: String,
    pub output_dir: String,
}

/// Personal dataset: one folder per session (raw IMU frames + decoded samples, R18, live HR,
/// predictions, labels) and global JSONL tables (sessions, segments, annotations, models).
pub fn export_dataset(args: &DatasetArgs) -> GooseResult<Value> {
    use std::io::Write;
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let root = std::path::Path::new(&args.output_dir);
    std::fs::create_dir_all(root)?;
    let dump = |sql: &str, name: &str| -> GooseResult<usize> {
        let mut st = conn.prepare(sql)?;
        let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
        let mut f = std::io::BufWriter::new(std::fs::File::create(root.join(name))?);
        let mut n = 0;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let mut o = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                o.insert(c.clone(), match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(x) => json!(x),
                    rusqlite::types::Value::Real(x) => json!(x),
                    rusqlite::types::Value::Text(s) => serde_json::from_str::<Value>(&s).ok().filter(|_| c.ends_with("_json")).unwrap_or(json!(s)),
                    rusqlite::types::Value::Blob(b) => json!(hex::encode(b)),
                });
            }
            writeln!(f, "{}", Value::Object(o))?;
            n += 1;
        }
        Ok(n)
    };
    let mut counts = serde_json::Map::new();
    counts.insert("sessions".into(), json!(dump("SELECT * FROM sport_sessions ORDER BY start_ts", "sessions.jsonl")?));
    counts.insert("segments".into(), json!(dump("SELECT * FROM sport_segments ORDER BY start_ts", "segments.jsonl")?));
    counts.insert("annotations".into(), json!(dump("SELECT * FROM sport_annotations ORDER BY id", "annotations.jsonl")?));
    counts.insert("models".into(), json!(dump("SELECT * FROM sport_models ORDER BY created_at", "models.jsonl")?));
    counts.insert("acquisition".into(), json!(dump("SELECT * FROM sport_acquisition ORDER BY id", "acquisition.jsonl")?));
    let sessions: Vec<(String, i64, i64)> = conn
        .prepare("SELECT id, start_ts, COALESCE(end_ts, start_ts) FROM sport_sessions WHERE status != 'discarded'")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut exported = 0;
    if let Ok(entries) = std::fs::read_dir(root.join("sessions")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !sessions.iter().any(|(id, _, _)| *id == name) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    for (id, s, e) in &sessions {
        let dir = root.join("sessions").join(id);
        if dir.join("imu.csv").exists() {
            continue; // immutable once written
        }
        std::fs::create_dir_all(&dir)?;
        let (a, b) = (s - PRE_BUFFER_S, e + POST_BUFFER_S);
        let imu = load_imu(&conn, a as f64, b as f64)?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("imu.csv"))?);
        writeln!(f, "t,ax_g,ay_g,az_g,gx_dps,gy_dps,gz_dps")?;
        for i in 0..imu.len() {
            writeln!(f, "{:.3},{:.5},{:.5},{:.5},{:.3},{:.3},{:.3}", imu.t[i], imu.a[0][i], imu.a[1][i], imu.a[2][i], imu.g[0][i], imu.g[1][i], imu.g[2][i])?;
        }
        let mut raw = std::io::BufWriter::new(std::fs::File::create(dir.join("imu_frames.jsonl"))?);
        let mut stf = conn.prepare("SELECT ts, arrival, frame FROM sport_imu WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        for row in stf.query_map(params![a as f64, b as f64], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, Vec<u8>>(2)?)))? {
            let (t, arr, fr) = row?;
            writeln!(raw, "{}", json!({"ts": t, "arrival": arr, "frame_hex": hex::encode(fr)}))?;
        }
        let secs = load_secs(&conn, a, b)?;
        let mut r18 = std::io::BufWriter::new(std::fs::File::create(dir.join("r18.csv"))?);
        writeln!(r18, "ts,hr,steps,cadence_raw,dyn_g,grav_x,grav_y,grav_z,activity,sleep")?;
        for x in secs.values() {
            let g = x.grav.unwrap_or([f64::NAN; 3]);
            writeln!(r18, "{},{},{},{},{},{},{},{},{},{}", x.ts, x.hr.map(|v| v.to_string()).unwrap_or_default(), x.steps.map(|v| v.to_string()).unwrap_or_default(), x.cadence_raw.map(|v| v.to_string()).unwrap_or_default(), x.dyn_g.map(|v| format!("{v:.4}")).unwrap_or_default(), g[0], g[1], g[2], x.activity.clone().unwrap_or_default(), x.sleep as u8)?;
        }
        let mut lh = std::io::BufWriter::new(std::fs::File::create(dir.join("live_hr.csv"))?);
        writeln!(lh, "ts,hr,rr_ms,arrival")?;
        let mut stl = conn.prepare("SELECT ts, hr, rr, arrival FROM sport_live WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        for row in stl.query_map(params![a, b], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<f64>>(3)?)))? {
            let (t, h, rr, arr) = row?;
            writeln!(lh, "{t},{},\"{rr}\",{}", h.map(|v| v.to_string()).unwrap_or_default(), arr.map(|v| v.to_string()).unwrap_or_default())?;
        }
        exported += 1;
    }
    std::fs::write(root.join("README.json"), json!({
        "engine_version": ENGINE_VERSION, "classes": CLASSES, "features": FEATURES,
        "time": "unix seconds (strap clock); imu t = record ts + sample/100 Hz; arrival = phone clock",
        "units": {"imu": "accel g (4096 LSB/g, +-8 g), gyro deg/s (+-2000)", "hr": "bpm", "rr": "ms"},
        "buffers_s": {"pre": PRE_BUFFER_S, "post": POST_BUFFER_S},
        "labels": "sessions.label_type / label_status; segments.reps_label / exercise_label; full history in annotations.jsonl",
    }).to_string())?;
    counts.insert("session_folders_written".into(), json!(exported));
    Ok(Value::Object(counts))
}

#[derive(Debug, Deserialize)]
pub struct InstallModelArgs {
    pub database_path: String,
    pub version: String,
    pub kind: String,
    pub params: Value,
    #[serde(default)]
    pub metrics: Option<Value>,
}

pub fn install_model(args: &InstallModelArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    conn.execute("UPDATE sport_models SET active = 0", [])?;
    conn.execute(
        "INSERT OR REPLACE INTO sport_models (version, kind, created_at, active, params_json, metrics_json) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
        params![args.version, args.kind, now_s(), args.params.to_string(), args.metrics.as_ref().map(|m| m.to_string())],
    )?;
    Ok(json!({"ok": true, "active": args.version}))
}

#[derive(Debug, Deserialize)]
pub struct AnalyzeArgs {
    pub database_path: String,
    pub id: String,
    #[serde(default)]
    pub age: Option<f64>,
}

pub fn analyze(args: &AnalyzeArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    analyze_session(&conn, &args.id, args.age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn imu_from(f: impl Fn(f64) -> ([f64; 3], [f64; 3]), secs: f64) -> Imu {
        let mut imu = Imu::default();
        let n = (secs * IMU_FS) as usize;
        for i in 0..n {
            let t = 1_800_000_000.0 + i as f64 / IMU_FS;
            let (a, g) = f(i as f64 / IMU_FS);
            imu.t.push(t);
            for k in 0..3 {
                imu.a[k].push(a[k]);
                imu.g[k].push(g[k]);
            }
        }
        imu
    }

    #[test]
    fn counts_reps_in_sets_and_finds_rests() {
        // 3 sets of 10 reps at 0.4 Hz (2.5 s / rep, vertical), 60 s rest between sets
        let imu = imu_from(
            |t| {
                let in_set = (t % 85.0) < 25.0 && t < 255.0;
                let v = if in_set { 0.25 * (2.0 * PI * 0.4 * (t % 85.0)).sin() } else { 0.0 };
                let w = if in_set { 60.0 * (2.0 * PI * 0.4 * (t % 85.0)).cos() } else { 0.3 };
                ([0.0, 0.0, 1.0 + v], [w, 0.0, 0.0])
            },
            260.0,
        );
        let sets = sets_and_reps(&imu);
        let per: Vec<&SetResult> = sets.iter().filter(|s| s.periodic).collect();
        assert_eq!(per.len(), 3, "{sets:?}");
        for s in per {
            assert!((9..=11).contains(&s.reps), "{s:?}");
            assert!((s.period_s.unwrap() - 2.5).abs() < 0.3);
            assert!(s.up_s.unwrap() > 0.5 && s.down_s.unwrap() > 0.5);
        }
        assert!(strokes(&imu).is_empty());
    }

    #[test]
    fn detects_tennis_strokes() {
        // a stroke every 4 s: 30 ms burst of 900 deg/s with a 4 g impact
        let imu = imu_from(
            |t| {
                let ph = t % 4.0;
                if (1.0..1.03).contains(&ph) { ([4.0, 0.5, 1.0], [900.0, 100.0, 0.0]) } else { ([0.0, 0.0, 1.0], [5.0, 0.0, 0.0]) }
            },
            60.0,
        );
        let s = strokes(&imu);
        assert!((14..=16).contains(&s.len()), "{}", s.len());
    }

    #[test]
    fn minute_score_separates_effort_from_walk_and_rest() {
        let p = Profile { rest: 50.0, hrmax: 190.0 };
        let m = |hr: f64, steps: f64, dyn_: f64| Minute { t: 0, n: 60, hr: Some(hr), steps, dyn_mean: Some(dyn_), ..Default::default() };
        assert!(minute_score(&m(58.0, 0.0, 0.01), &p) < 0.1);
        assert!(minute_score(&m(85.0, 90.0, 0.08), &p) < 0.5); // walking to the bus
        assert!(minute_score(&m(130.0, 10.0, 0.15), &p) > 0.5); // strength set
        assert!(minute_score(&m(150.0, 165.0, 0.3), &p) > 0.9); // running
    }

    #[test]
    fn rules_tell_running_tennis_strength_apart() {
        let base = |pairs: &[(&'static str, f64)]| -> BTreeMap<&'static str, f64> { pairs.iter().copied().collect() };
        let arg = |p: Vec<f64>| p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(CLASSES[arg(rules_v1(&base(&[("steps_pm", 165.0), ("run_frac", 0.9), ("cad_spm", 168.0), ("imu_cov", 0.9)])))], "course");
        assert_eq!(CLASSES[arg(rules_v1(&base(&[("steps_pm", 45.0), ("strokes_pm", 5.0), ("imu_cov", 0.9)])))], "tennis");
        assert_eq!(CLASSES[arg(rules_v1(&base(&[("steps_pm", 5.0), ("sets_per_10min", 2.5), ("periodic_frac", 0.8), ("imu_cov", 0.9)])))], "musculation");
    }

    #[test]
    fn online_engine_backdates_start_and_ends_after_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let conn = Connection::open(&db).unwrap();
        ensure_tables(&conn).unwrap();
        crate::whoop5_store::ensure_tables(&conn).unwrap();
        let t0: i64 = 1_800_000_000;
        // 5 min rest, 20 min running, then rest
        let hr_at = |t: i64| if (300..1500).contains(&(t - t0)) { 150 } else { 55 };
        for t in t0..t0 + 3000 {
            let run = (300..1500).contains(&(t - t0));
            conn.execute(
                "INSERT INTO whoop5_r18 (ts, record_index, frame_id, hr, rr_ms, steps, dyn_accel_g, activity, band_state, decoder_version) VALUES (?1, ?2, 'f', ?3, '', ?4, ?5, ?6, 'wake', 'v')",
                params![t, t - t0, hr_at(t), ((t - t0) * if run { 3 } else { 0 } / 1) % 65536, if run { 0.3 } else { 0.01 }, if run { "run" } else { "still" }],
            ).unwrap();
        }
        let mut states = Vec::new();
        for now in (t0 + 60..t0 + 3000).step_by(60) {
            let r = update(&UpdateArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), live: vec![], imu: vec![], age: Some(30.0), capturing: false, paused: false, history_lag_s: None, replay: false }).unwrap();
            states.push((now - t0, r["state"].as_str().unwrap().to_string(), r["event"].clone()));
        }
        let start_evt = states.iter().find(|s| s.2 == "start").expect("start");
        assert!(start_evt.0 <= 600, "{states:?}");
        let end_evt = states.iter().find(|s| s.2 == "end").expect("end");
        assert!(end_evt.0 >= 1500 + 600 && end_evt.0 <= 1500 + 800, "{end_evt:?}");
        let (s, e, ty): (i64, i64, String) = conn.query_row("SELECT start_ts, end_ts, predicted_type FROM sport_sessions", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert!((s - (t0 + 300 - PRE_BUFFER_S)).abs() <= 60, "start {}", s - t0);
        assert!((e - (t0 + 1500 + POST_BUFFER_S)).abs() <= 120, "end {}", e - t0);
        assert_eq!(ty, "course");
    }

    #[test]
    #[ignore]
    fn real_sensor_session() {
        // GOOSE_SENSOR_TSV = Documents/sensor_session.tsv pulled from the phone
        let Ok(path) = std::env::var("GOOSE_SENSOR_TSV") else { return };
        let mut by_phase: BTreeMap<String, Imu> = BTreeMap::new();
        for line in std::fs::read_to_string(path).unwrap().lines().skip(1) {
            let c: Vec<&str> = line.split('\t').collect();
            let Ok(b) = hex::decode(c[3]) else { continue };
            if b.len() != 1244 || b[8] != 43 { continue }
            if let Some(r) = crate::whoop5_records::decode_r21(&b[8..b.len() - 4]) {
                by_phase.entry(format!("{}|{}", c[1], c[2])).or_default().push_record(&r);
            }
        }
        for (k, imu) in &by_phase {
            let st = strokes(imu);
            let sets = sets_and_reps(imu);
            println!("{k}: {:.0} s, strokes {}, sets {:?}", imu.len() as f64 / IMU_FS, st.len(), sets.iter().map(|s| (round(s.end - s.start, 1), s.periodic, s.reps, s.period_s, s.confidence)).collect::<Vec<_>>());
        }
        let mut all = Imu::default();
        for imu in by_phase.values().filter(|i| !i.is_empty()) {
            for i in 0..imu.len() { all.t.push(imu.t[i]); for k in 0..3 { all.a[k].push(imu.a[k][i]); all.g[k].push(imu.g[k][i]); } }
        }
        println!("ALL strokes {} sets {:?}", strokes(&all).len(), sets_and_reps(&all).iter().map(|s| (round(s.end - s.start, 1), s.periodic, s.reps, s.period_s)).collect::<Vec<_>>());
    }

    #[test]
    #[ignore]
    fn replay_real_day() {
        // GOOSE_DB (a copy!), GOOSE_FROM, GOOSE_TO (unix), optional GOOSE_OUT (json path)
        let (Ok(db), Ok(a), Ok(b)) = (std::env::var("GOOSE_DB"), std::env::var("GOOSE_FROM"), std::env::var("GOOSE_TO")) else { return };
        let r = replay(&ReplayArgs { database_path: db, from_ts: a.parse().unwrap(), to_ts: b.parse().unwrap(), age: None }).unwrap();
        if let Ok(out) = std::env::var("GOOSE_OUT") {
            std::fs::write(out, serde_json::to_string_pretty(&r).unwrap()).unwrap();
        }
        println!("events {}", r["events"]);
        for s in r["sessions"].as_array().unwrap() {
            println!("session {} {}..{} {} p={} {}", s["id"], s["start_ts"], s["end_ts"], s["predicted_type"], s["confidence"], s["probs"]);
        }
    }

    #[test]
    fn daily_life_bursts_do_not_open_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let conn = Connection::open(&db).unwrap();
        ensure_tables(&conn).unwrap();
        crate::whoop5_store::ensure_tables(&conn).unwrap();
        let t0: i64 = 1_800_000_000;
        // 30 min of an active life: every 3 min, 1 min of stairs (HR 140, 150 steps/min), else walking at HR 90
        let mut steps = 0i64;
        for t in t0..t0 + 3600 {
            let k = t - t0;
            let burst = k >= 600 && k < 2400 && (k / 60) % 3 == 0;
            let walk = k >= 600 && k < 2400;
            steps += if burst { 150 } else if walk { 60 } else { 0 };
            conn.execute(
                "INSERT INTO whoop5_r18 (ts, record_index, frame_id, hr, rr_ms, steps, dyn_accel_g, activity, band_state, decoder_version) VALUES (?1, ?2, 'f', ?3, '', ?4, ?5, ?6, 'wake', 'v')",
                params![t, k, if burst { 140 } else if walk { 90 } else { 60 }, (steps / 60) % 65536, if burst { 0.6 } else if walk { 0.25 } else { 0.01 }, if walk { "walk" } else { "still" }],
            ).unwrap();
        }
        let mut watched = false;
        for now in (t0 + 60..t0 + 3600).step_by(60) {
            let r = update(&UpdateArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), live: vec![], imu: vec![], age: Some(30.0), capturing: false, paused: false, history_lag_s: None, replay: false }).unwrap();
            watched |= r["state"] == "watch";
            assert_ne!(r["event"], "start", "at {}: {}", now - t0, r["signature"]);
        }
        assert!(watched, "bursts should at least trigger the IMU watch");
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM sport_sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    #[ignore]
    fn backfill_real_db() {
        let (Ok(db), Ok(now)) = (std::env::var("GOOSE_DB"), std::env::var("GOOSE_NOW")) else { return };
        let t = std::time::Instant::now();
        let r = backfill(&BackfillArgs { database_path: db, days: Some(14), now_unix: Some(now.parse().unwrap()), age: None, labels: vec![SeedLabel { ts: 1_794_398_400, kind: "tennis".into() }] }).unwrap();
        println!("backfill {:.1} s: windows {} replayed {} labelled {}", t.elapsed().as_secs_f64(), r["windows"], r["replayed"], r["labelled"]);
        for s in r["sessions"].as_array().unwrap() {
            println!("  {} {}..{} {} {} label {} {}", s["id"], s["start_ts"], s["end_ts"], s["status"], s["predicted_type"], s["label_type"], s["label_status"]);
        }
    }

    #[test]
    fn groups_sets_by_exercise_and_recognises_them_next_time() {
        // exercise A: vertical press, wrist flat (gravity on z), 2.5 s reps; exercise B: curl-like
        // rotation about x, wrist on its side (gravity on y), 3.2 s reps; 3 sets each, 60 s rests
        let mk = |which: u8, t: f64| -> ([f64; 3], [f64; 3]) {
            if which == 0 {
                let v = 0.25 * (2.0 * PI * t / 2.5).sin();
                ([0.0, 0.05, 1.0 + v], [40.0 * (2.0 * PI * t / 2.5).cos(), 0.0, 0.0])
            } else {
                let a = 0.6 * (2.0 * PI * t / 3.2).sin();
                ([0.2 * a, 1.0 * a.cos(), 0.15 * a.sin()], [150.0 * (2.0 * PI * t / 3.2).cos(), 10.0, 0.0])
            }
        };
        let session = |order: &[u8]| {
            imu_from(
                |t| {
                    let k = (t / 90.0) as usize;
                    let ph = t % 90.0;
                    if k < order.len() && ph < 30.0 { mk(order[k], ph) } else { ([0.0, 0.05, 1.0], [0.3, 0.0, 0.0]) }
                },
                order.len() as f64 * 90.0,
            )
        };
        let s1 = sets_and_reps(&session(&[0, 0, 0, 1, 1, 1]));
        let per: Vec<&SetResult> = s1.iter().filter(|s| s.periodic).collect();
        assert_eq!(per.len(), 6, "{s1:?}");
        let c = cluster_sets(&per);
        assert_eq!(c, vec![0, 0, 0, 1, 1, 1], "{c:?}");
        let lib: Vec<LibSet> = per.iter().zip(&c).map(|(s, k)| LibSet { label: if *k == 0 { "développé".into() } else { "curl".into() }, features: s.features.clone(), period: s.period_s }).collect();
        let s2 = sets_and_reps_with(&session(&[1, 0, 1]), &BTreeMap::new(), &|shape| period_for_shape(&lib, shape));
        let names: Vec<String> = s2.iter().filter(|s| s.periodic).map(|s| recognize_exercise(&lib, &s.features).map(|e| e.0).unwrap_or_default()).collect();
        assert_eq!(names, vec!["curl", "développé", "curl"], "{s2:?}");
        for s in s2.iter().filter(|s| s.periodic) {
            assert!((9..=13).contains(&s.reps), "{s:?}");
        }
    }

    #[test]
    #[ignore]
    fn real_gym_session() {
        // GOOSE_SESSION_DIR = whoop-lab/sport/sessions/<id> (imu.csv, r18.csv)
        let Ok(dir) = std::env::var("GOOSE_SESSION_DIR") else { return };
        let mut imu = Imu::default();
        for line in std::fs::read_to_string(format!("{dir}/imu.csv")).unwrap().lines().skip(1) {
            let v: Vec<f64> = line.split(',').map(|x| x.parse().unwrap()).collect();
            imu.t.push(v[0]);
            for k in 0..3 {
                imu.a[k].push(v[1 + k]);
                imu.g[k].push(v[4 + k]);
            }
        }
        let mut secs: BTreeMap<i64, Sec> = BTreeMap::new();
        for line in std::fs::read_to_string(format!("{dir}/r18.csv")).unwrap().lines().skip(1) {
            let c: Vec<&str> = line.split(',').collect();
            let ts: i64 = c[0].parse().unwrap();
            secs.insert(ts, Sec { ts, steps: c[2].parse().ok(), ..Default::default() });
        }
        let t0 = std::time::Instant::now();
        let sets = sets_and_reps_with(&imu, &steps_per_second(&secs), &|_| None);
        println!("{} sets in {:.2} s", sets.len(), t0.elapsed().as_secs_f64());
        let mut agg: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for s in &sets {
            let f = movement_family(s).unwrap_or("?").to_string();
            let h = |t: f64| { let x = (t as i64 + 3600).rem_euclid(86400); format!("{:02}:{:02}:{:02}", x / 3600, x / 60 % 60, x % 60) };
            println!("{}-{} {:5.1}s T{:.2} reps {:3} g({:+.1},{:+.1},{:+.1}) r{:.2} spm{:.1} turn{:.2} {}", h(s.start), h(s.end), s.end - s.start, s.period_s.unwrap_or(0.0), s.reps, s.features[0], s.features[1], s.features[2], s.confidence, s.spm, s.net_turn, f);
            let e = agg.entry(f).or_default();
            e.0 += 1;
            e.1 += s.reps;
        }
        for (k, v) in agg {
            println!("  {k:28} séries {:3} rép {:4}", v.0, v.1);
        }
        let c = cluster_sets(&sets.iter().collect::<Vec<_>>());
        println!("groups {}", c.iter().max().map(|m| m + 1).unwrap_or(0));
    }

    #[test]
    #[ignore]
    fn real_signature_timeline() {
        // GOOSE_SESSION_DIR: replays the strength signature minute by minute on an exported session
        let Ok(dir) = std::env::var("GOOSE_SESSION_DIR") else { return };
        let mut imu = Imu::default();
        for line in std::fs::read_to_string(format!("{dir}/imu.csv")).unwrap().lines().skip(1) {
            let v: Vec<f64> = line.split(',').map(|x| x.parse().unwrap()).collect();
            imu.t.push(v[0]);
            for k in 0..3 { imu.a[k].push(v[1 + k]); imu.g[k].push(v[4 + k]); }
        }
        let mut secs: BTreeMap<i64, Sec> = BTreeMap::new();
        for line in std::fs::read_to_string(format!("{dir}/r18.csv")).unwrap().lines().skip(1) {
            let c: Vec<&str> = line.split(',').collect();
            let ts: i64 = c[0].parse().unwrap();
            secs.insert(ts, Sec { ts, hr: c[1].parse().ok(), steps: c[2].parse().ok(), dyn_g: c[4].parse().ok(), activity: Some(c[8].to_string()).filter(|a| !a.is_empty()), ..Default::default() });
        }
        for line in std::fs::read_to_string(format!("{dir}/live_hr.csv")).unwrap().lines().skip(1) {
            let c: Vec<&str> = line.split(',').collect();
            let ts: i64 = c[0].parse().unwrap();
            if let Ok(h) = c[1].parse::<f64>() { secs.entry(ts).or_insert(Sec { ts, ..Default::default() }).hr.get_or_insert(h); }
        }
        let p = Profile { rest: 52.0, hrmax: 187.0 };
        let (t0, t1) = (imu.t[0] as i64, *imu.t.last().unwrap() as i64);
        let steps = steps_per_second(&secs);
        for now in (t0 + 300..=t1).step_by(60) {
            let w: BTreeMap<i64, Sec> = secs.range(now - 2400..=now).map(|(k, v)| (*k, v.clone())).collect();
            let mins = minutes(&w, &BTreeMap::new());
            let mut win = imu_window(&imu, (now - 900) as f64);
            let k = win.t.partition_point(|t| *t <= now as f64);
            win.t.truncate(k);
            for i in 0..3 { win.a[i].truncate(k); win.g[i].truncate(k); }
            let sig = signature(&mins, &p, &win, &steps, now);
            let k = imu_window(&imu, 0.0);
            let _ = k;
            println!("{} {}", (now + 3600).rem_euclid(86400) / 60, sig);
        }
    }
}
