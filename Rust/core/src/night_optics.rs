//! Night optical windows: short persistent R20 recordings (cmd 153) during sleep, to see whether
//! the strap samples its SpO2 slots (block 1 = LED 3 on photodiode 1, block 2 = LED 2 on
//! photodiode 1) at night, and what that costs in battery.
//!
//! Timing comes from the strap itself: its R26 bursts (40 s) repeat every 1 161 s (+15 s now and
//! then) while it is in its night configuration, every 1 240 s by day (9-17 Nov: 2 consecutive
//! night-period gaps in 98 % of the 2-6 h bursts, in none from 12 h to 22 h). Windows alternate
//! between "aligned" (around the next predicted burst: does the strap measure SpO2 with its burst?)
//! and "between" (half a period later), 150 s each, about 13 % of the night.
//!
//! The app (NightOptics.swift) sends ON / OFF and keeps the window and battery log; this module
//! plans the next window and reports what each one recorded.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::dsp::{median, round};
use crate::ppg::{bandpass, i32s, ADC_FULL, FS, OFFSET_UA};
use crate::GooseResult;

pub const ALGORITHM_VERSION: &str = "night-optics-1";
/// night burst period range (s)
const NIGHT_GAP: (i64, i64) = (1150, 1195);
pub const WINDOW_S: i64 = 150;
/// aligned window starts this long before the predicted burst (prediction error <= 30 s for 2 periods)
const LEAD_S: i64 = 45;

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

/// Night mode, period and the next windows from the burst starts. `idx` parity (burst time /
/// period) fixes the kind, so successive calls agree on it.
pub fn plan_from(starts: &[i64], now: i64, off: i64) -> Value {
    let hour = (now + off).rem_euclid(86_400) / 3600;
    let night_hours = !(10..21).contains(&hour);
    let gaps: Vec<i64> = starts.windows(2).map(|w| w[1] - w[0]).collect();
    let recent: Vec<i64> = gaps.iter().rev().take(2).copied().collect();
    let night_period = recent.len() == 2 && recent.iter().all(|g| (NIGHT_GAP.0..=NIGHT_GAP.1).contains(g));
    let last = starts.last().copied();
    let fresh = last.is_some_and(|l| now - l < 2 * NIGHT_GAP.1 + 300);
    let night_mode = night_hours && night_period && fresh;
    let reason = if !night_hours {
        "hors des heures de nuit"
    } else if last.is_none() {
        "pas de salve récente"
    } else if !fresh {
        "salves pas encore synchronisées"
    } else if !night_period {
        "bracelet pas en configuration de nuit"
    } else {
        "nuit"
    };
    let mut next = Vec::new();
    if let (true, Some(l)) = (night_mode, last) {
        let p: Vec<f64> = gaps.iter().rev().take(6).filter(|g| (NIGHT_GAP.0..=NIGHT_GAP.1).contains(*g)).map(|g| *g as f64).collect();
        let period = median(&p).unwrap_or(1161.0).round() as i64;
        // k = 0: the last synced burst still owns its "between" window half a period later (17 Nov:
        // starting at k = 1 dropped every between window as soon as its burst was synced)
        for k in 0..=4 {
            let b = l + k * period;
            let idx = (b as f64 / period as f64).round() as i64;
            let (kind, start) = if idx % 2 == 0 { ("aligned", b - LEAD_S) } else { ("between", b + period / 2 - WINDOW_S / 2) };
            if start + WINDOW_S > now {
                next.push(json!({"kind": kind, "burst_ts": b, "start": start, "end": start + WINDOW_S}));
            }
        }
        next.sort_by_key(|w| w["start"].as_i64());
        return json!({"algorithm_version": ALGORITHM_VERSION, "night_mode": true, "reason": reason, "period_s": period, "last_burst": l, "next": next});
    }
    json!({"algorithm_version": ALGORITHM_VERSION, "night_mode": false, "reason": reason, "last_burst": last, "next": next})
}

pub fn plan(args: &PlanArgs) -> GooseResult<Value> {
    let now = now_or(args.now_unix);
    let conn = Connection::open(&args.database_path)?;
    // latest recorded optical frame: lets the app notice a recording left on
    let r20_last: Option<i64> = if table_exists(&conn, "whoop5_r20_slot")? { conn.query_row("SELECT MAX(ts) FROM whoop5_r20_slot", [], |r| r.get(0))? } else { None };
    let secs: Vec<i64> = if table_exists(&conn, "whoop5_r26")? {
        let mut st = conn.prepare("SELECT DISTINCT ts FROM whoop5_r26 WHERE ts >= ?1 AND ts <= ?2 ORDER BY ts")?;
        let v = st.query_map(params![now - 3 * 3600, now], |r| r.get(0))?.collect::<Result<_, _>>()?;
        v
    } else {
        Vec::new()
    };
    let mut v = plan_from(&burst_starts(&secs), now, args.utc_offset_seconds);
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
        windows.push(json!({
            "night": w.night, "kind": w.kind, "start": w.start, "end": w.end, "on_at": on, "off_at": w.off_at, "on_result": w.on_result,
            "synced": synced, "r20_seconds": r20_seconds, "spo2_seconds": spo2_seconds, "burst_seconds": burst_seconds,
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
        let ws: Vec<&Value> = windows.iter().filter(|w| w["night"] == n.night.as_str()).collect();
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
    let done: Vec<&Value> = windows.iter().filter(|w| w["synced"] == true).collect();
    let aligned_spo2 = done.iter().filter(|w| w["kind"] == "aligned" && w["spo2_seconds"].as_i64().unwrap_or(0) > 0).count();
    let between_spo2 = done.iter().filter(|w| w["kind"] == "between" && w["spo2_seconds"].as_i64().unwrap_or(0) > 0).count();
    let recorded = done.iter().filter(|w| w["r20_seconds"].as_i64().unwrap_or(0) > 0).count();
    if !done.is_empty() {
        conclusions.push(format!("{} fenêtres synchronisées, {} avec des trames optiques complètes.", done.len(), recorded));
        if aligned_spo2 + between_spo2 == 0 && recorded > 0 {
            conclusions.push("Aucune fenêtre ne contient les créneaux SpO₂ (LED 3 et LED 2 sur la photodiode 1) : le bracelet ne les transmet pas non plus la nuit dans ce mode.".into());
        } else if aligned_spo2 + between_spo2 > 0 {
            conclusions.push(format!("Créneaux SpO₂ présents dans {aligned_spo2} fenêtres alignées sur une salve et {between_spo2} fenêtres entre deux salves."));
        }
    }
    if let (Some(a), Some(b)) = (on_rate, off_rate) {
        conclusions.push(format!("Batterie : {:.2} %/h les nuits avec fenêtres, {:.2} %/h sans ({:+.2} %/h).", a, b, a - b));
    }
    let test_over = args.nights.last().is_some_and(|n| night_key(now, off) > n.night);
    Ok(json!({"algorithm_version": ALGORITHM_VERSION, "windows": windows, "nights": nights, "battery_on_pct_per_h": on_rate.map(|v| round(v, 3)), "battery_off_pct_per_h": off_rate.map(|v| round(v, 3)), "test_over": test_over, "conclusions": conclusions}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn night_mode_and_alternating_windows() {
        let off = 3600;
        // 02:00 local on 18 Nov 2026 (UTC+1): 4 night bursts, last 5 min ago
        let now = 1_794_963_600;
        let l = now - 300;
        let starts = vec![l - 1176 - 2 * 1161, l - 1176 - 1161, l - 1176, l];
        let p = plan_from(&starts, now, off);
        assert_eq!(p["night_mode"], true, "{p}");
        let next = p["next"].as_array().unwrap();
        assert!(next.len() >= 3);
        let kinds: Vec<&str> = next.iter().map(|w| w["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"aligned") && kinds.contains(&"between"));
        for w in next {
            assert_eq!(w["end"].as_i64().unwrap() - w["start"].as_i64().unwrap(), WINDOW_S);
            assert!(w["end"].as_i64().unwrap() > now);
        }
        // aligned window covers the 40 s burst with the drift margin
        let a = next.iter().find(|w| w["kind"] == "aligned").unwrap();
        let b = a["burst_ts"].as_i64().unwrap();
        assert!(a["start"].as_i64().unwrap() <= b - 30 && a["end"].as_i64().unwrap() >= b + 40 + 30);
        // the between window of the burst just synced is still offered
        let p_after = plan_from(&starts, l + 300, off);
        let between_now = p_after["next"].as_array().unwrap().iter().any(|w| w["kind"] == "between" && w["burst_ts"].as_i64().unwrap() <= l + 1200);
        let aligned_now = p_after["next"].as_array().unwrap().iter().any(|w| w["kind"] == "aligned");
        assert!(between_now || aligned_now);
        let lb = if (l as f64 / 1162.0).round() as i64 % 2 == 1 { l } else { l + 1162 };
        let starts_b: Vec<i64> = starts.iter().map(|t| t + lb - l).collect();
        let pb = plan_from(&starts_b, lb + 300, off);
        assert!(pb["next"].as_array().unwrap().iter().any(|w| w["kind"] == "between" && w["start"].as_i64().unwrap() < lb + 1162), "{pb}");
        // stable kind across calls
        let p2 = plan_from(&starts, now + 120, off);
        let a2 = p2["next"].as_array().unwrap().iter().find(|w| w["burst_ts"] == a["burst_ts"]).unwrap();
        assert_eq!(a2["kind"], a["kind"]);
        // day period: no window
        let day = vec![l - 3 * 1240, l - 2 * 1240, l - 1240, l];
        assert_eq!(plan_from(&day, now, off)["night_mode"], false);
        // 15:00 local: no window even with a night-like period
        assert_eq!(plan_from(&starts, now + 13 * 3600, off)["night_mode"], false);
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
        let p = plan(&PlanArgs { database_path: db.clone(), utc_offset_seconds: 3600, now_unix: Some(1_794_877_200) }).unwrap();
        println!("{p}");
        let first: i64 = Connection::open(&db).unwrap().query_row("SELECT MIN(ts) FROM whoop5_r20_slot", [], |r| r.get(0)).unwrap();
        let w = WindowLog { night: night_key(first, 3600), kind: "aligned".into(), start: first, end: first + 150, on_at: Some(first), off_at: Some(first + 150), on_result: Some(1) };
        let r = report(&ReportArgs { database_path: db, utc_offset_seconds: 3600, windows: vec![w.clone()], battery: vec![], nights: vec![TestNight { night: w.night.clone(), on: true }], now_unix: None }).unwrap();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
    }
}
