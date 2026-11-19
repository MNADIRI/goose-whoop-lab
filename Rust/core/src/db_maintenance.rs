//! Local database upkeep. The capture layer (raw_evidence: the frame bytes as hex; decoded_frames:
//! the same bytes as hex again plus their parsed JSON) was kept forever: 2.4 of the 3 GB on 17 Nov,
//! about 430 MB a day (live IMU frames 175 MB, history 116 MB, raw bytes 130 MB), for data the app
//! reads from the typed tables (whoop5_r18, sport_imu ...) once ingested. Kept here:
//!   - decoded frames: 48 h (the legacy metric features read since the start of yesterday), live
//!     sensor frames (packet 43, already in sport_imu / whoop5_r20_slot) 6 h; never a frame the
//!     typed ingests have not consumed yet;
//!   - raw bytes: 12 h;
//!   - live IMU for sport: 14 days inside sessions (re-analysis, labels), 2 days outside;
//!   - live heart rate, Smart Wake nights, legacy step samples: 30 days.
//! Deletes go by rowid ranges in bounded chunks (no index joins the two capture tables: a cascading
//! delete would scan 1.6 GB per row), so a run never holds the CPU long enough for iOS to kill the
//! app in the background. Freed pages are reused by new frames right away; `vacuum` gives them back
//! to the file system (incremental mode, switched on by one full VACUUM, run while charging).

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::GooseResult;

const DAY: i64 = 86_400;

#[derive(Debug, Deserialize)]
pub struct MaintainArgs {
    pub database_path: String,
    #[serde(default)]
    pub now_unix: Option<i64>,
    /// decoded frames retention (hours)
    #[serde(default)]
    pub keep_hours: Option<i64>,
    /// live sensor frames (packet 43) retention (hours)
    #[serde(default)]
    pub live_keep_hours: Option<i64>,
    /// raw bytes retention (hours)
    #[serde(default)]
    pub raw_keep_hours: Option<i64>,
    /// rows deleted at most per table and run
    #[serde(default)]
    pub max_rows: Option<i64>,
    /// "none" | "incremental" (give back free pages) | "full" (one-time VACUUM, switches to incremental)
    #[serde(default)]
    pub vacuum: Option<String>,
}

fn table_exists(c: &Connection, t: &str) -> GooseResult<bool> {
    Ok(c.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [t], |_| Ok(true)).optional()?.unwrap_or(false))
}

fn iso(ts: i64) -> String {
    let d = ts.div_euclid(DAY);
    let s = ts.rem_euclid(DAY);
    format!("{}T{:02}:{:02}:{:02}.000Z", crate::weekly::date_key(d), s / 3600, s % 3600 / 60, s % 60)
}

/// First rowid whose text time column is >= `t` (rowids follow insertion, so time).
fn first_rowid_at(c: &Connection, table: &str, col: &str, t: &str) -> GooseResult<i64> {
    let max: i64 = c.query_row(&format!("SELECT IFNULL(MAX(rowid), 0) FROM {table}"), [], |r| r.get(0))?;
    let (mut lo, mut hi) = (0i64, max + 1);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let v: Option<String> = c.query_row(&format!("SELECT {col} FROM {table} WHERE rowid >= ?1 ORDER BY rowid LIMIT 1"), [mid], |r| r.get(0)).optional()?;
        match v {
            Some(v) if v.as_str() < t => lo = mid + 1,
            _ => hi = mid,
        }
    }
    Ok(lo)
}

fn state_cursor(c: &Connection, table: &str) -> GooseResult<Option<i64>> {
    if !table_exists(c, table)? {
        return Ok(None);
    }
    Ok(c.query_row(&format!("SELECT value FROM {table} WHERE key = 'decoded_rowid'"), [], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()))
}

/// Deletes rowid < `below`, oldest first, at most `max` rows. Returns the rows deleted.
fn delete_below(c: &Connection, table: &str, below: i64, max: i64) -> GooseResult<i64> {
    let n = c.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE rowid < ?1 ORDER BY rowid LIMIT ?2)"), params![below, max])?;
    Ok(n as i64)
}

pub fn maintain(args: &MaintainArgs) -> GooseResult<Value> {
    let now = args.now_unix.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0));
    let keep = args.keep_hours.unwrap_or(48).max(24) * 3600;
    let max = args.max_rows.unwrap_or(20_000).max(100);
    let c = Connection::open(&args.database_path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    // the capture layer's cascade would scan decoded_frames for every raw_evidence row
    c.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let mut out = serde_json::Map::new();
    let mut backlog = false;

    // capture layer: only what both typed ingests (whoop5_store, whoop_lab biometric) have consumed
    c.execute("CREATE TABLE IF NOT EXISTS db_maintenance_state (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
    if table_exists(&c, "decoded_frames")? {
        let _ = crate::whoop5_store::ingest(&c);
        let consumed = [state_cursor(&c, "whoop5_state")?, state_cursor(&c, "whoop_lab_state")?].into_iter().flatten().min().unwrap_or(0);
        let below = first_rowid_at(&c, "decoded_frames", "created_at", &iso(now - keep))?.min(consumed + 1);
        let n = delete_below(&c, "decoded_frames", below, max)?;
        backlog |= n == max;
        out.insert("decoded_frames".into(), json!(n));
        // large sensor frames (live IMU 1.2 kB, optical 2.1 kB) between the 48 h and the 6 h
        // boundaries, picked on raw_len: it sits before the hex / JSON columns, so the scan never
        // reads the rows' overflow pages (a packet_type filter read ~400 MB per pass on the phone);
        // a cursor avoids re-reading the rows already passed
        let live_below = first_rowid_at(&c, "decoded_frames", "created_at", &iso(now - args.live_keep_hours.unwrap_or(6).max(1) * 3600))?.min(consumed + 1);
        let from: i64 = c.query_row("SELECT value FROM db_maintenance_state WHERE key = 'live_rowid'", [], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let ids: Vec<(i64, Option<i64>)> = c
            .prepare("SELECT rowid, raw_len FROM decoded_frames WHERE rowid >= ?1 AND rowid < ?2 ORDER BY rowid LIMIT ?3")?
            .query_map(params![from.max(below), live_below, max * 4], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut n_live = 0;
        {
            let mut del = c.prepare_cached("DELETE FROM decoded_frames WHERE rowid = ?1")?;
            for (id, ty) in &ids {
                if ty.unwrap_or(0) >= 1000 {
                    n_live += del.execute([id])? as i64;
                }
            }
        }
        let next = ids.last().map(|v| v.0 + 1).unwrap_or(from.max(below));
        c.execute("INSERT OR REPLACE INTO db_maintenance_state (key, value) VALUES ('live_rowid', ?1)", [next.to_string()])?;
        backlog |= ids.len() as i64 == max * 4;
        out.insert("decoded_live_frames".into(), json!(n_live));
    }
    if table_exists(&c, "raw_evidence")? {
        let below = first_rowid_at(&c, "raw_evidence", "captured_at", &iso(now - args.raw_keep_hours.unwrap_or(12).max(1) * 3600))?;
        let n = delete_below(&c, "raw_evidence", below, max)?;
        backlog |= n == max;
        out.insert("raw_evidence".into(), json!(n));
    }

    // live IMU for sport: sessions keep theirs 14 days, the rest 2 days
    if table_exists(&c, "sport_imu")? && table_exists(&c, "sport_sessions")? {
        let n = c.execute(
            "DELETE FROM sport_imu WHERE rowid IN (SELECT rowid FROM sport_imu i WHERE i.ts < ?1 AND (i.ts < ?2 OR NOT EXISTS (
                 SELECT 1 FROM sport_sessions s WHERE s.status != 'discarded' AND i.ts >= s.start_ts - 300 AND i.ts <= COALESCE(s.end_ts, s.start_ts) + 300))
             ORDER BY ts LIMIT ?3)",
            params![(now - 2 * DAY) as f64, (now - 14 * DAY) as f64, max],
        )? as i64;
        backlog |= n == max;
        out.insert("sport_imu".into(), json!(n));
    }
    let month = now - 30 * DAY;
    for (table, sql) in [
        ("sport_live", "DELETE FROM sport_live WHERE ts IN (SELECT ts FROM sport_live WHERE ts < ?1 ORDER BY ts LIMIT ?2)"),
        ("whoop5_live_hr", "DELETE FROM whoop5_live_hr WHERE rowid IN (SELECT rowid FROM whoop5_live_hr WHERE ts < ?1 LIMIT ?2)"),
        ("smart_wake_imu", "DELETE FROM smart_wake_imu WHERE rowid IN (SELECT rowid FROM smart_wake_imu WHERE ts < ?1 LIMIT ?2)"),
        ("smart_wake_live", "DELETE FROM smart_wake_live WHERE rowid IN (SELECT rowid FROM smart_wake_live WHERE ts < ?1 LIMIT ?2)"),
        ("step_counter_samples", "DELETE FROM step_counter_samples WHERE rowid IN (SELECT rowid FROM step_counter_samples WHERE sample_time_unix_ms < ?1 * 1000 LIMIT ?2)"),
    ] {
        if table_exists(&c, table)? {
            let n = c.execute(sql, params![month, max])? as i64;
            backlog |= n == max;
            out.insert(table.into(), json!(n));
        }
    }

    // give space back
    let mode = args.vacuum.as_deref().unwrap_or("none");
    let auto_vacuum: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    let free_before: i64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let page: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let mut vacuumed = "none";
    if mode == "full" && !backlog {
        c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
        vacuumed = "full";
    } else if mode != "none" && auto_vacuum == 2 && free_before > 0 {
        // a few thousand pages per run (~16 MB at 4 KiB): bounded work
        c.query_row("PRAGMA incremental_vacuum(4000)", [], |_| Ok(())).optional()?;
        vacuumed = "incremental";
    }
    let pages: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let free: i64 = c.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let auto_vacuum: i64 = c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    Ok(json!({
        "deleted": out, "backlog": backlog, "vacuum": vacuumed, "auto_vacuum": auto_vacuum,
        "file_bytes": pages * page, "free_bytes": free * page,
        // a full VACUUM is due once the backlog is gone and the file still holds much free space
        "full_vacuum_due": auto_vacuum != 2 && !backlog && free * page > 200_000_000,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_recent_and_unconsumed_frames() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("m.sqlite");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE raw_evidence (evidence_id TEXT PRIMARY KEY, captured_at TEXT NOT NULL, payload_hex TEXT NOT NULL);
             CREATE TABLE decoded_frames (frame_id TEXT PRIMARY KEY, evidence_id TEXT NOT NULL, created_at TEXT NOT NULL, payload_hex TEXT NOT NULL,
                 packet_type INTEGER, header_crc_valid INTEGER, payload_crc_valid INTEGER, raw_len INTEGER);
             CREATE TABLE whoop_lab_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        let now = 1_795_000_000;
        // 100 frames, one per hour back from now (oldest first)
        for k in (0..100).rev() {
            let t = now - k * 3600;
            c.execute("INSERT INTO raw_evidence VALUES (?1, ?2, 'aa')", params![format!("e{k}"), iso(t)]).unwrap();
            c.execute("INSERT INTO decoded_frames VALUES (?1, ?2, ?3, 'aa', 36, 1, 1, 8)", params![format!("f{k}"), format!("e{k}"), iso(t)]).unwrap();
        }
        // whoop_lab has consumed the 60 oldest frames only
        c.execute("INSERT INTO whoop_lab_state VALUES ('decoded_rowid', '60')", []).unwrap();
        drop(c);
        let r = maintain(&MaintainArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), keep_hours: Some(48), live_keep_hours: None, raw_keep_hours: Some(49), max_rows: Some(1000), vacuum: None }).unwrap();
        let c = Connection::open(&db).unwrap();
        let left: i64 = c.query_row("SELECT COUNT(*) FROM decoded_frames", [], |r| r.get(0)).unwrap();
        // older than 48 h: frames 1..=51 (rowids); all consumed (<= 60) -> deleted, 49 left
        assert_eq!(left, 49, "{r}");
        let raw_left: i64 = c.query_row("SELECT COUNT(*) FROM raw_evidence", [], |r| r.get(0)).unwrap();
        assert_eq!(raw_left, 50, "{r}");
        // nothing consumed: nothing deleted
        c.execute("UPDATE whoop_lab_state SET value = '0'", []).unwrap();
        c.execute("INSERT INTO decoded_frames VALUES ('old', 'e', ?1, 'aa', 36, 1, 1, 8)", [iso(now - 90 * 3600)]).unwrap();
        drop(c);
        let r = maintain(&MaintainArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), keep_hours: Some(48), live_keep_hours: None, raw_keep_hours: None, max_rows: Some(1000), vacuum: None }).unwrap();
        assert_eq!(r["deleted"]["decoded_frames"], 0, "{r}");
    }

    #[test]
    fn live_sensor_frames_go_after_6_hours() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("l.sqlite");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE decoded_frames (frame_id TEXT PRIMARY KEY, created_at TEXT NOT NULL, packet_type INTEGER, raw_len INTEGER);
                         CREATE TABLE whoop_lab_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                         INSERT INTO whoop_lab_state VALUES ('decoded_rowid', '1000');").unwrap();
        let now = 1_795_000_000;
        // 24 h, one frame every 30 min alternating history (47) / live (43)
        for k in (0..48).rev() {
            c.execute("INSERT INTO decoded_frames VALUES (?1, ?2, ?3, ?4)", params![format!("f{k}"), iso(now - k * 1800), if k % 2 == 0 { 43 } else { 47 }, if k % 2 == 0 { 1244 } else { 124 }]).unwrap();
        }
        drop(c);
        let a = MaintainArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), keep_hours: Some(48), live_keep_hours: Some(6), raw_keep_hours: None, max_rows: Some(1000), vacuum: None };
        let r = maintain(&a).unwrap();
        let c = Connection::open(&db).unwrap();
        let live: i64 = c.query_row("SELECT COUNT(*) FROM decoded_frames WHERE packet_type = 43", [], |r| r.get(0)).unwrap();
        let hist: i64 = c.query_row("SELECT COUNT(*) FROM decoded_frames WHERE packet_type = 47", [], |r| r.get(0)).unwrap();
        // live frames younger than 6 h: k = 0, 2, ..., 10 -> 6 (plus k=12 exactly at the boundary)
        assert!((6..=7).contains(&live), "{live} {r}");
        assert_eq!(hist, 24);
        // second run: the cursor skips what was already read
        drop(c);
        let r = maintain(&a).unwrap();
        assert_eq!(r["deleted"]["decoded_live_frames"], 0);
    }

    #[test]
    fn chunks_report_a_backlog_and_full_vacuum_switches_mode() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("v.sqlite");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE raw_evidence (evidence_id TEXT PRIMARY KEY, captured_at TEXT NOT NULL, payload_hex TEXT NOT NULL);").unwrap();
        let now = 1_795_000_000;
        for k in 0..500 {
            c.execute("INSERT INTO raw_evidence VALUES (?1, ?2, ?3)", params![format!("e{k}"), iso(now - 10 * DAY + k), "ab".repeat(2000)]).unwrap();
        }
        drop(c);
        let args = |v: &str| MaintainArgs { database_path: db.to_string_lossy().into(), now_unix: Some(now), keep_hours: Some(48), live_keep_hours: None, raw_keep_hours: None, max_rows: Some(200), vacuum: Some(v.into()) };
        let r = maintain(&args("full")).unwrap();
        assert_eq!(r["backlog"], true);
        assert_eq!(r["vacuum"], "none"); // not while a backlog remains
        maintain(&args("none")).unwrap();
        let r = maintain(&args("full")).unwrap();
        assert_eq!(r["deleted"]["raw_evidence"], 100);
        assert_eq!(r["vacuum"], "full");
        assert_eq!(r["auto_vacuum"], 2);
        assert!(r["file_bytes"].as_i64().unwrap() < 200_000, "{r}");
    }

    #[test]
    #[ignore]
    fn real_timing() {
        let Ok(db) = std::env::var("GOOSE_MAINT_DB") else { return };
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let r = maintain(&MaintainArgs { database_path: db.clone(), now_unix: Some(1_794_952_000), keep_hours: None, live_keep_hours: None, raw_keep_hours: None, max_rows: Some(10_000), vacuum: Some("incremental".into()) });
            println!("{:.2} s {:?}", t.elapsed().as_secs_f64(), r.map(|v| v["deleted"].clone()));
        }
    }
}
