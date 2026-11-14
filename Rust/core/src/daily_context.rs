//! Daily health context for the Lock Screen widget: which moment of the day it is (morning
//! recovery, daytime physiology, evening sleep preparation, night) and the few metrics that matter
//! then, each with its quality, timestamps and producer.
//!
//! Nothing here computes a physiological score. Every value comes from an existing producer:
//!   - `whoop_lab.analyze` (whoop_lab_analysis.rs): night (sleep, HRV, resting HR, baselines,
//!     respiration, SpO2, skin temperature), day (strain, stress outside exercise, steps, workouts)
//!     and per-day indices (recovery);
//!   - `smart_wake.plan` (smart_wake.rs): sleep need for tonight, latency, debt, habitual bedtime;
//!   - the Smart Wake configuration from the app (the alarm actually set, read-only here);
//!   - the latest heart rate seen by the app (BLE) or stored in `whoop_lab_biometric`;
//!   - the activity currently detected by the sport engine (sport.rs, via the app).
//!
//! Components (names match docs/WIDGET.md):
//!   - MetricQualityEvaluator: `Metric::at` (staleness, live label) and the quality rules in the
//!     builders below;
//!   - DailyContextEngine: `decide_context` (observed events first, fallback clock times,
//!     hysteresis with the previous context, naps, fragmented sleep, missing data);
//!   - SleepSchedulePlanner: `plan_schedule` (bedtime from the alarm, the plan's need and latency,
//!     limited by sleep regularity);
//!   - DailyInsightEngine: `insight` (one deterministic sentence, fixed priority order);
//!   - snapshot + precomputed timeline: `snapshot` (what the App Group store holds and what the
//!     WidgetKit timeline provider replays without computing anything).

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::GooseResult;

pub const ENGINE_VERSION: &str = "mn-daily.v1";
pub const SCHEMA: &str = "mn.daily_snapshot.v1";

// ------------------------------------------------------------------ inputs

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    /// morning card lasts this long after the confirmed awakening
    pub morning_duration_min: i64,
    /// evening starts this long before the recommended wind-down time
    pub evening_lead_min: i64,
    /// local clock fallbacks (minutes after midnight) when no event is observed
    pub fallback_evening_start: i64,
    pub fallback_morning_end: i64,
    pub fallback_night_start: i64,
    pub day_rollover: i64,
    /// awakening / sleep onset must persist this long before they count (fragmented sleep)
    pub confirm_min: i64,
    /// a heart rate is "live" for this long, then shown with its time; stale after `hr_recent_s`
    pub hr_live_s: i64,
    pub hr_recent_s: i64,
    /// cumulative day metrics (strain, steps, stress) are shown with their time after this
    pub day_metric_valid_s: i64,
    /// the whole snapshot is flagged as not refreshed after this
    pub snapshot_stale_s: i64,
    /// how much earlier than the habitual bedtime the planner may move bedtime
    pub max_bedtime_advance_min: i64,
    /// expected awakening for the timeline when no alarm is set (local minutes)
    pub expected_wake_fallback: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            morning_duration_min: 90,
            evening_lead_min: 60,
            fallback_evening_start: 21 * 60,
            fallback_morning_end: 11 * 60,
            fallback_night_start: 2 * 60,
            day_rollover: 4 * 60,
            confirm_min: 20,
            hr_live_s: 120,
            hr_recent_s: 15 * 60,
            day_metric_valid_s: 2 * 3600,
            snapshot_stale_s: 6 * 3600,
            max_bedtime_advance_min: 60,
            expected_wake_fallback: 7 * 60,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LiveHr {
    pub bpm: f64,
    pub ts: i64,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Activity {
    /// sport kind id from the sport engine (course, tennis, muscu, …) and its display title
    pub kind: String,
    #[serde(default)]
    pub title: String,
    pub start_ts: i64,
    /// labelled by the user (true) or only detected (false)
    #[serde(default)]
    pub confirmed: bool,
}

/// The Smart Wake configuration as the app holds it. Authoritative and read-only here.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SmartWakeInput {
    pub armed: bool,
    /// absolute deadline of the armed session, or the next occurrence of the chosen time
    pub deadline: Option<i64>,
    /// 0 = fixed time
    pub max_advance_min: i64,
    /// engine state of the armed session (armed / stimulating / probable_awake / awake)
    pub state: Option<String>,
    /// the user confirmed being awake ("Je suis réveillé" / double tap)
    pub confirmed_awake_at: Option<i64>,
}

/// Sleep state observed from data (firmware band state in R18), summarised by `observe_sleep`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SleepObservation {
    /// start of the latest sustained sleep block
    pub onset: Option<i64>,
    /// start of a sustained wake after that block (None while asleep)
    pub wake: Option<i64>,
    /// newest sample used (a gap after it is missing data, not wake)
    pub last_sample_ts: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Context {
    Morning,
    Daytime,
    Evening,
    Night,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Previous {
    pub context: Context,
    pub since: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotArgs {
    #[serde(default)]
    pub database_path: Option<String>,
    #[serde(default)]
    pub now_unix: Option<i64>,
    #[serde(default)]
    pub utc_offset_seconds: i64,
    /// output of `whoop_lab.analyze` (the app already holds it)
    #[serde(default)]
    pub analysis: Value,
    #[serde(default)]
    pub smart_wake: SmartWakeInput,
    /// output of `smart_wake.plan`; computed here from the database when absent
    #[serde(default)]
    pub plan: Option<Value>,
    #[serde(default)]
    pub live_hr: Option<LiveHr>,
    #[serde(default)]
    pub activity: Option<Activity>,
    /// overrides the database observation (tests, or the app when it already knows)
    #[serde(default)]
    pub sleep: Option<SleepObservation>,
    #[serde(default)]
    pub previous: Option<Previous>,
    #[serde(default)]
    pub settings: Settings,
}

// ------------------------------------------------------------------ metric + quality

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// measured by the strap (value as recorded)
    Measured,
    /// derived by an existing algorithm from measurements (scores, durations)
    Derived,
    /// estimated (heuristic producer, or a planning value)
    Estimated,
    /// produced but the personal reference is still being built
    Calibrating,
    /// a lower bound: the night has gaps (coverage < 90 %)
    Incomplete,
    /// older than its validity: shown with its time, never as current
    Stale,
    /// not available; `missing_reason` says why
    Missing,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    pub id: &'static str,
    pub label: &'static str,
    pub value: Option<f64>,
    pub unit: &'static str,
    /// formatted for display (French), "—" when missing
    pub display: String,
    /// short second line: comparison to the reference, need, time of observation…
    pub detail: Option<String>,
    pub observed_at: Option<i64>,
    pub computed_at: Option<i64>,
    /// after this instant the value is stale
    pub valid_until: Option<i64>,
    /// until this instant the value may be labelled live
    pub live_until: Option<i64>,
    pub live: bool,
    pub source: &'static str,
    pub algorithm: String,
    pub quality: Quality,
    pub missing_reason: Option<String>,
    /// good / bad / neutral, for a discreet emphasis only
    pub tone: &'static str,
}

impl Metric {
    fn new(id: &'static str, label: &'static str, unit: &'static str, source: &'static str, algorithm: impl Into<String>) -> Self {
        Metric {
            id, label, value: None, unit, display: "—".into(), detail: None, observed_at: None, computed_at: None,
            valid_until: None, live_until: None, live: false, source, algorithm: algorithm.into(),
            quality: Quality::Missing, missing_reason: None, tone: "neutral",
        }
    }

    fn missing(mut self, why: impl Into<String>) -> Self {
        self.value = None;
        self.display = "—".into();
        self.quality = Quality::Missing;
        self.missing_reason = Some(why.into());
        self
    }

    fn value(mut self, v: f64, display: String, quality: Quality) -> Self {
        self.value = Some(v);
        self.display = display;
        self.quality = quality;
        self
    }

    fn detail(mut self, d: Option<String>) -> Self {
        self.detail = d;
        self
    }

    /// MetricQualityEvaluator: the same metric seen at instant `t` (a timeline entry).
    pub fn at(&self, t: i64, off: i64) -> Metric {
        let mut m = self.clone();
        m.live = m.quality != Quality::Missing && m.live_until.is_some_and(|l| t <= l);
        if m.quality != Quality::Missing && m.valid_until.is_some_and(|v| t > v) {
            m.quality = Quality::Stale;
            m.live = false;
            if let Some(o) = m.observed_at.or(m.computed_at) {
                m.detail = Some(format!("à {}", clock(o, off)));
            }
        } else if m.live_until.is_some() && !m.live && m.quality != Quality::Missing {
            // was live, now only recent: say when it was measured
            if let Some(o) = m.observed_at {
                m.detail = Some(format!("à {}", clock(o, off)));
            }
        }
        m
    }
}

// ------------------------------------------------------------------ formatting

fn clock(ts: i64, off: i64) -> String {
    let m = (ts + off).rem_euclid(86_400) / 60;
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn dur(min: f64) -> String {
    let t = min.round() as i64;
    if t >= 60 { format!("{} h {:02}", t / 60, t % 60) } else { format!("{t} min") }
}

fn num(v: f64, d: usize) -> String {
    format!("{v:.d$}").replace('.', ",")
}

fn signed(v: f64, d: usize) -> String {
    let s = num(v.abs(), d);
    if (v * 10f64.powi(d as i32)).round() > 0.0 { format!("+{s}") } else if (v * 10f64.powi(d as i32)).round() < 0.0 { format!("−{s}") } else { s }
}

fn local_min(ts: i64, off: i64) -> i64 {
    (ts + off).rem_euclid(86_400) / 60
}

fn local_midnight(ts: i64, off: i64) -> i64 {
    ts - (ts + off).rem_euclid(86_400)
}

/// The instant of local clock time `minutes` on the local day containing `ts`.
fn at_local(ts: i64, off: i64, minutes: i64) -> i64 {
    local_midnight(ts, off) + minutes * 60
}

// ------------------------------------------------------------------ analysis access

struct Analysis<'a> {
    root: &'a Value,
    days: Vec<&'a Value>,
}

impl<'a> Analysis<'a> {
    fn new(root: &'a Value) -> Self {
        Analysis { root, days: root["days"].as_array().map(|d| d.iter().collect()).unwrap_or_default() }
    }

    fn generated(&self) -> Option<i64> {
        self.root["generated"].as_i64()
    }

    /// Latest analysed night (largest wake time).
    fn last_night(&self) -> Option<&'a Value> {
        self.days.iter().filter(|d| d["night"]["available"] == true).max_by_key(|d| d["night"]["wake_time"].as_i64().unwrap_or(0)).copied()
    }

    fn day(&self, key: &str) -> Option<&'a Value> {
        self.days.iter().find(|d| d["date"] == key).copied()
    }
}

// ------------------------------------------------------------------ SleepSchedulePlanner

#[derive(Debug, Clone, Serialize)]
pub struct Schedule {
    pub wake: Option<i64>,
    /// the wake time comes from an armed Smart Wake session
    pub alarm_armed: bool,
    pub window_start: Option<i64>,
    pub need_min: Option<f64>,
    pub latency_min: Option<f64>,
    pub bedtime: Option<i64>,
    pub wind_down: Option<i64>,
    /// minutes of the need that the regularity limit leaves uncovered
    pub shortfall_min: f64,
    pub debt_7d_min: Option<f64>,
    pub debt_payback_min: Option<f64>,
    pub limited_by_regularity: bool,
}

/// Bedtime = wake − (tonight's need + usual latency), from `smart_wake.plan` (the need already
/// contains a capped share of the 7-night debt: no full catch-up in one night). Regularity: the
/// body rarely falls asleep much earlier than usual, so bedtime moves at most
/// `max_bedtime_advance_min` before the habitual bedtime; the rest is reported as a shortfall
/// instead of an unrealistic bedtime. The alarm itself is never changed.
pub fn plan_schedule(sw: &SmartWakeInput, plan: Option<&Value>, settings: &Settings) -> Schedule {
    let wake = sw.deadline;
    let num_of = |v: &Value| v.as_f64();
    let need = plan.and_then(|p| num_of(&p["recommended_sleep_min"]));
    let latency = plan.and_then(|p| num_of(&p["latency_min"]));
    let debt = plan.and_then(|p| num_of(&p["components"]["debt_7d_min"]));
    let payback = plan.and_then(|p| num_of(&p["components"]["debt_min"]));
    let early_by = plan.and_then(|p| num_of(&p["habitual_bedtime_offset_min"]));
    let mut s = Schedule {
        wake, alarm_armed: sw.armed, window_start: None, need_min: need, latency_min: latency, bedtime: None, wind_down: None,
        shortfall_min: 0.0, debt_7d_min: debt, debt_payback_min: payback, limited_by_regularity: false,
    };
    if sw.armed && sw.max_advance_min > 0 {
        s.window_start = wake.map(|w| w - sw.max_advance_min.clamp(0, 60) * 60);
    }
    if let (Some(w), Some(n)) = (wake, need) {
        let ideal = w - ((n + latency.unwrap_or(15.0)) * 60.0) as i64;
        let mut bed = ideal;
        if let Some(e) = early_by {
            let cap = settings.max_bedtime_advance_min as f64;
            if e > cap {
                bed = ideal + ((e - cap) * 60.0) as i64;
                s.shortfall_min = (e - cap).round();
                s.limited_by_regularity = true;
            }
        }
        s.bedtime = Some(bed);
        // same "décrocher" lead as the app's plan (15 min before bedtime)
        s.wind_down = Some(bed - 15 * 60);
    }
    s
}

// ------------------------------------------------------------------ sleep observation (database)

/// Latest sustained sleep block and sustained wake from the firmware band state (R18), last 18 h.
pub fn observe_sleep(conn: &Connection, now: i64, confirm_min: i64) -> GooseResult<SleepObservation> {
    if conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'whoop_lab_biometric'", [], |_| Ok(())).optional()?.is_none() {
        return Ok(SleepObservation::default());
    }
    let mut st = conn.prepare("SELECT ts, band FROM whoop_lab_biometric WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts")?;
    let rows: Vec<(i64, String)> = st.query_map(params![now - 18 * 3600, now], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect();
    Ok(summarise_bands(&rows, confirm_min * 60))
}

/// Runs of the same state, bridging gaps < 5 min; a run counts when it lasts `confirm_s`.
pub fn summarise_bands(rows: &[(i64, String)], confirm_s: i64) -> SleepObservation {
    let mut obs = SleepObservation { last_sample_ts: rows.last().map(|r| r.0), ..Default::default() };
    let mut runs: Vec<(bool, i64, i64)> = Vec::new(); // (asleep, start, end)
    for (ts, band) in rows {
        let asleep = band == "sleep";
        match runs.last_mut() {
            Some(r) if r.0 == asleep && ts - r.2 <= 300 => r.2 = *ts,
            _ => runs.push((asleep, *ts, *ts)),
        }
    }
    let sustained: Vec<&(bool, i64, i64)> = runs.iter().filter(|r| r.2 - r.1 >= confirm_s).collect();
    if let Some(i) = sustained.iter().rposition(|r| r.0) {
        // merge consecutive sustained sleep runs separated only by short awakenings
        let mut onset = sustained[i].1;
        let mut j = i;
        while j > 0 && sustained[j - 1].0 && sustained[j].1 - sustained[j - 1].2 <= 3600 {
            j -= 1;
            onset = sustained[j].1;
        }
        obs.onset = Some(onset);
        obs.wake = sustained.iter().skip(i + 1).find(|r| !r.0).map(|r| r.1);
    }
    obs
}

fn latest_db_hr(conn: &Connection, now: i64) -> GooseResult<Option<LiveHr>> {
    if conn.query_row("SELECT 1 FROM sqlite_master WHERE name = 'whoop_lab_biometric'", [], |_| Ok(())).optional()?.is_none() {
        return Ok(None);
    }
    Ok(conn
        .query_row("SELECT ts, hr FROM whoop_lab_biometric WHERE hr > 0 AND ts <= ?1 AND ts >= ?2 ORDER BY ts DESC LIMIT 1", params![now, now - 12 * 3600], |r| {
            Ok(LiveHr { ts: r.get(0)?, bpm: r.get::<_, i64>(1)? as f64, source: "R18 (historique synchronisé)".into() })
        })
        .optional()?)
}

// ------------------------------------------------------------------ DailyContextEngine

#[derive(Debug, Clone, Serialize)]
pub struct ContextDecision {
    pub context: Context,
    pub since: i64,
    /// observed event (awakening, onset, …) or a fallback clock time
    pub trigger: String,
    pub awakening: Option<i64>,
    pub evening_start: i64,
}

fn awakening(now: i64, a: &Analysis, sw: &SmartWakeInput, sleep: &SleepObservation, s: &Settings) -> Option<(i64, &'static str)> {
    let horizon = now - 16 * 3600;
    let mut cands: Vec<(i64, &'static str)> = Vec::new();
    if let Some(t) = sw.confirmed_awake_at.filter(|t| *t <= now && *t > horizon) {
        cands.push((t, "réveil confirmé (Smart Wake)"));
    }
    if let Some(w) = sleep.wake.filter(|w| *w <= now - s.confirm_min * 60 && *w > horizon) {
        cands.push((w, "éveil soutenu observé"));
    }
    if let Some(w) = a.last_night().and_then(|n| n["night"]["wake_time"].as_i64()).filter(|w| *w <= now && *w > horizon) {
        cands.push((w, "nuit analysée"));
    }
    // a later sustained sleep block (back to bed, nap) cancels an earlier awakening
    let latest_onset = sleep.onset.filter(|o| sleep.wake.is_none_or(|w| w < *o));
    cands.into_iter().filter(|(t, _)| latest_onset.is_none_or(|o| *t > o)).max_by_key(|(t, _)| *t)
}

/// Night-like onset: not a daytime nap (a block starting 09:00–19:00 and shorter than 3 h).
fn night_onset(now: i64, off: i64, sleep: &SleepObservation, s: &Settings) -> Option<i64> {
    let o = sleep.onset?;
    if sleep.wake.is_some_and(|w| w > o) {
        return None;
    }
    if now - o < s.confirm_min * 60 {
        return None;
    }
    let lm = local_min(o, off);
    let nap = (9 * 60..19 * 60).contains(&lm) && now - o < 3 * 3600;
    (!nap).then_some(o)
}

fn decide_context(now: i64, off: i64, a: &Analysis, sw: &SmartWakeInput, sleep: &SleepObservation, sched: &Schedule, prev: Option<&Previous>, s: &Settings) -> ContextDecision {
    let lm = local_min(now, off);
    // tonight's evening start: wind-down − lead, else the fallback clock time
    let evening_start = sched
        .wind_down
        .map(|w| w - s.evening_lead_min * 60)
        .filter(|e| (e - now).abs() < 20 * 3600)
        .unwrap_or_else(|| at_local(now, off, s.fallback_evening_start));
    let wake = awakening(now, a, sw, sleep, s);
    let mk = |context, since, trigger: &str| ContextDecision { context, since, trigger: trigger.into(), awakening: wake.map(|w| w.0), evening_start };
    // 1. confirmed sleep onset (not a nap), no sustained wake after it
    if let Some(o) = night_onset(now, off, sleep, s) {
        return mk(Context::Night, o, "endormissement observé");
    }
    // 2. confirmed awakening: morning for `morning_duration_min`
    if let Some((w, why)) = wake {
        if now - w < s.morning_duration_min * 60 {
            return mk(Context::Morning, w, why);
        }
    }
    // hysteresis: a night without new evidence (BLE gap, strap off) stays a night until the
    // fallback morning end; a fresh evening is not undone by a later wind-down estimate
    if let Some(p) = prev {
        let recent = now - p.since < 14 * 3600;
        if p.context == Context::Night && recent && wake.is_none_or(|(w, _)| w < p.since) && (lm < s.fallback_morning_end || lm >= 18 * 60) {
            return mk(Context::Night, p.since, "nuit en cours (sans nouvelle donnée)");
        }
        if p.context == Context::Evening && recent && now >= evening_start - 30 * 60 && now < evening_start {
            return mk(Context::Evening, p.since, "soirée (hystérésis)");
        }
    }
    let after_rollover = lm >= s.day_rollover;
    // 3. after midnight without observed onset: evening until the night fallback, then night
    if !after_rollover {
        return if lm >= s.fallback_night_start { mk(Context::Night, at_local(now, off, s.fallback_night_start), "heure de repli (nuit)") } else { mk(Context::Evening, evening_start.min(now), "soirée (avant endormissement)") };
    }
    if now >= evening_start {
        return mk(Context::Evening, evening_start, if sched.wind_down.is_some() { "approche de l'heure de décrocher" } else { "heure de repli (soirée)" });
    }
    // 4. daytime once the morning is over; before the fallback morning end without any awakening
    // seen today, the morning card is shown with its data pending
    let woke_today = wake.is_some_and(|(w, _)| w >= at_local(now, off, s.day_rollover) - 3 * 3600);
    if !woke_today && lm < s.fallback_morning_end {
        return mk(Context::Morning, at_local(now, off, s.day_rollover), "heure de repli (matin)");
    }
    let since = wake.map(|(w, _)| w + s.morning_duration_min * 60).unwrap_or_else(|| at_local(now, off, s.fallback_morning_end));
    mk(Context::Daytime, since.min(now), if woke_today { "fin de la période matinale" } else { "heure de repli (journée)" })
}

// ------------------------------------------------------------------ metric builders

struct Inputs<'a> {
    now: i64,
    off: i64,
    a: Analysis<'a>,
    sw: &'a SmartWakeInput,
    plan: Option<&'a Value>,
    live_hr: Option<LiveHr>,
    activity: Option<&'a Activity>,
    sched: Schedule,
    s: &'a Settings,
}

const SRC_NIGHT: &str = "whoop_lab.analyze · nuit";
const SRC_DAY: &str = "whoop_lab.analyze · journée";
const SRC_PLAN: &str = "smart_wake.plan";
const SRC_SW: &str = "Smart Wake (réglage du réveil)";

fn morning_metrics(i: &Inputs, decision: &ContextDecision) -> (Vec<Metric>, Vec<Metric>) {
    let generated = i.a.generated();
    // the night must belong to this awakening (finalized analysis); otherwise it is pending
    let night_v = i.a.last_night().filter(|d| {
        let w = d["night"]["wake_time"].as_i64().unwrap_or(0);
        decision.awakening.is_none_or(|aw| (w - aw).abs() <= 4 * 3600) && i.now - w < 18 * 3600
    });
    let pending = "nuit pas encore analysée (synchronisation en attente)";
    let algo = |k: &str| format!("whoop_lab_analysis v{} · {k}", i.a.root["algorithm_version"].as_str().or(i.a.root["schema"].as_str()).unwrap_or("?"));
    let mut recovery = Metric::new("recovery", "Récupération", "/100", SRC_NIGHT, algo("baselines + health_indices"));
    let mut sleep = Metric::new("sleep", "Sommeil", "min", SRC_NIGHT, algo("hypnogramme"));
    let mut hrv = Metric::new("hrv", "VFC nuit", "ms", SRC_NIGHT, algo("RMSSD hrv_qc"));
    let mut rhr = Metric::new("resting_hr", "Pouls repos", "bpm", SRC_NIGHT, algo("FC de repos nocturne"));
    let mut secondary = Vec::new();
    let Some(d) = night_v else {
        return (vec![recovery.missing(pending), sleep.missing(pending), hrv.missing(pending), rhr.missing(pending)], secondary);
    };
    let n = &d["night"];
    let wake = n["wake_time"].as_i64();
    let stamp = |m: &mut Metric| {
        m.observed_at = wake;
        m.computed_at = generated;
    };
    for m in [&mut recovery, &mut sleep, &mut hrv, &mut rhr] {
        stamp(m);
    }
    let incomplete = n["sleep_duration_status"] == "INCOMPLETE_LOWER_BOUND";
    // recovery: produced by health_indices / baselines; never fabricated
    let rec = d["indices"]["recovery"]["score"].as_f64().or(n["recovery"]["score"].as_f64());
    recovery = match rec {
        Some(v) => {
            let calibrating = n["baselines"]["hrv_rmssd"]["status"] == "calibrating";
            let mut m = recovery.value(v, num(v, 0), if calibrating { Quality::Calibrating } else { Quality::Derived });
            m.tone = if v >= 67.0 { "good" } else if v < 34.0 { "bad" } else { "neutral" };
            m.detail(calibrating.then(|| "référence en cours".to_string()))
        }
        None => {
            let why = d["indices"]["recovery"]["reason"].as_str().or(n["recovery"]["reason"].as_str()).unwrap_or(if incomplete { "nuit incomplète" } else { "score indisponible" });
            recovery.missing(why)
        }
    };
    let need = n["sleep_score"]["need_min"].as_f64();
    sleep = match n["asleep_min"].as_f64() {
        Some(v) if incomplete => {
            let cov = n["sleep_coverage_pct"].as_f64();
            let mut m = sleep.value(v, format!("≥ {}", dur(v)), Quality::Incomplete);
            m.missing_reason = Some(format!("nuit incomplète{} : durée minimale", cov.map(|c| format!(" ({} % mesurés)", num(c, 0))).unwrap_or_default()));
            m.detail(need.map(|nd| format!("besoin {}", dur(nd))))
        }
        Some(v) => {
            let mut m = sleep.value(v, dur(v), Quality::Derived);
            if let Some(nd) = need {
                m.tone = if v >= nd - 15.0 { "good" } else if v < nd - 60.0 { "bad" } else { "neutral" };
            }
            m.detail(need.map(|nd| format!("besoin {}", dur(nd))))
        }
        None => sleep.missing("durée de sommeil indisponible"),
    };
    let base_detail = |b: &Value, unit: &str, d: usize| -> Option<String> {
        if b["status"] == "calibrating" || b["delta"].is_null() {
            return Some(format!("référence en cours ({} nuits)", b["nights"].as_i64().unwrap_or(0)));
        }
        b["delta"].as_f64().map(|x| format!("{} {unit} vs réf.", signed(x, d)))
    };
    hrv = match n["hrv_night"]["rmssd_ms"].as_f64() {
        Some(v) => {
            let b = &n["baselines"]["hrv_rmssd"];
            let mut m = hrv.value(v, num(v, 0), if b["status"] == "calibrating" { Quality::Calibrating } else { Quality::Measured });
            if let Some(z) = b["z"].as_f64() {
                m.tone = if z <= -1.0 { "bad" } else if z >= 1.0 { "good" } else { "neutral" };
            }
            m.detail(base_detail(b, "ms", 0))
        }
        None => hrv.missing("VFC insuffisante cette nuit (intervalles R-R trop rares ou artefacts)"),
    };
    rhr = match n["resting_hr_bpm"].as_f64() {
        Some(v) => {
            let b = &n["baselines"]["resting_hr"];
            let mut m = rhr.value(v, num(v, 0), if b["status"] == "calibrating" { Quality::Calibrating } else { Quality::Measured });
            if let Some(z) = b["z"].as_f64() {
                m.tone = if z >= 1.0 { "bad" } else if z <= -1.0 { "good" } else { "neutral" };
            }
            m.detail(base_detail(b, "bpm", 0))
        }
        None => rhr.missing("pouls de repos indisponible cette nuit"),
    };
    // secondary, conditional
    if let Some(debt) = i.sched.debt_7d_min.filter(|d| *d >= 60.0) {
        let mut m = Metric::new("sleep_debt", "Dette 7 nuits", "min", SRC_PLAN, "smart_wake.plan · nuits complètes").value(debt, dur(debt), Quality::Derived);
        m.computed_at = Some(i.now);
        m.tone = if debt >= 180.0 { "bad" } else { "neutral" };
        secondary.push(m);
    }
    if let Some(e) = n["efficiency_pct"].as_f64().filter(|_| !incomplete) {
        let mut m = Metric::new("efficiency", "Efficacité", "%", SRC_NIGHT, algo("temps endormi / temps au lit")).value(e, format!("{} %", num(e, 0)), Quality::Derived);
        stamp(&mut m);
        secondary.push(m);
    }
    let rb = &n["baselines"]["respiration"];
    if let (Some(r), Some(z)) = (n["respiration"]["rpm"].as_f64(), rb["z"].as_f64()) {
        if z.abs() >= 2.0 && rb["status"] != "calibrating" {
            let mut m = Metric::new("respiration", "Respiration", "/min", SRC_NIGHT, algo("fréquence respiratoire nocturne")).value(r, num(r, 1), Quality::Estimated);
            stamp(&mut m);
            m.tone = "bad";
            secondary.push(m.detail(rb["delta"].as_f64().map(|x| format!("{} vs réf.", signed(x, 1)))));
        }
    }
    if let Some(sp) = n["spo2"]["median_pct"].as_f64().filter(|v| *v < 94.0) {
        let mut m = Metric::new("spo2", "SpO₂", "%", SRC_NIGHT, algo("SpO₂ nocturne médiane")).value(sp, format!("{} %", num(sp, 0)), Quality::Estimated);
        stamp(&mut m);
        m.tone = "bad";
        secondary.push(m.detail(Some("médiane de la nuit".into())));
    }
    if let Some(t) = n["skin_temp_delta_c"].as_f64().filter(|t| t.abs() >= 0.5) {
        let mut m = Metric::new("skin_temp", "Température", "°C", SRC_NIGHT, algo("écart de température cutanée")).value(t, format!("{} °C", signed(t, 1)), Quality::Estimated);
        stamp(&mut m);
        m.tone = "bad";
        secondary.push(m.detail(Some("vs tes nuits".into())));
    }
    (vec![recovery, sleep, hrv, rhr], secondary)
}

fn hr_metric(i: &Inputs) -> Metric {
    let m = Metric::new("hr", "Fréquence cardiaque", "bpm", "bracelet", "FC mesurée (paquet 40 ou R18)");
    let Some(h) = &i.live_hr else { return m.missing("aucune mesure récente : bracelet non connecté") };
    if i.now - h.ts > 12 * 3600 {
        return m.missing("aucune mesure depuis plus de 12 h");
    }
    let mut m = m.value(h.bpm, num(h.bpm, 0), Quality::Measured);
    m.observed_at = Some(h.ts);
    m.computed_at = Some(h.ts);
    m.live_until = Some(h.ts + i.s.hr_live_s);
    m.valid_until = Some(h.ts + i.s.hr_recent_s);
    m.detail = Some(if i.now - h.ts <= i.s.hr_live_s { "en direct".into() } else { format!("à {}", clock(h.ts, i.off)) });
    m
}

fn today_day<'a>(i: &'a Inputs) -> Option<&'a Value> {
    let key = crate::whoop_lab_analysis::date_key(local_midnight(i.now, i.off), i.off);
    i.a.day(&key).map(|d| &d["day"]).filter(|d| d["coverage_h"].is_number() && d["steps"].is_number())
}

/// Day effort in % as the app shows it: 100 × (1 − e^(−TRIMP/250)), falling back to the 0–21
/// scale rescaled for days analysed before strain_pct existed.
fn strain_pct(day: &Value) -> Option<f64> {
    day["strain_pct"].as_f64().or_else(|| day["strain_0_21"].as_f64().map(|v| v / 21.0 * 100.0))
}

fn strain_metric(i: &Inputs, day: Option<&Value>) -> Metric {
    let m = Metric::new("strain", "Effort", "%", SRC_DAY, "TRIMP d'Edwards → 100 × (1 − e^(−TRIMP/250))");
    let Some(v) = day.and_then(strain_pct) else { return m.missing("pas assez de données aujourd'hui") };
    let mut m = m.value(v, num(v, 0), Quality::Derived);
    m.computed_at = i.a.generated();
    m.observed_at = i.a.generated();
    m.valid_until = i.a.generated().map(|g| g + i.s.day_metric_valid_s);
    // relative to the personal norm: mean of the previous 14 days
    let key = crate::whoop_lab_analysis::date_key(local_midnight(i.now, i.off), i.off);
    let past: Vec<f64> = i.a.days.iter().filter(|d| d["date"].as_str().is_some_and(|k| k < key.as_str())).rev().take(14).filter_map(|d| strain_pct(&d["day"])).collect();
    if past.len() >= 5 {
        let mean = past.iter().sum::<f64>() / past.len() as f64;
        m.detail = Some(format!("moyenne {} %", num(mean, 0)));
        m.tone = if v >= mean + 19.0 { "bad" } else { "neutral" };
    }
    m
}

fn stress_metric(i: &Inputs, day: Option<&Value>) -> Metric {
    let m = Metric::new("stress", "Stress", "/100", SRC_DAY, "FC au repos immobile hors effort (Karvonen)");
    let st = day.map(|d| &d["stress"]).filter(|s| s.is_object());
    let Some(st) = st else { return m.missing("pas assez de repos immobile pour l'estimer (l'effort n'est pas compté)") };
    let (v, obs) = match (st["last"]["score"].as_f64(), st["last"]["ts"].as_i64()) {
        (Some(v), Some(ts)) => (v, Some(ts)),
        _ => match st["mean"].as_f64() {
            Some(v) => (v, None),
            None => return m.missing("stress indisponible"),
        },
    };
    let mut m = m.value(v, num(v, 0), Quality::Estimated);
    m.observed_at = obs.or(i.a.generated());
    m.computed_at = i.a.generated();
    m.valid_until = m.observed_at.map(|o| o + 3600);
    m.tone = if v >= 75.0 { "bad" } else { "neutral" };
    m.detail = Some(if obs.is_some() { format!("hors effort · {}", clock(obs.unwrap(), i.off)) } else { "moyenne du jour, hors effort".into() });
    m
}

fn steps_metric(i: &Inputs, day: Option<&Value>) -> Metric {
    let m = Metric::new("steps", "Pas", "pas", SRC_DAY, "compteur de pas R18 (incréments positifs)");
    let Some(v) = day.and_then(|d| d["steps"].as_f64()) else { return m.missing("pas assez de données aujourd'hui") };
    let mut m = m.value(v, format!("{}", v as i64).chars().rev().collect::<Vec<_>>().chunks(3).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join("\u{202f}").chars().rev().collect(), Quality::Estimated);
    m.computed_at = i.a.generated();
    m.observed_at = i.a.generated();
    m.valid_until = i.a.generated().map(|g| g + i.s.day_metric_valid_s);
    m
}

fn daytime_metrics(i: &Inputs) -> (Vec<Metric>, Vec<Metric>) {
    let day = today_day(i);
    let mut secondary = Vec::new();
    if let Some(act) = i.activity {
        let minutes = ((i.now - act.start_ts) as f64 / 60.0).max(0.0);
        let mut m = Metric::new("activity", "Activité", "min", "sport.engine", if act.confirmed { "séance confirmée" } else { "séance détectée" })
            .value(minutes, format!("{} · {}", if act.title.is_empty() { act.kind.clone() } else { act.title.clone() }, dur(minutes)), if act.confirmed { Quality::Measured } else { Quality::Estimated });
        m.observed_at = Some(act.start_ts);
        m.computed_at = Some(i.now);
        m.live_until = Some(i.now + 300);
        m.valid_until = Some(i.now + 1800);
        secondary.push(m.detail(Some(if act.confirmed { "en cours".into() } else { "probable, en cours".into() })));
    } else if let Some(last) = day.and_then(|d| d["training"]["workouts"]["sessions"].as_array()).and_then(|s| s.last()) {
        let (a, b) = (last["start"].as_i64().unwrap_or(0), last["end"].as_i64().unwrap_or(0));
        if b > a {
            let minutes = (b - a) as f64 / 60.0;
            let kind = last["type"].as_str().unwrap_or("séance").to_string();
            let mut m = Metric::new("activity", "Dernière séance", "min", SRC_DAY, "training::workouts").value(minutes, format!("{kind} · {}", dur(minutes)), Quality::Derived);
            m.observed_at = Some(b);
            m.computed_at = i.a.generated();
            secondary.push(m.detail(Some(format!("finie à {}", clock(b, i.off)))));
        }
    }
    if let Some(high) = day.and_then(|d| d["stress"]["minutes"]["haut"].as_f64()).filter(|v| *v >= 30.0) {
        let mut m = Metric::new("stress_high", "Stress élevé", "min", SRC_DAY, "minutes > 75/100 au repos immobile").value(high, dur(high), Quality::Estimated);
        m.computed_at = i.a.generated();
        m.tone = "bad";
        secondary.push(m.detail(Some("hors effort".into())));
    }
    (vec![hr_metric(i), strain_metric(i, day), stress_metric(i, day), steps_metric(i, day)], secondary)
}

fn schedule_metrics(i: &Inputs) -> Vec<Metric> {
    let sc = &i.sched;
    let off = i.off;
    let mut out = Vec::new();
    let t = |id, label, ts: Option<i64>, src, algo: &str, why: &str| {
        let m = Metric::new(id, label, "heure", src, algo);
        match ts {
            Some(v) => {
                let mut m = m.value(v as f64, clock(v, off), Quality::Estimated);
                m.computed_at = Some(i.now);
                m
            }
            None => m.missing(why),
        }
    };
    out.push(t("wind_down", "Décrocher", sc.wind_down, SRC_PLAN, "coucher − 15 min", "besoin de sommeil ou heure de réveil inconnus"));
    let mut bed = t("bedtime", "Au lit", sc.bedtime, SRC_PLAN, "réveil − (besoin + latence), limité par la régularité", "besoin de sommeil ou heure de réveil inconnus");
    if sc.limited_by_regularity {
        bed.detail = Some(format!("{} de moins que le besoin", dur(sc.shortfall_min)));
    }
    out.push(bed);
    let mut wake = Metric::new("wake", "Réveil", "heure", SRC_SW, "réglage de l'alarme (lecture seule)");
    wake = match sc.wake {
        Some(w) => {
            let mut m = wake.value(w as f64, clock(w, off), if sc.alarm_armed { Quality::Measured } else { Quality::Estimated });
            m.computed_at = Some(i.now);
            m.detail(Some(match (sc.alarm_armed, sc.window_start) {
                (true, Some(ws)) => format!("fenêtre {}–{}", clock(ws, off), clock(w, off)),
                (true, None) => "heure fixe".into(),
                (false, _) => "réveil non programmé".into(),
            }))
        }
        None => wake.missing("aucune heure de réveil réglée"),
    };
    out.push(wake);
    let need = Metric::new("sleep_need", "Besoin", "min", SRC_PLAN, "base personnelle + activité + part de la dette");
    out.push(match sc.need_min {
        Some(n) => {
            let mut m = need.value(n, dur(n), Quality::Estimated);
            m.computed_at = Some(i.now);
            m.detail(sc.debt_payback_min.filter(|p| *p >= 5.0).map(|p| format!("dont {} de dette", dur(p))))
        }
        None => need.missing("plan du soir indisponible"),
    });
    out
}

fn evening_metrics(i: &Inputs) -> (Vec<Metric>, Vec<Metric>) {
    let day = today_day(i);
    let mut secondary = vec![strain_metric(i, day)];
    let st = stress_metric(i, day);
    if st.quality != Quality::Missing {
        let mut s = st;
        // in the evening the whole-day picture matters: mean outside exercise
        if let Some(mean) = day.and_then(|d| d["stress"]["mean"].as_f64()) {
            s.value = Some(mean);
            s.display = num(mean, 0);
            s.detail = Some("moyenne du jour, hors effort".into());
            s.valid_until = None;
        }
        secondary.push(s);
    }
    if let Some(debt) = i.sched.debt_7d_min.filter(|d| *d >= 60.0) {
        let mut m = Metric::new("sleep_debt", "Dette 7 nuits", "min", SRC_PLAN, "smart_wake.plan · nuits complètes").value(debt, dur(debt), Quality::Derived);
        m.computed_at = Some(i.now);
        secondary.push(m);
    }
    (schedule_metrics(i), secondary)
}

fn night_metrics(i: &Inputs) -> (Vec<Metric>, Vec<Metric>) {
    let all = schedule_metrics(i);
    let keep = |id: &str| all.iter().find(|m| m.id == id).cloned();
    (vec![keep("wake").unwrap(), keep("sleep_need").unwrap()], vec![])
}

// ------------------------------------------------------------------ DailyInsightEngine

#[derive(Debug, Clone, Serialize)]
pub struct Insight {
    pub id: &'static str,
    pub text: String,
    pub metric_ids: Vec<&'static str>,
}

fn find<'m>(ms: &'m [Metric], id: &str) -> Option<&'m Metric> {
    ms.iter().find(|m| m.id == id)
}

/// One sentence, the first rule that applies in a fixed priority order (deterministic).
fn insight(context: Context, primary: &[Metric], secondary: &[Metric], i: &Inputs) -> Insight {
    let all: Vec<Metric> = primary.iter().chain(secondary).cloned().collect();
    let v = |id| find(&all, id).and_then(|m| m.value);
    let ins = |id, text: String, ids: Vec<&'static str>| Insight { id, text, metric_ids: ids };
    match context {
        Context::Morning => {
            let rec = find(&all, "recovery");
            if rec.is_some_and(|m| m.quality == Quality::Missing) && find(&all, "sleep").is_some_and(|m| m.quality == Quality::Missing) {
                return ins("night_pending", "Nuit en cours d'analyse : les scores arrivent après la synchronisation.".into(), vec!["recovery"]);
            }
            if let Some(m) = find(&all, "sleep").filter(|m| m.quality == Quality::Incomplete) {
                return ins("night_incomplete", format!("Nuit incomplète : {}, pas de score fiable.", m.display), vec!["sleep"]);
            }
            let need = find(&all, "sleep").and_then(|_| i.a.last_night()).and_then(|d| d["night"]["sleep_score"]["need_min"].as_f64());
            if let (Some(s), Some(n)) = (v("sleep"), need) {
                if n - s >= 60.0 {
                    return ins("short_sleep", format!("Nuit courte : {} de moins que ton besoin.", dur(n - s)), vec!["sleep"]);
                }
            }
            let hz = i.a.last_night().and_then(|d| d["night"]["baselines"]["hrv_rmssd"]["z"].as_f64());
            let rz = i.a.last_night().and_then(|d| d["night"]["baselines"]["resting_hr"]["z"].as_f64());
            if hz.is_some_and(|z| z <= -1.0) && rz.is_some_and(|z| z >= 1.0) {
                return ins("strain_signals", "VFC basse et pouls élevé : récupération incomplète, journée légère conseillée.".into(), vec!["hrv", "resting_hr"]);
            }
            if hz.is_some_and(|z| z <= -1.5) {
                return ins("hrv_low", "VFC nettement sous ta référence : écoute ta fatigue aujourd'hui.".into(), vec!["hrv"]);
            }
            if let Some(m) = secondary.iter().find(|m| ["respiration", "spo2", "skin_temp"].contains(&m.id)) {
                return ins("vital_deviation", format!("{} inhabituelle cette nuit ({}) : à surveiller.", m.label, m.display), vec![m.id]);
            }
            if v("sleep_debt").is_some_and(|d| d >= 180.0) {
                return ins("debt", format!("Dette de sommeil de {} sur 7 nuits : vise un coucher plus tôt ce soir.", dur(v("sleep_debt").unwrap())), vec!["sleep_debt"]);
            }
            match v("recovery") {
                Some(r) if r >= 67.0 => ins("recovered", "Bien récupéré : bonne journée pour un effort soutenu.".into(), vec!["recovery"]),
                Some(r) if r < 34.0 => ins("low_recovery", "Récupération basse : privilégie une journée calme.".into(), vec!["recovery"]),
                Some(_) => ins("moderate", "Récupération moyenne : un effort modéré reste raisonnable.".into(), vec!["recovery"]),
                None => ins("no_score", format!("Pas de score ce matin : {}.", rec.and_then(|m| m.missing_reason.clone()).unwrap_or_else(|| "données insuffisantes".into())), vec!["recovery"]),
            }
        }
        Context::Daytime => {
            if let Some(a) = find(&all, "activity").filter(|m| m.live_until.is_some()) {
                return ins("workout", format!("Séance en cours : {}. La FC élevée vient de l'effort, pas du stress.", a.display), vec!["activity", "hr"]);
            }
            if let Some(h) = v("stress_high") {
                return ins("stress_rest", format!("{} de stress élevé au repos aujourd'hui, hors effort.", dur(h)), vec!["stress_high"]);
            }
            let rec = i.a.last_night().and_then(|d| d["indices"]["recovery"]["score"].as_f64());
            if let (Some(s), Some(r)) = (v("strain"), rec) {
                if r < 34.0 && s >= 57.0 {
                    return ins("load_vs_recovery", "Effort déjà élevé pour ta récupération du jour : lève le pied.".into(), vec!["strain"]);
                }
            }
            if find(&all, "hr").is_some_and(|m| m.quality == Quality::Missing) {
                return ins("disconnected", "Bracelet non connecté : valeurs de la dernière synchronisation.".into(), vec!["hr"]);
            }
            if let Some(m) = find(&all, "strain").filter(|m| m.tone == "bad") {
                return ins("strain_high", format!("Effort au-dessus de ta moyenne ({}).", m.detail.clone().unwrap_or_default()), vec!["strain"]);
            }
            ins("steady", "Journée régulière : rien d'inhabituel pour l'instant.".into(), vec![])
        }
        Context::Evening | Context::Night => {
            let sc = &i.sched;
            if context == Context::Night {
                return match sc.wake {
                    Some(w) if sc.alarm_armed => ins("night", format!("Réveil programmé à {}. Bonne nuit.", clock(w, i.off)), vec!["wake"]),
                    _ => ins("night_no_alarm", "Aucun réveil programmé. Bonne nuit.".into(), vec!["wake"]),
                };
            }
            if !sc.alarm_armed && sc.wake.is_some() {
                return ins("alarm_off", format!("Réveil non programmé : active-le pour {} dans l'app.", clock(sc.wake.unwrap(), i.off)), vec!["wake"]);
            }
            if sc.limited_by_regularity {
                return ins("regularity", format!("Coucher limité à 1 h avant ton heure habituelle : il manquera environ {}.", dur(sc.shortfall_min)), vec!["bedtime"]);
            }
            if let (Some(b), Some(n)) = (sc.bedtime, sc.need_min) {
                if i.now > b {
                    let possible = sc.wake.map(|w| ((w - i.now) as f64 / 60.0 - sc.latency_min.unwrap_or(15.0)).max(0.0));
                    return ins("late", format!("Heure de coucher dépassée : {} de sommeil possibles.", dur(possible.unwrap_or(0.0))), vec!["bedtime", "wake"]);
                }
                if sc.debt_payback_min.is_some_and(|p| p >= 15.0) {
                    return ins("debt_payback", format!("Au lit à {} : {} de sommeil, dont {} pour la dette.", clock(b, i.off), dur(n), dur(sc.debt_payback_min.unwrap())), vec!["bedtime", "sleep_need"]);
                }
                if v("strain").is_some_and(|s| s >= 67.0) {
                    return ins("hard_day", format!("Journée chargée : au lit à {} pour récupérer.", clock(b, i.off)), vec!["bedtime", "strain"]);
                }
                let reg = i.a.root["circadian"]["sleep_regularity_index"].as_f64();
                if reg.is_some_and(|r| r < 60.0) {
                    return ins("irregular", format!("Horaires irréguliers : au lit à {}, lever à heure fixe.", clock(b, i.off)), vec!["bedtime"]);
                }
                return ins("bedtime", format!("Au lit à {} pour {} de sommeil.", clock(b, i.off), dur(n)), vec!["bedtime", "sleep_need"]);
            }
            ins("no_plan", "Règle ton heure de réveil pour un conseil de coucher.".into(), vec!["wake"])
        }
    }
}

// ------------------------------------------------------------------ cards + timeline

#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub context: Context,
    pub title: &'static str,
    /// id of the metric shown large
    pub hero: &'static str,
    pub primary: Vec<Metric>,
    pub secondary: Vec<Metric>,
    pub insight: Insight,
    /// data of a past cycle may not be shown: a future morning/day starts empty
    pub pending: bool,
    /// the snapshot was not refreshed for `snapshot_stale_s`
    pub not_refreshed_since: Option<i64>,
}

fn title(c: Context) -> &'static str {
    match c {
        Context::Morning => "Récupération",
        Context::Daytime => "Journée",
        Context::Evening => "Ce soir",
        Context::Night => "Nuit",
    }
}

fn hero(c: Context) -> &'static str {
    match c {
        Context::Morning => "recovery",
        Context::Daytime => "strain",
        Context::Evening => "bedtime",
        Context::Night => "wake",
    }
}

fn card(context: Context, i: &Inputs, decision: &ContextDecision, at: i64, pending: bool) -> Card {
    let (mut primary, mut secondary) = match context {
        Context::Morning => morning_metrics(i, decision),
        Context::Daytime => daytime_metrics(i),
        Context::Evening => evening_metrics(i),
        Context::Night => night_metrics(i),
    };
    if pending {
        // a new morning/day seen from an older snapshot: nothing from the previous cycle
        let why = if context == Context::Morning { "nuit pas encore analysée (ouvre mn health ou attends la synchronisation)" } else { "pas encore de données pour cette journée" };
        primary = primary.into_iter().map(|m| if m.source == SRC_SW || m.source == SRC_PLAN { m } else { m.missing(why) }).collect();
        secondary.clear();
    }
    let stale_snapshot = at - i.now > i.s.snapshot_stale_s;
    primary = primary.iter().map(|m| m.at(at, i.off)).collect();
    secondary = secondary.iter().map(|m| m.at(at, i.off)).collect();
    if stale_snapshot && !pending {
        for m in primary.iter_mut().chain(secondary.iter_mut()) {
            if m.quality != Quality::Missing && m.source != SRC_SW && m.source != SRC_PLAN {
                m.quality = Quality::Stale;
                m.live = false;
            }
        }
    }
    let mut ins = insight(context, &primary, &secondary, i);
    if pending && context == Context::Morning {
        ins = Insight { id: "night_pending", text: "Nuit en cours d'analyse : les scores arrivent après la synchronisation.".into(), metric_ids: vec!["recovery"] };
    }
    if pending && context == Context::Daytime {
        ins = Insight { id: "day_pending", text: "Nouvelle journée : les mesures arrivent avec la synchronisation.".into(), metric_ids: vec![] };
    }
    // the alarm and the plan do not age; only data cards say they were not refreshed
    let data_card = matches!(context, Context::Morning | Context::Daytime);
    if stale_snapshot && !pending && data_card {
        ins = Insight { id: "not_refreshed", text: format!("Données non actualisées depuis {}.", clock(i.now, i.off)), metric_ids: vec![] };
    }
    let flagged = stale_snapshot && !pending && data_card;
    Card { context, title: title(context), hero: hero(context), primary, secondary, insight: ins, pending, not_refreshed_since: flagged.then_some(i.now) }
}

#[derive(Debug, Clone, Serialize)]
pub struct Transition {
    pub at: i64,
    pub to: Context,
    /// true when the transition is waiting for an event (awakening, sleep onset): `at` is the
    /// expected or fallback time
    pub expected: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub at: i64,
    pub context: Context,
    pub card: Card,
}

fn planned_transitions(i: &Inputs, d: &ContextDecision) -> Vec<Transition> {
    let s = i.s;
    let off = i.off;
    let mut out = Vec::new();
    let next_wake = |from: i64| -> i64 {
        // expected awakening: the alarm, else the fallback morning start of the next day
        i.sched.wake.filter(|w| *w > from).unwrap_or_else(|| {
            let t = at_local(from, off, s.expected_wake_fallback);
            if t > from { t } else { t + 86_400 }
        })
    };
    let mut cur = d.context;
    let mut t = i.now;
    let mut passed_night = cur == Context::Night;
    for _ in 0..6 {
        let (to, at, expected, reason): (Context, i64, bool, &str) = match cur {
            Context::Morning => {
                let fallback = at_local(t, off, s.fallback_morning_end).max(t + 60);
                (Context::Daytime, d.awakening.map(|w| w + s.morning_duration_min * 60).filter(|x| *x > t && !passed_night).unwrap_or(fallback), false, "fin de la période matinale")
            }
            Context::Daytime => {
                let e = if d.evening_start > t && !passed_night { d.evening_start } else { at_local(t, off, s.fallback_evening_start).max(t + 60) };
                (Context::Evening, e, false, "approche de l'heure de décrocher")
            }
            Context::Evening => {
                let mut f = at_local(t, off, s.fallback_night_start);
                if f <= t {
                    f += 86_400;
                }
                (Context::Night, f, true, "endormissement attendu (repli horaire)")
            }
            Context::Night => (Context::Morning, next_wake(t), true, "réveil attendu"),
        };
        out.push(Transition { at, to, expected, reason: reason.into() });
        t = at;
        cur = to;
        if to == Context::Night {
            passed_night = true;
        }
        // stop after the next cycle's daytime has started
        if (passed_night && to == Context::Daytime) || t - i.now > 30 * 3600 {
            break;
        }
    }
    out
}

pub fn snapshot(args: &SnapshotArgs) -> GooseResult<Value> {
    let now = args.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    let off = args.utc_offset_seconds;
    let s = &args.settings;
    let conn = match &args.database_path {
        Some(p) if std::path::Path::new(p).exists() => Some(Connection::open(p)?),
        _ => None,
    };
    // plan for the actual alarm (read-only): computed here when the app did not pass one
    let plan_owned: Option<Value> = match (&args.plan, &conn, args.smart_wake.deadline) {
        (Some(p), _, _) => Some(p.clone()),
        (None, Some(_), Some(w)) => crate::smart_wake::plan(&crate::smart_wake::PlanArgs {
            database_path: args.database_path.clone().unwrap_or_default(), latest_wake: w, utc_offset_seconds: off, now_unix: Some(now),
            max_advance_min: args.smart_wake.max_advance_min,
        })
        .ok(),
        _ => None,
    };
    let sleep = match (&args.sleep, &conn) {
        (Some(o), _) => o.clone(),
        (None, Some(c)) => observe_sleep(c, now, s.confirm_min)?,
        _ => SleepObservation::default(),
    };
    // latest heart rate: the app's live value, else the newest synced R18 sample
    let db_hr = match &conn {
        Some(c) => latest_db_hr(c, now)?,
        None => None,
    };
    let live_hr = match (&args.live_hr, db_hr) {
        (Some(l), Some(d)) => Some(if d.ts > l.ts { d } else { l.clone() }),
        (Some(l), None) => Some(l.clone()),
        (None, d) => d,
    };
    let sched = plan_schedule(&args.smart_wake, plan_owned.as_ref(), s);
    let i = Inputs { now, off, a: Analysis::new(&args.analysis), sw: &args.smart_wake, plan: plan_owned.as_ref(), live_hr, activity: args.activity.as_ref(), sched, s };
    let decision = decide_context(now, off, &i.a, i.sw, &sleep, &i.sched, args.previous.as_ref(), s);
    let transitions = planned_transitions(&i, &decision);
    // timeline: now, each transition, each staleness crossing of the current card, and the
    // "not refreshed" instant; future cycles start empty (pending)
    let current = card(decision.context, &i, &decision, now, false);
    let mut entries = vec![Entry { at: now, context: decision.context, card: current.clone() }];
    let first_change = transitions.first().map_or(i64::MAX, |t| t.at);
    let mut cross: Vec<i64> = current.primary.iter().chain(&current.secondary).flat_map(|m| [m.live_until, m.valid_until]).flatten().filter(|t| *t > now && *t < first_change).collect();
    if now + s.snapshot_stale_s < first_change {
        cross.push(now + s.snapshot_stale_s + 1);
    }
    cross.sort_unstable();
    cross.dedup();
    for t in cross {
        entries.push(Entry { at: t, context: decision.context, card: card(decision.context, &i, &decision, t, false) });
    }
    let mut seen_night = decision.context == Context::Night;
    for tr in &transitions {
        // after a night, morning and daytime belong to a new cycle: no stale reuse
        let pending = seen_night && matches!(tr.to, Context::Morning | Context::Daytime);
        if tr.to == Context::Night {
            seen_night = true;
        }
        entries.push(Entry { at: tr.at, context: tr.to, card: card(tr.to, &i, &decision, tr.at, pending) });
    }
    entries.sort_by_key(|e| e.at);
    Ok(json!({
        "schema": SCHEMA,
        "engine_version": ENGINE_VERSION,
        "generated_at": now,
        "utc_offset_seconds": off,
        "context": decision.context,
        "context_since": decision.since,
        "trigger": decision.trigger,
        "awakening": decision.awakening,
        "schedule": i.sched,
        "transitions": transitions,
        "entries": entries,
        "sources": {
            "analysis_generated": i.a.generated(),
            "plan": i.plan.is_some(),
            "sleep_observation": {"onset": sleep.onset, "wake": sleep.wake, "last_sample_ts": sleep.last_sample_ts},
            "live_hr": i.live_hr.as_ref().map(|h| json!({"ts": h.ts, "source": h.source})),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thursday 2026-11-12, UTC+1. Local 07:00 = 06:00 UTC.
    const OFF: i64 = 3600;
    const MIDNIGHT: i64 = 1_794_438_000; // 2026-11-12 00:00 local
    fn local(h: i64, m: i64) -> i64 {
        MIDNIGHT + h * 3600 + m * 60
    }
    const KEY: &str = "2026-11-12";

    fn night(asleep: f64, hrv: Option<f64>, hrv_z: f64, rhr_z: f64, status: &str) -> Value {
        json!({
            "available": true, "bed_time": local(-1, -15) , "sleep_onset": local(-1, 0), "wake_time": local(6, 55),
            "asleep_min": asleep, "sleep_duration_status": status, "sleep_coverage_pct": if status == "COMPLETE" { 97.0 } else { 78.0 },
            "efficiency_pct": 91.0, "sleep_score": {"need_min": 480.0},
            "hrv_night": {"rmssd_ms": hrv}, "resting_hr_bpm": 52.0,
            "baselines": {
                "hrv_rmssd": {"delta": hrv.map(|_| hrv_z * 8.0), "z": hrv.map(|_| hrv_z), "nights": 12, "status": "stable"},
                "resting_hr": {"delta": rhr_z * 2.0, "z": rhr_z, "nights": 12, "status": "stable"},
                "respiration": {"delta": 0.2, "z": 0.4, "status": "stable"},
            },
            "respiration": {"rpm": 14.1}, "spo2": {"median_pct": 96.0}, "skin_temp_delta_c": 0.1,
        })
    }

    fn analysis(n: Value, recovery: Option<f64>, day: Value) -> Value {
        let mut days: Vec<Value> = (1..=10)
            .map(|k| json!({"date": format!("2026-11-{:02}", 12 - k), "night": {"available": false}, "day": {"strain_0_21": 9.0, "steps": 8000, "coverage_h": 15.0}, "indices": {}}))
            .rev()
            .collect();
        days.push(json!({"date": KEY, "night": n, "day": day, "indices": {"recovery": {"score": recovery, "reason": recovery.map_or(json!("VFC insuffisante cette nuit"), |_| Value::Null)}}}));
        json!({"schema": "whoop-lab.results.v4", "generated": local(7, 5), "days": days, "circadian": {"sleep_regularity_index": 82.0}})
    }

    fn day(strain: f64, steps: i64, stress_last: Option<(f64, i64)>, high_min: f64) -> Value {
        json!({
            "coverage_h": 9.0, "steps": steps, "strain_0_21": strain,
            "stress": {"mean": 28.0, "max": 70.0, "minutes": {"haut": high_min}, "last": stress_last.map(|(s, t)| json!({"score": s, "ts": t}))},
            "training": {"workouts": {"sessions": []}},
        })
    }

    fn plan(need: f64, debt7: f64, payback: f64, early_by: f64) -> Value {
        json!({"recommended_sleep_min": need, "latency_min": 15.0, "components": {"debt_7d_min": debt7, "debt_min": payback}, "habitual_bedtime_offset_min": early_by})
    }

    fn sw(deadline: i64, max_adv: i64) -> SmartWakeInput {
        SmartWakeInput { armed: true, deadline: Some(deadline), max_advance_min: max_adv, state: Some("armed".into()), confirmed_awake_at: None }
    }

    fn args(now: i64, a: Value, sw: SmartWakeInput, p: Value) -> SnapshotArgs {
        SnapshotArgs {
            database_path: None, now_unix: Some(now), utc_offset_seconds: OFF, analysis: a, smart_wake: sw, plan: Some(p),
            live_hr: None, activity: None, sleep: Some(SleepObservation::default()), previous: None, settings: Settings::default(),
        }
    }

    fn run(a: &SnapshotArgs) -> Value {
        snapshot(a).unwrap()
    }

    fn metric<'v>(entry: &'v Value, id: &str) -> &'v Value {
        entry["card"]["primary"].as_array().unwrap().iter().chain(entry["card"]["secondary"].as_array().unwrap()).find(|m| m["id"] == id).unwrap_or(&Value::Null)
    }

    fn tomorrow_7h() -> i64 {
        local(24 + 7, 0)
    }

    #[test]
    fn normal_day_morning_then_daytime_then_evening() {
        let a = analysis(night(470.0, Some(62.0), 0.3, -0.2, "COMPLETE"), Some(74.0), day(6.5, 3200, Some((22.0, local(9, 40))), 0.0));
        let mut x = args(local(7, 20), a.clone(), sw(tomorrow_7h(), 30), plan(480.0, 20.0, 5.0, 10.0));
        let r = run(&x);
        assert_eq!(r["context"], "morning", "{}", r["trigger"]);
        let e0 = &r["entries"][0];
        assert_eq!(metric(e0, "recovery")["display"], "74");
        assert_eq!(metric(e0, "sleep")["display"], "7 h 50");
        assert_eq!(metric(e0, "sleep")["detail"], "besoin 8 h 00");
        assert_eq!(metric(e0, "hrv")["quality"], "measured");
        assert_eq!(e0["card"]["insight"]["id"], "recovered");
        // every metric carries its provenance
        for m in e0["card"]["primary"].as_array().unwrap() {
            assert!(m["source"].is_string() && m["algorithm"].is_string() && m["quality"].is_string(), "{m}");
            assert!(m["computed_at"].is_number(), "{m}");
        }
        // morning → daytime 90 min after the awakening (06:55 + 90 = 08:25)
        assert_eq!(r["transitions"][0]["to"], "daytime");
        assert_eq!(r["transitions"][0]["at"], local(8, 25));
        x.now_unix = Some(local(10, 0));
        let r = run(&x);
        assert_eq!(r["context"], "daytime");
        let e0 = &r["entries"][0];
        assert_eq!(metric(e0, "strain")["display"], "31"); // 6.5 / 21 → %
        assert_eq!(metric(e0, "steps")["display"], "3\u{202f}200");
        assert_eq!(metric(e0, "stress")["value"], 22.0);
        // evening: wind-down = 07:00 − (480 + 15) min − 15 min = 22:30, evening from 21:30
        let r = run(&SnapshotArgs { now_unix: Some(local(21, 45)), ..x.clone() });
        assert_eq!(r["context"], "evening", "{}", r["trigger"]);
        let e0 = &r["entries"][0];
        assert_eq!(metric(e0, "bedtime")["display"], "22:45");
        assert_eq!(metric(e0, "wind_down")["display"], "22:30");
        assert_eq!(metric(e0, "wake")["display"], "07:00");
        assert_eq!(metric(e0, "wake")["detail"], "fenêtre 06:30–07:00");
        assert_eq!(metric(e0, "sleep_need")["display"], "8 h 00");
    }

    #[test]
    fn short_sleep_and_high_debt() {
        let a = analysis(night(330.0, Some(48.0), -0.4, 0.3, "COMPLETE"), Some(41.0), day(3.0, 900, None, 0.0));
        let r = run(&args(local(7, 10), a.clone(), sw(tomorrow_7h(), 30), plan(540.0, 260.0, 60.0, 30.0)));
        let e0 = &r["entries"][0];
        assert_eq!(r["context"], "morning");
        assert_eq!(e0["card"]["insight"]["id"], "short_sleep");
        assert_eq!(e0["card"]["insight"]["text"], "Nuit courte : 2 h 30 de moins que ton besoin.");
        assert_eq!(metric(e0, "sleep_debt")["display"], "4 h 20");
        // evening: the need includes a capped payback (60 min), bedtime moves accordingly
        let r = run(&args(local(21, 0), a, sw(tomorrow_7h(), 30), plan(540.0, 260.0, 60.0, 30.0)));
        let e0 = &r["entries"][0];
        assert_eq!(r["context"], "evening");
        assert_eq!(metric(e0, "bedtime")["display"], "21:45");
        assert_eq!(metric(e0, "sleep_need")["detail"], "dont 1 h 00 de dette");
        assert_eq!(e0["card"]["insight"]["id"], "debt_payback");
    }

    #[test]
    fn missing_hrv_is_never_fabricated() {
        let a = analysis(night(460.0, None, 0.0, 0.0, "COMPLETE"), None, day(2.0, 500, None, 0.0));
        let r = run(&args(local(7, 30), a, sw(tomorrow_7h(), 0), plan(480.0, 0.0, 0.0, 0.0)));
        let e0 = &r["entries"][0];
        let hrv = metric(e0, "hrv");
        assert_eq!(hrv["quality"], "missing");
        assert!(hrv["value"].is_null());
        assert_eq!(hrv["display"], "—");
        assert!(hrv["missing_reason"].as_str().unwrap().contains("VFC insuffisante"));
        let rec = metric(e0, "recovery");
        assert_eq!(rec["quality"], "missing");
        assert_eq!(rec["missing_reason"], "VFC insuffisante cette nuit");
        assert_eq!(e0["card"]["insight"]["id"], "no_score");
    }

    #[test]
    fn ble_disconnection_never_shows_a_live_value() {
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(60.0), day(5.0, 4000, None, 0.0));
        let mut x = args(local(14, 0), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        // last HR 40 min ago (strap out of range since)
        x.live_hr = Some(LiveHr { bpm: 71.0, ts: local(13, 20), source: "whoop.packet40".into() });
        let r = run(&x);
        let hr = metric(&r["entries"][0], "hr");
        assert_eq!(hr["live"], false);
        assert_eq!(hr["quality"], "stale");
        assert_eq!(hr["detail"], "à 13:20");
        // a fresh value is live, then flips to its time in a later timeline entry, then stale
        x.live_hr = Some(LiveHr { bpm: 64.0, ts: local(13, 59), source: "whoop.packet40".into() });
        let r = run(&x);
        let entries = r["entries"].as_array().unwrap();
        assert_eq!(metric(&entries[0], "hr")["live"], true);
        assert_eq!(metric(&entries[0], "hr")["detail"], "en direct");
        let later = entries.iter().find(|e| e["at"].as_i64().unwrap() > local(14, 1)).unwrap();
        assert_eq!(metric(later, "hr")["live"], false);
        assert!(metric(later, "hr")["detail"].as_str().unwrap().starts_with("à "));
        let stale = entries.iter().find(|e| e["at"].as_i64().unwrap() > local(14, 14) && e["context"] == "daytime").unwrap();
        assert_eq!(metric(stale, "hr")["quality"], "stale");
        // no heart rate at all
        x.live_hr = None;
        let r = run(&x);
        assert_eq!(metric(&r["entries"][0], "hr")["quality"], "missing");
        assert_eq!(r["entries"][0]["card"]["insight"]["id"], "disconnected");
    }

    #[test]
    fn intense_workout_is_not_stress() {
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(70.0), day(15.5, 12000, Some((18.0, local(16, 30))), 0.0));
        let mut x = args(local(18, 0), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        x.live_hr = Some(LiveHr { bpm: 168.0, ts: local(17, 59), source: "whoop.packet40".into() });
        x.activity = Some(Activity { kind: "course".into(), title: "Course".into(), start_ts: local(17, 25), confirmed: false });
        let r = run(&x);
        let e0 = &r["entries"][0];
        assert_eq!(r["context"], "daytime");
        assert_eq!(metric(e0, "activity")["display"], "Course · 35 min");
        assert_eq!(e0["card"]["insight"]["id"], "workout");
        // the stress metric stays the calm-period estimate, not the exercise HR
        assert_eq!(metric(e0, "stress")["value"], 18.0);
        assert_eq!(metric(e0, "strain")["tone"], "bad"); // 15.5 vs 9.0 mean
    }

    #[test]
    fn elevated_hr_outside_exercise() {
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(70.0), day(4.0, 5000, Some((81.0, local(15, 50))), 45.0));
        let mut x = args(local(16, 0), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        x.live_hr = Some(LiveHr { bpm: 96.0, ts: local(15, 59), source: "whoop.packet40".into() });
        let r = run(&x);
        let e0 = &r["entries"][0];
        assert_eq!(metric(e0, "stress")["display"], "81");
        assert_eq!(metric(e0, "stress")["tone"], "bad");
        assert_eq!(metric(e0, "stress_high")["display"], "45 min");
        assert_eq!(e0["card"]["insight"]["id"], "stress_rest");
    }

    #[test]
    fn irregular_schedule_limits_bedtime_advance() {
        // habitual bedtime 01:00 but the plan would want 22:45 (135 min earlier)
        let a = analysis(night(400.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(55.0), day(5.0, 5000, None, 0.0));
        // evening follows the realistic bedtime: wind-down 23:45, evening from 22:45
        let r = run(&args(local(22, 0), a.clone(), sw(tomorrow_7h(), 30), plan(480.0, 120.0, 30.0, 135.0)));
        assert_eq!(r["context"], "daytime");
        let r = run(&args(local(23, 0), a, sw(tomorrow_7h(), 30), plan(480.0, 120.0, 30.0, 135.0)));
        let e0 = &r["entries"][0];
        assert_eq!(r["context"], "evening");
        // bedtime only 60 min before the habit: 00:00, 75 min short of the need
        assert_eq!(metric(e0, "bedtime")["display"], "00:00");
        assert_eq!(metric(e0, "bedtime")["detail"], "1 h 15 de moins que le besoin");
        assert_eq!(e0["card"]["insight"]["id"], "regularity");
    }

    #[test]
    fn smart_wake_is_authoritative_and_never_changed() {
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(70.0), day(5.0, 5000, None, 0.0));
        let base = args(local(21, 50), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        let r1 = run(&base);
        // the user moves the alarm to 06:30, fixed time
        let r2 = run(&SnapshotArgs { smart_wake: sw(tomorrow_7h() - 1800, 0), ..base.clone() });
        let (w1, w2) = (metric(&r1["entries"][0], "wake"), metric(&r2["entries"][0], "wake"));
        assert_eq!(w1["display"], "07:00");
        assert_eq!(w2["display"], "06:30");
        assert_eq!(w2["detail"], "heure fixe");
        assert_eq!(r2["schedule"]["wake"], tomorrow_7h() - 1800, "the alarm is read, never moved");
        let (b1, b2) = (metric(&r1["entries"][0], "bedtime")["value"].as_f64().unwrap(), metric(&r2["entries"][0], "bedtime")["value"].as_f64().unwrap());
        assert_eq!(b1 - b2, 1800.0);
        // alarm disabled: the time is shown as not programmed, and said so
        let off = run(&SnapshotArgs { smart_wake: SmartWakeInput { armed: false, ..sw(tomorrow_7h(), 30) }, ..base });
        let w = metric(&off["entries"][0], "wake");
        assert_eq!(w["detail"], "réveil non programmé");
        assert_eq!(w["quality"], "estimated");
        assert_eq!(off["entries"][0]["card"]["insight"]["id"], "alarm_off");
    }

    #[test]
    fn overnight_transition_with_fragmented_sleep_and_ble_gap() {
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(70.0), day(5.0, 5000, None, 0.0));
        let mut x = args(local(23, 30), a.clone(), sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        // asleep since 23:05 (sustained)
        x.sleep = Some(SleepObservation { onset: Some(local(23, 5)), wake: None, last_sample_ts: Some(local(23, 29)) });
        let r = run(&x);
        assert_eq!(r["context"], "night");
        assert_eq!(r["entries"][0]["card"]["insight"]["text"], "Réveil programmé à 07:00. Bonne nuit.");
        // the timeline's next morning is pending: yesterday's recovery must not be reused
        // 7 h later the night card is unchanged: the alarm does not age
        let late_night = r["entries"].as_array().unwrap().iter().filter(|e| e["context"] == "night").last().unwrap();
        assert_eq!(late_night["card"]["not_refreshed_since"], Value::Null);
        let morning = r["entries"].as_array().unwrap().iter().find(|e| e["context"] == "morning").unwrap();
        assert_eq!(morning["at"], tomorrow_7h());
        assert_eq!(morning["card"]["pending"], true);
        assert_eq!(metric(morning, "recovery")["quality"], "missing");
        let next_day = r["entries"].as_array().unwrap().iter().find(|e| e["context"] == "daytime").unwrap();
        assert_eq!(next_day["card"]["insight"]["id"], "day_pending");
        // 03:10: a 12-min awakening (bathroom) does not end the night
        x.now_unix = Some(local(24 + 3, 10));
        x.sleep = Some(SleepObservation { onset: Some(local(23, 5)), wake: Some(local(24 + 3, 0)), last_sample_ts: Some(local(24 + 3, 9)) });
        x.previous = Some(Previous { context: Context::Night, since: local(23, 5) });
        assert_eq!(run(&x)["context"], "night");
        // 04:30: BLE gap, no sleep observation at all: still night (hysteresis)
        x.now_unix = Some(local(24 + 4, 30));
        x.sleep = Some(SleepObservation::default());
        assert_eq!(run(&x)["context"], "night");
        // 07:05: user confirmed awake via Smart Wake → morning
        x.now_unix = Some(local(24 + 7, 5));
        x.smart_wake.confirmed_awake_at = Some(local(24 + 7, 2));
        let r = run(&x);
        assert_eq!(r["context"], "morning");
        assert_eq!(r["trigger"], "réveil confirmé (Smart Wake)");
        // a daytime nap is not a night
        let mut y = args(local(14, 40), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        y.sleep = Some(SleepObservation { onset: Some(local(14, 0)), wake: None, last_sample_ts: Some(local(14, 39)) });
        assert_eq!(run(&y)["context"], "daytime");
    }

    #[test]
    fn stale_snapshot_is_flagged_and_incomplete_night_is_a_lower_bound() {
        let a = analysis(night(310.0, Some(60.0), 0.0, 0.0, "INCOMPLETE_LOWER_BOUND"), None, day(5.0, 5000, Some((20.0, local(10, 50))), 0.0));
        let mut x = args(local(11, 0), a, sw(tomorrow_7h(), 30), plan(480.0, 0.0, 0.0, 0.0));
        x.live_hr = Some(LiveHr { bpm: 66.0, ts: local(10, 59), source: "whoop.packet40".into() });
        let r = run(&x);
        let entries = r["entries"].as_array().unwrap();
        // an entry 6 h after generation flags the whole card as not refreshed
        let late = entries.iter().find(|e| e["at"].as_i64().unwrap() > local(17, 0) && e["context"] == "daytime").expect("not-refreshed entry");
        assert_eq!(late["card"]["not_refreshed_since"], local(11, 0));
        assert_eq!(late["card"]["insight"]["id"], "not_refreshed");
        for m in late["card"]["primary"].as_array().unwrap() {
            assert!(m["quality"] == "stale" || m["quality"] == "missing", "{m}");
            assert_eq!(m["live"], false);
        }
        // morning of an incomplete night: lower bound, no score
        x.now_unix = Some(local(7, 30));
        x.live_hr = None;
        let r = run(&x);
        let s = metric(&r["entries"][0], "sleep");
        assert_eq!(s["quality"], "incomplete");
        assert_eq!(s["display"], "≥ 5 h 10");
        assert_eq!(r["entries"][0]["card"]["insight"]["id"], "night_incomplete");
        assert_eq!(metric(&r["entries"][0], "recovery")["quality"], "missing");
    }

    #[test]
    fn timezone_change_keeps_local_clock_logic() {
        // same instant seen from UTC−5 (travel): 02:20 local → night fallback, not morning
        let a = analysis(night(470.0, Some(60.0), 0.0, 0.0, "COMPLETE"), Some(70.0), day(5.0, 5000, None, 0.0));
        let mut x = args(local(8, 20), a, SmartWakeInput::default(), plan(480.0, 0.0, 0.0, 0.0));
        x.utc_offset_seconds = -5 * 3600;
        x.analysis["days"][10]["night"]["wake_time"] = json!(local(-20, 0)); // old night, not this cycle
        let r = run(&x);
        assert_eq!(r["context"], "night", "{}", r["trigger"]);
    }

    /// WIDGET_FIXTURE_DIR=/tmp/x cargo test --release --lib widget_fixtures -- --ignored
    /// Writes scenario snapshots (for checking the widget design in the simulator).
    #[test]
    #[ignore]
    fn widget_fixtures() {
        let Ok(dir) = std::env::var("WIDGET_FIXTURE_DIR") else { return };
        let a = analysis(night(470.0, Some(62.0), 0.4, -0.3, "COMPLETE"), Some(74.0), day(11.8, 8420, Some((24.0, local(13, 52))), 0.0));
        let mut x = args(local(7, 20), a, sw(tomorrow_7h(), 30), plan(480.0, 70.0, 15.0, 10.0));
        std::fs::write(format!("{dir}/morning.json"), run(&x).to_string()).unwrap();
        x.now_unix = Some(local(14, 0));
        x.live_hr = Some(LiveHr { bpm: 72.0, ts: local(13, 59), source: "whoop.packet40".into() });
        std::fs::write(format!("{dir}/daytime.json"), run(&x).to_string()).unwrap();
        x.now_unix = Some(local(21, 50));
        std::fs::write(format!("{dir}/evening.json"), run(&x).to_string()).unwrap();
    }

    #[test]
    fn band_summary_bridges_short_awakenings() {
        let mut rows = Vec::new();
        for t in 0..7200 {
            rows.push((t, if (3000..3300).contains(&t) { "wake".to_string() } else { "sleep".to_string() }));
        }
        for t in 7200..9000 {
            rows.push((t, "wake".to_string()));
        }
        let o = summarise_bands(&rows, 1200);
        assert_eq!(o.onset, Some(0));
        assert_eq!(o.wake, Some(7200));
    }
}
