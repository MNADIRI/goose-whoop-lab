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
    /// live IMU (packet 43): SD of |a| within the second (g)
    pub imu_dyn: Option<f64>,
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
    /// P(at least one light-or-wake epoch within the next 10 min), i.e. a first-passage
    /// probability of the transition model, not the maximum of the marginals.
    pub p_reach_light_or_wake_10min: f64,
    pub sleep_loss_per_min: f64,
}

/// Versioned decision parameters (heuristic choices, not validated on Goose users; see
/// docs/SMART_WAKE.md). Exposed in every engine response so a log can be re-read with its policy.
pub const POLICY_VERSION: &str = "duration-first.v1";
/// Waking now must beat waiting by at least this expected-cost margin.
pub const MIN_BENEFIT: f64 = 0.003;
/// Favourable state (P(light) + P(wake)) required on each of the last `PERSIST_EPOCHS` epochs.
pub const FAVOURABLE_MIN: f64 = 0.6;
pub const PERSIST_EPOCHS: usize = 2;
/// 7-night sleep debt (h) above which the engine never anticipates.
pub const DEBT_HOLD_H: f64 = 3.0;
/// Observed share of the night (and of the last 30 min) below which anticipation is withheld;
/// 0.9 is the same threshold that makes a night `INCOMPLETE_LOWER_BOUND`.
pub const MIN_NIGHT_COVERAGE: f64 = 0.9;
pub const MIN_RECENT_COVERAGE: f64 = 0.8;
/// Default and largest user-selectable anticipation before the deadline (minutes).
pub const DEFAULT_MAX_ADVANCE_MIN: i64 = 30;
pub const MAX_ADVANCE_LIMIT_MIN: i64 = 60;

fn favourable(p: &[f64; 4]) -> f64 {
    p[W] + p[L]
}

/// First-passage probability: mass that visits wake or light at least once in `epochs` epochs.
pub fn reach_light_or_wake(post: &[f64; 4], asleep_h: f64, to_habit_min: Option<f64>, epochs: usize) -> f64 {
    let mut hit = favourable(post);
    let mut q = [0.0, 0.0, post[D], post[R]];
    for k in 1..=epochs {
        q = step(&q, &transitions(asleep_h + k as f64 / 120.0, to_habit_min.map(|m| m - k as f64 / 2.0)));
        hit += q[W] + q[L];
        q[W] = 0.0;
        q[L] = 0.0;
    }
    hit.clamp(0.0, 1.0)
}

/// `remaining` epochs until the deadline; `alpha` = sleep-loss cost per minute of waking early.
pub fn decide(post: &[f64; 4], model: &Model, asleep_h: f64, to_habit_min: Option<f64>, remaining: usize, alpha: f64) -> Decision {
    let ec = |p: &[f64; 4]| (0..4).map(|s| p[s] * model.cost[s]).sum::<f64>();
    let minutes_left = remaining as f64 * EPOCH as f64 / 60.0;
    let cost_now = ec(post) + alpha * minutes_left;
    let p_reach = reach_light_or_wake(post, asleep_h, to_habit_min, 20);
    if remaining == 0 {
        return Decision { fire: true, reason: "deadline".into(), cost_now, value_wait: cost_now, p_reach_light_or_wake_10min: p_reach, sleep_loss_per_min: alpha };
    }
    // value of waiting = expected cost of a feasible policy: wake at the first future epoch where
    // the state is favourable AND detected (detection 0.5 per epoch in wake/light, false alarm 0.03
    // in N3/REM), otherwise at the deadline. Evaluated exactly by propagating the not-yet-stopped
    // probability mass through the transition model (deterministic: no Monte-Carlo noise, so two
    // successive calls on the same data give the same answer). The frame is this model only; it
    // says nothing about the true benefit of an early wake.
    let mut q = *post;
    let mut value_wait = 0.0;
    for k in 1..=remaining {
        q = step(&q, &transitions(asleep_h + k as f64 / 120.0, to_habit_min.map(|m| m - k as f64 / 2.0)));
        let left = (remaining - k) as f64 * EPOCH as f64 / 60.0;
        for s in 0..4 {
            let stop = if k == remaining { 1.0 } else if s == W || s == L { 0.5 } else { 0.03 };
            value_wait += q[s] * stop * (model.cost[s] + alpha * left);
            q[s] *= 1.0 - stop;
        }
    }
    let fire = cost_now + MIN_BENEFIT <= value_wait;
    Decision {
        fire,
        reason: if fire { "waking now beats waiting by the minimum margin".into() } else { "waiting is expected to be at least as good".into() },
        cost_now,
        value_wait,
        p_reach_light_or_wake_10min: p_reach,
        sleep_loss_per_min: alpha,
    }
}

/// Reason shown to the user (short, French) with a stable code for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    FixedTime,
    ShortNight,
    SleepDebt,
    LowCoverage,
    DataGap,
    OnsetUnknown,
    BeforeWindow,
    StaleData,
    DeepOrRem,
    NotPersistent,
    BetterLater,
    LightPersistent,
    AlreadyAwake,
    Deadline,
}

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Reason::FixedTime => "fixed_time",
            Reason::ShortNight => "short_night",
            Reason::SleepDebt => "sleep_debt",
            Reason::LowCoverage => "low_coverage",
            Reason::DataGap => "data_gap",
            Reason::OnsetUnknown => "onset_unknown",
            Reason::BeforeWindow => "before_window",
            Reason::StaleData => "stale_data",
            Reason::DeepOrRem => "deep_or_rem",
            Reason::NotPersistent => "not_persistent",
            Reason::BetterLater => "better_later",
            Reason::LightPersistent => "light_persistent",
            Reason::AlreadyAwake => "already_awake",
            Reason::Deadline => "deadline",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Reason::FixedTime => "heure fixe",
            Reason::ShortNight => "nuit courte",
            Reason::SleepDebt => "dette de sommeil élevée",
            Reason::LowCoverage | Reason::DataGap | Reason::StaleData => "données insuffisantes",
            Reason::OnsetUnknown => "endormissement non détecté",
            Reason::BeforeWindow => "avant la fenêtre",
            Reason::DeepOrRem => "sommeil profond ou paradoxal probable",
            Reason::NotPersistent => "sommeil léger à confirmer",
            Reason::BetterLater => "moment plus favorable attendu",
            Reason::LightPersistent => "sommeil léger persistant",
            Reason::AlreadyAwake => "éveil probable",
            Reason::Deadline => "heure limite",
        }
    }
    fn json(self) -> Value {
        json!({"code": self.code(), "label": self.label()})
    }
}

/// Inputs of the "duration first" rule: any reason returned means no anticipation tonight, the
/// alarm stays at the deadline.
#[derive(Debug, Clone, Copy)]
pub struct HoldInputs {
    pub max_advance_min: i64,
    pub onset_known: bool,
    pub asleep_min_so_far: f64,
    pub need_tonight_min: f64,
    pub sleep_debt_h: f64,
    pub night_coverage: f64,
    pub recent_coverage: f64,
    /// tonight's own analysis (whoop_lab_days) already says INCOMPLETE_LOWER_BOUND
    pub night_incomplete: bool,
}

pub fn hold_reasons(h: &HoldInputs) -> Vec<Reason> {
    let mut r = Vec::new();
    if h.max_advance_min <= 0 {
        r.push(Reason::FixedTime);
    }
    if !h.onset_known {
        r.push(Reason::OnsetUnknown);
    } else if h.asleep_min_so_far < h.need_tonight_min {
        r.push(Reason::ShortNight);
    }
    if h.sleep_debt_h >= DEBT_HOLD_H {
        r.push(Reason::SleepDebt);
    }
    if h.night_incomplete || h.night_coverage < MIN_NIGHT_COVERAGE {
        r.push(Reason::LowCoverage);
    }
    if h.recent_coverage < MIN_RECENT_COVERAGE {
        r.push(Reason::DataGap);
    }
    r
}

/// Persistence / hysteresis: a smart trigger needs the decision to fire and the favourable state
/// to hold, with observed data, on each of the last `PERSIST_EPOCHS` epochs. `fires[i]` and
/// `posts[i]` are oldest first. Returns the reason to wait, or None when the trigger is allowed.
pub fn persistence_block(posts: &[[f64; 4]], fires: &[bool], observed: &[bool]) -> Option<Reason> {
    let n = PERSIST_EPOCHS;
    if posts.len() < n || fires.len() < n || observed.len() < n {
        return Some(Reason::NotPersistent);
    }
    let (p, f, o) = (&posts[posts.len() - n..], &fires[fires.len() - n..], &observed[observed.len() - n..]);
    if !o.iter().all(|x| *x) {
        return Some(Reason::StaleData);
    }
    let last = p[n - 1];
    if favourable(&last) < FAVOURABLE_MIN {
        return Some(if last[D] + last[R] > favourable(&last) { Reason::DeepOrRem } else { Reason::NotPersistent });
    }
    if !p.iter().all(|q| favourable(q) >= FAVOURABLE_MIN) {
        return Some(Reason::NotPersistent);
    }
    if !f[n - 1] {
        return Some(Reason::BetterLater);
    }
    if !f.iter().all(|x| *x) {
        return Some(Reason::NotPersistent);
    }
    None
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
    /// Largest anticipation the user accepts (minutes before `window_end`); 0 = fixed time.
    #[serde(default = "default_max_advance")]
    pub max_advance_min: i64,
}

fn default_max_advance() -> i64 {
    DEFAULT_MAX_ADVANCE_MIN
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
    /// live IMU frames (packet 43 / R21 layout, TOGGLE_IMU_MODE) since the last call: [arrival_ts, frame_hex]
    #[serde(default)]
    pub imu: Vec<(i64, String)>,
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
        CREATE TABLE IF NOT EXISTS smart_wake_imu (session_id TEXT NOT NULL, ts INTEGER NOT NULL, frame BLOB NOT NULL, PRIMARY KEY (session_id, ts));
        "#,
    )?;
    // probable_ts: start of the "probable awake" state (strong but unconfirmed evidence)
    let has_probable: bool = conn.query_row("SELECT COUNT(*) FROM pragma_table_info('smart_wake_sessions') WHERE name = 'probable_ts'", [], |r| r.get::<_, i64>(0))? > 0;
    if !has_probable {
        conn.execute("ALTER TABLE smart_wake_sessions ADD COLUMN probable_ts INTEGER", [])?;
    }
    Ok(())
}

/// Tonight's own analysis, if the day pipeline already produced one for this session: true when it
/// is marked as a lower bound (coverage < 90 %).
fn tonight_incomplete(conn: &Connection, session_start: i64) -> GooseResult<bool> {
    if conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'whoop_lab_days'", [], |_| Ok(())).optional()?.is_none() {
        return Ok(false);
    }
    let mut st = conn.prepare("SELECT result_json FROM whoop_lab_days ORDER BY date_key DESC LIMIT 2")?;
    for j in st.query_map([], |r| r.get::<_, String>(0))? {
        let Ok(v) = serde_json::from_str::<Value>(&j?) else { continue };
        let n = &v["night"];
        if n["bed_time"].as_i64().is_some_and(|b| b >= session_start - 3 * 3600) && n["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND" {
            return Ok(true);
        }
    }
    Ok(false)
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
                imu_dyn: None,
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
        for r in load_imu(conn, id, from, to)? {
            let n = r.accel_raw.iter().map(Vec::len).min().unwrap_or(0);
            if n < 50 {
                continue;
            }
            let g: [f64; 3] = std::array::from_fn(|a| r.accel_raw[a][..n].iter().map(|v| f64::from(*v)).sum::<f64>() / n as f64 / crate::whoop5_records::ACCEL_LSB_PER_G);
            let mags: Vec<f64> = (0..n).map(|i| ((0..3).map(|a| (f64::from(r.accel_raw[a][i]) / crate::whoop5_records::ACCEL_LSB_PER_G).powi(2)).sum::<f64>()).sqrt()).collect();
            let e = secs.entry(r.ts.floor() as i64).or_default();
            e.grav = Some(g); // live orientation beats the lagged R18 gravity
            e.imu_dyn = std_sample(&mags);
            e.live = true;
        }
    }
    Ok(secs)
}

/// Live IMU records of a session, decoded from the stored raw frames (raw kept as received).
pub fn load_imu(conn: &Connection, session_id: &str, from: i64, to: i64) -> GooseResult<Vec<crate::whoop5_records::R21Record>> {
    let mut st = conn.prepare_cached("SELECT frame FROM smart_wake_imu WHERE session_id = ?1 AND ts >= ?2 AND ts <= ?3 ORDER BY ts")?;
    let mut rows = st.query(params![session_id, from, to])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let frame: Vec<u8> = r.get(0)?;
        if frame.len() == crate::whoop5_records::R21_FRAME_LEN {
            if let Some(rec) = crate::whoop5_records::decode_r21(&frame[crate::whoop5_records::HEADER_LEN..frame.len() - 4]) {
                out.push(rec);
            }
        }
    }
    Ok(out)
}

/// Hypnogram code -> HMM state. 'U' marks epochs without enough data: never a sleep stage.
fn code_state(c: char) -> Option<usize> {
    match c { 'W' => Some(W), 'L' => Some(L), 'D' => Some(D), 'R' => Some(R), _ => None }
}

/// Sleep minutes of a night usable for need / debt: an incomplete capture is only a lower bound,
/// so it must not read as a short night.
fn complete_asleep_min(n: &Value) -> Option<f64> {
    if n["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND" { return None; }
    n["asleep_min"].as_f64()
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
    let asleep: Vec<f64> = nights.iter().filter_map(complete_asleep_min).collect();
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
        "recommended_sleep_min": plan(&PlanArgs { database_path: conn.path().unwrap_or_default().to_string(), latest_wake: before_ts + 8 * 3600, utc_offset_seconds: utc_offset, now_unix: Some(before_ts), max_advance_min: DEFAULT_MAX_ADVANCE_MIN }).ok().and_then(|p| p["recommended_sleep_min"].as_f64()), "sleep_debt_h": round(debt_h, 2),
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
            // 'U' (insufficient data) and any other code never trains a state
            let Some(s) = code_state(c) else { continue };
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

struct SessionRow {
    state: String,
    stims: Vec<(i64, u8)>,
    trigger_ts: Option<i64>,
    probable_ts: Option<i64>,
}

fn session_row(conn: &Connection, id: &str) -> GooseResult<Option<SessionRow>> {
    Ok(conn
        .query_row("SELECT state, stimulations_json, trigger_ts, probable_ts FROM smart_wake_sessions WHERE session_id = ?1", [id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?
        .map(|(state, j, trigger_ts, probable_ts)| SessionRow { state, stims: serde_json::from_str(&j).unwrap_or_default(), trigger_ts, probable_ts }))
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
            let mut imu = tx.prepare_cached("INSERT OR REPLACE INTO smart_wake_imu (session_id, ts, frame) VALUES (?1, ?2, ?3)")?;
            for (_arrival, hex_frame) in &args.imu {
                let Ok(frame) = hex::decode(hex_frame.trim()) else { continue };
                // CRC-checked frames only; key by the record's own device timestamp (frame byte 15)
                let Ok(parsed) = crate::protocol::parse_frame(crate::protocol::DeviceType::Goose, &frame) else { continue };
                if !(parsed.header_crc_valid && parsed.payload_crc_valid) || frame.len() != crate::whoop5_records::R21_FRAME_LEN {
                    continue;
                }
                let ts = i64::from(u32::from_le_bytes([frame[15], frame[16], frame[17], frame[18]]));
                imu.execute(params![cfg.id, ts, frame])?;
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
        return Ok(json!({"action": "none", "reason": "session just started", "decision_reason": Reason::BeforeWindow.json()}));
    }
    let f = features(&secs, t0, n);
    let post = filter(&f, t0, ctx);
    let k = n - 1;
    let p = post[k];
    // an epoch is "observed" only with real measurements: a silent strap (BLE gap, strap off)
    // leaves the filter on its prior, which must never read as wake or as sleep
    let observed = |i: usize| f[i].hr.is_some() || f[i].movement.is_some();
    let onset = post.iter().enumerate().position(|(i, q)| q[W] < 0.3 && observed(i));
    let asleep_h = onset.map_or(0.0, |o| (k - o) as f64 * EPOCH as f64 / 3600.0);
    let last_hr_age = secs.iter().rev().find(|(_, s)| s.hr.is_some()).map(|(t, _)| now - t);
    let fresh = last_hr_age.is_some_and(|a| a <= 120);
    // measured sleep only (unobserved epochs are not sleep): a lower bound, which is conservative
    let asleep_min_so_far = post.iter().enumerate().filter(|(i, q)| q[W] < 0.5 && observed(*i)).count() as f64 / 2.0;
    let need = info["sleep_need_min"].as_f64().unwrap_or(480.0);
    let debt = info["sleep_debt_h"].as_f64().unwrap_or(0.0);
    // sleep-loss penalty: stronger with debt and while still short of the night's need
    let short = ((need - asleep_min_so_far) / 60.0).max(0.0);
    let alpha = 0.004 * (1.0 + debt / 2.0 + short / 2.0);
    let remaining = ((cfg.window_end - now).max(0) / EPOCH) as usize;
    // coverage ('U' epochs = no measurement): whole night since onset, and the last 30 min; the
    // newest 5 min are excluded because R18 only arrives with the next history sync
    let lag_epochs = (300 / EPOCH) as usize;
    let cov = |from: usize, to: usize| -> f64 {
        if to <= from {
            return 0.0;
        }
        (from..to).filter(|i| observed(*i)).count() as f64 / (to - from) as f64
    };
    let cov_end = n.saturating_sub(lag_epochs).max(1);
    let night_coverage = onset.map_or(0.0, |o| cov(o.min(cov_end - 1), cov_end));
    // window: never more than the user's maximum anticipation before the deadline (0 = fixed
    // time), and never before onset + tonight's recommended sleep; no forced opening
    let need_tonight = info["recommended_sleep_min"].as_f64().unwrap_or(need);
    let onset_ts = onset.map(|o| t0 + o as i64 * EPOCH);
    let max_adv = cfg.max_advance_min.clamp(0, MAX_ADVANCE_LIMIT_MIN);
    let earliest = cfg.window_end - max_adv * 60;
    // recent coverage = the 30 min before the earliest opening plus everything since: a gap near
    // the window keeps anticipation withheld until the deadline instead of sliding out of view
    let recent_from_ts = now.min(earliest) - 1800;
    let recent_from = (((recent_from_ts - t0).max(0)) / EPOCH) as usize;
    let recent_coverage = cov(recent_from.min(cov_end - 1), cov_end);
    let planned_start = if cfg.window_start > 0 {
        cfg.window_start.max(earliest).min(cfg.window_end)
    } else {
        onset_ts.map_or(cfg.window_end, |o| o + (need_tonight * 60.0) as i64).clamp(earliest, cfg.window_end)
    };
    let holds = hold_reasons(&HoldInputs {
        max_advance_min: max_adv,
        onset_known: onset.is_some(),
        asleep_min_so_far,
        need_tonight_min: need_tonight,
        sleep_debt_h: debt,
        night_coverage,
        recent_coverage,
        night_incomplete: tonight_incomplete(conn, cfg.start_ts)?,
    });
    let anticipation_allowed = holds.is_empty();
    let window_start = if anticipation_allowed { planned_start } else { cfg.window_end };
    let in_window = now >= window_start || remaining == 0;
    // sustained wake: the strap reports "up" (standing / orthostatic) in a recent epoch
    let up_since = |from: usize| f[from.min(k)..=k].iter().any(|x| x.band == Some(3));
    // live IMU (only while the app streams it, near the window): steps and sustained motion in the
    // last 30 s; `quiet_after` excludes the seconds where our own vibration shakes the accelerometer
    let imu_activity = |quiet_after: Option<i64>| -> GooseResult<Option<(usize, usize)>> {
        let Some(id) = live_id else { return Ok(None) };
        let from = quiet_after.map_or(now - 30, |q| (now - 30).max(q));
        if now - from < 15 {
            return Ok(None);
        }
        let recs = load_imu(conn, id, from, now)?;
        if recs.len() < 10 {
            return Ok(None);
        }
        let series = crate::imu::series(&recs);
        let feats = crate::imu::features(&series, 10.0);
        let steps = feats["steps"].as_u64().unwrap_or(0) as usize;
        let moving = secs.range(from..=now).filter(|(_, s)| s.imu_dyn.is_some_and(|d| d > 0.06)).count();
        Ok(Some((steps, moving)))
    };
    let to_h = to_habit(ctx, now);
    let dec = decide(&p, &ctx.model, asleep_h, to_h, remaining, alpha);
    // persistence: the same deterministic policy evaluated on the previous epoch(s)
    let first = (k + 1).saturating_sub(PERSIST_EPOCHS);
    let (mut posts, mut fires, mut obs) = (Vec::new(), Vec::new(), Vec::new());
    for i in first..=k {
        let back = (k - i) as i64;
        let fire_i = if i == k {
            dec.fire
        } else {
            decide(&post[i], &ctx.model, (asleep_h - back as f64 / 120.0).max(0.0), to_habit(ctx, now - back * EPOCH), remaining + back as usize, alpha).fire
        };
        posts.push(post[i]);
        fires.push(fire_i);
        obs.push(observed(i));
    }
    let wake_two = |i: usize| i >= 1 && post[i][W] >= 0.8 && post[i - 1][W] >= 0.8 && observed(i) && observed(i - 1);
    let already_awake = fresh && k >= 1 && post[k - 1][W] >= 0.75 && p[W] >= 0.75 && observed(k) && observed(k - 1);
    let smart_block = if !fresh { Some(Reason::StaleData) } else { persistence_block(&posts, &fires, &obs) };
    // why the engine waits or fires right now (journaled every call, shown to the user)
    let (reason, smart_fire) = if remaining == 0 {
        (Reason::Deadline, true)
    } else if max_adv > 0 && now < earliest {
        (Reason::BeforeWindow, false)
    } else if let Some(h) = holds.first() {
        (*h, false)
    } else if now < window_start {
        (Reason::BeforeWindow, false)
    } else if already_awake {
        (Reason::AlreadyAwake, true)
    } else if let Some(b) = smart_block {
        (b, false)
    } else {
        (Reason::LightPersistent, true)
    };
    let quality = json!({
        "last_hr_age_s": last_hr_age, "fresh": fresh,
        "observed_last_epoch": observed(k),
        "night_coverage": round(night_coverage, 3), "recent_coverage": round(recent_coverage, 3),
        "live_secs_last_epoch": f[k].live_secs,
        "r18_lag_s": secs.iter().rev().find(|(_, s)| s.grav.is_some() && !s.live).map(|(t, _)| now - t),
        "imu_lag_s": secs.iter().rev().find(|(_, s)| s.imu_dyn.is_some()).map(|(t, _)| now - t),
        "features_present": {"hr": f[k].hr.is_some(), "hrv": f[k].ln_rmssd.is_some(), "movement": f[k].movement.is_some(), "band": f[k].band.is_some()},
    });
    let anticipation = json!({
        "allowed": anticipation_allowed, "max_advance_min": max_adv,
        "hold_reasons": holds.iter().map(|r| r.json()).collect::<Vec<_>>(),
        "asleep_min_so_far": asleep_min_so_far, "need_tonight_min": need_tonight, "sleep_debt_h": debt,
    });
    // ---- state machine. Evidence levels are kept apart:
    //   reaction        = one index (HR rise, HMM wake): logged, escalation and deadline unchanged;
    //   probable_awake  = strong or concordant indices: ramp paused, deadline alarm kept, relapse
    //                     re-stimulates;
    //   awake           = explicit confirmation (button / double tap) only: everything stops.
    // Silence (no data) is never evidence of anything.
    let row = if persist { session_row(conn, &cfg.id)? } else { None };
    let SessionRow { state, mut stims, trigger_ts, mut probable_ts } = row.unwrap_or(SessionRow { state: "armed".into(), stims: vec![], trigger_ts: None, probable_ts: None });
    let mut action = json!({"action": "none"});
    let mut new_state = state.clone();
    let mut reaction = false;
    let mut outcome = if smart_fire { "fire" } else { "wait" };
    if stim_failed && !stims.is_empty() {
        stims.pop();
    }
    let timeout = now >= cfg.window_end + 30 * 60;
    let probable = |why: &str, new_state: &mut String, action: &mut Value, probable_ts: &mut Option<i64>| {
        *new_state = "probable_awake".into();
        *probable_ts = Some(now);
        *action = json!({"action": "probable_awake", "why": why});
    };
    match state.as_str() {
        "armed" if in_window => {
            let walking = imu_activity(None)?.is_some_and(|(steps, _)| steps >= 20);
            if up_since(k.saturating_sub(1)) || walking {
                // already up (or walking) inside the window: no vibration, deadline alarm kept
                probable(if walking { "marche détectée avant l'alarme" } else { "levé détecté avant l'alarme" }, &mut new_state, &mut action, &mut probable_ts);
                outcome = "probable_awake";
            } else if smart_fire && (remaining == 0 || fresh) {
                let why = reason.label();
                stims.push((now, 1));
                new_state = "stimulating".into();
                action = json!({"action": "stimulate", "level": 1, "why": why});
                if persist {
                    conn.execute("UPDATE smart_wake_sessions SET trigger_ts = ?2, trigger_json = ?3 WHERE session_id = ?1", params![cfg.id, now, json!({"why": reason.code(), "posterior": p, "decision": dec}).to_string()])?;
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
            // the ramp plays ~60 s and shakes the accelerometer: IMU rules start 70 s after it
            let moved = imu_activity(Some(last_ts + 70))?.is_some_and(|(steps, moving)| steps >= 10 || moving >= 15);
            let stood_up = up_since(stim_epoch);
            let hr_up = hr_rise.is_some_and(|d| d >= 12.0);
            let hmm_wake = fresh && wake_two(k);
            reaction = hr_up || hmm_wake;
            if acknowledged || double_tap_since(conn, last_ts)? {
                new_state = "awake".into();
                action = json!({"action": "awake", "acknowledged": acknowledged, "why": if acknowledged { "confirmé" } else { "double tap" }});
            } else if moved || stood_up || (hr_up && hmm_wake) {
                let why = if moved { "mouvement soutenu détecté" } else if stood_up { "levé (UP) détecté" } else { "réveil probable (FC et état)" };
                probable(why, &mut new_state, &mut action, &mut probable_ts);
            } else if timeout {
                new_state = "timeout".into();
                action = json!({"action": "timeout", "why": "pas de confirmation 30 min après l'heure limite"});
            } else if level < 3 && now - last_ts >= 120 {
                stims.push((now, level + 1));
                action = json!({"action": "stimulate", "level": level + 1, "why": if reaction { "réaction sans éveil confirmé" } else { "pas de réaction" }, "hr_rise": hr_rise});
            } else if now >= cfg.window_end {
                action = json!({"action": "deadline", "why": "alarme du bracelet à l'heure limite"});
            }
            outcome = "stimulating";
        }
        "probable_awake" => {
            let since = probable_ts.unwrap_or(now);
            let since_epoch = ((since - t0) / EPOCH).max(0) as usize;
            let back_to_sleep = now - since >= 300
                && fresh
                && k >= 3
                && (k - 3..=k).all(|i| observed(i) && post[i][W] < 0.3)
                && !up_since(since_epoch.max(k - 3))
                && !imu_activity(None)?.is_some_and(|(steps, moving)| steps >= 5 || moving >= 5);
            if acknowledged || double_tap_since(conn, since)? {
                new_state = "awake".into();
                action = json!({"action": "awake", "acknowledged": acknowledged, "why": if acknowledged { "confirmé" } else { "double tap" }});
            } else if timeout {
                new_state = "timeout".into();
                action = json!({"action": "timeout", "why": "pas de confirmation 30 min après l'heure limite"});
            } else if back_to_sleep {
                let level = stims.last().map_or(1, |(_, l)| (*l + 1).min(3));
                stims.push((now, level));
                new_state = "stimulating".into();
                action = json!({"action": "stimulate", "level": level, "why": "rendormissement probable"});
            } else if now >= cfg.window_end {
                action = json!({"action": "deadline", "why": "alarme du bracelet à l'heure limite"});
            }
            outcome = "probable_awake";
        }
        _ => {}
    }
    let policy = json!({
        "version": POLICY_VERSION, "outcome": outcome, "reason": reason.json(),
        "anticipation_allowed": anticipation_allowed, "reaction": reaction,
        "persistence": {"epochs": PERSIST_EPOCHS, "fires": fires, "favourable": posts.iter().map(|q| round(favourable(q), 3)).collect::<Vec<_>>(), "observed": obs},
    });
    let epoch = json!({
        "ts": t0 + k as i64 * EPOCH, "posterior": p.map(|v| round(v, 3)), "features": f[k], "decision": dec,
        "asleep_h": round(asleep_h, 2), "in_window": in_window, "quality": quality, "policy": policy,
    });
    if persist {
        conn.execute("UPDATE smart_wake_sessions SET state = ?2, stimulations_json = ?3, probable_ts = ?4 WHERE session_id = ?1", params![cfg.id, new_state, serde_json::to_string(&stims)?, probable_ts])?;
        if new_state == "awake" && state != "awake" {
            conn.execute("UPDATE smart_wake_sessions SET awake_ts = ?2 WHERE session_id = ?1", params![cfg.id, now])?;
        }
        // every call is journaled, the decisions to wait as well as the triggers
        conn.execute("INSERT OR REPLACE INTO smart_wake_log (session_id, ts, epoch_json) VALUES (?1, ?2, ?3)", params![cfg.id, now, epoch.to_string()])?;
    }
    let recent: Vec<Value> = post.iter().enumerate().skip(n.saturating_sub(40)).map(|(i, q)| json!([t0 + i as i64 * EPOCH, q.map(|v| round(v, 3))])).collect();
    Ok(json!({
        "engine_version": ENGINE_VERSION,
        "policy_version": POLICY_VERSION,
        "state": new_state,
        "action": action["action"], "level": action.get("level"), "why": action.get("why"),
        "decision_reason": reason.json(),
        "anticipation": anticipation,
        "evidence": {"reaction": reaction, "probable_awake": new_state == "probable_awake", "confirmed": new_state == "awake"},
        "epoch": epoch,
        "window": {"start": window_start, "planned_start": planned_start, "earliest": earliest, "end": cfg.window_end, "automatic": cfg.window_start == 0, "onset_estimate": onset_ts},
        "recent_posteriors": recent,
        "personal": info,
        "labels": {"posterior": "ESTIMATED (causal HMM)", "p_reach_light_or_wake_10min": "PREDICTED (first passage under the transition model)", "features": "MEASURED (HR, R-R live; gravity, band from R18 sync)"},
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
    /// user's maximum anticipation (minutes, 0 = fixed time), same meaning as in SessionConfig
    #[serde(default = "default_max_advance")]
    pub max_advance_min: i64,
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
    let asleep: Vec<f64> = nights.iter().take(14).filter_map(|n| complete_asleep_min(n)).collect();
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
    // same window rule as the live engine: never before latest - max anticipation, no forced opening
    let earliest = args.latest_wake - args.max_advance_min.clamp(0, MAX_ADVANCE_LIMIT_MIN) * 60;
    let win_start = (onset + (recommended * 60.0) as i64).clamp(earliest, args.latest_wake);
    // if going to bed now
    let now_sleep = ((args.latest_wake - now) as f64 / 60.0 - latency).max(0.0);
    let now_win = (now + (latency * 60.0) as i64 + (recommended * 60.0) as i64).clamp(earliest, args.latest_wake);
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
            code.chars().nth(((ts - hstart) / EPOCH) as usize).and_then(code_state)
        };
        let (ctx, info) = personal_context(&conn, bed, args.utc_offset_seconds)?;
        let cfg = SessionConfig { id: format!("replay-{bed}"), start_ts: bed - 600, window_start: wake - window, window_end: wake, utc_offset_seconds: args.utc_offset_seconds, max_advance_min: window / 60 };
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
            // same persistence rule as the live engine (policy only; hold rules need live context)
            let mut fires: Vec<bool> = Vec::new();
            while now <= wake && trigger.is_none() {
                let k = ((now - t0) / EPOCH) as usize - 1;
                let onset = post.iter().position(|q| q[W] < 0.3);
                let asleep_h = onset.map_or(0.0, |o| k.saturating_sub(o) as f64 / 120.0);
                let ts = std::time::Instant::now();
                let dec = decide(&post[k], &ctx.model, asleep_h, to_habit(&ctx, now), ((wake - now) / EPOCH) as usize, 0.004);
                step_ms.push(ts.elapsed().as_secs_f64() * 1000.0);
                fires.push(dec.fire);
                let posts: Vec<[f64; 4]> = post[(k + 1).saturating_sub(PERSIST_EPOCHS)..=k].to_vec();
                let obs = vec![true; posts.len()];
                let persistent = dec.fire && persistence_block(&posts, &fires, &obs).is_none();
                let awake = post[k][W] >= 0.75 && post[k.saturating_sub(1)][W] >= 0.75;
                if persistent || awake || now >= wake {
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
                imu_dyn: None,
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
        let deep = decide(&[0.01, 0.05, 0.9, 0.04], &m, 3.0, None, 50, 0.004);
        assert!(!deep.fire);
        let light = decide(&[0.2, 0.75, 0.02, 0.03], &m, 6.0, None, 10, 0.004);
        assert!(light.fire, "{light:?}");
        assert!(decide(&[0.0, 0.0, 1.0, 0.0], &m, 3.0, None, 0, 0.004).fire);
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

    #[test]
    fn reach_probability_is_a_first_passage_not_a_max_of_marginals() {
        let post = [0.02, 0.10, 0.68, 0.20];
        let reach = reach_light_or_wake(&post, 5.0, Some(20.0), 20);
        // the old output: max over horizons of P(light or wake at that epoch)
        let (mut q, mut max_marg) = (post, favourable(&post));
        for k in 1..=20 {
            q = step(&q, &transitions(5.0 + k as f64 / 120.0, Some(20.0 - k as f64 / 2.0)));
            max_marg = max_marg.max(favourable(&q));
        }
        assert!(reach >= max_marg - 1e-12, "reach {reach} < max marginal {max_marg}");
        assert!(reach <= 1.0);
        // all mass already favourable -> certain; no time -> current share
        assert!((reach_light_or_wake(&[0.3, 0.7, 0.0, 0.0], 5.0, None, 20) - 1.0).abs() < 1e-12);
        assert!((reach_light_or_wake(&post, 5.0, None, 0) - 0.12).abs() < 1e-12);
    }

    #[test]
    fn decision_is_deterministic() {
        let m = Model::default();
        let a = decide(&[0.1, 0.5, 0.3, 0.1], &m, 6.0, Some(10.0), 40, 0.006);
        let b = decide(&[0.1, 0.5, 0.3, 0.1], &m, 6.0, Some(10.0), 40, 0.006);
        assert_eq!(a.value_wait, b.value_wait);
        assert_eq!(a.fire, b.fire);
    }

    fn ok_inputs() -> HoldInputs {
        HoldInputs { max_advance_min: 30, onset_known: true, asleep_min_so_far: 500.0, need_tonight_min: 480.0, sleep_debt_h: 0.5, night_coverage: 0.97, recent_coverage: 1.0, night_incomplete: false }
    }

    #[test]
    fn duration_first_hold_rules() {
        assert!(hold_reasons(&ok_inputs()).is_empty());
        assert_eq!(hold_reasons(&HoldInputs { max_advance_min: 0, ..ok_inputs() }), vec![Reason::FixedTime]);
        assert_eq!(hold_reasons(&HoldInputs { asleep_min_so_far: 300.0, ..ok_inputs() }), vec![Reason::ShortNight]);
        assert_eq!(hold_reasons(&HoldInputs { sleep_debt_h: 4.0, ..ok_inputs() }), vec![Reason::SleepDebt]);
        assert_eq!(hold_reasons(&HoldInputs { night_incomplete: true, ..ok_inputs() }), vec![Reason::LowCoverage]);
        assert_eq!(hold_reasons(&HoldInputs { night_coverage: 0.85, ..ok_inputs() }), vec![Reason::LowCoverage]);
        assert_eq!(hold_reasons(&HoldInputs { recent_coverage: 0.5, ..ok_inputs() }), vec![Reason::DataGap]);
        assert_eq!(hold_reasons(&HoldInputs { onset_known: false, ..ok_inputs() }), vec![Reason::OnsetUnknown]);
        assert_eq!(Reason::LowCoverage.label(), "données insuffisantes");
        assert_eq!(Reason::ShortNight.label(), "nuit courte");
    }

    #[test]
    fn a_single_light_epoch_does_not_trigger() {
        let light = [0.1, 0.8, 0.05, 0.05];
        let deep = [0.02, 0.1, 0.85, 0.03];
        assert_eq!(persistence_block(&[deep, light], &[false, true], &[true, true]), Some(Reason::NotPersistent));
        assert_eq!(persistence_block(&[light, light], &[true, true], &[true, false]), Some(Reason::StaleData));
        assert_eq!(persistence_block(&[light, deep], &[true, false], &[true, true]), Some(Reason::DeepOrRem));
        assert_eq!(persistence_block(&[light, light], &[true, false], &[true, true]), Some(Reason::BetterLater));
        assert_eq!(persistence_block(&[light, light], &[true, true], &[true, true]), None);
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
        let r = super::plan(&super::PlanArgs { database_path: db, latest_wake: 1_794_376_800, utc_offset_seconds: 3600, now_unix: Some(1_794_330_000), max_advance_min: 30 }).unwrap();
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
        call_adv(path, now, ack, 30)
    }

    fn call_adv(path: &str, now: i64, ack: bool, max_advance_min: i64) -> Value {
        update(&UpdateArgs {
            database_path: path.into(),
            session: SessionConfig { id: "s1".into(), start_ts: BED, window_start: 0, window_end: DEADLINE, utc_offset_seconds: 3600, max_advance_min },
            live: vec![],
            now_unix: Some(now),
            acknowledged: ack,
            stimulation_failed: false,
            imu: vec![],
        })
        .unwrap()
    }

    /// Steps the engine like the app (10 min before the window, 30 s inside); returns (ts, response)
    /// of the first non-"none" action, and every response seen on the way.
    fn run_adv(path: &str, from: i64, to: i64, adv: i64) -> (Option<(i64, Value)>, Vec<Value>) {
        let mut now = from;
        let mut seen = Vec::new();
        while now <= to {
            let r = call_adv(path, now, false, adv);
            if r["action"] != "none" {
                return (Some((now, r)), seen);
            }
            let ws = r["window"]["start"].as_i64().unwrap_or(DEADLINE - 1800);
            now += if now < ws.min(DEADLINE - 1800) - 600 { 600 } else { 30 };
            seen.push(r);
        }
        (None, seen)
    }

    fn run_until_action(path: &str, from: i64, to: i64) -> Option<(i64, Value)> {
        run_adv(path, from, to, 30).0
    }

    /// Deep until 06:00, then light sleep to the end: the most tempting case for an early wake.
    fn light_end(t: i64) -> St {
        if t >= DEADLINE - 60 * 60 { St::Light } else { night(t) }
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
        // window = estimated onset (~22:20) + 8 h default need, never more than the 30 min allowed
        let onset = r["window"]["onset_estimate"].as_i64().unwrap();
        assert!((onset - (BED + 20 * 60)).abs() <= 120, "onset {} min after bed", (onset - BED) / 60);
        assert_eq!(ws, (onset + 480 * 60).clamp(DEADLINE - 1800, DEADLINE), "{}", r["window"]);
        assert_eq!(r["decision_reason"]["code"], "light_persistent", "{r}");
        assert_eq!(r["action"], "stimulate", "{r}");
        assert!(ts >= DEADLINE - 25 * 60 && ts < DEADLINE, "fired at {} min before deadline", (DEADLINE - ts) / 60);
        assert_ne!(r["why"], "heure limite");
        // user stands up 90 s after the vibration
        write(&p, ts + 60, ts + 400, &|_| St::Up);
        let r2 = call(&p, ts + 150, false);
        // standing up is strong but unconfirmed evidence: ramp paused, deadline alarm kept
        assert_eq!(r2["action"], "probable_awake", "{r2}");
        assert_eq!(r2["why"], "levé (UP) détecté");
        assert_eq!(r2["evidence"]["confirmed"], false);
        assert!(r2["latency_ms"].as_f64().unwrap() < 2000.0);
        let r3 = call(&p, ts + 200, true);
        assert_eq!(r3["action"], "awake", "{r3}");
        assert_eq!(r3["evidence"]["confirmed"], true);
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
        assert_eq!(r["action"], "probable_awake", "{r}");
        assert_eq!(r["state"], "probable_awake");
        assert!(ts >= DEADLINE - 30 * 60);
        // unconfirmed: the session (and its deadline alarm) stays alive, no vibration either
        let r2 = call(&p, ts + 60, false);
        assert_eq!(r2["state"], "probable_awake", "{r2}");
        assert_ne!(r2["action"], "stimulate");
    }

    #[test]
    fn stale_data_never_fires_before_the_deadline() {
        let (_d, p) = db();
        // data stops at 06:00 (strap off / disconnected): only the deadline may fire
        write(&p, BED - 600, DEADLINE - 3600, &night);
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE + 60).expect("deadline");
        assert_eq!(r["why"], "heure limite", "{r}");
        assert!(ts >= DEADLINE - EPOCH);
        assert_eq!(r["epoch"]["quality"]["fresh"], false);
        // the silent strap was never read as awake
        assert_eq!(r["state"], "stimulating");
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
        let r = plan(&PlanArgs { database_path: p, latest_wake: DEADLINE, utc_offset_seconds: 3600, now_unix: Some(BED), max_advance_min: 30 }).unwrap();
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

    /// Live IMU frame (packet 43, R21 layout) for second `ts`; `walk` = 1.8 Hz vertical bounce.
    fn imu_frame(ts: i64, walk: bool) -> String {
        let mut pl = crate::whoop5_records::tests::r21_payload(100, (0, 0, 4096), (0, 0, 0));
        pl[0] = crate::whoop5_records::PACKET_REALTIME_RAW;
        pl[15 - 8..19 - 8].copy_from_slice(&(ts as u32).to_le_bytes());
        for i in 0..100 {
            let t = ts as f64 + i as f64 / 100.0;
            let z = if walk { 4096.0 * (1.0 + 0.3 * (2.0 * std::f64::consts::PI * 1.8 * t).sin()) } else { 4096.0 };
            pl[428 - 8 + 2 * i..428 - 8 + 2 * i + 2].copy_from_slice(&(z as i16).to_le_bytes());
        }
        hex::encode(crate::protocol::build_v5_payload_frame(&pl))
    }

    fn call_imu(path: &str, now: i64, imu: Vec<(i64, String)>) -> Value {
        update(&UpdateArgs {
            database_path: path.into(),
            session: SessionConfig { id: "s1".into(), start_ts: BED, window_start: 0, window_end: DEADLINE, utc_offset_seconds: 3600, max_advance_min: 30 },
            live: vec![], now_unix: Some(now), acknowledged: false, stimulation_failed: false, imu,
        })
        .unwrap()
    }

    #[test]
    fn live_imu_walking_stops_alarm_but_vibration_shake_does_not() {
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 900, &|t| if t >= DEADLINE - 60 * 60 { St::Deep } else { night(t) });
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE + 60).expect("deadline stimulation");
        assert_eq!(r["action"], "stimulate");
        // strap shakes during the ramp (looks like motion) -> must NOT count as awake
        let shake: Vec<(i64, String)> = (ts..ts + 30).map(|t| (t, imu_frame(t, true))).collect();
        let r1 = call_imu(&p, ts + 30, shake);
        assert_ne!(r1["action"], "awake", "own vibration must be ignored: {r1}");
        assert!(r1["epoch"]["quality"]["imu_lag_s"].as_i64().unwrap() <= 2, "{}", r1["epoch"]["quality"]);
        // real walking after the ramp -> awake through the live IMU, before any R18 sync
        let walk: Vec<(i64, String)> = (ts + 75..ts + 110).map(|t| (t, imu_frame(t, true))).collect();
        let r2 = call_imu(&p, ts + 110, walk);
        assert_eq!(r2["action"], "probable_awake", "{r2}");
        assert_eq!(r2["why"], "mouvement soutenu détecté");
    }

    #[test]
    fn live_imu_walking_in_window_needs_no_alarm() {
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 600, &|t| if t >= DEADLINE - 60 * 60 { St::Deep } else { night(t) });
        let now = DEADLINE - 20 * 60; // inside the automatic window
        let _ = call_imu(&p, now - 600, vec![]);
        let walk: Vec<(i64, String)> = (now - 30..now).map(|t| (t, imu_frame(t, true))).collect();
        let r = call_imu(&p, now, walk);
        assert_eq!(r["action"], "probable_awake", "{r}");
        assert_eq!(r["why"], "marche détectée avant l'alarme");
    }

    #[test]
    fn zero_anticipation_is_respected_even_in_light_sleep() {
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 600, &light_end);
        let (res, seen) = run_adv(&p, BED + 600, DEADLINE + 60, 0);
        let (ts, r) = res.expect("deadline stimulation");
        assert_eq!(r["action"], "stimulate", "{r}");
        assert_eq!(r["why"], "heure limite", "{r}");
        assert!(ts >= DEADLINE - EPOCH, "fired {} s before the deadline with max anticipation 0", DEADLINE - ts);
        assert_eq!(r["window"]["start"], DEADLINE);
        // every wait before the deadline was journaled with the fixed-time reason
        let late: Vec<&Value> = seen.iter().filter(|v| v["epoch"]["ts"].as_i64().is_some_and(|t| t >= DEADLINE - 1800)).collect();
        assert!(!late.is_empty());
        assert!(late.iter().all(|v| v["decision_reason"]["code"] == "fixed_time"), "{:?}", late.iter().map(|v| v["decision_reason"].clone()).collect::<Vec<_>>());
        let c = Connection::open(&p).unwrap();
        let waits: i64 = c.query_row("SELECT COUNT(*) FROM smart_wake_log WHERE session_id = 's1' AND json_extract(epoch_json, '$.policy.outcome') = 'wait'", [], |r| r.get(0)).unwrap();
        assert!(waits as usize >= late.len(), "waits journaled: {waits}");
    }

    #[test]
    fn same_light_sleep_with_anticipation_wakes_early() {
        // control for the test above: the data alone would justify an early wake
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 600, &light_end);
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE).expect("an action");
        assert_eq!(r["action"], "stimulate", "{r}");
        assert!(ts < DEADLINE - 60, "{r}");
        assert!(ts >= DEADLINE - 30 * 60);
    }

    #[test]
    fn incomplete_night_gets_no_anticipation() {
        let (_d, p) = db();
        write(&p, BED - 600, DEADLINE + 600, &light_end);
        // tonight's own analysis (day pipeline) marks the night as a lower bound
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE IF NOT EXISTS whoop_lab_days (date_key TEXT PRIMARY KEY, input_count INTEGER, input_last_ts INTEGER, algorithm_version TEXT, result_json TEXT, updated_at TEXT)").unwrap();
        let night = json!({"available": true, "bed_time": BED + 300, "wake_time": DEADLINE - 3600, "asleep_min": 400.0, "sleep_duration_status": "INCOMPLETE_LOWER_BOUND", "sleep_coverage_pct": 82.0});
        c.execute("INSERT INTO whoop_lab_days VALUES ('2026-11-11', 0, 0, '12', ?1, '')", [json!({"night": night}).to_string()]).unwrap();
        let (res, seen) = run_adv(&p, BED + 600, DEADLINE + 60, 30);
        let (ts, r) = res.expect("deadline stimulation");
        assert_eq!(r["why"], "heure limite", "{r}");
        assert!(ts >= DEADLINE - EPOCH);
        let last_wait = seen.last().unwrap();
        assert_eq!(last_wait["anticipation"]["allowed"], false);
        assert_eq!(last_wait["decision_reason"]["code"], "low_coverage", "{last_wait}");
        assert_eq!(last_wait["decision_reason"]["label"], "données insuffisantes");
    }

    #[test]
    fn unknown_epochs_near_the_window_get_no_anticipation() {
        let (_d, p) = db();
        // 20 min without any data just before the window (strap off / BLE gap), then light sleep
        write(&p, BED - 600, DEADLINE - 50 * 60, &night);
        write(&p, DEADLINE - 30 * 60, DEADLINE + 600, &|_| St::Light);
        let (res, seen) = run_adv(&p, BED + 600, DEADLINE + 60, 30);
        let (ts, r) = res.expect("deadline stimulation");
        assert!(ts >= DEADLINE - EPOCH, "fired {} min early on a gappy night: {r}", (DEADLINE - ts) / 60);
        let codes: Vec<String> = seen.iter().filter(|v| v["epoch"]["ts"].as_i64().is_some_and(|t| t >= DEADLINE - 1500)).map(|v| v["decision_reason"]["code"].as_str().unwrap_or("").to_string()).collect();
        assert!(codes.iter().any(|c| c == "data_gap"), "{codes:?}");
    }

    #[test]
    fn short_night_or_debt_keeps_the_deadline() {
        let (_d, p) = db();
        // in bed at 03:00: 4 h of sleep before a 07:00 deadline
        let late_bed = DEADLINE - 4 * 3600;
        write(&p, late_bed - 600, DEADLINE + 600, &|t| if t < late_bed + 600 { St::Awake } else { St::Light });
        let r = update(&UpdateArgs {
            database_path: p.clone(),
            session: SessionConfig { id: "short".into(), start_ts: late_bed, window_start: 0, window_end: DEADLINE, utc_offset_seconds: 3600, max_advance_min: 30 },
            live: vec![], now_unix: Some(DEADLINE - 10 * 60), acknowledged: false, stimulation_failed: false, imu: vec![],
        })
        .unwrap();
        assert_eq!(r["action"], "none", "{r}");
        assert_eq!(r["decision_reason"]["code"], "short_night", "{r}");
        assert_eq!(r["decision_reason"]["label"], "nuit courte");
    }

    #[test]
    fn silence_after_the_vibration_is_never_read_as_awake() {
        let (_d, p) = db();
        // data stops right when the ramp starts (BLE lost): escalation goes on, never awake
        write(&p, BED - 600, DEADLINE - 3600, &night);
        write(&p, DEADLINE - 3600, DEADLINE, &|_| St::Deep);
        let (ts, r) = run_until_action(&p, BED + 600, DEADLINE + 60).expect("deadline");
        assert_eq!(r["action"], "stimulate");
        for dt in [60, 125, 250, 400, 900] {
            let rr = call(&p, ts + dt, false);
            assert_ne!(rr["action"], "awake", "{rr}");
            assert_ne!(rr["state"], "probable_awake", "silence read as wake at +{dt} s: {rr}");
        }
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
                session: super::SessionConfig { id: format!("lat-{label}"), start_ts: wake - 10 * 3600, window_start: 0, window_end: wake, utc_offset_seconds: 3600, max_advance_min: 30 },
                live: vec![], now_unix: Some(now), acknowledged: false, stimulation_failed: false, imu: vec![],
            }).unwrap();
            println!("{label}: wall {:?} engine {} ms action {} post {} window {} fresh {}", t.elapsed(), r["latency_ms"], r["action"], r["epoch"]["posterior"], r["window"], r["epoch"]["quality"]["fresh"]);
        }
    }

    #[test]
    fn unknown_epochs_never_become_rem_and_incomplete_nights_add_no_debt() {
        use super::*;
        assert_eq!(code_state('U'), None);
        assert_eq!(code_state('?'), None);
        assert_eq!(code_state('R'), Some(R));
        let partial = json!({"asleep_min": 180.0, "sleep_duration_status": "INCOMPLETE_LOWER_BOUND"});
        assert_eq!(complete_asleep_min(&partial), None);
        assert_eq!(complete_asleep_min(&json!({"asleep_min": 450.0, "sleep_duration_status": "COMPLETE"})), Some(450.0));
    }
}
