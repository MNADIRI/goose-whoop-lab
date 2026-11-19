//! Development: a time slice of the database in a small file (the full database is over the size a
//! device copy can transfer). Tables with a `ts` column keep [from, to]; small tables are copied
//! whole; large tables without `ts` are left out.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::GooseResult;

#[derive(Debug, Deserialize)]
pub struct SliceArgs {
    pub database_path: String,
    pub out_path: String,
    pub from: i64,
    pub to: i64,
}

pub fn export_slice(args: &SliceArgs) -> GooseResult<Value> {
    let _ = std::fs::remove_file(&args.out_path);
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute("ATTACH DATABASE ?1 AS out", [&args.out_path])?;
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM main.sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut copied = Vec::new();
    for t in tables {
        let cols: Vec<String> = conn.prepare(&format!("PRAGMA main.table_info(\"{t}\")"))?.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
        let n = if cols.iter().any(|c| c == "ts") {
            conn.execute(&format!("CREATE TABLE out.\"{t}\" AS SELECT * FROM main.\"{t}\" WHERE ts >= ?1 AND ts <= ?2"), params![args.from, args.to])?
        } else {
            // small tables only (sessions, state, models...): a row count on a huge table is slow
            let rows: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM (SELECT 1 FROM main.\"{t}\" LIMIT 200001)"), [], |r| r.get(0))?;
            if rows > 200_000 {
                continue;
            }
            conn.execute(&format!("CREATE TABLE out.\"{t}\" AS SELECT * FROM main.\"{t}\""), [])?
        };
        copied.push(json!({"table": t, "rows": n}));
    }
    conn.execute("DETACH DATABASE out", [])?;
    let size = std::fs::metadata(&args.out_path).map(|m| m.len()).unwrap_or(0);
    Ok(json!({"out_path": args.out_path, "bytes": size, "tables": copied}))
}

#[derive(Debug, Deserialize)]
pub struct StatsArgs {
    pub database_path: String,
    pub out_path: String,
    /// window for the per-type volume (hours back from the newest frame)
    #[serde(default)]
    pub hours: Option<i64>,
}

/// Development: row counts and byte volumes of the capture tables per packet type and layout over
/// the last `hours`, and the oldest / newest capture, written as JSON next to the slice.
pub fn stats(args: &StatsArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    let hours = args.hours.unwrap_or(24);
    let max_rowid: i64 = conn.query_row("SELECT IFNULL(MAX(rowid), 0) FROM decoded_frames", [], |r| r.get(0))?;
    let newest: Option<String> = conn.query_row("SELECT created_at FROM decoded_frames WHERE rowid = ?1", [max_rowid], |r| r.get(0)).optional()?;
    let oldest: Option<String> = conn.query_row("SELECT created_at FROM decoded_frames ORDER BY rowid LIMIT 1", [], |r| r.get(0)).optional()?;
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM decoded_frames", [], |r| r.get(0))?;
    let since = conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', ?1, ?2)", params![newest.clone().unwrap_or_default(), format!("-{hours} hours")], |r| r.get::<_, Option<String>>(0))?;
    // rowid of the first frame in the window (rowids follow insertion)
    let mut lo = 0i64;
    let mut hi = max_rowid;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let c: Option<String> = conn.query_row("SELECT created_at FROM decoded_frames WHERE rowid >= ?1 ORDER BY rowid LIMIT 1", [mid], |r| r.get(0)).optional()?;
        if c.as_deref().unwrap_or("") < since.as_deref().unwrap_or("") { lo = mid + 1 } else { hi = mid }
    }
    let mut st = conn.prepare(
        "SELECT packet_type, substr(payload_hex, 3, 2), COUNT(*), SUM(length(payload_hex)), SUM(length(parsed_payload_json)), SUM(length(frame_id) + length(evidence_id) + length(warnings_json) + 120)
         FROM decoded_frames WHERE rowid >= ?1 GROUP BY 1, 2 ORDER BY 4 DESC",
    )?;
    let types: Vec<Value> = st
        .query_map([lo], |r| {
            Ok(json!({"packet_type": r.get::<_, Option<i64>>(0)?, "layout_hex": r.get::<_, Option<String>>(1)?, "rows": r.get::<_, i64>(2)?,
                "payload_hex_bytes": r.get::<_, Option<i64>>(3)?, "json_bytes": r.get::<_, Option<i64>>(4)?, "other_bytes": r.get::<_, Option<i64>>(5)?}))
        })?
        .collect::<Result<_, _>>()?;
    let raw: (i64, Option<i64>) = conn.query_row("SELECT COUNT(*), SUM(length(payload_hex) + length(evidence_id) + length(sha256) + 150) FROM raw_evidence WHERE captured_at >= ?1", [since.clone().unwrap_or_default()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let page: (i64, i64, i64) = conn.query_row("SELECT (SELECT page_count FROM pragma_page_count), (SELECT page_size FROM pragma_page_size), (SELECT freelist_count FROM pragma_freelist_count)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    let v = json!({"decoded_frames_total": total, "oldest": oldest, "newest": newest, "window_hours": hours, "window_from_rowid": lo, "by_type": types,
        "raw_evidence_window": {"rows": raw.0, "bytes": raw.1}, "file_bytes": page.0 * page.1, "free_bytes": page.2 * page.1, "auto_vacuum": auto_vacuum});
    std::fs::write(&args.out_path, serde_json::to_string_pretty(&v).unwrap_or_default())?;
    Ok(v)
}
