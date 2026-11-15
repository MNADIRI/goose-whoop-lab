//! Dry sauna detection from the strap's three R18 thermometers (docs/SENSORS.md, "Temperature").
//!
//! Worn normally the skin thermistor reads 29-35 degC and the two board sensors sit 2-5 degC lower
//! (heat flows from the wrist outwards). In a hot room the gradient inverts: the outer sensor
//! (aux 1) climbs 2-2.5 degC/min past the skin and stays above it (15 Nov sauna: aux 1 45.4, aux 2
//! 42.8, skin 40.8 degC) while the heart rate rises with no movement. Charging on the battery pack
//! also heats the strap (13 Nov: aux 2 led, aux 1 reached 43 degC) but the heart rate stays at rest
//! and CHARGING_ON / BATTERY_PACK events bracket it, so both are required to be absent.
//! A hot bath or a steam room gives the same picture and is reported as a sauna too.

use super::*;

/// a minute is "hot" when the outer sensor is above this and above the skin by INVERSION_C
const HOT_AUX_C: f64 = 38.0;
const INVERSION_C: f64 = 1.0;
/// entry / exit: the outer sensor moves faster than this (degC per minute)
const RAMP_C_PER_MIN: f64 = 1.0;
const MIN_HOT_MIN: usize = 5;
const MAX_GAP_MIN: i64 = 2;
/// median heart rate over the hot minutes, above the awake resting heart rate
const HR_RISE_BPM: f64 = 20.0;
/// CHARGING_ON/OFF, EXTERNAL_5V_ON/OFF, BATTERY_PACK_CONNECTED/REMOVED, BATTERY_PACK_INFO
const CHARGE_EVENTS: &str = "5,6,7,8,21,22,109";
const CHARGE_MARGIN_S: i64 = 10 * 60;
pub const SAUNA_SOURCE: &str = "auto_heat";

#[derive(Debug, Clone, Copy, Default)]
pub struct TempMinute {
    pub t: i64,
    pub n: usize,
    pub skin: Option<f64>,
    pub aux1: Option<f64>,
    pub aux2: Option<f64>,
    pub hr: Option<f64>,
    pub hr_max: Option<f64>,
    pub dyn_g: Option<f64>,
}

impl TempMinute {
    fn hot(&self) -> bool {
        matches!((self.aux1, self.skin), (Some(a), Some(s)) if a >= HOT_AUX_C && a >= s + INVERSION_C && s >= 34.0)
    }
}

pub fn load_minutes(conn: &Connection, from: i64, to: i64) -> GooseResult<Vec<TempMinute>> {
    let mut st = conn.prepare_cached(
        "SELECT ts / 60, COUNT(*), AVG(skin_temp_c), AVG(aux_temp1_c), AVG(aux_temp2_c), AVG(NULLIF(hr, 0)), MAX(hr), AVG(dyn_accel_g)
         FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2 GROUP BY ts / 60 ORDER BY ts / 60",
    )?;
    let rows = st.query_map(params![from, to], |r| {
        Ok(TempMinute {
            t: r.get::<_, i64>(0)? * 60,
            n: r.get::<_, i64>(1)? as usize,
            skin: r.get(2)?,
            aux1: r.get(3)?,
            aux2: r.get(4)?,
            hr: r.get(5)?,
            hr_max: r.get::<_, Option<i64>>(6)?.filter(|v| *v > 0).map(|v| v as f64),
            dyn_g: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Debug, Clone)]
pub struct Sauna {
    pub start: i64,
    pub end: i64,
    pub summary: Value,
}

/// Hot blocks -> sauna sessions. `charging` = timestamps of charge / battery-pack events.
pub fn detect(mins: &[TempMinute], rest_hr: f64, charging: &[i64]) -> Vec<Sauna> {
    let mins: Vec<&TempMinute> = mins.iter().filter(|m| m.n >= 20).collect();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, m) in mins.iter().enumerate() {
        if !m.hot() {
            continue;
        }
        match blocks.last_mut() {
            Some(b) if m.t - mins[b.1].t <= 60 * (MAX_GAP_MIN + 1) => b.1 = i,
            _ => blocks.push((i, i)),
        }
    }
    let slope = |i: usize| -> Option<f64> {
        (i > 0 && mins[i].t - mins[i - 1].t == 60).then_some(())?;
        Some(mins[i].aux1? - mins[i - 1].aux1?)
    };
    let mut out = Vec::new();
    for (a, b) in blocks {
        let hot: Vec<&&TempMinute> = mins[a..=b].iter().filter(|m| m.hot()).collect();
        if hot.len() < MIN_HOT_MIN {
            continue;
        }
        // entry: back to the minute before the steep rise; exit: the minute before the steep fall
        let mut s = a;
        while s > 0 && slope(s).is_some_and(|d| d >= RAMP_C_PER_MIN) {
            s -= 1;
        }
        let mut e = b;
        while e > a && slope(e).is_some_and(|d| d <= -RAMP_C_PER_MIN) {
            e -= 1;
        }
        let (start, end) = (mins[s].t, mins[e].t + 60);
        if charging.iter().any(|t| *t >= start - CHARGE_MARGIN_S && *t <= end + CHARGE_MARGIN_S) {
            continue;
        }
        let inside: Vec<&&TempMinute> = mins[s..=e].iter().collect();
        let hr: Vec<f64> = hot.iter().filter_map(|m| m.hr).collect();
        let Some(hr_med) = median(&hr) else { continue };
        if hr_med < rest_hr + HR_RISE_BPM {
            continue;
        }
        let maxf = |f: &dyn Fn(&TempMinute) -> Option<f64>| inside.iter().filter_map(|m| f(m)).fold(f64::NAN, f64::max);
        let hr_in: Vec<f64> = inside.iter().filter_map(|m| m.hr).collect();
        let first_hr = inside.iter().take(2).filter_map(|m| m.hr).collect::<Vec<_>>();
        let skin_38 = inside.iter().filter(|m| m.skin.is_some_and(|v| v >= 38.0)).count();
        let peak_rise = (s + 1..=e).filter_map(slope).fold(f64::NAN, f64::max);
        let still = inside.iter().filter(|m| m.dyn_g.is_some_and(|d| d < 0.15)).count() as f64 / inside.len() as f64;
        out.push(Sauna {
            start,
            end,
            summary: json!({
                "kind": "sauna",
                "duration_min": round((end - start) as f64 / 60.0, 1),
                "hr_mean": mean(&hr_in).map(|v| round(v, 0)),
                "hr_max": maxf(&|m| m.hr_max).is_finite().then(|| maxf(&|m| m.hr_max)),
                "hr_start": mean(&first_hr).map(|v| round(v, 0)),
                "heat": {
                    "skin_max_c": round(maxf(&|m| m.skin), 1),
                    "outer_max_c": round(maxf(&|m| m.aux1), 1),
                    "inner_max_c": round(maxf(&|m| m.aux2), 1),
                    "skin_above_38_min": skin_38,
                    "hot_minutes": hot.len(),
                    "entry_rise_c_per_min": round(peak_rise, 1),
                    "still_frac": round(still, 2),
                },
                "profile": {"rest_hr": round(rest_hr, 0)},
                "hr_series": inside.iter().filter_map(|m| m.hr.map(|h| json!([m.t, round(h, 0)]))).collect::<Vec<_>>(),
                "temp_series": inside.iter().map(|m| json!([m.t, m.skin.map(|v| round(v, 2)), m.aux1.map(|v| round(v, 1))])).collect::<Vec<_>>(),
                "method": "R18 outer sensor >= 38 degC and >= skin + 1 degC for >= 5 min, heart rate >= rest + 20 bpm, no charging",
            }),
        });
    }
    out
}

fn charge_events(conn: &Connection, from: i64, to: i64) -> GooseResult<Vec<i64>> {
    crate::whoop5_store::ensure_tables(conn)?;
    let sql = format!("SELECT ts FROM whoop5_events WHERE event_id IN ({CHARGE_EVENTS}) AND ts >= ?1 AND ts < ?2");
    let v = conn.prepare(&sql)?.query_map(params![from, to], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(v)
}

/// Scans [from, to) and stores new saunas as ended sessions (type "sauna", to confirm like any
/// detected session). A sauna the user deleted is not stored again; an automatic sport session
/// running into it is cut at the sauna's entry. Returns the ids created.
pub fn sync(conn: &Connection, from: i64, to: i64, rest_hr: f64, now: i64) -> GooseResult<Vec<String>> {
    let mins = load_minutes(conn, from, to)?;
    if mins.iter().all(|m| !m.hot()) {
        return Ok(Vec::new());
    }
    let charging = charge_events(conn, from - CHARGE_MARGIN_S, to + CHARGE_MARGIN_S)?;
    let mut created = Vec::new();
    for s in detect(&mins, rest_hr, &charging) {
        // still inside: wait for the exit (the block must be followed by cooler minutes)
        if s.end > to - 120 {
            continue;
        }
        let id = format!("sauna-{}", s.start);
        let known: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM sport_sessions WHERE source = ?1 AND start_ts < ?3 AND COALESCE(end_ts, start_ts) > ?2)
                  + (SELECT COUNT(*) FROM sport_annotations WHERE target = 'session' AND target_id = ?4 AND field = 'deleted')",
            params![SAUNA_SOURCE, s.start, s.end, id],
            |r| r.get(0),
        )?;
        if known > 0 {
            continue;
        }
        conn.execute(
            "UPDATE sport_sessions SET end_ts = ?1, updated_at = ?2 WHERE source = 'auto' AND start_ts < ?1 AND COALESCE(end_ts, ?2) > ?1 AND status = 'ended'",
            params![s.start, now],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO sport_sessions (id, start_ts, end_ts, status, source, predicted_type, probs_json, confidence, summary_json, engine_version, model_version, label_status, analyzed_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'ended', ?4, 'sauna', ?5, ?6, ?7, ?8, 'heat-1', 'pending', ?9, ?9, ?9)",
            params![id, s.start, s.end, SAUNA_SOURCE, json!({"sauna": 1.0}).to_string(), 0.9, s.summary.to_string(), ENGINE_VERSION, now],
        )?;
        created.push(id);
    }
    Ok(created)
}

/// The last minutes are a hot room (used to keep the sport engine from opening an "effort
/// soutenu" session on the sauna's heart rate).
pub fn hot_now(conn: &Connection, now: i64) -> GooseResult<bool> {
    let mins = load_minutes(conn, now - 10 * 60, now + 1)?;
    Ok(mins.iter().filter(|m| m.hot()).count() >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(t: i64, skin: f64, aux1: f64, aux2: f64, hr: f64) -> TempMinute {
        TempMinute { t, n: 60, skin: Some(skin), aux1: Some(aux1), aux2: Some(aux2), hr: Some(hr), hr_max: Some(hr + 5.0), dyn_g: Some(0.05) }
    }

    /// 15 Nov 2026 13:20-13:55, per-minute R18 averages (skin, aux 1, aux 2, HR)
    pub(crate) fn sauna_15_nov(t0: i64) -> Vec<TempMinute> {
        let rows: [(f64, f64, f64, f64); 36] = [
            (30.09, 26.2, 27.8, 105.0), (30.2, 26.5, 27.8, 99.0), (30.73, 26.9, 28.2, 102.0), (30.83, 27.1, 28.6, 111.0),
            (31.05, 27.4, 29.0, 103.0), (31.31, 27.6, 29.1, 104.0), (31.56, 27.8, 29.4, 106.0), (31.62, 28.1, 29.4, 104.0),
            (31.9, 28.6, 29.7, 102.0), (32.59, 30.9, 31.0, 98.0), (33.86, 33.3, 32.6, 100.0), (35.37, 35.7, 34.4, 98.0),
            (36.45, 37.9, 36.0, 98.0), (37.37, 39.6, 37.5, 103.0), (38.22, 41.8, 38.8, 108.0), (38.92, 43.4, 39.9, 111.0),
            (39.38, 44.5, 40.7, 110.0), (39.92, 45.1, 41.4, 111.0), (40.22, 45.4, 42.0, 115.0), (40.55, 45.4, 42.3, 112.0),
            (40.82, 45.4, 42.6, 122.0), (40.78, 44.9, 42.7, 127.0), (39.1, 41.5, 40.9, 123.0), (36.76, 38.4, 37.9, 114.0),
            (35.65, 35.5, 35.3, 115.0), (34.82, 33.7, 34.2, 111.0), (34.17, 32.6, 33.3, 104.0), (33.97, 31.8, 32.7, 100.0),
            (33.67, 30.7, 32.1, 109.0), (33.24, 29.3, 31.3, 104.0), (32.77, 28.6, 30.7, 92.0), (32.26, 27.7, 30.0, 98.0),
            (31.99, 27.1, 29.5, 95.0), (31.79, 26.8, 29.3, 97.0), (31.37, 26.5, 29.0, 97.0), (31.19, 26.3, 28.8, 97.0),
        ];
        rows.iter().enumerate().map(|(i, r)| m(t0 + 60 * i as i64, r.0, r.1, r.2, r.3)).collect()
    }

    /// 13 Nov 2026 18:37-19:06: battery pack charging, heart rate at rest
    fn charging_13_nov(t0: i64) -> Vec<TempMinute> {
        let rows: [(f64, f64, f64, f64); 18] = [
            (32.08, 29.4, 30.5, 66.0), (32.76, 29.6, 32.7, 63.0), (33.53, 30.7, 34.8, 56.0), (33.87, 31.9, 35.5, 56.0),
            (34.34, 33.3, 36.3, 57.0), (34.82, 34.6, 37.4, 56.0), (35.22, 35.8, 38.2, 55.0), (35.69, 37.2, 38.9, 55.0),
            (36.13, 38.2, 39.7, 53.0), (36.42, 39.2, 40.3, 55.0), (36.43, 40.0, 40.9, 60.0), (36.71, 40.7, 41.5, 55.0),
            (36.9, 41.8, 42.0, 59.0), (37.17, 42.4, 42.4, 56.0), (36.68, 42.4, 40.1, 57.0), (36.39, 41.5, 38.8, 55.0),
            (36.23, 40.5, 38.4, 55.0), (35.91, 39.9, 39.1, 62.0),
        ];
        rows.iter().enumerate().map(|(i, r)| m(t0 + 60 * i as i64, r.0, r.1, r.2, r.3)).collect()
    }

    #[test]
    fn detects_the_15_nov_sauna_with_its_bounds() {
        let t0 = 1_794_748_800; // stands for 13:20
        let s = detect(&sauna_15_nov(t0), 60.0, &[]);
        assert_eq!(s.len(), 1);
        // entry during 13:28 (steep rise from 13:29), exit after 13:41 (steep fall from 13:42)
        assert_eq!((s[0].start - t0) / 60, 8, "{:?}", s[0]);
        assert_eq!((s[0].end - t0) / 60, 22);
        assert_eq!(s[0].summary["heat"]["skin_max_c"], 40.8);
        assert_eq!(s[0].summary["heat"]["outer_max_c"], 45.4);
        assert_eq!(s[0].summary["duration_min"], 14.0);
    }

    #[test]
    fn charging_and_resting_heart_rate_are_not_a_sauna() {
        let t0 = 1_794_591_420;
        // heart rate at rest: rejected even without the charging events
        assert!(detect(&charging_13_nov(t0), 52.0, &[]).is_empty());
        // a sauna-like heart rate during charging: rejected by the CHARGING_ON event
        let hot_hr: Vec<TempMinute> = charging_13_nov(t0).into_iter().map(|mut x| { x.hr = Some(110.0); x }).collect();
        assert_eq!(detect(&hot_hr, 52.0, &[]).len(), 1);
        assert!(detect(&hot_hr, 52.0, &[t0 + 60]).is_empty());
        // a short hot spell (3 minutes) is not a session
        let short: Vec<TempMinute> = sauna_15_nov(0).into_iter().enumerate().map(|(i, mut x)| { if !(14..17).contains(&i) { x.aux1 = x.skin.map(|s| s - 3.0) } x }).collect();
        assert!(detect(&short, 60.0, &[]).is_empty());
    }

    fn store(conn: &Connection, mins: &[TempMinute]) {
        ensure_tables(conn).unwrap();
        crate::whoop5_store::ensure_tables(conn).unwrap();
        for x in mins {
            for k in 0..60 {
                conn.execute(
                    "INSERT INTO whoop5_r18 (ts, record_index, frame_id, hr, rr_ms, dyn_accel_g, aux_temp1_c, aux_temp2_c, skin_temp_c, decoder_version) VALUES (?1, ?1, 'f', ?2, '', 0.05, ?3, ?4, ?5, 'v')",
                    params![x.t + k, x.hr.unwrap() as i64, x.aux1, x.aux2, x.skin],
                ).unwrap();
            }
        }
    }

    #[test]
    fn sync_stores_once_and_respects_a_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.sqlite")).unwrap();
        let t0 = 1_794_748_800;
        store(&conn, &sauna_15_nov(t0));
        // a workout session that ran on into the sauna is cut at the entry
        conn.execute("INSERT INTO sport_sessions (id, start_ts, end_ts, status, source, engine_version, label_status, created_at, updated_at) VALUES ('sp-1', ?1, ?2, 'ended', 'auto', 'x', 'pending', 0, 0)", params![t0 - 3600, t0 + 900]).unwrap();
        let now = t0 + 36 * 60;
        let ids = sync(&conn, t0 - 600, now, 60.0, now).unwrap();
        assert_eq!(ids, vec![format!("sauna-{}", t0 + 480)]);
        let end: i64 = conn.query_row("SELECT end_ts FROM sport_sessions WHERE id = 'sp-1'", [], |r| r.get(0)).unwrap();
        assert_eq!(end, t0 + 480);
        assert!(sync(&conn, t0 - 600, now, 60.0, now).unwrap().is_empty(), "stored once");
        remove_session(&conn, &ids[0], now, "app").unwrap();
        assert!(sync(&conn, t0 - 600, now, 60.0, now).unwrap().is_empty(), "deleted stays deleted");
        // still in the sauna (data ends while hot): nothing yet
        let conn2 = Connection::open(dir.path().join("t2.sqlite")).unwrap();
        store(&conn2, &sauna_15_nov(t0)[..20]);
        assert!(sync(&conn2, t0 - 600, t0 + 20 * 60, 60.0, t0 + 20 * 60).unwrap().is_empty());
        assert!(hot_now(&conn2, t0 + 20 * 60).unwrap());
    }

    /// every minute of R18 recovered from the phone (GOOSE_HEAT_MINS: table mins + ev, see
    /// scratch scan_mins.py): only the 15 Nov sauna, not the 13 Nov charging
    #[test]
    #[ignore]
    fn real_history() {
        let Ok(path) = std::env::var("GOOSE_HEAT_MINS") else { return };
        let c = Connection::open(path).unwrap();
        let mins: Vec<TempMinute> = c.prepare("SELECT m * 60, n, skin, aux1, aux2, hr, dyn FROM mins ORDER BY m").unwrap()
            .query_map([], |r| Ok(TempMinute { t: r.get(0)?, n: r.get::<_, i64>(1)? as usize, skin: r.get(2)?, aux1: r.get(3)?, aux2: r.get(4)?, hr: r.get(5)?, hr_max: r.get(5)?, dyn_g: r.get(6)? }))
            .unwrap().flatten().collect();
        let charging: Vec<i64> = c.prepare(&format!("SELECT ts FROM ev WHERE event_id IN ({CHARGE_EVENTS})")).unwrap().query_map([], |r| r.get(0)).unwrap().flatten().collect();
        let found = detect(&mins, 60.0, &charging);
        let without_events = detect(&mins, 60.0, &[]);
        for s in &found { println!("sauna {} -> {} {}", s.start, s.end, s.summary["heat"]); }
        println!("{} minutes, {} charge events, saunas {} (without charge events {})", mins.len(), charging.len(), found.len(), without_events.len());
        assert_eq!(found.len(), 1);
        assert_eq!(without_events.len(), 1, "the charging spell is rejected by its heart rate alone");
    }
}
