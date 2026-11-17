//! Weekly summary (Saturday -> Friday), shown in Indicateurs and sent as a notification every
//! Friday evening: sessions by type, energy, steps, sleep, recovery, HRV, resting HR, strain, and
//! the change from the previous week. Inputs: the daily analysis the app already holds
//! (`whoop_lab.analyze` result: days with night / day / indices) and `sport_sessions`.
//! Nothing is estimated for missing days: averages are over the days that have the value, and
//! the number of days is reported with each.

use crate::GooseResult;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const ALGORITHM_VERSION: &str = "weekly-1";
const DAY: i64 = 86_400;
/// a day counts for calories and steps only when the strap was worn most of it (today: so far)
const MIN_COVERAGE_H: f64 = 12.0;

#[derive(Debug, Deserialize)]
pub struct SummaryArgs {
    pub database_path: String,
    /// `whoop_lab.analyze` result (days: [{date, night, day, indices}])
    #[serde(default)]
    pub analysis: Value,
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
    /// 0 = the week ending this Friday (Mon-Fri) or last Friday (Sat / Sun); 1 = the one before...
    #[serde(default)]
    pub weeks_back: i64,
}

/// [start, end) local day numbers (days since 1970-01-01) of the Saturday -> Friday week, and
/// whether it is over.
pub fn week_bounds(now: i64, off: i64, weeks_back: i64) -> (i64, i64, bool) {
    let today = (now + off).div_euclid(DAY);
    // Monday = 0 ... Friday = 4, Saturday = 5, Sunday = 6 (1970-01-01 was a Thursday)
    let wd = (today + 3).rem_euclid(7);
    let end = if wd <= 4 { today + (4 - wd) + 1 } else { today - (wd - 5) } - 7 * weeks_back;
    (end - 7, end, (now + off).div_euclid(DAY) >= end)
}

pub fn date_key(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
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

const MONTHS: [&str; 12] = ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."];

fn label(start: i64, end: i64) -> String {
    let (_, m1, d1) = civil_from_days(start);
    let (_, m2, d2) = civil_from_days(end - 1);
    if m1 == m2 {
        format!("{d1} – {d2} {}", MONTHS[(m2 - 1) as usize])
    } else {
        format!("{d1} {} – {d2} {}", MONTHS[(m1 - 1) as usize], MONTHS[(m2 - 1) as usize])
    }
}

#[derive(Debug, Default, Clone)]
struct Acc {
    sum: f64,
    n: usize,
}

impl Acc {
    fn add(&mut self, v: Option<f64>) {
        if let Some(v) = v.filter(|v| v.is_finite()) {
            self.sum += v;
            self.n += 1;
        }
    }
    fn mean(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
    fn total(&self) -> Option<f64> {
        (self.n > 0).then_some(self.sum)
    }
}

fn r(v: Option<f64>, d: i32) -> Value {
    v.map_or(Value::Null, |v| {
        let f = 10f64.powi(d);
        json!((v * f).round() / f)
    })
}

#[derive(Debug, Clone)]
struct Session {
    kind: String,
    start: i64,
    minutes: f64,
    reps: Option<f64>,
}

fn sessions(conn: &Connection, from: i64, to: i64) -> GooseResult<Vec<Session>> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sport_sessions'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !exists {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(
        "SELECT COALESCE(label_type, predicted_type, 'séance'), start_ts, COALESCE(end_ts, start_ts), summary_json FROM sport_sessions
         WHERE start_ts >= ?1 AND start_ts < ?2 AND status != 'discarded' AND status != 'active' AND label_status != 'rejected' ORDER BY start_ts",
    )?;
    let rows = st.query_map(params![from, to], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<String>>(3)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, s, e, summary) = row?;
        if kind == "aucune" {
            continue;
        }
        let summary: Value = summary.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or(Value::Null);
        let minutes = summary["duration_min"].as_f64().unwrap_or((e - s) as f64 / 60.0);
        out.push(Session { kind, start: s, minutes, reps: summary["reps"].as_f64() });
    }
    Ok(out)
}

/// Metrics of one week from the analysis days and the sessions.
fn week(days: &BTreeMap<String, &Value>, sess: &[Session], start: i64, end: i64, today: i64) -> Value {
    let (mut total, mut active, mut steps, mut strain) = (Acc::default(), Acc::default(), Acc::default(), Acc::default());
    let (mut sleep, mut rec, mut hrv, mut rhr) = (Acc::default(), Acc::default(), Acc::default(), Acc::default());
    let mut best: Option<(String, f64)> = None;
    for d in start..end.min(today + 1) {
        let Some(v) = days.get(&date_key(d)) else { continue };
        let day = &v["day"];
        let worn = day["coverage_h"].as_f64().unwrap_or(0.0);
        if worn >= MIN_COVERAGE_H || (d == today && worn > 0.0) {
            total.add(day["energy"]["total_kcal"].as_f64());
            active.add(day["energy"]["active_kcal"].as_f64());
            steps.add(day["steps"].as_f64());
            if let Some(a) = day["energy"]["active_kcal"].as_f64() {
                if best.as_ref().is_none_or(|b| a > b.1) {
                    best = Some((date_key(d), a));
                }
            }
        }
        strain.add(day["strain_pct"].as_f64());
        let n = &v["night"];
        if n["available"] == true {
            sleep.add(n["asleep_min"].as_f64());
            hrv.add(n["hrv_night"]["rmssd_ms"].as_f64());
            rhr.add(n["resting_hr_bpm"].as_f64());
        }
        rec.add(v["indices"]["recovery"]["score"].as_f64());
    }
    let mut by: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    let mut n_sessions = 0;
    let mut minutes = 0.0;
    let mut reps = 0.0;
    for s in sess {
        n_sessions += 1;
        minutes += s.minutes;
        reps += s.reps.unwrap_or(0.0);
        let e = by.entry(s.kind.clone()).or_default();
        e.0 += 1;
        e.1 += s.minutes;
    }
    let mut types: Vec<Value> = by.iter().map(|(k, (c, m))| json!({"type": k, "count": c, "minutes": m.round()})).collect();
    types.sort_by(|a, b| b["minutes"].as_f64().partial_cmp(&a["minutes"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "sessions": {"count": n_sessions, "minutes": minutes.round(), "by_type": types, "reps": if reps > 0.0 { json!(reps) } else { Value::Null }},
        "energy": {"total_kcal": r(total.total(), 0), "active_kcal": r(active.total(), 0), "per_day_kcal": r(total.mean(), 0), "days": total.n,
                   "best_day": best.map(|b| json!({"date": b.0, "active_kcal": b.1.round()}))},
        "steps": {"total": r(steps.total(), 0), "per_day": r(steps.mean(), 0), "days": steps.n},
        "sleep": {"mean_min": r(sleep.mean(), 0), "nights": sleep.n},
        "recovery": {"mean": r(rec.mean(), 0), "days": rec.n},
        "hrv": {"mean_ms": r(hrv.mean(), 1), "nights": hrv.n},
        "resting_hr": {"mean_bpm": r(rhr.mean(), 1), "nights": rhr.n},
        "strain": {"mean_pct": r(strain.mean(), 0), "days": strain.n},
    })
}

fn dur(min: f64) -> String {
    let m = min.round() as i64;
    if m >= 60 {
        format!("{} h {:02}", m / 60, m % 60)
    } else {
        format!("{m} min")
    }
}

fn thousands(v: f64) -> String {
    let n = v.round() as i64;
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

fn kind_label(k: &str) -> String {
    match k {
        "musculation" => "muscu".into(),
        "course" => "course".into(),
        "tennis" => "tennis".into(),
        "sauna" => "sauna".into(),
        "velo" | "vélo" => "vélo".into(),
        other => other.to_string(),
    }
}

/// Notification text (title, body): sessions first, then energy, sleep, recovery, with the change
/// from the previous week where both are known.
fn text(cur: &Value, prev: &Value, lbl: &str) -> (String, String) {
    let n = cur["sessions"]["count"].as_i64().unwrap_or(0);
    let pn = prev["sessions"]["count"].as_i64().unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    if n == 0 {
        lines.push("Aucune séance enregistrée".into());
    } else {
        let types: Vec<String> = cur["sessions"]["by_type"].as_array().into_iter().flatten().map(|t| format!("{} {}", t["count"], kind_label(t["type"].as_str().unwrap_or("")))).collect();
        let diff = n - pn;
        let cmp = if diff > 0 { format!(" (+{diff})") } else if diff < 0 { format!(" ({diff})") } else { String::new() };
        lines.push(format!("{n} séance{}{cmp} · {} · {}", if n > 1 { "s" } else { "" }, dur(cur["sessions"]["minutes"].as_f64().unwrap_or(0.0)), types.join(", ")));
    }
    if let Some(t) = cur["energy"]["total_kcal"].as_f64() {
        let a = cur["energy"]["active_kcal"].as_f64().map(|a| format!(" dont {} actives", thousands(a))).unwrap_or_default();
        lines.push(format!("{} kcal{a}", thousands(t)));
    }
    let mut tail: Vec<String> = Vec::new();
    if let Some(s) = cur["sleep"]["mean_min"].as_f64() {
        tail.push(format!("sommeil {} / nuit", dur(s)));
    }
    if let Some(rv) = cur["recovery"]["mean"].as_f64() {
        let d = prev["recovery"]["mean"].as_f64().map(|p| rv - p).filter(|d| d.abs() >= 3.0).map(|d| format!(" ({:+.0})", d)).unwrap_or_default();
        tail.push(format!("récup {rv:.0}{d}"));
    }
    if let Some(st) = cur["steps"]["per_day"].as_f64() {
        tail.push(format!("{} pas / jour", thousands(st)));
    }
    if !tail.is_empty() {
        let mut t = tail.join(" · ");
        if let Some(c) = t.get(0..1) {
            t = c.to_uppercase() + &t[1..];
        }
        lines.push(t);
    }
    (format!("Ta semaine · {lbl}"), lines.join("\n"))
}

pub fn summary(args: &SummaryArgs) -> GooseResult<Value> {
    let now = args.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    let off = args.utc_offset_seconds;
    let (start, end, complete) = week_bounds(now, off, args.weeks_back.max(0));
    let today = (now + off).div_euclid(DAY);
    let days: BTreeMap<String, &Value> = args.analysis["days"].as_array().into_iter().flatten().filter_map(|d| d["date"].as_str().map(|k| (k.to_string(), d))).collect();
    let conn = if std::path::Path::new(&args.database_path).exists() { Some(Connection::open(&args.database_path)?) } else { None };
    let load = |a: i64, b: i64| -> GooseResult<Vec<Session>> {
        match &conn {
            Some(c) => sessions(c, a * DAY - off, b * DAY - off),
            None => Ok(Vec::new()),
        }
    };
    let cur_s = load(start, end)?;
    let prev_s = load(start - 7, start)?;
    let cur = week(&days, &cur_s, start, end, today);
    let prev = week(&days, &prev_s, start - 7, start, today);
    let lbl = label(start, end);
    let (title, body) = text(&cur, &prev, &lbl);
    // Friday 19:00 local: when the notification is due
    let notify_at = (end - 1) * DAY - off + 19 * 3600;
    Ok(json!({
        "algorithm_version": ALGORITHM_VERSION,
        "week": {"start": date_key(start), "end": date_key(end - 1), "label": lbl, "complete": complete, "start_ts": start * DAY - off, "end_ts": end * DAY - off},
        "current": cur,
        "previous": prev,
        "sessions": cur_s.iter().map(|s| json!({"type": s.kind, "start_ts": s.start, "minutes": s.minutes.round()})).collect::<Vec<_>>(),
        "notification": {"title": title, "body": body, "at": notify_at, "id": format!("weekly-{}", date_key(end - 1))},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tuesday 17 Nov 2026 14:00 local (UTC+1)
    const NOW: i64 = 1_794_920_400;
    const OFF: i64 = 3600;

    #[test]
    fn bounds_saturday_to_friday() {
        let (s, e, done) = week_bounds(NOW, OFF, 0);
        assert_eq!((date_key(s).as_str(), date_key(e - 1).as_str(), done), ("2026-11-14", "2026-11-20", false));
        // Saturday 21 Nov: the week that just ended
        let (s, e, done) = week_bounds(NOW + 4 * DAY, OFF, 0);
        assert_eq!((date_key(s).as_str(), date_key(e - 1).as_str(), done), ("2026-11-14", "2026-11-20", true));
        // Friday 20 Nov 23:30 local: still this week
        let (_, e, _) = week_bounds(NOW + 3 * DAY + 9 * 3600 + 1800, OFF, 0);
        assert_eq!(date_key(e - 1), "2026-11-20");
        let (s, _, _) = week_bounds(NOW, OFF, 1);
        assert_eq!(date_key(s), "2026-11-07");
        assert_eq!(label(s + 7, s + 14), "14 – 20 nov.");
    }

    fn day(date: &str, total: f64, active: f64, steps: f64, asleep: Option<f64>, rec: Option<f64>) -> Value {
        json!({"date": date, "day": {"coverage_h": 20.0, "energy": {"total_kcal": total, "active_kcal": active}, "steps": steps, "strain_pct": 40.0},
               "night": {"available": asleep.is_some(), "asleep_min": asleep, "hrv_night": {"rmssd_ms": 50.0}, "resting_hr_bpm": 48.0},
               "indices": {"recovery": {"score": rec}}})
    }

    #[test]
    fn summary_counts_sessions_and_days() {
        let dir = std::env::temp_dir().join(format!("weekly-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("w.sqlite");
        let _ = std::fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sport_sessions (id TEXT PRIMARY KEY, start_ts INTEGER NOT NULL, end_ts INTEGER, status TEXT NOT NULL, source TEXT NOT NULL, predicted_type TEXT, summary_json TEXT, label_type TEXT, label_status TEXT NOT NULL DEFAULT 'pending');",
        )
        .unwrap();
        let ins = |id: &str, s: i64, min: i64, st: &str, ty: &str, label: Option<&str>, ls: &str| {
            conn.execute("INSERT INTO sport_sessions VALUES (?1, ?2, ?3, ?4, 'auto', ?5, NULL, ?6, ?7)", params![id, s, s + min * 60, st, ty, label, ls]).unwrap();
        };
        // this week: gym Sat 14 (corrected), sauna Sun 15, a discarded bout, a rejected one
        ins("a", 1_794_656_160, 43, "ended", "musculation", None, "pending");
        ins("b", 1_794_741_480, 59, "ended", "tennis", Some("musculation"), "corrected");
        ins("c", 1_794_745_680, 14, "ended", "sauna", Some("sauna"), "confirmed");
        ins("d", 1_794_907_020, 8, "discarded", "course", None, "auto_discarded");
        ins("e", 1_794_562_380, 10, "ended", "autre", Some("aucune"), "rejected");
        // previous week: tennis Wed 11
        ins("f", 1_794_394_500, 98, "ended", "tennis", Some("tennis"), "confirmed");
        drop(conn);
        let analysis = json!({"days": [
            day("2026-11-11", 3400.0, 1300.0, 14000.0, Some(560.0), Some(70.0)),
            day("2026-11-14", 2900.0, 900.0, 11000.0, Some(440.0), Some(55.0)),
            day("2026-11-15", 3100.0, 1100.0, 12000.0, Some(450.0), Some(60.0)),
            day("2026-11-16", 2600.0, 600.0, 9000.0, Some(370.0), Some(48.0)),
            day("2026-11-17", 1500.0, 300.0, 4000.0, Some(385.0), None),
        ]});
        let v = summary(&SummaryArgs { database_path: db.to_string_lossy().into(), analysis, utc_offset_seconds: OFF, now_unix: Some(NOW), weeks_back: 0 }).unwrap();
        let c = &v["current"];
        assert_eq!(c["sessions"]["count"], 3);
        assert_eq!(c["sessions"]["by_type"][0]["type"], "musculation");
        assert_eq!(c["sessions"]["by_type"][0]["count"], 2);
        assert_eq!(c["energy"]["total_kcal"], 10_100.0);
        assert_eq!(c["energy"]["days"], 4);
        assert_eq!(c["energy"]["best_day"]["date"], "2026-11-15");
        assert_eq!(c["sleep"]["nights"], 4);
        assert_eq!(c["recovery"]["mean"], 54.0);
        assert_eq!(v["previous"]["sessions"]["count"], 1);
        assert_eq!(v["week"]["complete"], false);
        assert_eq!(v["notification"]["id"], "weekly-2026-11-20");
        // Friday 20 Nov 19:00 local
        assert_eq!(v["notification"]["at"], 1_795_197_600);
        let body = v["notification"]["body"].as_str().unwrap();
        assert!(body.starts_with("3 séances (+2) · 1 h 56 · 2 muscu, 1 sauna"), "{body}");
        assert!(body.contains("10\u{202f}100 kcal dont 2\u{202f}900 actives"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
