//! Night optical windows: short recordings of the full optical frame (R20: the 5 LED time slots on
//! both photodiodes) during sleep, to see the strap's red + infrared pair (block 1 = LED 3, block
//! 2 = LED 2, photodiode 1) while it measures SpO2, and what that costs in battery.
//!
//! The strap's own optical pattern (9-19 Nov, R18 / R26):
//!   - SpO2: exactly 30 s every 1 200 s, the first one 20 min after its sleep onset (onset 23:01
//!     -> 23:21:18; 23:10 -> 23:30:19), only while its band state is "sleep", until waking; the
//!     per-second value (R18 byte 82) is a saturation in 33 of 47 windows, status codes otherwise.
//!   - green bursts (R26): 40 s every 1 161 s at night, 1 240 s by day, on their own schedule (no
//!     lock to the SpO2 windows: offsets -544..+540 s).
//!   - infrared contact sensing (LED 2 at 2 mA) and green heart rate: always.
//! Night 1 (17 Nov) lined its windows up on the green bursts and sent the record flag alone
//! (cmd 153): none overlapped an SpO2 window and the strap wrote no frame (its console logged the
//! flag; on 10 Nov frames came with ENABLE_OPTICAL_DATA 107 sent too).
//!
//! Windows now: around each predicted SpO2 window (20 s before -> 20 s after, 70 s), and a control
//! window halfway between two SpO2 windows every third cycle (is the pair ever sampled outside
//! them?). The app sends 107 + 153 (mode A) and escalates to the live optical mode 108 as well
//! (mode B) when a synced mode-A window brought no frame.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dsp::{median, round};
use crate::ppg::{bandpass, i32s, ADC_FULL, FS, OFFSET_UA};
use crate::GooseResult;

pub const ALGORITHM_VERSION: &str = "night-optics-2";
/// strap SpO2 schedule
pub const SPO2_PERIOD_S: i64 = 1200;
pub const SPO2_LEN_S: i64 = 30;
const SPO2_FIRST_AFTER_ONSET_S: i64 = 1200;
/// window margins around a predicted SpO2 window, in phone time: the strap clock runs a few seconds
/// behind (19 Nov: frames stamped 9 s before the command that opened them), so more margin after
const PRE_S: i64 = 20;
const POST_S: i64 = 30;
/// first window of the night, predicted from the sleep onset only: wider
const FIRST_PRE_S: i64 = 60;
const FIRST_POST_S: i64 = 60;
const CONTROL_LEN_S: i64 = 60;
/// kept for the window-length checks of the report
pub const WINDOW_S: i64 = SPO2_LEN_S + PRE_S + POST_S;

#[derive(Debug, Deserialize)]
pub struct PlanArgs {
    pub database_path: String,
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

fn now_or(n: Option<i64>) -> i64 {
    n.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
}

fn table_exists(c: &Connection, t: &str) -> GooseResult<bool> {
    Ok(c.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [t], |_| Ok(true)).optional()?.unwrap_or(false))
}

/// burst start times from the distinct R26 seconds (a new burst after > 5 s without samples)
pub fn burst_starts(seconds: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    let mut last = i64::MIN;
    for &t in seconds {
        if last == i64::MIN || t - last > 5 {
            out.push(t);
        }
        last = t;
    }
    out
}

/// SpO2 window starts from the seconds whose R18 SpO2 byte is set (value or status code)
pub fn spo2_starts(seconds: &[i64]) -> Vec<i64> {
    burst_starts(seconds)
}

/// Start of the current sustained "sleep" run of the strap's band state (>= 5 min), if any.
pub fn sleep_onset(bands: &[(i64, bool)], now: i64) -> Option<i64> {
    let mut onset: Option<i64> = None;
    let mut last_t = i64::MIN;
    for &(t, asleep) in bands {
        if asleep {
            if onset.is_none() || t - last_t > 300 {
                onset = Some(t);
            }
            last_t = t;
        } else if last_t != i64::MIN && t - last_t > 120 {
            onset = None;
        }
    }
    onset.filter(|o| last_t - o >= 300 && now - last_t < 900)
}

/// Next windows from the observed SpO2 window starts (or the sleep onset before the first one).
pub fn plan_from(spo2: &[i64], onset: Option<i64>, now: i64, off: i64) -> Value {
    let hour = (now + off).rem_euclid(86_400) / 3600;
    let night_hours = !(10..21).contains(&hour);
    let last = spo2.last().copied().filter(|l| now - l < 2 * SPO2_PERIOD_S + 600);
    let mut next: Vec<Value> = Vec::new();
    let reason;
    if !night_hours {
        reason = "hors des heures de nuit";
    } else if let Some(l) = last {
        reason = "nuit (SpO2 du bracelet)";
        for k in 0..=4 {
            let s = l + k * SPO2_PERIOD_S;
            if k > 0 {
                next.push(json!({"kind": "spo2", "spo2_ts": s, "start": s - PRE_S, "end": s + SPO2_LEN_S + POST_S}));
            }
            // control: halfway to the next SpO2 window, every third cycle
            if (s / SPO2_PERIOD_S) % 3 == 0 {
                let c = s + SPO2_PERIOD_S / 2;
                next.push(json!({"kind": "control", "spo2_ts": s, "start": c - CONTROL_LEN_S / 2, "end": c + CONTROL_LEN_S / 2}));
            }
        }
    } else if let Some(o) = onset {
        reason = "endormi, première mesure SpO2 prévue";
        let s = o + SPO2_FIRST_AFTER_ONSET_S;
        next.push(json!({"kind": "spo2_first", "spo2_ts": s, "start": s - FIRST_PRE_S, "end": s + SPO2_LEN_S + FIRST_POST_S}));
    } else {
        reason = "bracelet pas en sommeil";
    }
    next.retain(|w| w["end"].as_i64().unwrap_or(0) > now);
    next.sort_by_key(|w| w["start"].as_i64());
    json!({"algorithm_version": ALGORITHM_VERSION, "night_mode": !next.is_empty(), "reason": reason, "last_spo2": last, "sleep_onset": onset, "period_s": SPO2_PERIOD_S, "next": next})
}

pub fn plan(args: &PlanArgs) -> GooseResult<Value> {
    let now = now_or(args.now_unix);
    let conn = Connection::open(&args.database_path)?;
    // latest recorded optical frame: lets the app notice a recording left on
    let r20_last: Option<i64> = if table_exists(&conn, "whoop5_r20_slot")? { conn.query_row("SELECT MAX(ts) FROM whoop5_r20_slot", [], |r| r.get(0))? } else { None };
    let (spo2, bands): (Vec<i64>, Vec<(i64, bool)>) = if table_exists(&conn, "whoop5_r18")? {
        let mut st = conn.prepare("SELECT ts, sleep_adjacent_raw, band_state FROM whoop5_r18 WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts")?;
        let rows: Vec<(i64, Option<i64>, Option<String>)> = st.query_map(params![now - 3 * 3600, now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
        (
            spo2_starts(&rows.iter().filter(|r| r.1.unwrap_or(0) > 0).map(|r| r.0).collect::<Vec<_>>()),
            rows.iter().map(|r| (r.0, r.2.as_deref() == Some("sleep"))).collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let mut v = plan_from(&spo2, sleep_onset(&bands, now), now, args.utc_offset_seconds);
    v["r20_last_ts"] = json!(r20_last);
    Ok(v)
}

// MARK: report

#[derive(Debug, Deserialize, Clone)]
pub struct WindowLog {
    pub night: String,
    pub kind: String,
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub on_at: Option<i64>,
    #[serde(default)]
    pub off_at: Option<i64>,
    /// strap answer to the ON command (1 = success)
    #[serde(default)]
    pub on_result: Option<i64>,
    /// "A": ENABLE_OPTICAL_DATA + persistent R20; "B": + live optical mode; absent: flag only (night 1)
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BatterySample {
    pub ts: i64,
    pub pct: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TestNight {
    pub night: String,
    pub on: bool,
}

#[derive(Debug, Deserialize)]
pub struct ReportArgs {
    pub database_path: String,
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub windows: Vec<WindowLog>,
    #[serde(default)]
    pub battery: Vec<BatterySample>,
    #[serde(default)]
    pub nights: Vec<TestNight>,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

/// (photocurrent uA per sample) of one channel over consecutive seconds; the offset of a frame
/// applies from the next one
fn photocurrent(frames: &[(i64, Vec<f64>, f64, f64)]) -> Vec<f64> {
    let mut x = Vec::new();
    for (k, f) in frames.iter().enumerate() {
        let off = if k == 0 { f.3 } else { frames[k - 1].3 };
        x.extend(f.1.iter().map(|v| v * f.2 / ADC_FULL + off * OFFSET_UA));
    }
    x
}

/// level (uA), pulse amplitude (uA), pulsatility (%)
pub fn level_pulse(x: &[f64]) -> Option<(f64, f64, f64)> {
    if x.len() < 25 * 20 {
        return None;
    }
    let dc = median(x)?;
    let m = x.iter().sum::<f64>() / x.len() as f64;
    let y = bandpass(&x.iter().map(|v| v - m).collect::<Vec<_>>(), FS, 0.5, 4.0);
    let y = &y[25..y.len() - 25];
    let pp: Vec<f64> = y
        .chunks(38)
        .filter(|w| w.len() == 38)
        .map(|w| w.iter().copied().fold(f64::NEG_INFINITY, f64::max) - w.iter().copied().fold(f64::INFINITY, f64::min))
        .collect();
    let ac = median(&pp)?;
    (dc > 0.05).then(|| (dc, ac, 100.0 * ac / dc))
}

/// local night key: the date of the evening (ts shifted by 12 h)
pub fn night_key(ts: i64, off: i64) -> String {
    let d = (ts + off - 12 * 3600).div_euclid(86_400);
    crate::weekly::date_key(d)
}

type Frames = BTreeMap<(i64, String), Vec<(i64, Vec<f64>, f64, f64)>>;

fn window_frames(conn: &Connection, from: i64, to: i64) -> GooseResult<(Frames, BTreeMap<(i64, String), (i64, i64, i64)>)> {
    let mut st = conn.prepare("SELECT ts, block, slot, source_raw, drive_raw, detector_raw, range_raw, offset_raw, count, samples FROM whoop5_r20_slot WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts")?;
    let mut frames: Frames = BTreeMap::new();
    let mut cfg: BTreeMap<(i64, String), (i64, i64, i64)> = BTreeMap::new();
    let rows = st.query_map(params![from, to], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?, r.get::<_, i64>(7)?, r.get::<_, i64>(8)?, r.get::<_, Vec<u8>>(9)?))
    })?;
    for row in rows {
        let (ts, block, slot, src, drive, det, range, offset, count, samples) = row?;
        cfg.insert((block, slot.clone()), (src, drive, det));
        if count > 0 {
            frames.entry((block, slot)).or_default().push((ts, i32s(&samples), range as f64, offset as f64));
        }
    }
    Ok((frames, cfg))
}

pub fn report(args: &ReportArgs) -> GooseResult<Value> {
    let off = args.utc_offset_seconds;
    let now = now_or(args.now_unix);
    let conn = Connection::open(&args.database_path)?;
    let has_r20 = table_exists(&conn, "whoop5_r20_slot")?;
    let has_r26 = table_exists(&conn, "whoop5_r26")?;
    let synced_until: Option<i64> = if table_exists(&conn, "whoop5_r18")? { conn.query_row("SELECT MAX(ts) FROM whoop5_r18", [], |r| r.get(0))? } else { None };
    let mut windows = Vec::new();
    for w in &args.windows {
        let Some(on) = w.on_at else { continue };
        let to = w.off_at.unwrap_or(w.end).max(on) + 5;
        let synced = synced_until.is_some_and(|s| s >= to);
        let (frames, cfg) = if has_r20 { window_frames(&conn, on, to)? } else { Default::default() };
        let secs = |b: i64, s: &str| frames.get(&(b, s.to_string())).map(|v| v.len()).unwrap_or(0);
        let r20_seconds = secs(0, "a");
        let spo2_seconds = secs(1, "a").max(secs(2, "a"));
        let burst_seconds: i64 = if has_r26 { conn.query_row("SELECT COUNT(DISTINCT ts) FROM whoop5_r26 WHERE ts >= ?1 AND ts <= ?2", params![on, to], |r| r.get(0))? } else { 0 };
        let chan = |b: i64, s: &str| -> Value {
            let lp = frames.get(&(b, s.to_string())).and_then(|f| level_pulse(&photocurrent(f)));
            let c = cfg.get(&(b, s.to_string()));
            json!({
                "block": b, "slot": s, "seconds": secs(b, s),
                "led": c.map(|c| c.0), "led_ma": c.map(|c| round(c.1 as f64 * 0.01, 1)), "photodiode": c.map(|c| c.2),
                "photocurrent_ua": lp.map(|v| round(v.0, 3)), "pulse_ua": lp.map(|v| round(v.1, 4)), "pi_pct": lp.map(|v| round(v.2, 3)),
            })
        };
        let red = chan(1, "a");
        let ir = chan(2, "a");
        let ratio = match (red["pi_pct"].as_f64(), ir["pi_pct"].as_f64()) {
            (Some(r), Some(i)) if i > 0.0 => Some(round(r / i, 3)),
            _ => None,
        };
        // the strap's own SpO2 seconds inside the window (R18 byte 82: 70-100 = %, else status)
        let (strap_secs, strap_vals): (i64, Vec<f64>) = if table_exists(&conn, "whoop5_r18")? {
            let v: Vec<i64> = conn.prepare_cached("SELECT sleep_adjacent_raw FROM whoop5_r18 WHERE ts >= ?1 AND ts <= ?2 AND sleep_adjacent_raw > 0")?
                .query_map(params![on, to], |r| r.get(0))?.collect::<Result<_, _>>()?;
            (v.len() as i64, v.iter().filter(|x| (70..=100).contains(*x)).map(|x| *x as f64).collect())
        } else {
            (0, Vec::new())
        };
        windows.push(json!({
            "night": w.night, "kind": w.kind, "mode": w.mode, "start": w.start, "end": w.end, "on_at": on, "off_at": w.off_at, "on_result": w.on_result,
            "synced": synced, "r20_seconds": r20_seconds, "spo2_seconds": spo2_seconds, "burst_seconds": burst_seconds,
            "strap_spo2_seconds": strap_secs, "strap_spo2_pct": median(&strap_vals).map(|v| round(v, 1)), "strap_spo2_valid": strap_vals.len(),
            "green": chan(0, "a"), "red": red, "ir": ir, "contact_ir": chan(4, "a"), "ratio_of_ratios": ratio,
        }));
    }

    // battery per night: 21 h -> 10 h local, discharge only (a rise = charging, the night stops there)
    let mut by_night: BTreeMap<String, Vec<&BatterySample>> = BTreeMap::new();
    for b in &args.battery {
        let h = (b.ts + off).rem_euclid(86_400) / 3600;
        if !(10..21).contains(&h) {
            by_night.entry(night_key(b.ts, off)).or_default().push(b);
        }
    }
    let mut nights = Vec::new();
    for n in &args.nights {
        let mut s: Vec<&BatterySample> = by_night.get(&n.night).cloned().unwrap_or_default();
        s.sort_by_key(|b| b.ts);
        let mut seg: Vec<&BatterySample> = Vec::new();
        for b in s {
            if seg.last().is_some_and(|l| b.pct > l.pct) {
                break;
            }
            seg.push(b);
        }
        let rate = match (seg.first(), seg.last()) {
            (Some(a), Some(z)) if z.ts - a.ts >= 4 * 3600 => Some(round((a.pct - z.pct) / ((z.ts - a.ts) as f64 / 3600.0), 3)),
            _ => None,
        };
        let ws: Vec<&Value> = windows.iter().filter(|w| w["night"] == n.night.as_str() && w["kind"] != "test").collect();
        let recorded: i64 = ws.iter().map(|w| w["r20_seconds"].as_i64().unwrap_or(0)).sum();
        let spo2: i64 = ws.iter().map(|w| w["spo2_seconds"].as_i64().unwrap_or(0)).sum();
        let ratios: Vec<f64> = ws.iter().filter_map(|w| w["ratio_of_ratios"].as_f64()).collect();
        nights.push(json!({
            "night": n.night, "on": n.on, "windows": ws.len(), "r20_seconds": recorded, "spo2_seconds": spo2,
            "ratio_of_ratios": median(&ratios).map(|v| round(v, 3)),
            "battery_pct_per_h": rate, "battery_hours": seg.first().zip(seg.last()).map(|(a, z)| round((z.ts - a.ts) as f64 / 3600.0, 1)),
            "battery_from": seg.first().map(|b| b.pct), "battery_to": seg.last().map(|b| b.pct),
        }));
    }
    let mean = |on: bool| -> Option<f64> {
        let v: Vec<f64> = nights.iter().filter(|n| n["on"] == on).filter_map(|n| n["battery_pct_per_h"].as_f64()).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    };
    let (on_rate, off_rate) = (mean(true), mean(false));
    let mut conclusions: Vec<String> = Vec::new();
    let done: Vec<&Value> = windows.iter().filter(|w| w["synced"] == true && w["on_result"] == 1 && w["kind"] != "test").collect();
    let n_of = |pred: &dyn Fn(&&Value) -> bool| done.iter().filter(|w| pred(w)).count();
    let recorded = n_of(&|w| w["r20_seconds"].as_i64().unwrap_or(0) > 0);
    let pair_in_spo2 = n_of(&|w| w["kind"].as_str().unwrap_or("").starts_with("spo2") && w["spo2_seconds"].as_i64().unwrap_or(0) > 0);
    let pair_in_control = n_of(&|w| w["kind"] == "control" && w["spo2_seconds"].as_i64().unwrap_or(0) > 0);
    let spo2_recorded = n_of(&|w| w["kind"].as_str().unwrap_or("").starts_with("spo2") && w["r20_seconds"].as_i64().unwrap_or(0) > 0);
    let control_recorded = n_of(&|w| w["kind"] == "control" && w["r20_seconds"].as_i64().unwrap_or(0) > 0);
    // escalation: a synced, acknowledged mode-A window that brought no frame -> try mode B
    let a_empty = n_of(&|w| w["mode"] == "A" && w["r20_seconds"].as_i64().unwrap_or(0) == 0);
    let a_ok = n_of(&|w| w["mode"] == "A" && w["r20_seconds"].as_i64().unwrap_or(0) > 0);
    let escalate = a_empty >= 2 && a_ok == 0;
    if !done.is_empty() {
        conclusions.push(format!("{} fenêtres synchronisées, {} avec des trames optiques complètes.", done.len(), recorded));
        if spo2_recorded > 0 {
            conclusions.push(format!("Paire rouge + infrarouge présente dans {pair_in_spo2} des {spo2_recorded} fenêtres enregistrées pendant une mesure SpO₂ du bracelet."));
        }
        if control_recorded > 0 {
            conclusions.push(format!("Hors mesure SpO₂ : paire présente dans {pair_in_control} des {control_recorded} fenêtres témoins."));
        }
        if escalate {
            conclusions.push("Activer le flux optique avec l'enregistrement n'a pas suffi : les fenêtres suivantes ajoutent le mode optique en direct.".into());
        }
    }
    // red / infrared ratio against the strap's own SpO2 of the same seconds: a personal calibration
    // line once there are enough points
    let pts: Vec<(f64, f64)> = done.iter().filter_map(|w| Some((w["ratio_of_ratios"].as_f64()?, w["strap_spo2_pct"].as_f64()?))).collect();
    let calibration = (pts.len() >= 3).then(|| {
        let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / pts.len() as f64, pts.iter().map(|p| p.1).sum::<f64>() / pts.len() as f64);
        let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
        let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let a = my - b * mx;
        let rmse = (pts.iter().map(|p| (p.1 - (a + b * p.0)).powi(2)).sum::<f64>() / pts.len() as f64).sqrt();
        json!({"points": pts.len(), "intercept": round(a, 2), "slope": round(b, 2), "rmse_pct": round(rmse, 2)})
    });
    if let Some(c) = &calibration {
        conclusions.push(format!("Rapport rouge / infrarouge contre la SpO₂ du bracelet : {} points, écart résiduel {} %.", c["points"], c["rmse_pct"]));
    }
    if let (Some(a), Some(b)) = (on_rate, off_rate) {
        conclusions.push(format!("Batterie : {:.2} %/h les nuits avec fenêtres, {:.2} %/h sans ({:+.2} %/h).", a, b, a - b));
    }
    let test_over = args.nights.last().is_some_and(|n| night_key(now, off) > n.night);
    Ok(json!({"algorithm_version": ALGORITHM_VERSION, "windows": windows, "nights": nights, "battery_on_pct_per_h": on_rate.map(|v| round(v, 3)), "battery_off_pct_per_h": off_rate.map(|v| round(v, 3)), "test_over": test_over, "escalate": escalate, "calibration": calibration, "conclusions": conclusions}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_lock_onto_the_strap_spo2_schedule() {
        let off = 3600;
        // 17 Nov: SpO2 windows at 23:21:18 + k * 1200; now 00:05 local (18 Nov)
        let first = 1_794_957_678; // 23:21:18 local
        let starts = vec![first, first + 1200, first + 2400];
        let now = first + 2400 + 240;
        let p = plan_from(&starts, None, now, off);
        assert_eq!(p["night_mode"], true, "{p}");
        let next = p["next"].as_array().unwrap();
        let spo2: Vec<&Value> = next.iter().filter(|w| w["kind"] == "spo2").collect();
        // next SpO2 window 00:21:18: window 00:20:58 -> 00:22:08
        assert_eq!(spo2[0]["start"], first + 3600 - 20);
        assert_eq!(spo2[0]["end"], first + 3600 + 60);
        // one control window every third cycle, halfway between two SpO2 windows
        let controls = next.iter().filter(|w| w["kind"] == "control").count();
        assert!((1..=2).contains(&controls), "{p}");
        // before the first SpO2 window: predicted from the sleep onset (+20 min)
        let onset = first - 1200;
        let p = plan_from(&[], Some(onset), onset + 600, off);
        let w = &p["next"][0];
        assert_eq!(w["kind"], "spo2_first");
        assert!(w["start"].as_i64().unwrap() <= first - 30 && w["end"].as_i64().unwrap() >= first + 60, "{p}");
        // daytime or awake: nothing
        assert_eq!(plan_from(&starts, None, now + 12 * 3600, off)["night_mode"], false);
        assert_eq!(plan_from(&[], None, now, off)["night_mode"], false);
    }

    #[test]
    fn sleep_onset_from_band_state() {
        let mut b: Vec<(i64, bool)> = (0..600).map(|t| (t, false)).collect();
        b.extend((600..1500).map(|t| (t, true)));
        assert_eq!(sleep_onset(&b, 1500), Some(600));
        assert_eq!(sleep_onset(&b[..700], 700), None); // under 5 min
    }

    #[test]
    fn bursts_from_seconds() {
        let mut s: Vec<i64> = (0..40).collect();
        s.extend(1161..1201);
        assert_eq!(burst_starts(&s), vec![0, 1161]);
    }

    #[test]
    fn night_key_is_the_evening() {
        // 02:00 local 18 Nov -> night of 17 Nov
        assert_eq!(night_key(1_794_963_600, 3600), "2026-11-17");
    }

    #[test]
    #[ignore]
    fn real_plan_and_report() {
        let Ok(db) = std::env::var("GOOSE_PPG_DB") else { return };
        // 17 Nov 02:00 local
        let p = plan(&PlanArgs { database_path: db.clone(), utc_offset_seconds: 3600, now_unix: Some(1_794_966_000) }).unwrap();
        println!("{p}");
        let first: i64 = Connection::open(&db).unwrap().query_row("SELECT MIN(ts) FROM whoop5_r20_slot", [], |r| r.get(0)).unwrap();
        let w = WindowLog { night: night_key(first, 3600), kind: "aligned".into(), start: first, end: first + 150, on_at: Some(first), off_at: Some(first + 150), on_result: Some(1), mode: Some("A".into()) };
        let r = report(&ReportArgs { database_path: db, utc_offset_seconds: 3600, windows: vec![w.clone()], battery: vec![], nights: vec![TestNight { night: w.night.clone(), on: true }], now_unix: None }).unwrap();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }

    #[test]
    #[ignore]
    fn real_spo2_coverage() {
        // GOOSE_NIGHT_DB: whoop5_r18 of 17-19 Nov. The app plans every 2 min with the history synced
        // up to 5 min behind; a window is taken when its start falls within the next 60 s.
        let Ok(db) = std::env::var("GOOSE_NIGHT_DB") else { return };
        let c = Connection::open(&db).unwrap();
        let rows: Vec<(i64, i64, String)> = c.prepare("SELECT ts, COALESCE(sleep_adjacent_raw, 0), COALESCE(band_state, '') FROM whoop5_r18 ORDER BY ts").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|r| r.unwrap()).collect();
        let actual = spo2_starts(&rows.iter().filter(|r| r.1 > 0).map(|r| r.0).collect::<Vec<_>>());
        let (t0, t1) = (rows.first().unwrap().0, rows.last().unwrap().0);
        let mut taken: Vec<(i64, i64, String)> = Vec::new();
        let mut now = t0 + 3 * 3600;
        while now < t1 {
            let seen = now - 300;
            let lo = rows.partition_point(|r| r.0 < now - 3 * 3600);
            let hi = rows.partition_point(|r| r.0 <= seen);
            let w = &rows[lo..hi];
            let sp = spo2_starts(&w.iter().filter(|r| r.1 > 0).map(|r| r.0).collect::<Vec<_>>());
            let bands: Vec<(i64, bool)> = w.iter().map(|r| (r.0, r.2 == "sleep")).collect();
            let p = plan_from(&sp, sleep_onset(&bands, seen), now, 3600);
            for x in p["next"].as_array().unwrap() {
                let (a, b) = (x["start"].as_i64().unwrap(), x["end"].as_i64().unwrap());
                if a >= now - 20 && a < now + 120 && !taken.iter().any(|t| t.0 == a) {
                    taken.push((a, b, x["kind"].as_str().unwrap().to_string()));
                }
            }
            now += 120;
        }
        let covered = actual.iter().filter(|s| taken.iter().any(|t| t.0 <= **s && t.1 >= **s + 30)).count();
        let kinds = taken.iter().fold(std::collections::BTreeMap::new(), |mut m, t| { *m.entry(t.2.clone()).or_insert(0) += 1; m });
        println!("actual SpO2 windows {}, fully covered {}, windows taken {:?}, minutes recorded {}", actual.len(), covered, kinds, taken.iter().map(|t| t.1 - t.0).sum::<i64>() / 60);
    }
}
