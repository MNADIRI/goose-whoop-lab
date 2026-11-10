//! Layer B storage for WHOOP 5.0: typed tables per record family, filled incrementally from the
//! immutable Layer A (`raw_evidence` -> `decoded_frames`, never modified here).
//!
//! - Every row keeps `frame_id` (-> decoded_frames -> raw_evidence) and `decoder_version`.
//! - High-frequency arrays (R26 samples, R20 slots, R21 axes) are little-endian BLOBs, not JSON.
//! - Duplicates (the strap re-sends records across syncs) collapse on (ts, record_index).
//! - Frames failing CRC are not decoded (they stay in Layer A and are counted in the inventory).
//! - Migrations are additive only (CREATE ... IF NOT EXISTS); re-decoding after a decoder change
//!   is done with `rebuild`, which drops and refills Layer B only.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::whoop5_records::{self as rec, Record, DECODER_VERSION};
use crate::GooseResult;

pub fn ensure_tables(conn: &Connection) -> GooseResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS whoop5_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS whoop5_r18 (
            ts INTEGER NOT NULL, record_index INTEGER NOT NULL, frame_id TEXT NOT NULL,
            subseconds INTEGER, hr INTEGER NOT NULL, rr_ms TEXT NOT NULL, rr_count_raw INTEGER,
            cardiac_flags_raw INTEGER, hr_quality_raw INTEGER, alt_hr INTEGER, rr_adjacent_raw INTEGER,
            dyn_accel_g REAL, grav_x_g REAL, grav_y_g REAL, grav_z_g REAL,
            steps INTEGER, cadence_raw INTEGER, steps_hw INTEGER, counter_override INTEGER,
            activity_raw INTEGER, activity TEXT,
            aux_temp1_c REAL, aux_temp2_c REAL, skin_temp_c REAL, skin_temp_raw INTEGER,
            status0_raw INTEGER, status1_raw INTEGER, status2_raw INTEGER,
            wrist_bits INTEGER, wake_quality_bits INTEGER, band_state TEXT, extra_state_bits INTEGER,
            sleep_adjacent_raw INTEGER, optical_baseline_a INTEGER, optical_baseline_b INTEGER,
            optical_amplitude_a INTEGER, optical_amplitude_b INTEGER, unknown_f32 REAL,
            unmapped_hex TEXT, warnings TEXT, decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, record_index)
        );
        CREATE TABLE IF NOT EXISTS whoop5_r26 (
            ts INTEGER NOT NULL, record_index INTEGER NOT NULL, frame_id TEXT NOT NULL,
            time_word INTEGER, burst INTEGER, base INTEGER NOT NULL,
            deltas BLOB NOT NULL, samples BLOB NOT NULL,
            motion REAL, status_raw INTEGER, on_wrist_raw INTEGER, acceptance_raw INTEGER,
            decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, record_index)
        );
        CREATE TABLE IF NOT EXISTS whoop5_r20_slot (
            ts INTEGER NOT NULL, record_index INTEGER NOT NULL, block INTEGER NOT NULL, slot TEXT NOT NULL,
            frame_id TEXT NOT NULL, marker_raw INTEGER, count INTEGER NOT NULL,
            source_raw INTEGER, drive_raw INTEGER, detector_raw INTEGER, range_raw INTEGER, offset_raw INTEGER,
            samples BLOB NOT NULL, padding_nonzero INTEGER NOT NULL, status TEXT NOT NULL,
            decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, record_index, block, slot)
        );
        CREATE TABLE IF NOT EXISTS whoop5_r21 (
            ts REAL NOT NULL, record_index INTEGER NOT NULL, frame_id TEXT NOT NULL, marker_raw INTEGER,
            accel_count INTEGER, gyro_count INTEGER,
            accel_x BLOB, accel_y BLOB, accel_z BLOB, gyro_x BLOB, gyro_y BLOB, gyro_z BLOB,
            accel_lsb_per_g REAL NOT NULL, gyro_dps_per_lsb REAL NOT NULL, warnings TEXT,
            decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, record_index)
        );
        CREATE TABLE IF NOT EXISTS whoop5_live_hr (
            ts INTEGER NOT NULL, time_word INTEGER NOT NULL, frame_id TEXT NOT NULL,
            hr INTEGER NOT NULL, rr_ms TEXT NOT NULL, decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, time_word)
        );
        CREATE TABLE IF NOT EXISTS whoop5_events (
            ts INTEGER NOT NULL, subseconds INTEGER NOT NULL, event_id INTEGER NOT NULL, frame_id TEXT NOT NULL,
            name TEXT NOT NULL, data_hex TEXT NOT NULL, decoded_json TEXT NOT NULL, decoder_version TEXT NOT NULL,
            PRIMARY KEY (ts, subseconds, event_id)
        );
        CREATE TABLE IF NOT EXISTS whoop5_metadata (
            frame_id TEXT PRIMARY KEY, kind_raw INTEGER, kind TEXT, body_hex TEXT, decoder_version TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS whoop5_console (frame_id TEXT PRIMARY KEY, text TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS whoop5_unknown (
            frame_id TEXT PRIMARY KEY, packet_type INTEGER, layout INTEGER, payload_len INTEGER
        );
        CREATE INDEX IF NOT EXISTS whoop5_r18_index ON whoop5_r18 (record_index);
        CREATE INDEX IF NOT EXISTS whoop5_r26_burst ON whoop5_r26 (burst, ts);
        CREATE INDEX IF NOT EXISTS whoop5_r21_index ON whoop5_r21 (record_index);
        "#,
    )?;
    Ok(())
}

fn blob_i16(v: &[i16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn blob_i32(v: &[i32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn blob_i64(v: &[i64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
pub fn i16s(b: &[u8]) -> Vec<i16> {
    b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}
pub fn i32s(b: &[u8]) -> Vec<i32> {
    b.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}
pub fn i64s(b: &[u8]) -> Vec<i64> {
    b.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect()
}
fn join(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

#[derive(Debug, Default, serde::Serialize)]
pub struct IngestStats {
    pub frames_scanned: usize,
    pub crc_invalid_skipped: usize,
    pub inserted: std::collections::BTreeMap<&'static str, usize>,
    pub duplicates: usize,
    pub unknown: usize,
    pub cursor: i64,
}

/// Decodes decoded_frames rows newer than the cursor into the Layer B tables.
pub fn ingest(conn: &Connection) -> GooseResult<IngestStats> {
    ensure_tables(conn)?;
    let cursor: i64 = conn
        .query_row("SELECT value FROM whoop5_state WHERE key = 'decoded_rowid'", [], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // decoder changed: Layer B is re-derived from the immutable Layer A
    let stored_version: Option<String> = conn.query_row("SELECT value FROM whoop5_state WHERE key = 'decoder_version'", [], |r| r.get(0)).optional()?;
    let cursor = if stored_version.as_deref().is_some_and(|v| v != DECODER_VERSION) {
        conn.execute_batch(
            "DELETE FROM whoop5_r18; DELETE FROM whoop5_r26; DELETE FROM whoop5_r20_slot; DELETE FROM whoop5_r21;
             DELETE FROM whoop5_live_hr; DELETE FROM whoop5_events; DELETE FROM whoop5_metadata; DELETE FROM whoop5_console;
             DELETE FROM whoop5_unknown; DELETE FROM whoop5_state;",
        )?;
        0
    } else {
        cursor
    };
    let mut stats = IngestStats { cursor, ..Default::default() };
    let tx = conn.unchecked_transaction()?;
    {
        let mut st = tx.prepare(
            "SELECT rowid, frame_id, payload_hex, header_crc_valid, payload_crc_valid FROM decoded_frames
             WHERE rowid > ?1 AND packet_type IN (40, 47, 48, 49, 50) ORDER BY rowid LIMIT 200000",
        )?;
        let rows = st.query_map([cursor], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?, r.get::<_, bool>(4)?)))?;
        for row in rows {
            let (rowid, frame_id, hex_payload, hok, pok) = row?;
            stats.cursor = stats.cursor.max(rowid);
            stats.frames_scanned += 1;
            if !hok || !pok {
                stats.crc_invalid_skipped += 1;
                continue;
            }
            let Ok(pl) = hex::decode(hex_payload.trim()) else { continue };
            let Some(record) = rec::decode_payload(&pl) else { continue };
            let (table, n) = insert(&tx, &frame_id, &record)?;
            if table == "whoop5_unknown" {
                stats.unknown += 1;
            }
            if n == 0 {
                stats.duplicates += 1;
            } else {
                *stats.inserted.entry(table).or_default() += n;
            }
        }
    }
    if stats.frames_scanned == 0 {
        // nothing new for our packet types: move the cursor to the end anyway
        let max: i64 = tx.query_row("SELECT IFNULL(MAX(rowid), 0) FROM decoded_frames", [], |r| r.get(0))?;
        stats.cursor = stats.cursor.max(max);
    }
    tx.execute("INSERT OR REPLACE INTO whoop5_state (key, value) VALUES ('decoded_rowid', ?1)", [stats.cursor.to_string()])?;
    tx.execute("INSERT OR REPLACE INTO whoop5_state (key, value) VALUES ('decoder_version', ?1)", [DECODER_VERSION])?;
    tx.commit()?;
    Ok(stats)
}

fn insert(tx: &Connection, frame_id: &str, record: &Record) -> GooseResult<(&'static str, usize)> {
    let v = DECODER_VERSION;
    Ok(match record {
        Record::R18(r) => {
            let g = r.gravity_g.map(|g| g.map(f64::from));
            let n = tx.prepare_cached(
                "INSERT OR IGNORE INTO whoop5_r18 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32,?33,?34,?35,?36,?37,?38,?39,?40,?41)",
            )?
            .execute(params![
                r.ts, r.record_index, frame_id, r.subseconds, r.hr, join(&r.rr_ms), r.rr_count_raw,
                r.cardiac_flags_raw, r.hr_quality_raw, r.alt_hr, r.rr_adjacent_raw,
                r.dyn_accel_g.map(f64::from), g.map(|g| g[0]), g.map(|g| g[1]), g.map(|g| g[2]),
                r.steps, r.cadence_raw, r.steps_hw, r.counter_override, r.activity_raw, r.activity,
                r.aux_temp1_c.map(f64::from), r.aux_temp2_c.map(f64::from), r.skin_temp_c.map(|t| f64::from(t * 100.0).round() / 100.0), r.skin_temp_raw,
                r.status_raw[0], r.status_raw[1], r.status_raw[2],
                r.wrist_bits, r.wake_quality_bits, r.band_state, r.extra_state_bits,
                r.sleep_adjacent_raw, r.optical_baseline_raw[0], r.optical_baseline_raw[1],
                r.optical_amplitude_raw[0], r.optical_amplitude_raw[1], r.unknown_f32.map(f64::from),
                r.unmapped_hex, r.warnings.join(","), v,
            ])?;
            ("whoop5_r18", n)
        }
        Record::R26(r) => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_r26 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)")?.execute(params![
                r.ts, r.record_index, frame_id, r.time_word, r.burst, r.base, blob_i16(&r.deltas), blob_i64(&r.samples),
                r.motion.map(f64::from), r.status_raw, r.on_wrist_raw, r.acceptance_raw, v,
            ])?;
            ("whoop5_r26", n)
        }
        Record::R20(r) => {
            let mut n = 0;
            for b in &r.blocks {
                for s in &b.slots {
                    n += tx.prepare_cached("INSERT OR IGNORE INTO whoop5_r20_slot VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)")?.execute(params![
                        r.ts, r.record_index, b.index, s.slot, frame_id, r.marker_raw, b.count,
                        s.source_raw, s.drive_raw, s.detector_raw, s.range_raw, s.offset_raw,
                        blob_i32(&s.samples), b.padding_nonzero, r.status.as_str(), v,
                    ])?;
                }
            }
            ("whoop5_r20_slot", n)
        }
        Record::R21(r) => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_r21 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)")?.execute(params![
                r.ts, r.record_index, frame_id, r.marker_raw, r.accel_count_raw, r.gyro_count_raw,
                blob_i16(&r.accel_raw[0]), blob_i16(&r.accel_raw[1]), blob_i16(&r.accel_raw[2]),
                blob_i16(&r.gyro_raw[0]), blob_i16(&r.gyro_raw[1]), blob_i16(&r.gyro_raw[2]),
                rec::ACCEL_LSB_PER_G, rec::GYRO_DPS_PER_LSB, r.warnings.join(","), v,
            ])?;
            ("whoop5_r21", n)
        }
        Record::LiveHr(r) => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_live_hr VALUES (?1,?2,?3,?4,?5,?6)")?.execute(params![r.ts, r.time_word, frame_id, r.hr, join(&r.rr_ms), v])?;
            ("whoop5_live_hr", n)
        }
        Record::Event(e) => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_events VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?.execute(params![e.ts, e.subseconds, e.event_id, frame_id, e.name, e.data_hex, e.decoded.to_string(), v])?;
            ("whoop5_events", n)
        }
        Record::Metadata(m) => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_metadata VALUES (?1,?2,?3,?4,?5)")?.execute(params![frame_id, m.kind_raw, m.kind, m.body_hex, v])?;
            ("whoop5_metadata", n)
        }
        Record::Console { text } => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_console VALUES (?1,?2)")?.execute(params![frame_id, text])?;
            ("whoop5_console", n)
        }
        Record::Unknown { packet_type, layout, payload_len } => {
            let n = tx.prepare_cached("INSERT OR IGNORE INTO whoop5_unknown VALUES (?1,?2,?3,?4)")?.execute(params![frame_id, packet_type, layout, *payload_len as i64])?;
            ("whoop5_unknown", n)
        }
    })
}

/// Drops and refills Layer B (e.g. after a decoder change). Layer A is untouched.
pub fn rebuild(conn: &Connection) -> GooseResult<IngestStats> {
    ensure_tables(conn)?;
    conn.execute_batch(
        "DELETE FROM whoop5_r18; DELETE FROM whoop5_r26; DELETE FROM whoop5_r20_slot; DELETE FROM whoop5_r21;
         DELETE FROM whoop5_live_hr; DELETE FROM whoop5_events; DELETE FROM whoop5_metadata; DELETE FROM whoop5_console;
         DELETE FROM whoop5_unknown; DELETE FROM whoop5_state;",
    )?;
    let mut total = IngestStats::default();
    loop {
        let s = ingest(conn)?;
        let done = s.frames_scanned == 0;
        total.frames_scanned += s.frames_scanned;
        total.crc_invalid_skipped += s.crc_invalid_skipped;
        total.duplicates += s.duplicates;
        total.unknown += s.unknown;
        for (k, v) in s.inserted {
            *total.inserted.entry(k).or_default() += v;
        }
        total.cursor = s.cursor;
        if done {
            break;
        }
    }
    Ok(total)
}

pub fn load_r26(conn: &Connection, start: i64, end: i64) -> GooseResult<Vec<rec::R26Record>> {
    ensure_tables(conn)?;
    let mut st = conn.prepare_cached(
        "SELECT ts, record_index, time_word, burst, base, deltas, samples, motion, status_raw, on_wrist_raw, acceptance_raw FROM whoop5_r26 WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = st.query_map(params![start, end], |r| {
        Ok(rec::R26Record {
            ts: r.get(0)?,
            record_index: r.get(1)?,
            time_word: r.get(2)?,
            burst: r.get(3)?,
            base: r.get(4)?,
            deltas: i16s(&r.get::<_, Vec<u8>>(5)?),
            samples: i64s(&r.get::<_, Vec<u8>>(6)?),
            motion: r.get::<_, Option<f64>>(7)?.map(|v| v as f32),
            status_raw: r.get(8)?,
            on_wrist_raw: r.get(9)?,
            acceptance_raw: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn load_r21(conn: &Connection, start: f64, end: f64) -> GooseResult<Vec<rec::R21Record>> {
    ensure_tables(conn)?;
    let mut st = conn.prepare_cached(
        "SELECT ts, record_index, marker_raw, accel_count, gyro_count, accel_x, accel_y, accel_z, gyro_x, gyro_y, gyro_z FROM whoop5_r21 WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = st.query_map(params![start, end], |r| {
        let b = |i: usize| -> rusqlite::Result<Vec<i16>> { Ok(i16s(&r.get::<_, Vec<u8>>(i)?)) };
        Ok(rec::R21Record {
            ts: r.get(0)?,
            record_index: r.get(1)?,
            marker_raw: r.get(2)?,
            accel_count_raw: r.get(3)?,
            gyro_count_raw: r.get(4)?,
            accel_raw: [b(5)?, b(6)?, b(7)?],
            gyro_raw: [b(8)?, b(9)?, b(10)?],
            warnings: vec![],
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Coverage, duplicates, CRC failures, record-index gaps and unknown layouts.
pub fn inventory(conn: &Connection) -> GooseResult<Value> {
    ensure_tables(conn)?;
    let one = |sql: &str| -> GooseResult<Value> {
        Ok(conn.query_row(sql, [], |r| Ok(json!({"rows": r.get::<_, i64>(0)?, "first_ts": r.get::<_, Option<f64>>(1)?, "last_ts": r.get::<_, Option<f64>>(2)?})))?)
    };
    let gaps = |table: &str| -> GooseResult<Value> {
        let sql = format!(
            "SELECT IFNULL(SUM(d - 1), 0), COUNT(*) FROM (SELECT record_index - LAG(record_index) OVER (ORDER BY record_index) AS d FROM (SELECT DISTINCT record_index FROM {table})) WHERE d > 1 AND d < 1000000"
        );
        Ok(conn.query_row(&sql, [], |r| Ok(json!({"missing_records": r.get::<_, i64>(0)?, "gap_count": r.get::<_, i64>(1)?})))?)
    };
    let frames: Vec<Value> = {
        let mut st = conn.prepare(
            "SELECT packet_type, raw_len, SUM(header_crc_valid AND payload_crc_valid), SUM(NOT (header_crc_valid AND payload_crc_valid)), COUNT(*) FROM decoded_frames GROUP BY packet_type, raw_len ORDER BY COUNT(*) DESC LIMIT 40",
        )?;
        let rows = st.query_map([], |r| Ok(json!({"packet_type": r.get::<_, Option<i64>>(0)?, "frame_len": r.get::<_, i64>(1)?, "crc_ok": r.get::<_, i64>(2)?, "crc_failed": r.get::<_, i64>(3)?, "frames": r.get::<_, i64>(4)?})))?;
        rows.collect::<Result<_, _>>()?
    };
    let unknown: Vec<Value> = {
        let mut st = conn.prepare("SELECT packet_type, layout, payload_len, COUNT(*) FROM whoop5_unknown GROUP BY 1, 2, 3")?;
        let rows = st.query_map([], |r| Ok(json!({"packet_type": r.get::<_, i64>(0)?, "layout": r.get::<_, i64>(1)?, "payload_len": r.get::<_, i64>(2)?, "frames": r.get::<_, i64>(3)?})))?;
        rows.collect::<Result<_, _>>()?
    };
    Ok(json!({
        "decoder_version": DECODER_VERSION,
        "layer_a_frames": frames,
        "r18": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_r18")?,
        "r18_gaps": gaps("whoop5_r18")?,
        "r26": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_r26")?,
        "r26_gaps": gaps("whoop5_r26")?,
        "r20_slots": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_r20_slot")?,
        "r21": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_r21")?,
        "live_hr": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_live_hr")?,
        "events": one("SELECT COUNT(*), MIN(ts), MAX(ts) FROM whoop5_events")?,
        "metadata": conn.query_row("SELECT COUNT(*) FROM whoop5_metadata", [], |r| r.get::<_, i64>(0))?,
        "console": conn.query_row("SELECT COUNT(*) FROM whoop5_console", [], |r| r.get::<_, i64>(0))?,
        "unknown_layouts": unknown,
        "r18_fields": rec::FIELD_CATALOG.iter().filter(|f| f.record == "R18").count(),
    }))
}

// ------------------------------------------------------------------ export

#[derive(Debug, Clone, Deserialize)]
pub struct ExportArgs {
    pub database_path: String,
    pub output_dir: String,
    /// "jsonl" (default) or "csv"
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub start_ts: Option<i64>,
    #[serde(default)]
    pub end_ts: Option<i64>,
    /// also write raw_frames.bin (length-prefixed original BLE frames from raw_evidence)
    #[serde(default)]
    pub include_raw: bool,
}

struct Sink {
    w: BufWriter<File>,
    csv: bool,
    header_written: bool,
    rows: usize,
}

impl Sink {
    fn new(dir: &Path, name: &str, csv: bool) -> GooseResult<Self> {
        let f = File::create(dir.join(format!("{name}.{}", if csv { "csv" } else { "jsonl" })))?;
        Ok(Self { w: BufWriter::new(f), csv, header_written: false, rows: 0 })
    }
    fn row(&mut self, v: &Value) -> GooseResult<()> {
        let o = v.as_object().expect("object row");
        if self.csv {
            if !self.header_written {
                writeln!(self.w, "{}", o.keys().cloned().collect::<Vec<_>>().join(","))?;
                self.header_written = true;
            }
            let cells: Vec<String> = o
                .values()
                .map(|x| match x {
                    Value::Null => String::new(),
                    Value::String(s) if s.contains(',') || s.contains('"') => format!("\"{}\"", s.replace('"', "\"\"")),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            writeln!(self.w, "{}", cells.join(","))?;
        } else {
            writeln!(self.w, "{v}")?;
        }
        self.rows += 1;
        Ok(())
    }
}

/// Units / status / provenance description written next to the data files.
fn schema_json(csv: bool) -> Value {
    json!({
        "decoder_version": DECODER_VERSION,
        "format": if csv { "csv (long format for arrays)" } else { "jsonl" },
        "time": "ts = unix seconds UTC (device clock); r21.t = ts + sample/fs_estimated",
        "provenance": "frame_id -> decoded_frames.frame_id -> raw_evidence.evidence_id (original bytes)",
        "files": {
            "r18": {"unit": {"hr": "bpm", "rr_ms": "ms", "dyn_accel_g": "g", "grav_*_g": "g", "skin_temp_c": "degC (skin, not core)", "aux_temp*_c": "degC (board?)", "steps": "cumulative count"}, "status": "see field_catalog"},
            "r26_samples": {"unit": {"value": "ADC code, unidentified optical channel"}, "fs": "estimated from records (~25 Hz on fw 50.38)", "status": "DECODED_UNVALIDATED"},
            "r20_samples": {"unit": {"value": "ADC code"}, "note": "block 0-4 x slot a/b, only counted positions", "status": "DECODED_UNVALIDATED"},
            "r21_samples": {"unit": {"a*_raw": "int16", "a*_g": "g (raw/4096, tentative)", "g*_raw": "int16", "g*_dps": "deg/s (raw*2000/32768, tentative)"}, "status": "DECODED_UNVALIDATED"},
            "live_hr": {"unit": {"hr": "bpm", "rr_ms": "ms"}},
            "events": {"note": "names from goose/NOOP tables; decoded_json values are candidates"},
        },
        "field_catalog": rec::field_catalog_json()["fields"],
    })
}

pub fn export(args: &ExportArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ingest(&conn)?;
    let dir = Path::new(&args.output_dir);
    std::fs::create_dir_all(dir)?;
    let csv = args.format.as_deref() == Some("csv");
    let (t0, t1) = (args.start_ts.unwrap_or(0), args.end_ts.unwrap_or(i64::MAX));
    let mut counts = serde_json::Map::new();

    // R18: one row per second, every column
    {
        let mut sink = Sink::new(dir, "r18", csv)?;
        let mut st = conn.prepare("SELECT * FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2 ORDER BY ts")?;
        let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = st.query(params![t0, t1])?;
        while let Some(r) = rows.next()? {
            let mut o = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                o.insert(n.clone(), sql_to_json(v));
            }
            sink.row(&Value::Object(o))?;
        }
        counts.insert("r18".into(), json!(sink.rows));
    }
    // R26: jsonl = one row per window with arrays; csv = one row per sample
    {
        let mut sink = Sink::new(dir, if csv { "r26_samples" } else { "r26" }, csv)?;
        for w in load_r26(&conn, t0, t1)? {
            if csv {
                for (i, s) in w.samples.iter().enumerate() {
                    sink.row(&json!({"ts": w.ts, "record_index": w.record_index, "sample": i, "t_est": w.ts as f64 + i as f64 / 25.0, "value": s, "motion": w.motion, "acceptance_raw": w.acceptance_raw}))?;
                }
            } else {
                sink.row(&json!({"ts": w.ts, "record_index": w.record_index, "time_word": w.time_word, "burst": w.burst, "base": w.base, "deltas": w.deltas, "samples": w.samples, "motion": w.motion, "status_raw": w.status_raw, "on_wrist_raw": w.on_wrist_raw, "acceptance_raw": w.acceptance_raw, "decoder_version": DECODER_VERSION}))?;
            }
        }
        counts.insert("r26".into(), json!(sink.rows));
    }
    // R20 slots
    {
        let mut sink = Sink::new(dir, if csv { "r20_samples" } else { "r20" }, csv)?;
        let mut st = conn.prepare("SELECT ts, record_index, block, slot, frame_id, count, source_raw, drive_raw, detector_raw, range_raw, offset_raw, samples, status FROM whoop5_r20_slot WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, block, slot")?;
        let mut rows = st.query(params![t0, t1])?;
        while let Some(r) = rows.next()? {
            let samples = i32s(&r.get::<_, Vec<u8>>(11)?);
            let meta = json!({"ts": r.get::<_, i64>(0)?, "record_index": r.get::<_, i64>(1)?, "block": r.get::<_, i64>(2)?, "slot": r.get::<_, String>(3)?, "frame_id": r.get::<_, String>(4)?,
                "count": r.get::<_, i64>(5)?, "source_raw": r.get::<_, i64>(6)?, "drive_raw": r.get::<_, i64>(7)?, "detector_raw": r.get::<_, i64>(8)?, "range_raw": r.get::<_, i64>(9)?, "offset_raw": r.get::<_, i64>(10)?, "status": r.get::<_, String>(12)?});
            if csv {
                for (i, s) in samples.iter().enumerate() {
                    let mut m = meta.clone();
                    m["position"] = json!(i);
                    m["value"] = json!(s);
                    sink.row(&m)?;
                }
            } else {
                let mut m = meta;
                m["samples"] = json!(samples);
                sink.row(&m)?;
            }
        }
        counts.insert("r20".into(), json!(sink.rows));
    }
    // R21: per sample, raw + converted
    {
        let mut sink = Sink::new(dir, if csv { "r21_samples" } else { "r21" }, csv)?;
        let recs = load_r21(&conn, t0 as f64, t1 as f64)?;
        let series = crate::imu::series(&recs);
        if csv {
            for i in 0..series.t.len() {
                let a = |k: usize| series.accel_raw[k][i];
                let g = |k: usize| series.gyro_raw[k][i];
                sink.row(&json!({"t": series.t[i], "ax_raw": a(0), "ay_raw": a(1), "az_raw": a(2), "gx_raw": g(0), "gy_raw": g(1), "gz_raw": g(2),
                    "ax_g": a(0) / rec::ACCEL_LSB_PER_G, "ay_g": a(1) / rec::ACCEL_LSB_PER_G, "az_g": a(2) / rec::ACCEL_LSB_PER_G,
                    "gx_dps": g(0) * rec::GYRO_DPS_PER_LSB, "gy_dps": g(1) * rec::GYRO_DPS_PER_LSB, "gz_dps": g(2) * rec::GYRO_DPS_PER_LSB}))?;
            }
        } else {
            for r in &recs {
                sink.row(&json!({"ts": r.ts, "record_index": r.record_index, "accel_raw": r.accel_raw, "gyro_raw": r.gyro_raw, "accel_lsb_per_g": rec::ACCEL_LSB_PER_G, "gyro_dps_per_lsb": rec::GYRO_DPS_PER_LSB, "fs_hz_estimated": series.fs_hz, "decoder_version": DECODER_VERSION}))?;
            }
        }
        counts.insert("r21".into(), json!(sink.rows));
    }
    for (name, sql) in [
        ("live_hr", "SELECT ts, time_word, frame_id, hr, rr_ms, decoder_version FROM whoop5_live_hr WHERE ts >= ?1 AND ts < ?2 ORDER BY ts"),
        ("events", "SELECT ts, subseconds, event_id, name, frame_id, data_hex, decoded_json, decoder_version FROM whoop5_events WHERE ts >= ?1 AND ts < ?2 ORDER BY ts"),
    ] {
        let mut sink = Sink::new(dir, name, csv)?;
        let mut st = conn.prepare(sql)?;
        let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = st.query(params![t0, t1])?;
        while let Some(r) = rows.next()? {
            let mut o = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                o.insert(n.clone(), sql_to_json(r.get(i)?));
            }
            sink.row(&Value::Object(o))?;
        }
        counts.insert(name.into(), json!(sink.rows));
    }
    if args.include_raw {
        // original bytes, length-prefixed: [u32 LE len][frame bytes] ...
        let mut w = BufWriter::new(File::create(dir.join("raw_frames.bin"))?);
        let mut index = Sink::new(dir, "raw_frames_index", csv)?;
        let mut st = conn.prepare("SELECT evidence_id, captured_at, payload_hex FROM raw_evidence ORDER BY captured_at, evidence_id")?;
        let mut rows = st.query([])?;
        let mut offset = 0u64;
        while let Some(r) = rows.next()? {
            let Ok(bytes) = hex::decode(r.get::<_, String>(2)?.trim()) else { continue };
            w.write_all(&(bytes.len() as u32).to_le_bytes())?;
            w.write_all(&bytes)?;
            index.row(&json!({"evidence_id": r.get::<_, String>(0)?, "captured_at": r.get::<_, String>(1)?, "offset": offset, "len": bytes.len()}))?;
            offset += 4 + bytes.len() as u64;
        }
        counts.insert("raw_frames".into(), json!(index.rows));
    }
    std::fs::write(dir.join("schema.json"), serde_json::to_vec_pretty(&schema_json(csv))?)?;
    Ok(json!({"output_dir": args.output_dir, "format": if csv { "csv" } else { "jsonl" }, "rows": counts, "parquet": "not built in (no Arrow dependency in the iOS core); convert CSV/JSONL with pandas/pyarrow"}))
}

fn sql_to_json(v: rusqlite::types::Value) -> Value {
    use rusqlite::types::Value as V;
    match v {
        V::Null => Value::Null,
        V::Integer(i) => json!(i),
        V::Real(f) => json!(f),
        V::Text(t) => json!(t),
        V::Blob(b) => json!(hex::encode(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whoop5_records::tests::{r18_payload, r20_payload, r21_payload};

    fn db_with(payloads: &[Vec<u8>]) -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("t.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE decoded_frames (frame_id TEXT PRIMARY KEY, payload_hex TEXT, header_crc_valid INTEGER, payload_crc_valid INTEGER, packet_type INTEGER, raw_len INTEGER);
             CREATE TABLE raw_evidence (evidence_id TEXT, captured_at TEXT, payload_hex TEXT);",
        )
        .unwrap();
        for (i, p) in payloads.iter().enumerate() {
            conn.execute("INSERT INTO decoded_frames VALUES (?1, ?2, 1, 1, ?3, ?4)", params![format!("f{i}"), hex::encode(p), p[0], p.len() + 12]).unwrap();
        }
        (dir, conn)
    }

    #[test]
    fn ingest_is_incremental_dedupes_and_counts_crc_failures() {
        let a = r18_payload(1_794_000_000, 60, &[1024], 10, 0);
        let (_d, conn) = db_with(&[a.clone(), a.clone(), r20_payload([3, 0, 0, 0, 0]), r21_payload(100, (0, 0, 4096), (0, 0, 0)), vec![47, 99, 0, 0]]);
        conn.execute("INSERT INTO decoded_frames VALUES ('bad', ?1, 1, 0, 47, 124)", [hex::encode(&a)]).unwrap();
        let s = ingest(&conn).unwrap();
        assert_eq!(s.frames_scanned, 6);
        assert_eq!(s.crc_invalid_skipped, 1);
        assert_eq!(s.inserted["whoop5_r18"], 1);
        assert_eq!(s.duplicates, 1);
        assert_eq!(s.inserted["whoop5_r20_slot"], 10);
        assert_eq!(s.unknown, 1);
        assert_eq!(ingest(&conn).unwrap().frames_scanned, 0);
        let r21 = load_r21(&conn, 0.0, f64::MAX).unwrap();
        assert_eq!(r21[0].accel_raw[2][99], 4096);
        let inv = inventory(&conn).unwrap();
        assert_eq!(inv["r18"]["rows"], 1);
        assert_eq!(inv["unknown_layouts"][0]["layout"], 99);
    }

    #[test]
    fn export_writes_jsonl_and_csv_with_schema() {
        let (d, conn) = db_with(&[r18_payload(1_794_000_000, 60, &[1024], 10, 0), r21_payload(10, (1, 2, 4096), (3, 4, 5))]);
        drop(conn);
        let db = d.path().join("t.sqlite").to_string_lossy().to_string();
        for fmt in ["jsonl", "csv"] {
            let out = d.path().join(fmt);
            let r = export(&ExportArgs { database_path: db.clone(), output_dir: out.to_string_lossy().into(), format: Some(fmt.into()), start_ts: None, end_ts: None, include_raw: true }).unwrap();
            assert_eq!(r["rows"]["r18"], 1);
            assert!(out.join("schema.json").exists());
        }
        let csv = std::fs::read_to_string(d.path().join("csv/r21_samples.csv")).unwrap();
        assert_eq!(csv.lines().count(), 11);
        assert!(csv.lines().next().unwrap().contains("az_g"));
    }
}
