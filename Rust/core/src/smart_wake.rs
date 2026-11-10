//! Smart wake-up engine: causal real-time sleep-state filter + predictive optimal-stopping alarm.
//!
//! What is actually available live on WHOOP 5.0 (verified on this strap, fw 50.38.1):
//!   - packet 40 at 1 Hz: heart rate + R-R intervals (TOGGLE_REALTIME_HR, keeps the app awake in
//!     the background through bluetooth-central);
//!   - R18 history (gravity, firmware band state, HR, R-R) only through a history sync, i.e. with a
//!     lag equal to the sync interval (the app syncs every 2 min inside the wake window).
//! Not available live: continuous PPG (R26 is bursty), raw IMU (R21 never observed), R10/R11.
//!
//! Model choice. Published wearable stagers (Walch 2019 accel+HR, SleepPPG-Net raw PPG, HRV LSTMs)
//! either need inputs we do not have live (continuous PPG, raw accelerometer at 32-50 Hz) or ship no
//! weights compatible with 1 Hz HR/R-R + lagged gravity. A 4-state hidden Markov model with a
//! forward (causal) filter is the right tool here: it is exact Bayesian filtering, handles missing
//! observations natively (uninformative emission), gives calibrated-by-construction posteriors and
//! runs in microseconds. Emissions are literature-informed z-scored features (HR lower and HRV higher
//! in N3, HR higher and more variable in REM, movement and HR rise in wake), personalised by
//! shrinkage towards the user's own past nights; transitions depend on time asleep (N3 early, REM
//! late) and on the user's habitual wake time (circadian/sleep-pressure proxy).
//!
//! Decision. Expected sleep-inertia cost of waking now (N3 1.0, REM 0.45, light 0.15, awake 0;
//! Tassi & Muzet 2000, Trotti 2017) plus a sleep-loss penalty per minute before the deadline (larger
//! with sleep debt) is compared with the value of waiting, estimated by Monte-Carlo projection of the
//! state posterior through the transition model until the deadline (optimal stopping). Wake fires
//! when waking now is at least as good as waiting, or when the user is already awake. Never fires on
//! stale data except at the deadline. Escalation and wake confirmation are part of the state machine.
//!
//! Everything is recomputed from stored inputs at each call (stateless, restart-safe); predictions use
//! only data timestamped <= now.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::dsp::{mean, median, percentile, round, std_sample};
use crate::hrv_qc::{self, Beat};
use crate::{whoop5_store, GooseResult};

pub const ENGINE_VERSION: &str = "smart-wake.v1";
pub const EPOCH: i64 = 30;
pub const STATES: [&str; 4] = ["wake", "light", "deep", "rem"];
const W: usize = 0;
const L: usize = 1;
const D: usize = 2;
const R: usize = 3;

// ------------------------------------------------------------------ inputs

#[derive(Debug, Clone, Default)]
pub struct Sec {
    pub hr: Option<f64>,
    pub rr: Vec<f64>,
    pub grav: Option<[f64; 3]>,
    pub band: Option<u8>, // 0 wake, 1 still, 2 sleep, 3 up
    pub live: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Feat {
    pub hr: Option<f64>,
    pub ln_rmssd: Option<f64>,
    pub hr_sd: Option<f64>,
    pub movement: Option<f64>,
    pub band: Option<u8>,
    pub live_secs: u32,
}

/// Epoch features, each computed from data inside or before the epoch only.
pub fn features(secs: &BTreeMap<i64, Sec>, t0: i64, n: usize) -> Vec<Feat> {
    let mut out = vec![Feat::default(); n];
    let mut ep_hr: Vec<Option<f64>> = vec![None; n];
    for k in 0..n {
        let (a, b) = (t0 + k as i64 * EPOCH, t0 + (k as i64 + 1) * EPOCH);
        let range: Vec<(&i64, &Sec)> = secs.range(a..b).collect();
        let hrs: Vec<f64> = range.iter().filter_map(|(_, s)| s.hr).filter(|h| *h > 25.0).collect();
        ep_hr[k] = (hrs.len() >= 10).then(|| mean(&hrs).unwrap());
        out[k].hr = ep_hr[k];
        out[k].live_secs = range.iter().filter(|(_, s)| s.live).count() as u32;
        // movement: gravity change above a 10 mg floor (same counts as the offline stager)
        let mut prev: Option<[f64; 3]> = secs.range(a - 5..a).next_back().and_then(|(_, s)| s.grav);
        let (mut cnt, mut gsec) = (0.0, 0);
        for (_, s) in &range {
            if let Some(g) = s.grav {
                if let Some(p) = prev {
                    let dg = ((g[0] - p[0]).powi(2) + (g[1] - p[1]).powi(2) + (g[2] - p[2]).powi(2)).sqrt();
                    cnt += (dg - 0.01).max(0.0) * 1000.0;
                }
                prev = Some(g);
                gsec += 1;
            }
        }
        out[k].movement = (gsec >= 10).then_some(cnt);
        let mut bands = [0u32; 4];
        for (_, s) in &range {
            if let Some(bd) = s.band {
                bands[usize::from(bd.min(3))] += 1;
            }
        }
        let total: u32 = bands.iter().sum();
        if total >= 10 {
            out[k].band = Some(bands.iter().enumerate().max_by_key(|(_, c)| **c).map(|(i, _)| i as u8).unwrap());
        }
        // trailing 5 min HR variability (SD of epoch means) and RMSSD on successive beats
        let lo = k.saturating_sub(9);
        let w: Vec<f64> = ep_hr[lo..=k].iter().flatten().copied().collect();
        // log scale: a single HR spike must not look like an extreme value
        out[k].hr_sd = (w.len() >= 5).then(|| (std_sample(&w).unwrap_or(0.0) + 0.5).ln());
        let mut runs: Vec<Vec<Beat>> = Vec::new();
        let mut last: Option<i64> = None;
        for (ts, s) in secs.range(b - 300..b) {
            if s.rr.is_empty() {
                continue;
            }
            if last.is_none_or(|p| *ts != p + 1 && *ts != p) {
                runs.push(Vec::new());
            }
            last = Some(*ts);
            runs.last_mut().unwrap().extend(s.rr.iter().map(|v| Beat { ts: *ts, rr_ms: *v }));
        }
        let (segs, qc) = hrv_qc::clean(&runs);
        if qc.n_successive_diffs >= 30 {
            let d: Vec<f64> = segs.iter().flat_map(|s| s.windows(2).map(|p| p[1].rr_ms - p[0].rr_ms)).collect();
            let rmssd = (d.iter().map(|x| x * x).sum::<f64>() / d.len() as f64).sqrt();
            out[k].ln_rmssd = (rmssd > 1.0).then(|| rmssd.ln());
        }
    }
    out
}

// ------------------------------------------------------------------ model

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Gauss {
    pub mu: f64,
    pub sd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    /// per state: HR z, ln RMSSD z, HR SD z
    pub hr: [Gauss; 4],
    pub hrv: [Gauss; 4],
    pub hr_sd: [Gauss; 4],
    /// P(movement level none/small/large | state)
    pub movement: [[f64; 3]; 4],
    /// P(band wake/still/sleep/up | state)
    pub band: [[f64; 4]; 4],
    /// inertia cost of waking from each state
    pub cost: [f64; 4],
    pub personalised_nights: usize,
}

const fn g(mu: f64, sd: f64) -> Gauss {
    Gauss { mu, sd }
}

impl Default for Model {
    fn default() -> Self {
        Model {
            hr: [g(1.3, 1.1), g(0.0, 0.8), g(-0.8, 0.6), g(0.4, 0.8)],
            hrv: [g(-0.6, 1.0), g(0.0, 0.8), g(0.7, 0.7), g(-0.4, 0.8)],
            hr_sd: [g(1.0, 1.1), g(0.0, 0.8), g(-0.7, 0.6), g(0.6, 0.9)],
            movement: [[0.30, 0.30, 0.40], [0.75, 0.20, 0.05], [0.92, 0.07, 0.01], [0.90, 0.09, 0.01]],
            band: [[0.45, 0.25, 0.10, 0.20], [0.05, 0.20, 0.74, 0.01], [0.02, 0.10, 0.875, 0.005], [0.03, 0.15, 0.815, 0.005]],
            cost: [0.0, 0.15, 1.0, 0.45],
            personalised_nights: 0,
        }
    }
}

/// Robust likelihood: Gaussian mixed with a broad component (outlier epochs cannot dominate).
fn pdf(x: f64, g: &Gauss) -> f64 {
    let x = x.clamp(-4.0, 4.0);
    let z = (x - g.mu) / g.sd;
    0.9 * (-0.5 * z * z).exp() / g.sd + 0.1 * (-0.5 * (x / 3.0).powi(2)).exp() / 3.0
}

/// Emission tempering: the features are correlated (HR, HRV and HR variability share the same
/// beats), so their naive product is over-confident; 0.6 restores calibration on replayed nights.
pub const EMISSION_TEMPER: f64 = 0.6;

fn movement_level(m: f64) -> usize {
    if m < 1.0 { 0 } else if m < 50.0 { 1 } else { 2 }
}

/// Running robust normaliser (median / scaled MAD of values seen so far), blended with a prior.
struct Norm {
    seen: Vec<f64>,
    prior_mu: f64,
    prior_sd: f64,
}

impl Norm {
    fn z(&mut self, x: Option<f64>) -> Option<f64> {
        let x = x?;
        let n = self.seen.len() as f64;
        let w = n / (n + 40.0);
        let (mu, sd) = if self.seen.len() >= 10 {
            let m = median(&self.seen).unwrap();
            let dev: Vec<f64> = self.seen.iter().map(|v| (v - m).abs()).collect();
            (m, (1.4826 * median(&dev).unwrap()).max(self.prior_sd * 0.6))
        } else {
            (self.prior_mu, self.prior_sd)
        };
        let mu = w * mu + (1.0 - w) * self.prior_mu;
        let sd = w * sd + (1.0 - w) * self.prior_sd;
        self.seen.push(x);
        Some((x - mu) / sd.max(1e-6))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Priors {
    pub hr_mu: f64,
    pub hr_sd: f64,
    pub hrv_mu: f64,
    pub hrv_sd: f64,
    pub hr_sd_mu: f64,
    pub hr_sd_sd: f64,
}

impl Default for Priors {
    fn default() -> Self {
        Priors { hr_mu: 55.0, hr_sd: 5.0, hrv_mu: 50f64.ln(), hrv_sd: 0.35, hr_sd_mu: 2.0f64.ln(), hr_sd_sd: 0.5 }
    }
}

pub fn zscores(f: &[Feat], p: &Priors) -> Vec<[Option<f64>; 3]> {
    let mut a = Norm { seen: vec![], prior_mu: p.hr_mu, prior_sd: p.hr_sd };
    let mut b = Norm { seen: vec![], prior_mu: p.hrv_mu, prior_sd: p.hrv_sd };
    let mut c = Norm { seen: vec![], prior_mu: p.hr_sd_mu, prior_sd: p.hr_sd_sd };
    f.iter().map(|x| [a.z(x.hr), b.z(x.ln_rmssd), c.z(x.hr_sd)]).collect()
}

fn emission(m: &Model, z: &[Option<f64>; 3], f: &Feat) -> [f64; 4] {
    let mut e = [1.0; 4];
    for s in 0..4 {
        if let Some(v) = z[0] {
            e[s] *= pdf(v, &m.hr[s]);
        }
        if let Some(v) = z[1] {
            e[s] *= pdf(v, &m.hrv[s]);
        }
        if let Some(v) = z[2] {
            e[s] *= pdf(v, &m.hr_sd[s]);
        }
        if let Some(v) = f.movement {
            e[s] *= m.movement[s][movement_level(v)];
        }
        if let Some(bd) = f.band {
            e[s] *= m.band[s][usize::from(bd)];
        }
    }
    e.map(|v| v.powf(EMISSION_TEMPER))
}

/// Per-epoch transition matrix; `asleep_h` = hours since first likely sleep, `to_habit_min` =
/// minutes to the habitual wake time (None if unknown).
pub fn transitions(asleep_h: f64, to_habit_min: Option<f64>) -> [[f64; 4]; 4] {
    let deep_drive = 0.035 * (-asleep_h / 2.5).exp() + 0.002;
    let rem_drive = 0.006 + 0.012 * (asleep_h / 3.0).min(2.0);
    let habit = to_habit_min.map_or(1.0, |m| 1.0 + 2.0 * (-(m / 30.0).powi(2)).exp());
    let mut t = [
        [0.0, 0.10, 0.0, 0.004],
        [0.008 * habit, 0.0, deep_drive, rem_drive],
        [0.004, 0.035, 0.0, 0.001],
        [0.012 * habit, 0.028, 0.0, 0.0],
    ];
    for (i, row) in t.iter_mut().enumerate() {
        let out: f64 = row.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, v)| v).sum();
        row[i] = (1.0 - out).max(0.0);
    }
    t
}

fn step(p: &[f64; 4], t: &[[f64; 4]; 4]) -> [f64; 4] {
    let mut q = [0.0; 4];
    for i in 0..4 {
        for j in 0..4 {
            q[j] += p[i] * t[i][j];
        }
    }
    q
}

pub struct Context {
    pub priors: Priors,
    pub model: Model,
    /// local clock minutes of the habitual wake time (0..1440)
    pub habitual_wake_min: Option<f64>,
    pub utc_offset: i64,
}

/// Forward filter; returns the causal posterior of every epoch.
pub fn filter(f: &[Feat], t0: i64, ctx: &Context) -> Vec<[f64; 4]> {
    let z = zscores(f, &ctx.priors);
    let mut p = [0.8, 0.2, 0.0, 0.0];
    let mut onset: Option<usize> = None;
    let mut out = Vec::with_capacity(f.len());
    for k in 0..f.len() {
        let asleep_h = onset.map_or(0.0, |o| (k - o) as f64 * EPOCH as f64 / 3600.0);
        let t = transitions(asleep_h, to_habit(ctx, t0 + k as i64 * EPOCH));
        let prior = if k == 0 { p } else { step(&p, &t) };
        let e = emission(&ctx.model, &z[k], &f[k]);
        let mut post = [0.0; 4];
        for s in 0..4 {
            post[s] = prior[s] * e[s];
        }
        let sum: f64 = post.iter().sum();
        p = if sum > 0.0 && sum.is_finite() { post.map(|v| v / sum) } else { prior };
        // floor to keep the filter responsive after long confident stretches
        p = p.map(|v| v.max(1e-4));
        let s: f64 = p.iter().sum();
        p = p.map(|v| v / s);
        if onset.is_none() && p[W] < 0.3 {
            onset = Some(k);
        }
        out.push(p);
    }
    out
}

fn to_habit(ctx: &Context, ts: i64) -> Option<f64> {
    let h = ctx.habitual_wake_min?;
    let local = ((ts + ctx.utc_offset).rem_euclid(86_400)) as f64 / 60.0;
    let mut d = h - local;
    if d > 720.0 {
        d -= 1440.0;
    }
    if d < -720.0 {
        d += 1440.0;
    }
    Some(d)
}

// ------------------------------------------------------------------ decision

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub fire: bool,
    pub reason: String,
    pub cost_now: f64,
    pub value_wait: f64,
    pub p_light_or_wake_next_10min: f64,
    pub sleep_loss_per_min: f64,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick(&mut self, p: &[f64; 4]) -> usize {
        let r = self.next();
        let mut acc = 0.0;
        for (i, v) in p.iter().enumerate() {
            acc += v;
            if r < acc {
                return i;
            }
        }
        3
    }
}

/// `remaining` epochs until the deadline; `alpha` = sleep-loss cost per minute of waking early.
pub fn decide(post: &[f64; 4], model: &Model, asleep_h: f64, to_habit_min: Option<f64>, remaining: usize, alpha: f64, seed: u64) -> Decision {
    let ec = |p: &[f64; 4]| (0..4).map(|s| p[s] * model.cost[s]).sum::<f64>();
    let minutes_left = remaining as f64 * EPOCH as f64 / 60.0;
    let cost_now = ec(post) + alpha * minutes_left;
    // probability of light/wake within the next 10 min (prediction exposed to the UI)
    let mut q = *post;
    let mut p_good = post[W] + post[L];
    for k in 1..=20 {
        q = step(&q, &transitions(asleep_h + k as f64 / 120.0, to_habit_min.map(|m| m - k as f64 / 2.0)));
        p_good = p_good.max(q[W] + q[L]);
    }
    if remaining == 0 {
        return Decision { fire: true, reason: "deadline".into(), cost_now, value_wait: cost_now, p_light_or_wake_next_10min: p_good, sleep_loss_per_min: alpha };
    }
    // value of waiting = value of a feasible policy simulated on Monte-Carlo paths of the state
    // model: wake at the first future epoch where the state is favourable AND detected (the live
    // detector confirms a favourable state with probability 0.5 per epoch, and wrongly fires in
    // N3/REM with probability 0.03), otherwise at the deadline. Waking now is chosen when it is at
    // least as good as this policy, which keeps the decision realisable (no future knowledge).
    let mut rng = Rng(seed | 1);
    let mats: Vec<[[f64; 4]; 4]> = (1..=remaining).map(|k| transitions(asleep_h + k as f64 / 120.0, to_habit_min.map(|m| m - k as f64 / 2.0))).collect();
    let n_paths = 256;
    let mut total = 0.0;
    for _ in 0..n_paths {
        let mut s = rng.pick(post);
        let mut cost = None;
        for (k, t) in mats.iter().enumerate() {
            s = rng.pick(&t[s]);
            let left = (remaining - k - 1) as f64 * EPOCH as f64 / 60.0;
            let favourable = s == W || s == L;
            let detect = rng.next();
            if (favourable && detect < 0.5) || (!favourable && detect < 0.03) || k + 1 == remaining {
                cost = Some(model.cost[s] + alpha * left);
                break;
            }
        }
        total += cost.unwrap_or(model.cost[s]);
    }
    let value_wait = total / n_paths as f64;
    let fire = cost_now <= value_wait;
    Decision {
        fire,
        reason: if fire { "waking now is at least as good as waiting".into() } else { "a better moment is likely before the deadline".into() },
        cost_now,
        value_wait,
        p_light_or_wake_next_10min: p_good,
        sleep_loss_per_min: alpha,
    }
}

// ------------------------------------------------------------------ storage + session

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionConfig {
    pub id: String,
    /// when the user started the session (bed time)
    pub start_ts: i64,
    /// 0 = automatic: the engine opens the window once the night's sleep need is covered
    #[serde(default)]
    pub window_start: i64,
    pub window_end: i64,
    #[serde(default)]
    pub utc_offset_seconds: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateArgs {
    pub database_path: String,
    pub session: SessionConfig,
    /// live seconds from packet 40 since the last call: [ts, hr, [rr_ms...]]
    #[serde(default)]
    pub live: Vec<(i64, f64, Vec<f64>)>,
    #[serde(default)]
    pub now_unix: Option<i64>,
    /// user acknowledged being awake (app button or strap double tap seen by the app)
    #[serde(default)]
    pub acknowledged: bool,
    /// Swift reports that the last stimulation could not be sent
    #[serde(default)]
    pub stimulation_failed: bool,
}

pub fn ensure_tables(conn: &Connection) -> GooseResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS smart_wake_live (session_id TEXT NOT NULL, ts INTEGER NOT NULL, hr REAL, rr TEXT NOT NULL, PRIMARY KEY (session_id, ts));
        CREATE TABLE IF NOT EXISTS smart_wake_sessions (
            session_id TEXT PRIMARY KEY, config_json TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'armed',
            stimulations_json TEXT NOT NULL DEFAULT '[]', trigger_ts INTEGER, trigger_json TEXT,
            awake_ts INTEGER, feedback_json TEXT, engine_version TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
        );
        CREATE TABLE IF NOT EXISTS smart_wake_log (session_id TEXT NOT NULL, ts INTEGER NOT NULL, epoch_json TEXT NOT NULL, PRIMARY KEY (session_id, ts));
        CREATE TABLE IF NOT EXISTS smart_wake_model (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        "#,
    )?;
    Ok(())
}

/// Seconds from session start (or `from`) to `to`: live packet-40 seconds win over R18 for HR/R-R;
/// gravity and band only come from R18.
pub fn load_secs(conn: &Connection, session_id: Option<&str>, from: i64, to: i64, drop_fraction: f64) -> GooseResult<BTreeMap<i64, Sec>> {
    let mut secs: BTreeMap<i64, Sec> = BTreeMap::new();
    let keep = |ts: i64| drop_fraction <= 0.0 || ((ts.wrapping_mul(2_654_435_761) as u64 % 1000) as f64 / 1000.0) >= drop_fraction;
    {
        let mut st = conn.prepare_cached("SELECT ts, hr, rr, gx, gy, gz, band FROM whoop_lab_biometric WHERE ts >= ?1 AND ts <= ?2")?;
        let mut rows = st.query(params![from, to])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            if !keep(ts) {
                continue;
            }
            let rr: String = r.get(2)?;
            let g = (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?);
            let band: String = r.get(6)?;
            secs.insert(ts, Sec {
                hr: Some(r.get::<_, i64>(1)? as f64).filter(|h| *h > 0.0),
                rr: rr.split(',').filter_map(|v| v.parse().ok()).collect(),
                grav: match g { (Some(x), Some(y), Some(z)) => Some([x as f64 / 1000.0, y as f64 / 1000.0, z as f64 / 1000.0]), _ => None },
                band: match band.as_str() { "wake" => Some(0), "still" => Some(1), "sleep" => Some(2), "up" => Some(3), _ => None },
                live: false,
            });
        }
    }
    if let Some(id) = session_id {
        let mut st = conn.prepare_cached("SELECT ts, hr, rr FROM smart_wake_live WHERE session_id = ?1 AND ts >= ?2 AND ts <= ?3")?;
        let mut rows = st.query(params![id, from, to])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let rr: String = r.get(2)?;
            let e = secs.entry(ts).or_default();
            e.hr = r.get::<_, Option<f64>>(1)?.filter(|h| *h > 0.0).or(e.hr);
            let live_rr: Vec<f64> = rr.split(',').filter_map(|v| v.parse().ok()).collect();
            if !live_rr.is_empty() {
                e.rr = live_rr;
            }
            e.live = true;
        }
    }
    Ok(secs)
}

/// Personal context from past nights (whoop_lab_days): baselines, habitual wake, sleep need/debt,
/// and emission means refitted on the user's own nights (shrinkage towards the default model).
pub fn personal_context(conn: &Connection, before_ts: i64, utc_offset: i64) -> GooseResult<(Context, Value)> {
    ensure_tables(conn)?;
    let mut nights: Vec<Value> = Vec::new();
    if conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'whoop_lab_days'", [], |_| Ok(())).optional()?.is_some() {
        let mut st = conn.prepare("SELECT result_json FROM whoop_lab_days ORDER BY date_key DESC LIMIT 21")?;
        for j in st.query_map([], |r| r.get::<_, String>(0))? {
            if let Ok(v) = serde_json::from_str::<Value>(&j?) {
                let n = v["night"].clone();
                if n["available"] == true && n["wake_time"].as_i64().is_some_and(|w| w < before_ts - 3600) {
                    nights.push(n);
                }
            }
        }
    }
    let wake_mins: Vec<f64> = nights.iter().filter_map(|n| n["wake_time"].as_i64()).map(|t| ((t + utc_offset).rem_euclid(86_400)) as f64 / 60.0).collect();
    let asleep: Vec<f64> = nights.iter().filter_map(|n| n["asleep_min"].as_f64()).collect();
    let need_min = if asleep.len() >= 5 { percentile(&asleep, 75.0).unwrap().clamp(390.0, 570.0) } else { 480.0 };
    let debt_h = asleep.iter().take(7).map(|a| (need_min - a).max(0.0)).sum::<f64>() / 60.0;
    let mut priors = Priors::default();
    let rhr: Vec<f64> = nights.iter().filter_map(|n| n["resting_hr_bpm"].as_f64()).collect();
    let mhr: Vec<f64> = nights.iter().filter_map(|n| n["mean_sleep_hr_bpm"].as_f64()).collect();
    if let (Some(r), Some(m)) = (median(&rhr), median(&mhr)) {
        priors.hr_mu = m;
        priors.hr_sd = ((m - r) * 1.2).clamp(2.5, 8.0);
    }
    let hrv: Vec<f64> = nights.iter().filter_map(|n| n["hrv_night"]["rmssd_ms"].as_f64()).collect();
    if let Some(h) = median(&hrv) {
        priors.hrv_mu = h.ln();
    }
    let mut model = Model::default();
    // personalised emissions: cached per day, refitted from up to 10 past nights
    let key = format!("model:{}", (before_ts + utc_offset).div_euclid(86_400));
    let cached: Option<String> = conn.query_row("SELECT value FROM smart_wake_model WHERE key = ?1", [&key], |r| r.get(0)).optional()?;
    if let Some(m) = cached.and_then(|c| serde_json::from_str::<Model>(&c).ok()) {
        model = m;
    } else {
        model = personalise(conn, &nights, &priors, model)?;
        conn.execute("INSERT OR REPLACE INTO smart_wake_model (key, value) VALUES (?1, ?2)", params![key, serde_json::to_string(&model)?])?;
    }
    let ctx = Context { priors, model, habitual_wake_min: (wake_mins.len() >= 3).then(|| circular_median(&wake_mins)), utc_offset };
    let info = json!({
        "past_nights": nights.len(), "sleep_need_min": need_min,
        "recommended_sleep_min": plan(&PlanArgs { database_path: conn.path().unwrap_or_default().to_string(), latest_wake: before_ts + 8 * 3600, utc_offset_seconds: utc_offset, now_unix: Some(before_ts) }).ok().and_then(|p| p["recommended_sleep_min"].as_f64()), "sleep_debt_h": round(debt_h, 2),
        "habitual_wake_local_min": ctx.habitual_wake_min.map(|v| v.round()),
        "priors": priors, "personalised_nights": ctx.model.personalised_nights,
    });
    Ok((ctx, info))
}

fn circular_median(mins: &[f64]) -> f64 {
    // wake times cluster in the morning; shift by 12 h to avoid the midnight wrap, then unshift
    let shifted: Vec<f64> = mins.iter().map(|m| (m + 720.0) % 1440.0).collect();
    (median(&shifted).unwrap() + 720.0) % 1440.0
}

fn personalise(conn: &Connection, nights: &[Value], priors: &Priors, mut model: Model) -> GooseResult<Model> {
    let mut acc: [[Vec<f64>; 3]; 4] = Default::default();
    let mut used = 0;
    for n in nights.iter().take(10) {
        let (Some(start), Some(code)) = (n["hypnogram_start"].as_i64(), n["hypnogram"].as_str()) else { continue };
        let bed = n["bed_time"].as_i64().unwrap_or(start);
        let secs = load_secs(conn, None, bed - 600, start + code.len() as i64 * EPOCH, 0.0)?;
        let t0 = bed - bed.rem_euclid(EPOCH);
        let nep = ((start + code.len() as i64 * EPOCH - t0) / EPOCH) as usize;
        let f = features(&secs, t0, nep);
        let z = zscores(&f, priors);
        for (k, c) in code.chars().enumerate() {
            let e = ((start - t0) / EPOCH) as usize + k;
            let s = match c { 'W' => W, 'L' => L, 'D' => D, _ => R };
            for (dim, v) in z.get(e).map(|x| *x).unwrap_or([None; 3]).iter().enumerate() {
                if let Some(v) = v {
                    acc[s][dim].push(*v);
                }
            }
        }
        used += 1;
    }
    // shrinkage: n0 = 600 epochs (~5 nights of one state) of prior weight
    for s in 0..4 {
        for (dim, g) in [&mut model.hr[s], &mut model.hrv[s], &mut model.hr_sd[s]].into_iter().enumerate() {
            let v = &acc[s][dim];
            if v.len() < 60 {
                continue;
            }
            let n = v.len() as f64;
            let w = n / (n + 600.0);
            let mu = mean(v).unwrap();
            let sd = std_sample(v).unwrap_or(g.sd).clamp(0.3, 2.0);
            g.mu = w * mu + (1.0 - w) * g.mu;
            g.sd = w * sd + (1.0 - w) * g.sd;
        }
    }
    // inertia costs from feedback (rating 1 = groggy .. 5 = fresh), only with >= 8 rated mornings
    let rated: Vec<([f64; 4], f64)> = {
        let mut st = conn.prepare("SELECT trigger_json, feedback_json FROM smart_wake_sessions WHERE trigger_json IS NOT NULL AND feedback_json IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.filter_map(|r| r.ok())
            .filter_map(|(t, f)| {
                let t: Value = serde_json::from_str(&t).ok()?;
                let f: Value = serde_json::from_str(&f).ok()?;
                let p: Vec<f64> = t["posterior"].as_array()?.iter().filter_map(Value::as_f64).collect();
                Some(([p[0], p[1], p[2], p[3]], f["rating"].as_f64()?))
            })
            .collect()
    };
    if rated.len() >= 8 {
        // per-state mean "badness" (5 - rating)/4 weighted by posterior, shrunk to the prior costs
        for s in 1..4 {
            let wsum: f64 = rated.iter().map(|(p, _)| p[s]).sum();
            if wsum < 1.0 {
                continue;
            }
            let bad = rated.iter().map(|(p, r)| p[s] * (5.0 - r) / 4.0).sum::<f64>() / wsum;
            let w = wsum / (wsum + 8.0);
            model.cost[s] = w * bad + (1.0 - w) * model.cost[s];
        }
    }
    model.personalised_nights = used;
    Ok(model)
}

fn session_row(conn: &Connection, id: &str) -> GooseResult<Option<(String, Vec<(i64, u8)>, Option<i64>, Option<i64>)>> {
    Ok(conn
        .query_row("SELECT state, stimulations_json, trigger_ts, awake_ts FROM smart_wake_sessions WHERE session_id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?
        .map(|(s, j, t, a)| (s, serde_json::from_str(&j).unwrap_or_default(), t, a)))
}

/// One real-time step. Returns the action for the app: none | stimulate{level} | awake | deadline.
pub fn update(args: &UpdateArgs) -> GooseResult<Value> {
    let t_start = std::time::Instant::now();
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_tables(&conn)?;
    let cfg = &args.session;
    let now = args.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    conn.execute(
        "INSERT OR IGNORE INTO smart_wake_sessions (session_id, config_json, engine_version) VALUES (?1, ?2, ?3)",
        params![cfg.id, serde_json::to_string(cfg)?, ENGINE_VERSION],
    )?;
    {
        let tx = conn.unchecked_transaction()?;
        {
            let mut ins = tx.prepare_cached("INSERT OR REPLACE INTO smart_wake_live (session_id, ts, hr, rr) VALUES (?1, ?2, ?3, ?4)")?;
            for (ts, hr, rr) in &args.live {
                if *ts <= now + 5 {
                    ins.execute(params![cfg.id, ts, hr, rr.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(",")])?;
                }
            }
        }
        tx.commit()?;
    }
    // R18 from the last history sync (lagged), through the existing decode path
    crate::whoop_lab_analysis::sync_biometric_table(&conn)?;
    let _ = whoop5_store::ingest(&conn);
    let (ctx, info) = personal_context(&conn, cfg.start_ts, cfg.utc_offset_seconds)?;
    let mut out = step_engine(&conn, cfg, now, &ctx, &info, Some(&cfg.id), 0.0, args.acknowledged, args.stimulation_failed, true)?;
    out["latency_ms"] = json!(round(t_start.elapsed().as_secs_f64() * 1000.0, 2));
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn step_engine(conn: &Connection, cfg: &SessionConfig, now: i64, ctx: &Context, info: &Value, live_id: Option<&str>, drop: f64, acknowledged: bool, stim_failed: bool, persist: bool) -> GooseResult<Value> {
    let t0 = cfg.start_ts - cfg.start_ts.rem_euclid(EPOCH);
    let secs = load_secs(conn, live_id, t0 - 300, now, drop)?;
    let n = ((now - t0) / EPOCH).max(0) as usize;
    if n == 0 {
        return Ok(json!({"action": "none", "reason": "session just started"}));
    }
    let f = features(&secs, t0, n);
    let post = filter(&f, t0, ctx);
    let k = n - 1;
    let p = post[k];
    let onset = post.iter().position(|q| q[W] < 0.3);
    let asleep_h = onset.map_or(0.0, |o| (k - o) as f64 * EPOCH as f64 / 3600.0);
    let last_hr_age = secs.iter().rev().find(|(_, s)| s.hr.is_some()).map(|(t, _)| now - t);
    let fresh = last_hr_age.is_some_and(|a| a <= 120);
    let asleep_min_so_far = post.iter().filter(|q| q[W] < 0.5).count() as f64 / 2.0;
    let need = info["sleep_need_min"].as_f64().unwrap_or(480.0);
    let debt = info["sleep_debt_h"].as_f64().unwrap_or(0.0);
    // sleep-loss penalty: stronger with debt and while still short of the night's need
    let short = ((need - asleep_min_so_far) / 60.0).max(0.0);
    let alpha = 0.004 * (1.0 + debt / 2.0 + short / 2.0);
    let remaining = ((cfg.window_end - now).max(0) / EPOCH) as usize;
    // automatic window: opens when onset + tonight's recommended sleep is reached, at most 45 min
    // and at least 10 min before the deadline (30 min until onset is known)
    let need_tonight = info["recommended_sleep_min"].as_f64().unwrap_or(need);
    let onset_ts = onset.map(|o| t0 + o as i64 * EPOCH);
    let window_start = if cfg.window_start > 0 {
        cfg.window_start
    } else {
        onset_ts.map_or(cfg.window_end - 1800, |o| o + (need_tonight * 60.0) as i64).clamp(cfg.window_end - 2700, cfg.window_end - 600)
    };
    let in_window = now >= window_start;
    // sustained wake: the strap reports "up" (standing / orthostatic) in a recent epoch
    let up_since = |from: usize| f[from.min(k)..=k].iter().any(|x| x.band == Some(3));
    let to_h = to_habit(ctx, now);
    let dec = decide(&p, &ctx.model, asleep_h, to_h, remaining, alpha, now as u64);
    let quality = json!({
        "last_hr_age_s": last_hr_age, "fresh": fresh,
        "live_secs_last_epoch": f[k].live_secs,
        "r18_lag_s": secs.iter().rev().find(|(_, s)| s.grav.is_some()).map(|(t, _)| now - t),
        "features_present": {"hr": f[k].hr.is_some(), "hrv": f[k].ln_rmssd.is_some(), "movement": f[k].movement.is_some(), "band": f[k].band.is_some()},
    });
    let epoch = json!({
        "ts": t0 + k as i64 * EPOCH, "posterior": p.map(|v| round(v, 3)), "features": f[k], "decision": dec,
        "asleep_h": round(asleep_h, 2), "in_window": in_window, "quality": quality,
    });
    // ---- state machine (stimulation, escalation, wake confirmation)
    let (state, mut stims, trigger_ts, _awake) = if persist { session_row(conn, &cfg.id)?.unwrap_or(("armed".into(), vec![], None, None)) } else { ("armed".into(), vec![], None, None) };
    let mut action = json!({"action": "none"});
    let mut new_state = state.clone();
    if stim_failed && !stims.is_empty() {
        stims.pop();
    }
    match state.as_str() {
        "armed" if in_window => {
            let already_awake = post[k.saturating_sub(1)][W] >= 0.75 && p[W] >= 0.75 && fresh;
            if up_since(k.saturating_sub(1)) {
                // already up inside the window: no alarm at all
                new_state = "awake".into();
                action = json!({"action": "awake", "why": "levé détecté avant l'alarme"});
                if persist {
                    conn.execute("UPDATE smart_wake_sessions SET awake_ts = ?2 WHERE session_id = ?1", params![cfg.id, now])?;
                }
            } else if remaining == 0 || (fresh && (dec.fire || already_awake)) {
                let why = if remaining == 0 { "deadline" } else if already_awake { "already awake" } else { "optimal moment" };
                stims.push((now, 1));
                new_state = "stimulating".into();
                action = json!({"action": "stimulate", "level": 1, "why": why});
                if persist {
                    conn.execute("UPDATE smart_wake_sessions SET trigger_ts = ?2, trigger_json = ?3 WHERE session_id = ?1", params![cfg.id, now, json!({"why": why, "posterior": p, "decision": dec}).to_string()])?;
                }
            }
        }
        "stimulating" => {
            let (last_ts, level) = *stims.last().unwrap_or(&(trigger_ts.unwrap_or(now), 1));
            let pre: Vec<f64> = secs.range(last_ts - 300..last_ts).filter_map(|(_, s)| s.hr).collect();
            let post_hr: Vec<f64> = secs.range(now - 60..=now).filter_map(|(_, s)| s.hr).collect();
            // sustained rise only: >= 12 bpm over the last 60 s with >= 40 samples
            let hr_rise = match (median(&pre), median(&post_hr)) { (Some(a), Some(b)) if post_hr.len() >= 40 => Some(b - a), _ => None };
            let stim_epoch = ((last_ts - t0) / EPOCH).max(0) as usize;
            let stood_up = up_since(stim_epoch);
            let woke = acknowledged || stood_up || hr_rise.is_some_and(|d| d >= 12.0) || (p[W] >= 0.8 && post[k.saturating_sub(1)][W] >= 0.8) || double_tap_since(conn, last_ts)?;
            if woke {
                new_state = "awake".into();
                action = json!({"action": "awake", "hr_rise": hr_rise, "acknowledged": acknowledged, "why": if stood_up { "levé (UP) détecté" } else if acknowledged { "confirmé" } else { "réveil détecté" }});
                if persist {
                    conn.execute("UPDATE smart_wake_sessions SET awake_ts = ?2 WHERE session_id = ?1", params![cfg.id, now])?;
                }
            } else if level < 3 && now - last_ts >= 120 {
                stims.push((now, level + 1));
                action = json!({"action": "stimulate", "level": level + 1, "why": "no wake response", "hr_rise": hr_rise});
            } else if now >= cfg.window_end {
                action = json!({"action": "deadline", "why": "strap alarm at deadline takes over"});
            }
        }
        _ => {}
    }
    if persist {
        conn.execute("UPDATE smart_wake_sessions SET state = ?2, stimulations_json = ?3 WHERE session_id = ?1", params![cfg.id, new_state, serde_json::to_string(&stims)?])?;
        conn.execute("INSERT OR REPLACE INTO smart_wake_log (session_id, ts, epoch_json) VALUES (?1, ?2, ?3)", params![cfg.id, now, epoch.to_string()])?;
    }
    let recent: Vec<Value> = post.iter().enumerate().skip(n.saturating_sub(40)).map(|(i, q)| json!([t0 + i as i64 * EPOCH, q.map(|v| round(v, 3))])).collect();
    Ok(json!({
        "engine_version": ENGINE_VERSION,
        "state": new_state,
        "action": action["action"], "level": action.get("level"), "why": action.get("why"),
        "epoch": epoch,
        "window": {"start": window_start, "end": cfg.window_end, "automatic": cfg.window_start == 0, "onset_estimate": onset_ts},
        "recent_posteriors": recent,
        "personal": info,
        "labels": {"posterior": "ESTIMATED (causal HMM)", "p_light_or_wake_next_10min": "PREDICTED", "features": "MEASURED (HR, R-R live; gravity, band from R18 sync)"},
    }))
}

fn double_tap_since(conn: &Connection, ts: i64) -> GooseResult<bool> {
    whoop5_store::ensure_tables(conn)?;
    Ok(conn.query_row("SELECT COUNT(*) FROM whoop5_events WHERE event_id = 14 AND ts >= ?1", [ts], |r| r.get::<_, i64>(0))? > 0)
}

pub fn feedback(conn: &Connection, session_id: &str, rating: f64, note: Option<&str>) -> GooseResult<Value> {
    ensure_tables(conn)?;
    let fb = json!({"rating": rating.clamp(1.0, 5.0), "note": note});
    let n = conn.execute("UPDATE smart_wake_sessions SET feedback_json = ?2 WHERE session_id = ?1", params![session_id, fb.to_string()])?;
    conn.execute("DELETE FROM smart_wake_model", [])?; // refit costs with the new rating
    Ok(json!({"updated": n}))
}

// ------------------------------------------------------------------ bedtime planner

#[derive(Debug, Clone, Deserialize)]
pub struct PlanArgs {
    pub database_path: String,
    /// latest acceptable wake time (unix)
    pub latest_wake: i64,
    #[serde(default)]
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

/// Tonight's recommended sleep, bedtime and expected wake, with every component exposed.
/// Need = personal base (75th percentile of the last 14 nights, 8 h by default)
///      + activity (load of the last 2 days vs personal norm; higher load -> longer recovery sleep)
///      + debt repayment (25 % of the 7-night deficit, max 60 min)
///      + deep-sleep rebound (half of last night's N3 deficit vs personal median, max 20 min).
pub fn plan(args: &PlanArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let now = args.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    let mut days: Vec<Value> = Vec::new();
    if conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'whoop_lab_days'", [], |_| Ok(())).optional()?.is_some() {
        let mut st = conn.prepare("SELECT result_json FROM whoop_lab_days ORDER BY date_key DESC LIMIT 21")?;
        for j in st.query_map([], |r| r.get::<_, String>(0))? {
            if let Ok(v) = serde_json::from_str::<Value>(&j?) {
                days.push(v);
            }
        }
    }
    let nights: Vec<&Value> = days.iter().map(|d| &d["night"]).filter(|n| n["available"] == true).collect();
    let asleep: Vec<f64> = nights.iter().take(14).filter_map(|n| n["asleep_min"].as_f64()).collect();
    let (base, base_src) = if asleep.len() >= 5 { (percentile(&asleep, 75.0).unwrap().clamp(420.0, 540.0), "personnel (p75 des 14 dernières nuits)") } else { (480.0, "par défaut (8 h, moins de 5 nuits)") };
    // activity
    let loads: Vec<f64> = days.iter().filter_map(|d| d["day"]["edwards_trimp"].as_f64()).collect();
    let recent = loads.iter().take(2).copied().fold(0.0f64, f64::max);
    let activity_add = if loads.len() >= 5 {
        let m = mean(&loads).unwrap();
        let sd = std_sample(&loads).unwrap_or(10.0).max(10.0);
        (15.0 * (recent - m) / sd).clamp(0.0, 45.0)
    } else {
        ((recent - 40.0) * 0.25).clamp(0.0, 45.0)
    };
    // debt
    let debt_min: f64 = asleep.iter().take(7).map(|a| (base - a).max(0.0)).sum();
    let debt_add = (0.25 * debt_min).min(60.0);
    // deep rebound
    let deeps: Vec<f64> = nights.iter().filter_map(|n| n["stages_min"]["deep"].as_f64()).collect();
    let last_deep = deeps.first().copied();
    let deep_ref = if deeps.len() >= 4 { median(&deeps[1..]).unwrap() } else { 60.0 };
    let deep_add = last_deep.map_or(0.0, |d| (0.5 * (deep_ref - d)).clamp(0.0, 20.0));
    let recommended = (base + activity_add + debt_add + deep_add).clamp(420.0, 600.0).round();
    let lat: Vec<f64> = nights.iter().filter_map(|n| n["latency_min"].as_f64()).collect();
    let latency = median(&lat).unwrap_or(15.0).clamp(5.0, 45.0).round();
    let bedtime = args.latest_wake - ((recommended + latency) * 60.0) as i64;
    // circadian check: habitual bedtime (median of past bed times, local clock)
    let off = args.utc_offset_seconds;
    let clock = |t: i64| ((t + off).rem_euclid(86_400) + 43_200) % 86_400; // noon-shifted seconds
    let beds: Vec<f64> = nights.iter().filter_map(|n| n["bed_time"].as_i64()).map(|t| clock(t) as f64).collect();
    let habitual_bed = (beds.len() >= 3).then(|| median(&beds).unwrap());
    let early_by_min = habitual_bed.map(|h| (h - clock(bedtime) as f64) / 60.0);
    // expected wake if the plan is followed: the automatic window opens at onset + need
    let onset = bedtime + (latency * 60.0) as i64;
    let win_start = (onset + (recommended * 60.0) as i64).clamp(args.latest_wake - 2700, args.latest_wake - 600);
    // if going to bed now
    let now_sleep = ((args.latest_wake - now) as f64 / 60.0 - latency).max(0.0);
    let now_win = (now + (latency * 60.0) as i64 + (recommended * 60.0) as i64).clamp(args.latest_wake - 2700, args.latest_wake - 600);
    Ok(json!({
        "recommended_sleep_min": recommended,
        "components": {
            "base_min": base.round(), "base_source": base_src,
            "activity_min": activity_add.round(), "recent_load_trimp": round(recent, 1),
            "debt_min": debt_add.round(), "debt_7d_min": debt_min.round(),
            "deep_min": deep_add.round(), "last_deep_min": last_deep, "deep_reference_min": round(deep_ref, 0),
        },
        "latency_min": latency,
        "bedtime": bedtime,
        "lights_out_advice": bedtime - 15 * 60,
        "habitual_bedtime_offset_min": early_by_min.map(|v| v.round()),
        "circadian_note": early_by_min.filter(|v| *v > 90.0).map(|_| "Plus de 1 h 30 avant ton coucher habituel : l'endormissement risque d'être difficile, avance progressivement."),
        "expected_wake": {"window_start": win_start, "window_end": args.latest_wake, "most_likely": (win_start + args.latest_wake) / 2},
        "if_bed_now": {"sleep_possible_min": now_sleep.round(), "deficit_min": (recommended - now_sleep).max(0.0).round(), "window_start": now_win},
        "nights_used": nights.len(),
        "recent_nights": nights.iter().take(7).map(|n| json!({"wake_time": n["wake_time"], "asleep_min": n["asleep_min"], "deep_min": n["stages_min"]["deep"], "rem_min": n["stages_min"]["rem"], "efficiency_pct": n["efficiency_pct"]})).collect::<Vec<_>>(),
        "status": "EXPERIMENTAL",
        "method": "besoin de base personnel + activité + 25 % de la dette + rebond de sommeil profond ; voir docs/SMART_WAKE.md",
    }))
}

// ------------------------------------------------------------------ replay / evaluation

#[derive(Debug, Clone, Deserialize)]
pub struct ReplayArgs {
    pub database_path: String,
    #[serde(default)]
    pub utc_offset_seconds: i64,
    /// window length before each night's actual wake time (default 30 min)
    #[serde(default)]
    pub window_min: Option<i64>,
    /// fractions of seconds dropped to test robustness (default [0, 0.3, 0.6])
    #[serde(default)]
    pub drop_fractions: Option<Vec<f64>>,
    /// optional external ground truth (e.g. PSG): [[ts, "wake|light|deep|rem"], ...]
    #[serde(default)]
    pub labels: Option<Vec<(i64, String)>>,
}

/// Replays past nights causally (R18 used as the live source, one step per epoch) and scores the
/// engine: latency, calibration against the reference labels (offline hypnogram or PSG), stage at
/// the chosen wake time, minutes of sleep given up, robustness to missing data.
pub fn replay(args: &ReplayArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_tables(&conn)?;
    crate::whoop_lab_analysis::sync_biometric_table(&conn)?;
    let mut nights: Vec<Value> = Vec::new();
    {
        let mut st = conn.prepare("SELECT result_json FROM whoop_lab_days ORDER BY date_key")?;
        for j in st.query_map([], |r| r.get::<_, String>(0))? {
            if let Ok(v) = serde_json::from_str::<Value>(&j?) {
                if v["night"]["available"] == true {
                    nights.push(v["night"].clone());
                }
            }
        }
    }
    let window = args.window_min.unwrap_or(30) * 60;
    let mut report = Vec::new();
    for n in &nights {
        let (Some(bed), Some(wake), Some(hstart), Some(code)) = (n["bed_time"].as_i64(), n["wake_time"].as_i64(), n["hypnogram_start"].as_i64(), n["hypnogram"].as_str()) else { continue };
        let reference = |ts: i64| -> Option<usize> {
            if let Some(l) = &args.labels {
                return l.iter().rev().find(|(t, _)| *t <= ts && ts - *t < EPOCH).and_then(|(_, s)| STATES.iter().position(|x| x == s));
            }
            if ts < hstart {
                return Some(W);
            }
            code.chars().nth(((ts - hstart) / EPOCH) as usize).map(|c| match c { 'W' => W, 'L' => L, 'D' => D, _ => R })
        };
        let (ctx, info) = personal_context(&conn, bed, args.utc_offset_seconds)?;
        let cfg = SessionConfig { id: format!("replay-{bed}"), start_ts: bed - 600, window_start: wake - window, window_end: wake, utc_offset_seconds: args.utc_offset_seconds };
        let mut per_drop = Vec::new();
        for &drop in args.drop_fractions.as_deref().unwrap_or(&[0.0, 0.3, 0.6]) {
            // causal posteriors: filter on data <= t only (the filter is causal by construction, so a
            // single pass equals stepping epoch by epoch; latency is measured on a true step)
            let t0 = cfg.start_ts - cfg.start_ts.rem_euclid(EPOCH);
            let secs = load_secs(&conn, None, t0 - 300, wake, drop)?;
            let nep = ((wake - t0) / EPOCH) as usize;
            let tl = std::time::Instant::now();
            let f = features(&secs, t0, nep);
            let post = filter(&f, t0, &ctx);
            let full_ms = tl.elapsed().as_secs_f64() * 1000.0;
            // calibration vs reference
            let (mut brier, mut nll, mut hits, mut count) = (0.0, 0.0, 0usize, 0usize);
            let mut bins = [[0.0f64; 2]; 10];
            let mut confusion = [[0usize; 4]; 4];
            let mut mean_post = [[0.0f64; 4]; 4];
            for (k, p) in post.iter().enumerate() {
                let Some(r) = reference(t0 + k as i64 * EPOCH) else { continue };
                count += 1;
                let am = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| x.0).unwrap();
                confusion[r][am] += 1;
                for s in 0..4 {
                    mean_post[r][s] += p[s];
                }
                for (s, v) in p.iter().enumerate() {
                    let y = if s == r { 1.0 } else { 0.0 };
                    brier += (v - y).powi(2);
                    let b = ((v * 10.0) as usize).min(9);
                    bins[b][0] += v;
                    bins[b][1] += y;
                }
                nll -= p[r].max(1e-6).ln();
                if p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| x.0) == Some(r) {
                    hits += 1;
                }
            }
            // decision simulation inside the window
            let mut trigger: Option<(i64, usize, [f64; 4])> = None;
            let mut step_ms = Vec::new();
            let mut now = cfg.window_start;
            while now <= wake && trigger.is_none() {
                let k = ((now - t0) / EPOCH) as usize - 1;
                let onset = post.iter().position(|q| q[W] < 0.3);
                let asleep_h = onset.map_or(0.0, |o| k.saturating_sub(o) as f64 / 120.0);
                let ts = std::time::Instant::now();
                let dec = decide(&post[k], &ctx.model, asleep_h, to_habit(&ctx, now), ((wake - now) / EPOCH) as usize, 0.004, now as u64);
                step_ms.push(ts.elapsed().as_secs_f64() * 1000.0);
                let awake = post[k][W] >= 0.75 && post[k.saturating_sub(1)][W] >= 0.75;
                if dec.fire || awake {
                    trigger = Some((now, k, post[k]));
                }
                now += EPOCH;
            }
            let (tt, tk, tp) = trigger.unwrap_or((wake, nep - 1, post[nep - 1]));
            let ref_stage = reference(tt);
            // what a fixed alarm at the deadline would have hit, and the window's reference stages
            let win_stages: Vec<usize> = (cfg.window_start..=wake).step_by(EPOCH as usize).filter_map(reference).collect();
            let share = |s: usize| round(win_stages.iter().filter(|x| **x == s).count() as f64 / win_stages.len().max(1) as f64, 2);
            per_drop.push(json!({
                "drop_fraction": drop,
                "epochs": count, "accuracy_vs_reference": round(hits as f64 / count.max(1) as f64, 3),
                "brier": round(brier / count.max(1) as f64, 3), "log_loss": round(nll / count.max(1) as f64, 3),
                "confusion_ref_rows_wake_light_deep_rem": confusion,
                "mean_posterior_by_reference": mean_post.iter().zip(confusion.iter()).map(|(m, c)| { let n = c.iter().sum::<usize>().max(1) as f64; m.map(|v| round(v / n, 2)) }).collect::<Vec<_>>(),
                "reliability": bins.iter().filter(|b| b[0] > 0.0).map(|b| json!([round(b[0], 1), round(b[1], 1)])).collect::<Vec<_>>(),
                "trigger_ts": tt, "minutes_before_deadline": round((wake - tt) as f64 / 60.0, 1),
                "posterior_at_trigger": tp.map(|v| round(v, 3)),
                "reference_stage_at_trigger": ref_stage.map(|s| STATES[s]),
                "reference_stage_at_deadline": reference(wake).map(|s| STATES[s]),
                "window_reference_share": {"wake": share(W), "light": share(L), "deep": share(D), "rem": share(R)},
                "latency_full_night_filter_ms": round(full_ms, 2),
                "latency_decision_ms_p95": percentile(&step_ms, 95.0).map(|v| round(v, 3)),
                "epoch_index": tk,
            }));
        }
        report.push(json!({"bed_time": bed, "wake_time": wake, "personal": info, "runs": per_drop}));
    }
    Ok(json!({
        "engine_version": ENGINE_VERSION,
        "reference": if args.labels.is_some() { "external labels" } else { "offline whole-night hypnogram (heuristic, not PSG)" },
        "nights": report,
        "note": "Awakening quality is only measurable with morning feedback (smart_wake.feedback).",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs_night(minutes: i64, pattern: impl Fn(i64) -> (f64, f64, bool)) -> BTreeMap<i64, Sec> {
        let mut m = BTreeMap::new();
        for t in 0..minutes * 60 {
            let (hr, rr_jitter, moving) = pattern(t);
            let rr = 60_000.0 / hr + if t % 2 == 0 { rr_jitter } else { -rr_jitter };
            m.insert(1_000_000 + t, Sec {
                hr: Some(hr),
                rr: vec![rr],
                grav: Some(if moving && t % 3 == 0 { [0.3, 0.6, -0.7] } else { [0.0, 0.6, -0.8] }),
                band: Some(if moving { 0 } else { 2 }),
                live: true,
            });
        }
        m
    }

    #[test]
    fn transitions_are_stochastic() {
        for h in [0.0, 2.0, 7.0] {
            for row in transitions(h, Some(5.0)) {
                assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-9);
                assert!(row.iter().all(|v| *v >= 0.0));
            }
        }
    }

    #[test]
    fn filter_tracks_deep_then_wake() {
        // 40 min awake-ish moving, 60 min deep-like (low HR, high HRV), 20 min awake moving
        let s = secs_night(120, |t| if t < 2400 { (70.0, 15.0, true) } else if t < 6000 { (50.0, 45.0, false) } else { (75.0, 10.0, true) });
        let f = features(&s, 1_000_000, 240);
        let ctx = Context { priors: Priors::default(), model: Model::default(), habitual_wake_min: None, utc_offset: 0 };
        let p = filter(&f, 1_000_000, &ctx);
        assert!(p[60][W] > 0.5, "{:?}", p[60]);
        assert!(p[180][D] + p[180][L] > 0.7, "{:?}", p[180]);
        assert!(p[235][W] > 0.6, "{:?}", p[235]);
    }

    #[test]
    fn decision_waits_in_deep_and_fires_when_awake_or_at_deadline() {
        let m = Model::default();
        let deep = decide(&[0.01, 0.05, 0.9, 0.04], &m, 3.0, None, 50, 0.004, 7);
        assert!(!deep.fire);
        let light = decide(&[0.2, 0.75, 0.02, 0.03], &m, 6.0, None, 10, 0.004, 7);
        assert!(light.fire, "{light:?}");
        assert!(decide(&[0.0, 0.0, 1.0, 0.0], &m, 3.0, None, 0, 0.004, 7).fire);
    }

    #[test]
    fn missing_data_keeps_prior_and_never_panics() {
        let mut s = secs_night(30, |_| (55.0, 30.0, false));
        s.retain(|t, _| t % 7 == 0);
        let f = features(&s, 1_000_000, 60);
        let ctx = Context { priors: Priors::default(), model: Model::default(), habitual_wake_min: Some(420.0), utc_offset: 0 };
        let p = filter(&f, 1_000_000, &ctx);
        assert_eq!(p.len(), 60);
        assert!(p.iter().all(|q| (q.iter().sum::<f64>() - 1.0).abs() < 1e-9));
    }
}

#[cfg(test)]
mod real_data {
    /// GOOSE_DB=/path/copy.sqlite cargo test --release --lib smart_wake_replay -- --ignored --nocapture
    #[test]
    #[ignore]
    fn smart_wake_replay() {
        let Ok(db) = std::env::var("GOOSE_DB") else { return };
        let r = super::replay(&super::ReplayArgs { database_path: db, utc_offset_seconds: 3600, window_min: Some(30), drop_fractions: None, labels: None }).unwrap();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}

#[cfg(test)]
mod debug_dump {
    use super::*;
    #[test]
    #[ignore]
    fn smart_wake_dump() {
        let Ok(db) = std::env::var("GOOSE_DB") else { return };
        let conn = Connection::open(db).unwrap();
        let j: String = conn.query_row("SELECT result_json FROM whoop_lab_days WHERE result_json LIKE '%\"available\":true%' ORDER BY date_key DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        let n: Value = serde_json::from_str::<Value>(&j).unwrap()["night"].clone();
        let (bed, start, code) = (n["bed_time"].as_i64().unwrap(), n["hypnogram_start"].as_i64().unwrap(), n["hypnogram"].as_str().unwrap().to_string());
        let t0 = (bed - 600) - (bed - 600).rem_euclid(EPOCH);
        let end = start + code.len() as i64 * EPOCH;
        let secs = load_secs(&conn, None, t0 - 300, end, 0.0).unwrap();
        let nep = ((end - t0) / EPOCH) as usize;
        let f = features(&secs, t0, nep);
        let ctx = Context { priors: Priors::default(), model: Model::default(), habitual_wake_min: None, utc_offset: 3600 };
        let z = zscores(&f, &ctx.priors);
        let p = filter(&f, t0, &ctx);
        for k in (0..nep).step_by(10) {
            let ts = t0 + k as i64 * EPOCH;
            let r = if ts < start { 'W' } else { code.chars().nth(((ts - start) / EPOCH) as usize).unwrap_or('?') };
            println!("{k:4} ref {r} post {:?} z {:?} mov {:?} band {:?} hr {:?}", p[k].map(|v| (v * 100.0).round()), z[k].map(|v| v.map(|x| (x * 10.0).round() / 10.0)), f[k].movement.map(|m| m.round()), f[k].band, f[k].hr.map(|h| h.round()));
        }
    }
}

#[cfg(test)]
mod plan_real {
    #[test]
    #[ignore]
    fn smart_wake_plan_real() {
        let Ok(db) = std::env::var("GOOSE_DB") else { return };
        let r = super::plan(&super::PlanArgs { database_path: db, latest_wake: 1_794_376_800, utc_offset_seconds: 3600, now_unix: Some(1_794_330_000) }).unwrap();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}

#[cfg(test)]
mod e2e {
    //! End-to-end: the real `update` entry point on a temporary database, called like the app does.
    use super::*;

    const DEADLINE: i64 = 1_794_376_800; // 2026-11-11 07:00 (UTC+1)
    const BED: i64 = DEADLINE - 9 * 3600; // 22:00

    #[derive(Clone, Copy)]
    enum St { Awake, Light, Deep, Rem, Up }

    fn db() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.sqlite").to_string_lossy().to_string();
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE decoded_frames (frame_id TEXT, payload_hex TEXT, header_crc_valid INTEGER, payload_crc_valid INTEGER, packet_type INTEGER, raw_len INTEGER);").unwrap();
        crate::whoop_lab_analysis::sync_biometric_table(&c).unwrap();
        (d, p)
    }

    /// Writes 1 Hz R18-like rows for [from, to) with the state given by `st(t)`.
    fn write(path: &str, from: i64, to: i64, st: &dyn Fn(i64) -> St) {
        let c = Connection::open(path).unwrap();
        let tx = c.unchecked_transaction().unwrap();
        for t in from..to {
            let s = st(t);
            let (hr, jitter, band, moving) = match s {
                St::Awake => (66.0, 12.0, "wake", t % 4 == 0),
                St::Light => (52.0, 22.0, "sleep", t % 97 == 0),
                St::Deep => (47.0, 38.0, "sleep", false),
                St::Rem => (55.0, 14.0, "sleep", false),
                St::Up => (82.0, 8.0, "up", t % 2 == 0),
            };
            let wobble = if matches!(s, St::Rem) { 4.0 * ((t as f64) / 40.0).sin() } else { 0.0 };
            let hr = hr + wobble;
            let rr = 60_000.0 / hr + if t % 2 == 0 { jitter } else { -jitter };
            let (gx, gy, gz) = if moving { (300, 600, -700) } else { (0, 600, -800) };
            tx.execute("INSERT OR REPLACE INTO whoop_lab_biometric VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 'still', 3400, ?7)", params![t, hr.round() as i64, format!("{rr:.0}"), gx, gy, gz, band]).unwrap();
        }
        tx.commit().unwrap();
    }

    fn call(path: &str, now: i64, ack: bool) -> Value {
        update(&UpdateArgs {
            database_path: path.into(),
            session: SessionConfig { id: "s1".into(), start_ts: BED, window_start: 0, window_end: DEADLINE, utc_offset_seconds: 3600 },
            live: vec![],
            now_unix: Some(now),
            acknowledged: ack,
            stimulation_failed: false,
        })
        .unwrap()
    }

    /// Steps the engine like the app (10 min before the window, 30 s inside); returns (ts, response)
    /// of the first non-"none" action.
    fn run_until_action(path: &str, from: i64, to: i64) -> Option<(i64, Value)> {
        let mut now = from;
        while now <= to {
            let r = call(path, now, false);
            if r["action"] != "none" {
                return Some((now, r));
            }
            let ws = r["window"]["start"].as_i64().unwrap_or(DEADLINE - 1800);
            now += if now < ws - 600 { 600 } else { 30 };
        }
        None
    }

    fn night(t: i64) -> St {
        let m = (t - BED) / 60;
        match m {
            0..=19 => St::Awake,
            20..=79 => St::Deep,
            _ => match (m - 80) % 90 { 0..=44 => St::Light, 45..=64 => St::Deep, _ => St::Rem },
        }
    }

    #[test]
    fn wakes_in_window_on_light_sleep_then_stops_on_up() {
        let (_d, p) = db();
        // night, then deep until 06:35 and light from 06:35
        write(&p, BED - 600, DEADLINE + 600, &|t| if t >= DEADLINE - 25 * 60 { St::Light } else if t >= DEADLINE - 60 * 60 { St::Deep } else { night(t) });
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE).expect("an action");
        let ws = r["window"]["start"].as_i64().unwrap();
        assert!(r["window"]["automatic"] == true);
        // window = estimated onset (~22:20) + 8 h default need, within [deadline-45, deadline-10] min
        let onset = r["window"]["onset_estimate"].as_i64().unwrap();
        assert!((onset - (BED + 20 * 60)).abs() <= 120, "onset {} min after bed", (onset - BED) / 60);
        assert_eq!(ws, (onset + 480 * 60).clamp(DEADLINE - 2700, DEADLINE - 600), "{}", r["window"]);
        assert_eq!(r["action"], "stimulate", "{r}");
        assert!(ts >= DEADLINE - 25 * 60 && ts < DEADLINE, "fired at {} min before deadline", (DEADLINE - ts) / 60);
        assert_ne!(r["why"], "deadline");
        // user stands up 90 s after the vibration
        write(&p, ts + 60, ts + 400, &|_| St::Up);
        let r2 = call(&p, ts + 150, false);
        assert_eq!(r2["action"], "awake", "{r2}");
        assert_eq!(r2["why"], "levé (UP) détecté");
        assert!(r2["latency_ms"].as_f64().unwrap() < 2000.0);
    }

    #[test]
    fn deep_sleep_to_the_end_waits_for_the_deadline_and_escalates() {
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 900, &|t| if t >= DEADLINE - 60 * 60 { St::Deep } else { night(t) });
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE + 60).expect("an action");
        assert_eq!(r["action"], "stimulate");
        assert!(ts >= DEADLINE - 120, "should wait for the deadline, fired {} min early: {r}", (DEADLINE - ts) / 60);
        // no reaction: level 2 after 120 s, level 3 after another 120 s
        let r2 = call(&p, ts + 125, false);
        assert_eq!((r2["action"].clone(), r2["level"].clone()), (json!("stimulate"), json!(2)), "{r2}");
        let r3 = call(&p, ts + 250, false);
        assert_eq!(r3["level"], 3, "{r3}");
        let r4 = call(&p, ts + 300, true);
        assert_eq!(r4["action"], "awake");
    }

    #[test]
    fn already_up_in_window_gets_no_alarm_but_night_bathroom_does_not_cancel() {
        let (_d, p) = db();
        // 03:00 bathroom trip (outside the window), then deep; up at 06:30 inside the window
        write(&p, BED - 600, DEADLINE + 600, &|t| {
            let m = (t - BED) / 60;
            if (300..305).contains(&m) { St::Up } else if t >= DEADLINE - 30 * 60 { St::Up } else if t >= DEADLINE - 60 * 60 { St::Deep } else { night(t) }
        });
        let r_night = call(&p, BED + 306 * 60, false);
        assert_eq!(r_night["action"], "none", "bathroom trip must not end the session: {r_night}");
        let (ts, r) = run_until_action(&p, BED + 310 * 60, DEADLINE).expect("an action");
        assert_eq!(r["action"], "awake", "{r}");
        assert!(ts >= DEADLINE - 30 * 60);
    }

    #[test]
    fn stale_data_never_fires_before_the_deadline() {
        let (_d, p) = db();
        // data stops at 06:00 (strap off / disconnected): only the deadline may fire
        write(&p, BED - 600, DEADLINE - 3600, &night);
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE + 60).expect("deadline");
        assert_eq!(r["why"], "deadline", "{r}");
        assert!(ts >= DEADLINE);
        assert_eq!(r["epoch"]["quality"]["fresh"], false);
    }

    #[test]
    fn planner_components_add_up() {
        let (_d, p) = db();
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE IF NOT EXISTS whoop_lab_days (date_key TEXT PRIMARY KEY, input_count INTEGER, input_last_ts INTEGER, algorithm_version TEXT, result_json TEXT, updated_at TEXT)").unwrap();
        for i in 0..7 {
            let wake = DEADLINE - (i + 1) * 86_400;
            let night = json!({"available": true, "asleep_min": 420.0, "latency_min": 20.0, "bed_time": wake - 8 * 3600, "wake_time": wake,
                               "stages_min": {"deep": if i == 0 { 30.0 } else { 70.0 }, "rem": 90.0}, "efficiency_pct": 88.0});
            let day = json!({"edwards_trimp": if i == 0 { 120.0 } else { 40.0 }});
            c.execute("INSERT INTO whoop_lab_days VALUES (?1, 0, 0, '4', ?2, '')", params![format!("2026-11-{:02}", 10 - i), json!({"night": night, "day": day}).to_string()]).unwrap();
        }
        let r = plan(&PlanArgs { database_path: p, latest_wake: DEADLINE, utc_offset_seconds: 3600, now_unix: Some(BED) }).unwrap();
        let comp = &r["components"];
        // base: p75 of 420 = 420; debt 7 x (420-420) = 0; deep: (70-30)/2 = 20; activity > 0
        assert_eq!(comp["base_min"], 420.0);
        assert_eq!(comp["deep_min"], 20.0);
        assert_eq!(comp["debt_min"], 0.0);
        assert!(comp["activity_min"].as_f64().unwrap() > 0.0);
        let rec = r["recommended_sleep_min"].as_f64().unwrap();
        let sum = comp["base_min"].as_f64().unwrap() + comp["activity_min"].as_f64().unwrap() + comp["debt_min"].as_f64().unwrap() + comp["deep_min"].as_f64().unwrap();
        assert!((rec - sum).abs() <= 1.0, "{rec} vs {sum}");
        assert_eq!(r["bedtime"].as_i64().unwrap(), DEADLINE - ((rec + 20.0) * 60.0) as i64);
        assert_eq!(r["latency_min"], 20.0);
    }
}

#[cfg(test)]
mod latency_real {
    #[test]
    #[ignore]
    fn smart_wake_latency_real() {
        let Ok(db) = std::env::var("GOOSE_DB") else { return };
        // real night 2026-11-09 -> 10: bed ~23:00, wake 08:35 (from the analysis)
        let wake = 1_794_299_727;
        for (label, now) in [("mid-night", wake - 4 * 3600), ("window", wake - 600), ("deadline", wake)] {
            let t = std::time::Instant::now();
            let r = super::update(&super::UpdateArgs {
                database_path: db.clone(),
                session: super::SessionConfig { id: format!("lat-{label}"), start_ts: wake - 10 * 3600, window_start: 0, window_end: wake, utc_offset_seconds: 3600 },
                live: vec![], now_unix: Some(now), acknowledged: false, stimulation_failed: false,
            }).unwrap();
            println!("{label}: wall {:?} engine {} ms action {} post {} window {} fresh {}", t.elapsed(), r["latency_ms"], r["action"], r["epoch"]["posterior"], r["window"], r["epoch"]["quality"]["fresh"]);
        }
    }
}
