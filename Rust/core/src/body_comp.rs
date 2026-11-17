//! Body composition: occasional bioimpedance (BIA) measurements, stored locally, turned into a
//! longitudinal dataset and joined with the continuous strap data (docs/BODY_COMPOSITION.md).
//!
//! * Import: InBody CSV export (every column kept verbatim, "-" = missing), HealthKit samples
//!   (weight, body fat %, lean body mass). Deterministic ids: re-importing never duplicates.
//! * Derived: fat-free mass, FFMI / FMI, muscle-to-fat ratio, left/right asymmetry, ECW/TBW, BMR
//!   by three equations (the InBody BMR *is* Katch-McArdle on the device's own FFM, so their
//!   agreement is not a validation).
//! * Longitudinal: changes vs the previous comparable measurement, recomposition classes from a
//!   regression over >= 3 measurements and >= 28 days, judged against BIA measurement error.
//! * Strap context: training, sleep, HRV, resting HR and energy over the 7/28/56/84 days *before*
//!   each measurement (no future data, nothing interpolated between measurements).
//!
//! The daily recovery score does not use any of this (no evidence of added predictive value).

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::dsp::{mean, round, std_sample};
use crate::GooseResult;

pub const PARSER_VERSION: &str = "inbody-csv-1";
pub const ALGORITHM_VERSION: &str = "bodycomp-1";

/// Precision assumed for one BIA measurement (standard error of measurement, same device, standard
/// conditions). Multi-frequency BIA test-retest studies report SEMs of ~0.3-0.7 kg for fat and lean
/// mass; day-to-day hydration and food add to it. Two measurements differ "for real" only beyond
/// the minimal detectable change MDC95 = 1.96 x sqrt(2) x SEM.
pub const SEM: [(&str, f64); 6] = [
    ("weight_kg", 0.5),
    ("fat_mass_kg", 0.6),
    ("smm_kg", 0.35),
    ("ffm_kg", 0.5),
    ("pbf_pct", 0.8),
    ("tbw_l", 0.5),
];

fn sem(key: &str) -> f64 {
    SEM.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(0.5)
}

pub fn mdc95(key: &str) -> f64 {
    1.96 * std::f64::consts::SQRT_2 * sem(key)
}

// ------------------------------------------------------------------ schema / catalog

/// canonical key, unit, category, physiological (false = proprietary device index)
struct Field {
    key: &'static str,
    unit: &'static str,
    category: &'static str,
    physiological: bool,
}

const fn f(key: &'static str, unit: &'static str, category: &'static str, physiological: bool) -> Field {
    Field { key, unit, category, physiological }
}

/// InBody export headers (normalised: lower case, letters and digits only) -> canonical field.
/// Unknown headers are still kept in the raw columns.
const INBODY: &[(&str, Field)] = &[
    ("weightkg", f("weight_kg", "kg", "anthropometric", true)),
    ("bmikgm2", f("bmi", "kg/m2", "anthropometric", true)),
    ("waistcircumferencecm", f("waist_cm", "cm", "anthropometric", true)),
    ("waisthipratio", f("whr", "ratio", "anthropometric", true)),
    ("skeletalmusclemasskg", f("smm_kg", "kg", "composition", true)),
    ("softleanmasskg", f("slm_kg", "kg", "composition", true)),
    ("bodyfatmasskg", f("fat_mass_kg", "kg", "composition", true)),
    ("percentbodyfat", f("pbf_pct", "%", "composition", true)),
    ("smikgm2", f("smi", "kg/m2", "composition", true)),
    ("rightarmleanmasskg", f("lean_right_arm_kg", "kg", "segmental_lean", true)),
    ("leftarmleanmasskg", f("lean_left_arm_kg", "kg", "segmental_lean", true)),
    ("trunkleanmasskg", f("lean_trunk_kg", "kg", "segmental_lean", true)),
    ("rightlegleanmasskg", f("lean_right_leg_kg", "kg", "segmental_lean", true)),
    ("leftlegleanmasskg", f("lean_left_leg_kg", "kg", "segmental_lean", true)),
    ("legleanmasskg", f("lean_legs_kg", "kg", "segmental_lean", true)),
    ("rightarmfatmasskg", f("fat_right_arm_kg", "kg", "segmental_fat", true)),
    ("leftarmfatmasskg", f("fat_left_arm_kg", "kg", "segmental_fat", true)),
    ("trunkfatmasskg", f("fat_trunk_kg", "kg", "segmental_fat", true)),
    ("rightlegfatmasskg", f("fat_right_leg_kg", "kg", "segmental_fat", true)),
    ("leftlegfatmasskg", f("fat_left_leg_kg", "kg", "segmental_fat", true)),
    ("totalbodywaterl", f("tbw_l", "L", "hydration", true)),
    ("intracellularwaterl", f("icw_l", "L", "hydration", true)),
    ("extracellularwaterl", f("ecw_l", "L", "hydration", true)),
    ("ecwratio", f("ecw_ratio", "ratio", "hydration", true)),
    ("rightarmecwratio", f("ecw_ratio_right_arm", "ratio", "hydration", true)),
    ("leftarmecwratio", f("ecw_ratio_left_arm", "ratio", "hydration", true)),
    ("trunkecwratio", f("ecw_ratio_trunk", "ratio", "hydration", true)),
    ("rightlegecwratio", f("ecw_ratio_right_leg", "ratio", "hydration", true)),
    ("leftlegecwratio", f("ecw_ratio_left_leg", "ratio", "hydration", true)),
    ("basalmetabolicratekcal", f("bmr_device_kcal", "kcal/day", "metabolic", true)),
    ("wholebodyphaseangle", f("phase_angle_deg", "deg", "physiological", true)),
    ("bodycellmasskg", f("bcm_kg", "kg", "physiological", true)),
    ("visceralfatareacm2", f("vfa_cm2", "cm2", "physiological", true)),
    ("proteinkg", f("protein_kg", "kg", "physiological", true)),
    ("mineralkg", f("mineral_kg", "kg", "physiological", true)),
    ("bonemineralcontentkg", f("bmc_kg", "kg", "physiological", true)),
    ("inbodyage", f("inbody_age", "years", "proprietary", false)),
    ("inbodyscore", f("inbody_score", "points", "proprietary", false)),
    ("visceralfatlevellevel", f("visceral_fat_level", "level", "proprietary", false)),
    ("legmusclelevellevel", f("leg_muscle_level", "level", "proprietary", false)),
    ("upperlower", f("balance_upper_lower", "code", "proprietary", false)),
    ("upper", f("balance_upper", "code", "proprietary", false)),
    ("lower", f("balance_lower", "code", "proprietary", false)),
];

fn norm(h: &str) -> String {
    // "kg/m²" and "cm²" keep their exponent
    h.chars().map(|c| match c { '²' => '2', '³' => '3', _ => c }).filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

pub fn catalog() -> Value {
    Value::Array(INBODY.iter().map(|(h, fl)| json!({"header_norm": h, "key": fl.key, "unit": fl.unit, "category": fl.category, "physiological": fl.physiological})).collect())
}

pub fn ensure_tables(conn: &Connection) -> GooseResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS body_comp_measurements (
            id TEXT PRIMARY KEY, ts INTEGER NOT NULL, local_time TEXT, source TEXT NOT NULL, device TEXT,
            values_json TEXT NOT NULL, raw_json TEXT, file_name TEXT, parser_version TEXT NOT NULL,
            content_hash TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 1, excluded INTEGER NOT NULL DEFAULT 0,
            note TEXT, imported_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS body_comp_ts ON body_comp_measurements (ts);",
    )?;
    Ok(())
}

// ------------------------------------------------------------------ CSV

/// RFC 4180-style split (quotes, doubled quotes, commas inside quotes).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).collect()
}

/// "-" or empty = missing; decimal comma accepted.
fn number(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t == "-" || t == "--" {
        return None;
    }
    t.replace(',', ".").parse::<f64>().ok().filter(|v| v.is_finite())
}

/// "20261115123039" (local wall-clock time) -> ("2026-11-15T12:30:39", unix with the given offset)
fn inbody_time(s: &str, utc_offset_s: i64) -> Option<(String, i64)> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() < 12 {
        return None;
    }
    let n = |a: usize, b: usize| d[a..b].parse::<i64>().ok();
    let (y, mo, da, h, mi) = (n(0, 4)?, n(4, 6)?, n(6, 8)?, n(8, 10)?, n(10, 12)?);
    let se = if d.len() >= 14 { n(12, 14)? } else { 0 };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    // days from civil (Howard Hinnant)
    let (yy, m) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * m + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let local = days * 86_400 + h * 3600 + mi * 60 + se;
    Some((format!("{y:04}-{mo:02}-{da:02}T{h:02}:{mi:02}:{se:02}"), local - utc_offset_s))
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub id: String,
    pub ts: i64,
    pub local_time: String,
    pub device: Option<String>,
    pub values: Map<String, Value>,
    pub raw: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

pub fn parse_inbody_csv(text: &str, utc_offset_s: i64) -> GooseResult<(Vec<Parsed>, Vec<String>)> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or_else(|| crate::GooseError::Message("empty CSV".into()))?;
    let cols = split_csv_line(header);
    let date_i = cols.iter().position(|c| norm(c) == "date").ok_or_else(|| crate::GooseError::Message("no Date column: not an InBody export".into()))?;
    let dev_i = cols.iter().position(|c| norm(c).starts_with("measurementdevice"));
    let mapped: Vec<Option<&Field>> = cols.iter().map(|c| INBODY.iter().find(|(h, _)| *h == norm(c)).map(|(_, fl)| fl)).collect();
    let unknown: Vec<String> = cols.iter().enumerate().filter(|(i, _)| mapped[*i].is_none() && *i != date_i && Some(*i) != dev_i).map(|(_, c)| c.clone()).collect();
    let mut out = Vec::new();
    for (row_no, line) in lines.enumerate() {
        let cells = split_csv_line(line);
        let mut warnings = Vec::new();
        if cells.len() != cols.len() {
            warnings.push(format!("row {}: {} cells for {} columns", row_no + 2, cells.len(), cols.len()));
        }
        let cell = |i: usize| cells.get(i).map(String::as_str).unwrap_or("");
        let Some((local_time, ts)) = inbody_time(cell(date_i), utc_offset_s) else {
            warnings.push(format!("row {}: unreadable date {:?}, skipped", row_no + 2, cell(date_i)));
            continue;
        };
        let device = dev_i.map(|i| cell(i).to_string()).filter(|s| !s.is_empty() && s != "-");
        let mut values = Map::new();
        for (i, fl) in mapped.iter().enumerate() {
            if let Some(fl) = fl {
                let raw = cell(i);
                let v = number(raw);
                if v.is_none() && !raw.is_empty() && raw != "-" {
                    warnings.push(format!("{}: {:?} is not a number", fl.key, raw));
                }
                values.insert(fl.key.to_string(), v.map(|x| json!(x)).unwrap_or(Value::Null));
            }
        }
        let raw: Vec<(String, String)> = cols.iter().enumerate().map(|(i, c)| (c.clone(), cell(i).to_string())).collect();
        let id = format!("inbody:{}:{}", device.as_deref().unwrap_or("unknown"), local_time);
        out.push(Parsed { id, ts, local_time, device, values, raw, warnings });
    }
    Ok((out, unknown))
}

fn hash(values: &Map<String, Value>, raw: &[(String, String)]) -> String {
    let d = Sha256::digest(json!({"v": values, "r": raw}).to_string().as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Deserialize)]
pub struct ImportCsvArgs {
    pub database_path: String,
    pub csv: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

/// Same id + same content = unchanged; same id + new content (a corrected export) = new revision.
pub fn import_csv(args: &ImportCsvArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let now = args.now_unix.unwrap_or_else(now_s);
    let (rows, unknown) = parse_inbody_csv(&args.csv, args.utc_offset_seconds)?;
    let (mut added, mut updated, mut unchanged) = (0, 0, 0);
    let mut warnings: Vec<String> = Vec::new();
    for p in &rows {
        warnings.extend(p.warnings.iter().cloned());
        let h = hash(&p.values, &p.raw);
        let old: Option<String> = conn.query_row("SELECT content_hash FROM body_comp_measurements WHERE id = ?1", [&p.id], |r| r.get(0)).optional()?;
        let raw = Value::Array(p.raw.iter().map(|(k, v)| json!([k, v])).collect()).to_string();
        let vals = Value::Object(p.values.clone()).to_string();
        match old {
            Some(o) if o == h => unchanged += 1,
            Some(_) => {
                conn.execute(
                    "UPDATE body_comp_measurements SET ts = ?2, values_json = ?3, raw_json = ?4, file_name = ?5, parser_version = ?6, content_hash = ?7, revision = revision + 1, updated_at = ?8 WHERE id = ?1",
                    params![p.id, p.ts, vals, raw, args.file_name, PARSER_VERSION, h, now],
                )?;
                updated += 1;
            }
            None => {
                conn.execute(
                    "INSERT INTO body_comp_measurements (id, ts, local_time, source, device, values_json, raw_json, file_name, parser_version, content_hash, imported_at, updated_at)
                     VALUES (?1, ?2, ?3, 'inbody_csv', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                    params![p.id, p.ts, p.local_time, p.device, vals, raw, args.file_name, PARSER_VERSION, h, now],
                )?;
                added += 1;
            }
        }
    }
    Ok(json!({"rows": rows.len(), "added": added, "updated": updated, "unchanged": unchanged, "unknown_columns": unknown, "warnings": warnings, "parser_version": PARSER_VERSION}))
}

#[derive(Debug, Deserialize)]
pub struct HealthKitSample {
    pub uuid: String,
    pub ts: i64,
    /// "weight_kg", "pbf_pct" (0-100), "lean_mass_kg" (HealthKit lean body mass = fat-free mass)
    pub kind: String,
    pub value: f64,
    #[serde(default)]
    pub source_name: Option<String>,
    #[serde(default)]
    pub source_bundle: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ImportHealthKitArgs {
    pub database_path: String,
    pub samples: Vec<HealthKitSample>,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

/// HealthKit samples within 10 minutes of each other (one weighing) become one measurement; a
/// measurement within 10 minutes of an InBody CSV row is the same weighing seen twice (the InBody
/// app writes to Health) and is skipped.
pub fn import_healthkit(args: &ImportHealthKitArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let now = args.now_unix.unwrap_or_else(now_s);
    let mut s: Vec<&HealthKitSample> = args.samples.iter().filter(|x| x.value.is_finite() && x.value > 0.0).collect();
    s.sort_by_key(|x| x.ts);
    let mut groups: Vec<Vec<&HealthKitSample>> = Vec::new();
    for x in s {
        match groups.last_mut() {
            Some(g) if x.ts - g[0].ts <= 600 => g.push(x),
            _ => groups.push(vec![x]),
        }
    }
    let (mut added, mut duplicates, mut known) = (0, 0, 0);
    for g in groups {
        let ts = g[0].ts;
        let mut uuids: Vec<&str> = g.iter().map(|x| x.uuid.as_str()).collect();
        uuids.sort();
        let id = format!("healthkit:{}", uuids.join("+"));
        if conn.query_row("SELECT 1 FROM body_comp_measurements WHERE id = ?1", [&id], |_| Ok(())).optional()?.is_some() {
            known += 1;
            continue;
        }
        let csv_twin: bool = conn
            .query_row("SELECT 1 FROM body_comp_measurements WHERE source = 'inbody_csv' AND ABS(ts - ?1) <= 600 LIMIT 1", [ts], |_| Ok(()))
            .optional()?
            .is_some();
        if csv_twin {
            duplicates += 1;
            continue;
        }
        let mut values = Map::new();
        for x in &g {
            let (k, v) = match x.kind.as_str() {
                "weight_kg" => ("weight_kg", x.value),
                "pbf_pct" => ("pbf_pct", if x.value <= 1.0 { x.value * 100.0 } else { x.value }),
                "lean_mass_kg" => ("ffm_kg", x.value),
                _ => continue,
            };
            values.insert(k.into(), json!(round(v, 2)));
        }
        if let (Some(w), Some(p)) = (values.get("weight_kg").and_then(Value::as_f64), values.get("pbf_pct").and_then(Value::as_f64)) {
            values.entry("fat_mass_kg").or_insert(json!(round(w * p / 100.0, 2)));
        }
        let source = g.iter().find_map(|x| x.source_name.clone()).unwrap_or_else(|| "Santé".into());
        let raw = Value::Array(g.iter().map(|x| json!({"uuid": x.uuid, "ts": x.ts, "kind": x.kind, "value": x.value, "source": x.source_name, "bundle": x.source_bundle})).collect());
        conn.execute(
            "INSERT INTO body_comp_measurements (id, ts, source, device, values_json, raw_json, parser_version, content_hash, imported_at, updated_at)
             VALUES (?1, ?2, 'healthkit', ?3, ?4, ?5, 'healthkit-1', ?6, ?7, ?7)",
            params![id, ts, source, Value::Object(values.clone()).to_string(), raw.to_string(), hash(&values, &[]), now],
        )?;
        added += 1;
    }
    Ok(json!({"added": added, "skipped_same_weighing_as_csv": duplicates, "already_known": known}))
}

#[derive(Debug, Deserialize)]
pub struct ExcludeArgs {
    pub database_path: String,
    pub id: String,
    pub excluded: bool,
    #[serde(default)]
    pub note: Option<String>,
}

/// A measurement taken in non-standard conditions (after a workout, a meal, a sauna) can be kept
/// but left out of the trends.
pub fn set_excluded(args: &ExcludeArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    let n = conn.execute("UPDATE body_comp_measurements SET excluded = ?2, note = COALESCE(?3, note), updated_at = ?4 WHERE id = ?1", params![args.id, args.excluded, args.note, now_s()])?;
    Ok(json!({"updated": n}))
}

#[derive(Debug, Deserialize)]
pub struct DeleteArgs {
    pub database_path: String,
    pub id: String,
}

pub fn delete(args: &DeleteArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    ensure_tables(&conn)?;
    Ok(json!({"deleted": conn.execute("DELETE FROM body_comp_measurements WHERE id = ?1", [&args.id])?}))
}

// ------------------------------------------------------------------ derived metrics

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Person {
    #[serde(default)]
    pub height_cm: Option<f64>,
    #[serde(default)]
    pub age: Option<f64>,
    #[serde(default)]
    pub sex: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Measurement {
    pub id: String,
    pub ts: i64,
    pub local_time: Option<String>,
    pub source: String,
    pub device: Option<String>,
    pub values: Map<String, Value>,
    pub excluded: bool,
    pub note: Option<String>,
    pub revision: i64,
}

impl Measurement {
    pub fn v(&self, k: &str) -> Option<f64> {
        self.values.get(k).and_then(Value::as_f64)
    }
    /// fat-free mass: measured (HealthKit lean body mass) or weight - fat mass. Never the skeletal
    /// muscle mass or the soft lean mass (which excludes bone mineral).
    pub fn ffm(&self) -> Option<f64> {
        self.v("ffm_kg").or_else(|| Some(self.v("weight_kg")? - self.v("fat_mass_kg")?))
    }
    pub fn fat(&self) -> Option<f64> {
        self.v("fat_mass_kg").or_else(|| Some(self.v("weight_kg")? * self.v("pbf_pct")? / 100.0))
    }
}

pub fn load(conn: &Connection) -> GooseResult<Vec<Measurement>> {
    ensure_tables(conn)?;
    let mut st = conn.prepare("SELECT id, ts, local_time, source, device, values_json, excluded, note, revision FROM body_comp_measurements ORDER BY ts")?;
    let rows = st.query_map([], |r| {
        let vals: String = r.get(5)?;
        Ok(Measurement {
            id: r.get(0)?,
            ts: r.get(1)?,
            local_time: r.get(2)?,
            source: r.get(3)?,
            device: r.get(4)?,
            values: serde_json::from_str::<Value>(&vals).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default(),
            excluded: r.get::<_, i64>(6)? != 0,
            note: r.get(7)?,
            revision: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// height: the profile's, else derived from the device's own BMI (weight / BMI).
fn height_m(m: &Measurement, person: &Person) -> (Option<f64>, &'static str) {
    if let Some(h) = person.height_cm.filter(|h| (100.0..250.0).contains(h)) {
        return (Some(h / 100.0), "profile");
    }
    match (m.v("weight_kg"), m.v("bmi")) {
        (Some(w), Some(b)) if b > 0.0 => (Some((w / b).sqrt()), "from_device_bmi"),
        _ => (None, "unknown"),
    }
}

fn asym(r: Option<f64>, l: Option<f64>) -> Value {
    match (r, l) {
        (Some(r), Some(l)) if r + l > 0.0 => {
            let pct = (r - l) / ((r + l) / 2.0) * 100.0;
            json!({"right": r, "left": l, "diff_kg": round(r - l, 2), "pct": round(pct, 1), "larger": if pct.abs() < 0.5 { "equal" } else if pct > 0.0 { "right" } else { "left" }})
        }
        _ => Value::Null,
    }
}

pub fn mifflin(weight: f64, height_m: f64, age: f64, male: bool) -> f64 {
    10.0 * weight + 625.0 * height_m - 5.0 * age + if male { 5.0 } else { -161.0 }
}

pub fn katch_mcardle(ffm: f64) -> f64 {
    370.0 + 21.6 * ffm
}

pub fn derived(m: &Measurement, person: &Person) -> Value {
    let (h, h_src) = height_m(m, person);
    let ffm = m.ffm();
    let fat = m.fat();
    let w = m.v("weight_kg");
    let per_h2 = |x: Option<f64>| -> Option<f64> { Some(round(x? / h?.powi(2), 2)) };
    let tbw = m.v("tbw_l");
    let ecw_tbw = match (m.v("ecw_l"), tbw) {
        (Some(e), Some(t)) if t > 0.0 => Some(round(e / t, 3)),
        _ => m.v("ecw_ratio"),
    };
    let male = person.sex.as_deref() != Some("female");
    let bmr_k = ffm.map(katch_mcardle);
    let bmr_c = ffm.map(|x| 500.0 + 22.0 * x);
    let bmr_m = match (w, h, person.age) {
        (Some(w), Some(h), Some(a)) => Some(mifflin(w, h, a, male)),
        _ => None,
    };
    let device = m.v("bmr_device_kcal");
    let same_eq = matches!((device, bmr_k), (Some(d), Some(k)) if (d - k).abs() <= 5.0);
    let mut quality = Vec::new();
    if let Some(r) = ecw_tbw {
        if !(0.36..=0.39).contains(&r) {
            quality.push(format!("ECW/TBW {r:.3} outside the device reference 0.360-0.390: hydration may bias lean / fat estimates"));
        }
    }
    let hydration_ffm = match (tbw, ffm) {
        (Some(t), Some(f)) if f > 0.0 => Some(round(t / f, 3)),
        _ => None,
    };
    if let Some(x) = hydration_ffm {
        if !(0.69..=0.77).contains(&x) {
            quality.push(format!("water / fat-free mass {x:.3} outside 0.69-0.77 (expected ~0.73)"));
        }
    }
    if w.is_none() || fat.is_none() {
        quality.push("weight or fat mass missing: core indices unavailable".into());
    }
    json!({
        "height_m": h.map(|v| round(v, 3)), "height_source": h_src,
        "ffm_kg": ffm.map(|v| round(v, 2)),
        "fat_mass_kg": fat.map(|v| round(v, 2)),
        "ffmi": per_h2(ffm), "fmi": per_h2(fat),
        "smm_to_fat": match (m.v("smm_kg"), fat) { (Some(s), Some(f)) if f > 0.0 => Some(round(s / f, 2)), _ => None },
        "smm_pct_weight": match (m.v("smm_kg"), w) { (Some(s), Some(w)) if w > 0.0 => Some(round(100.0 * s / w, 1)), _ => None },
        "arms": asym(m.v("lean_right_arm_kg"), m.v("lean_left_arm_kg")),
        "legs": asym(m.v("lean_right_leg_kg"), m.v("lean_left_leg_kg")),
        "arms_fat": asym(m.v("fat_right_arm_kg"), m.v("fat_left_arm_kg")),
        "legs_fat": asym(m.v("fat_right_leg_kg"), m.v("fat_left_leg_kg")),
        "ecw_tbw": ecw_tbw,
        "icw_ecw": match (m.v("icw_l"), m.v("ecw_l")) { (Some(i), Some(e)) if e > 0.0 => Some(round(i / e, 2)), _ => None },
        "water_per_ffm": hydration_ffm,
        "bmr": {
            "device_kcal": device,
            "katch_mcardle_kcal": bmr_k.map(|v| v.round()),
            "cunningham_kcal": bmr_c.map(|v| v.round()),
            "mifflin_st_jeor_kcal": bmr_m.map(|v| v.round()),
            "device_is_katch_mcardle": same_eq,
            "note": "Katch-McArdle and Cunningham both use the BIA fat-free mass: their agreement with the device is not an independent check. Mifflin-St Jeor uses weight, height, age and sex only.",
        },
        "quality": quality,
    })
}

// ------------------------------------------------------------------ longitudinal

fn usable(ms: &[Measurement]) -> Vec<&Measurement> {
    ms.iter().filter(|m| !m.excluded).collect()
}

fn value_of(m: &Measurement, key: &str) -> Option<f64> {
    match key {
        "ffm_kg" => m.ffm(),
        "fat_mass_kg" => m.fat(),
        _ => m.v(key),
    }
}

/// Least squares over (days, value): slope per day and its standard error, the noise being the
/// larger of the residual SD and the assumed measurement SEM.
fn fit(points: &[(f64, f64)], key: &str) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let tm = points.iter().map(|p| p.0).sum::<f64>() / n;
    let vm = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - tm).powi(2)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let slope = points.iter().map(|p| (p.0 - tm) * (p.1 - vm)).sum::<f64>() / sxx;
    let resid_sd = if points.len() > 2 {
        (points.iter().map(|p| (p.1 - vm - slope * (p.0 - tm)).powi(2)).sum::<f64>() / (n - 2.0)).sqrt()
    } else {
        0.0
    };
    let noise = resid_sd.max(sem(key));
    Some((slope, noise / sxx.sqrt()))
}

/// change over the span of the fit, with its 95 % half-width, and its direction if it is beyond
/// the error ("up" / "down" / "stable")
fn trend(ms: &[&Measurement], key: &str) -> Value {
    let pts: Vec<(f64, f64)> = ms.iter().filter_map(|m| value_of(m, key).map(|v| (m.ts as f64 / 86_400.0, v))).collect();
    let Some((slope, se)) = fit(&pts, key) else { return Value::Null };
    let span = pts.last().unwrap().0 - pts[0].0;
    let change = slope * span;
    let half = 1.96 * se * span;
    let dir = if change.abs() <= half { "stable" } else if change > 0.0 { "up" } else { "down" };
    json!({"n": pts.len(), "span_days": round(span, 0), "change": round(change, 2), "ci95": round(half, 2), "per_week": round(slope * 7.0, 3), "direction": dir})
}

pub const MIN_TREND_MEASUREMENTS: usize = 3;
pub const MIN_TREND_DAYS: f64 = 28.0;

pub fn recomposition(ms: &[&Measurement]) -> Value {
    let span = match (ms.first(), ms.last()) {
        (Some(a), Some(b)) => (b.ts - a.ts) as f64 / 86_400.0,
        _ => 0.0,
    };
    if ms.len() < MIN_TREND_MEASUREMENTS || span < MIN_TREND_DAYS {
        return json!({"class": "insufficient_data", "measurements": ms.len(), "span_days": round(span, 0),
            "needed": format!("{MIN_TREND_MEASUREMENTS} measurements over {MIN_TREND_DAYS:.0} days or more, same device and conditions")});
    }
    let smm = trend(ms, "smm_kg");
    let fat = trend(ms, "fat_mass_kg");
    let ffm = trend(ms, "ffm_kg");
    let tbw = trend(ms, "tbw_l");
    let d = |t: &Value| t["direction"].as_str().unwrap_or("unknown").to_string();
    // skeletal muscle when measured, else fat-free mass
    let muscle = if smm.is_null() { d(&ffm) } else { d(&smm) };
    let class = match (muscle.as_str(), d(&fat).as_str()) {
        ("up", "down") => "muscle_gain_fat_loss",
        ("up", "stable") => "muscle_gain_stable_fat",
        ("stable", "down") => "fat_loss_muscle_preserved",
        ("down", _) => "possible_muscle_loss",
        ("stable", "stable") => "indeterminate",
        _ => "mixed",
    };
    // lean tissue is ~73 % water: a lean change mostly made of water change may be hydration
    let hydration_caveat = match (ffm["change"].as_f64(), tbw["change"].as_f64()) {
        (Some(f), Some(t)) if f.abs() > 0.3 && (t / 0.73).abs() >= 0.7 * f.abs() && t.signum() == f.signum() => true,
        _ => false,
    };
    json!({"class": class, "measurements": ms.len(), "span_days": round(span, 0), "muscle": smm, "fat": fat, "ffm": ffm, "water": tbw,
           "weight": trend(ms, "weight_kg"), "pbf": trend(ms, "pbf_pct"), "hydration_may_explain_lean_change": hydration_caveat})
}

fn delta(a: &Measurement, b: &Measurement, key: &str) -> Value {
    match (value_of(a, key), value_of(b, key)) {
        (Some(x), Some(y)) => {
            let d = y - x;
            let mdc = mdc95(key);
            json!({"from": round(x, 2), "to": round(y, 2), "change": round(d, 2), "pct": if x != 0.0 { Some(round(100.0 * d / x, 1)) } else { None },
                   "mdc95": round(mdc, 2), "beyond_error": d.abs() > mdc})
        }
        _ => Value::Null,
    }
}

/// comparable = same device, measured within 2 h of the same time of day, ECW/TBW within 0.005
fn comparability(a: &Measurement, b: &Measurement) -> Value {
    let tod = |m: &Measurement| m.local_time.as_deref().and_then(|t| t.get(11..13)?.parse::<i64>().ok());
    let same_device = a.device == b.device && a.source == b.source;
    let hours = match (tod(a), tod(b)) {
        (Some(x), Some(y)) => Some((x - y).abs().min(24 - (x - y).abs())),
        _ => None,
    };
    let ecw = match (a.v("ecw_ratio"), b.v("ecw_ratio")) {
        (Some(x), Some(y)) => Some(round((y - x).abs(), 3)),
        _ => None,
    };
    let mut issues = Vec::new();
    if !same_device {
        issues.push("different device or source".to_string());
    }
    if hours.is_some_and(|h| h > 2) {
        issues.push(format!("time of day differs by {} h", hours.unwrap()));
    }
    if ecw.is_some_and(|e| e > 0.005) {
        issues.push("hydration (ECW/TBW) changed: lean and fat estimates may shift".into());
    }
    json!({"comparable": issues.is_empty(), "issues": issues, "hours_apart_time_of_day": hours, "ecw_ratio_change": ecw})
}

/// A left/right difference is "persistent" only when >= 3 measurements show the same side larger
/// by more than 5 %; a single measurement is only an observation (dominant side is normal).
fn segmental(ms: &[&Measurement]) -> Value {
    let seg = |r: &str, l: &str| -> Value {
        let obs: Vec<f64> = ms.iter().filter_map(|m| Some((m.v(r)? - m.v(l)?) / ((m.v(r)? + m.v(l)?) / 2.0) * 100.0)).collect();
        let persistent = obs.len() >= 3 && (obs.iter().all(|p| *p > 5.0) || obs.iter().all(|p| *p < -5.0));
        let r_tr = trend(ms, r);
        let l_tr = trend(ms, l);
        json!({"measurements": obs.len(), "latest_pct": obs.last().map(|v| round(*v, 1)), "mean_pct": mean(&obs).map(|v| round(v, 1)),
               "persistent_asymmetry": persistent, "right_trend": r_tr, "left_trend": l_tr})
    };
    let base = |k: &str| ms.first().and_then(|m| m.v(k));
    let latest = |k: &str| ms.last().and_then(|m| m.v(k));
    let vs_base = |k: &str| match (base(k), latest(k)) {
        (Some(a), Some(b)) if ms.len() >= 2 => json!({"baseline": a, "latest": b, "change": round(b - a, 2)}),
        _ => Value::Null,
    };
    json!({
        "arms": seg("lean_right_arm_kg", "lean_left_arm_kg"),
        "legs": seg("lean_right_leg_kg", "lean_left_leg_kg"),
        "vs_baseline": {
            "right_arm": vs_base("lean_right_arm_kg"), "left_arm": vs_base("lean_left_arm_kg"), "trunk": vs_base("lean_trunk_kg"),
            "right_leg": vs_base("lean_right_leg_kg"), "left_leg": vs_base("lean_left_leg_kg"),
        },
        "rule": "persistent = same side larger by > 5 % on >= 3 measurements",
    })
}

// ------------------------------------------------------------------ strap context

/// One analysed day from the strap pipeline (whoop_lab_days), only what the joins need.
#[derive(Debug, Clone, Default)]
pub struct StrapDay {
    pub day_start: i64,
    pub sleep_min: Option<f64>,
    pub rmssd: Option<f64>,
    pub rhr: Option<f64>,
    pub strain_pct: Option<f64>,
    pub total_kcal: Option<f64>,
    pub active_kcal: Option<f64>,
    pub coverage_h: Option<f64>,
}

fn load_days(conn: &Connection, utc_offset_s: i64) -> GooseResult<Vec<StrapDay>> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='whoop_lab_days'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !exists {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare("SELECT date_key, result_json FROM whoop_lab_days ORDER BY date_key")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (key, j) = row?;
        let Ok(v) = serde_json::from_str::<Value>(&j) else { continue };
        let Some((_, ts)) = inbody_time(&format!("{}000000", key.replace('-', "")), utc_offset_s) else { continue };
        let n = &v["night"];
        let d = &v["day"];
        let ok_night = n["available"] == true;
        out.push(StrapDay {
            day_start: ts,
            sleep_min: if ok_night { n["asleep_min"].as_f64() } else { None },
            rmssd: if ok_night { n["hrv_night"]["rmssd_ms"].as_f64() } else { None },
            rhr: if ok_night { n["resting_hr_bpm"].as_f64() } else { None },
            strain_pct: d["strain_pct"].as_f64(),
            total_kcal: d["energy"]["total_kcal"].as_f64(),
            active_kcal: d["energy"]["active_kcal"].as_f64(),
            coverage_h: d["coverage_h"].as_f64(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct SessionRow {
    start: i64,
    end: i64,
    kind: String,
    sets: i64,
    reps: i64,
}

fn load_sessions(conn: &Connection) -> GooseResult<Vec<SessionRow>> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sport_sessions'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !exists {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare(
        "SELECT s.start_ts, COALESCE(s.end_ts, s.start_ts), COALESCE(s.label_type, s.predicted_type, 'autre'),
                (SELECT COUNT(*) FROM sport_segments g WHERE g.session_id = s.id AND g.kind = 'set' AND g.label_status != 'rejected'),
                (SELECT COALESCE(SUM(COALESCE(g.reps_label, g.reps_pred, 0)), 0) FROM sport_segments g WHERE g.session_id = s.id AND g.kind = 'set' AND g.label_status != 'rejected')
         FROM sport_sessions s WHERE s.status = 'ended' AND s.label_status != 'rejected' AND COALESCE(s.label_type, '') != 'aucune'",
    )?;
    let rows = st.query_map([], |r| Ok(SessionRow { start: r.get(0)?, end: r.get(1)?, kind: r.get(2)?, sets: r.get(3)?, reps: r.get(4)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Aggregates over [t - days, t): strictly before the measurement, never after.
fn window(days: &[StrapDay], sessions: &[SessionRow], t: i64, n_days: i64) -> Value {
    let from = t - n_days * 86_400;
    let ds: Vec<&StrapDay> = days.iter().filter(|d| d.day_start >= from && d.day_start < t).collect();
    let covered = ds.iter().filter(|d| d.coverage_h.unwrap_or(0.0) >= 16.0).count();
    let col = |f: &dyn Fn(&StrapDay) -> Option<f64>| -> Vec<f64> { ds.iter().filter_map(|d| f(d)).collect() };
    let sleep = col(&|d| d.sleep_min);
    let ss: Vec<&SessionRow> = sessions.iter().filter(|s| s.start >= from && s.start < t).collect();
    let strength: Vec<&&SessionRow> = ss.iter().filter(|s| s.kind == "musculation").collect();
    let weeks = n_days as f64 / 7.0;
    let weeks_with_strength = (0..n_days / 7).filter(|w| strength.iter().any(|s| s.start >= from + w * 7 * 86_400 && s.start < from + (w + 1) * 7 * 86_400)).count();
    let mut by_type: BTreeMap<String, i64> = BTreeMap::new();
    for s in &ss {
        *by_type.entry(s.kind.clone()).or_default() += 1;
    }
    let enough = covered as f64 >= 0.5 * n_days as f64;
    json!({
        "days": n_days, "days_with_data": covered, "sufficient": enough,
        "sleep_h_mean": mean(&sleep).map(|v| round(v / 60.0, 2)),
        "sleep_h_sd": std_sample(&sleep).map(|v| round(v / 60.0, 2)),
        "rmssd_ms_mean": mean(&col(&|d| d.rmssd)).map(|v| round(v, 1)),
        "rhr_bpm_mean": mean(&col(&|d| d.rhr)).map(|v| round(v, 1)),
        "strain_pct_mean": mean(&col(&|d| d.strain_pct)).map(|v| round(v, 0)),
        "total_kcal_mean": mean(&col(&|d| if d.coverage_h.unwrap_or(0.0) >= 20.0 { d.total_kcal } else { None })).map(|v| v.round()),
        "active_kcal_mean": mean(&col(&|d| if d.coverage_h.unwrap_or(0.0) >= 20.0 { d.active_kcal } else { None })).map(|v| v.round()),
        "sessions": ss.len(), "sessions_by_type": by_type,
        "training_min": (ss.iter().map(|s| (s.end - s.start) as f64 / 60.0).sum::<f64>() + 0.0).round(),
        "strength_sessions": strength.len(),
        "strength_sessions_per_week": round(strength.len() as f64 / weeks, 2),
        "strength_sets": strength.iter().map(|s| s.sets).sum::<i64>(),
        "strength_reps": strength.iter().map(|s| s.reps).sum::<i64>(),
        "strength_consistency": if n_days >= 7 { Some(round(weeks_with_strength as f64 / (n_days / 7) as f64, 2)) } else { None },
    })
}

/// Spearman rank correlation (exploratory: shown only with >= 5 pairs, never as causation)
fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() < 5 || x.len() != y.len() {
        return None;
    }
    let rank = |v: &[f64]| -> Vec<f64> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|a, b| v[*a].total_cmp(&v[*b]));
        let mut r = vec![0.0; v.len()];
        let mut i = 0;
        while i < idx.len() {
            let mut j = i;
            while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
                j += 1;
            }
            for k in i..=j {
                r[idx[k]] = (i + j) as f64 / 2.0;
            }
            i = j + 1;
        }
        r
    };
    let (rx, ry) = (rank(x), rank(y));
    let n = rx.len() as f64;
    let (mx, my) = (rx.iter().sum::<f64>() / n, ry.iter().sum::<f64>() / n);
    let cov: f64 = rx.iter().zip(&ry).map(|(a, b)| (a - mx) * (b - my)).sum();
    let sx = rx.iter().map(|a| (a - mx).powi(2)).sum::<f64>().sqrt();
    let sy = ry.iter().map(|b| (b - my).powi(2)).sum::<f64>().sqrt();
    (sx > 0.0 && sy > 0.0).then(|| round(cov / (sx * sy), 2))
}

fn adaptation(ms: &[&Measurement], days: &[StrapDay], sessions: &[SessionRow], now: i64) -> Value {
    // muscle change over the last 4 / 8 / 12 weeks: a fit on the measurements inside the window,
    // only when they cover at least half of it
    let horizon = |weeks: i64| -> Value {
        let from = now - weeks * 7 * 86_400;
        let inside: Vec<&Measurement> = ms.iter().copied().filter(|m| m.ts >= from).collect();
        let span = match (inside.first(), inside.last()) {
            (Some(a), Some(b)) => (b.ts - a.ts) as f64 / 86_400.0,
            _ => 0.0,
        };
        if inside.len() < 2 || span < weeks as f64 * 7.0 * 0.5 {
            return json!({"weeks": weeks, "available": false, "measurements": inside.len()});
        }
        json!({"weeks": weeks, "available": true, "smm": trend(&inside, "smm_kg"), "fat": trend(&inside, "fat_mass_kg"), "ffm": trend(&inside, "ffm_kg")})
    };
    // resistance volume: last 4 weeks vs the 4 before
    let vol = |a: i64, b: i64| -> (i64, i64) {
        let s: Vec<&SessionRow> = sessions.iter().filter(|s| s.kind == "musculation" && s.start >= now - a * 86_400 && s.start < now - b * 86_400).collect();
        (s.len() as i64, s.iter().map(|x| x.sets).sum())
    };
    let (n_recent, sets_recent) = vol(28, 0);
    let (n_prev, sets_prev) = vol(56, 28);
    // intervals between consecutive measurements: change per week vs what happened in between
    let mut intervals = Vec::new();
    for p in ms.windows(2) {
        let (a, b) = (p[0], p[1]);
        let n_days = ((b.ts - a.ts) / 86_400).max(1);
        let ctx = window(days, sessions, b.ts, n_days);
        let wk = n_days as f64 / 7.0;
        intervals.push(json!({
            "from": a.ts, "to": b.ts, "days": n_days,
            "smm_per_week": value_of(b, "smm_kg").zip(value_of(a, "smm_kg")).map(|(y, x)| round((y - x) / wk, 3)),
            "fat_per_week": value_of(b, "fat_mass_kg").zip(value_of(a, "fat_mass_kg")).map(|(y, x)| round((y - x) / wk, 3)),
            "context": ctx,
        }));
    }
    let pick = |k: &str, path: &[&str]| -> (Vec<f64>, Vec<f64>) {
        let mut x = Vec::new();
        let mut y = Vec::new();
        for i in &intervals {
            let mut c = &i["context"];
            for p in path {
                c = &c[*p];
            }
            if let (Some(a), Some(b)) = (c.as_f64(), i[k].as_f64()) {
                x.push(a);
                y.push(b);
            }
        }
        (x, y)
    };
    let mut assoc = Map::new();
    for (name, path) in [("strength_sets", vec!["strength_sets"]), ("sleep_h_mean", vec!["sleep_h_mean"]), ("rmssd_ms_mean", vec!["rmssd_ms_mean"]), ("rhr_bpm_mean", vec!["rhr_bpm_mean"]), ("strain_pct_mean", vec!["strain_pct_mean"])] {
        for target in ["smm_per_week", "fat_per_week"] {
            let (x, y) = pick(target, &path);
            assoc.insert(format!("{name}~{target}"), json!({"pairs": x.len(), "spearman": spearman(&x, &y)}));
        }
    }
    json!({
        "horizons": [horizon(4), horizon(8), horizon(12)],
        "training_consistency_28d": window(days, sessions, now, 28)["strength_consistency"].clone(),
        "resistance_volume": {"sessions_last_4w": n_recent, "sets_last_4w": sets_recent, "sessions_prev_4w": n_prev, "sets_prev_4w": sets_prev},
        "intervals": intervals,
        "associations": assoc,
        "associations_note": "Spearman correlations across measurement intervals, shown from 5 intervals: exploratory, observational, not causal.",
        "composite_score": Value::Null,
        "composite_score_note": "No composite score: with sparse BIA measurements its error would exceed the changes it tries to rank.",
    })
}

fn metabolism(latest: Option<&Measurement>, person: &Person, days: &[StrapDay], now: i64, utc_offset_s: i64) -> Value {
    let _ = utc_offset_s;
    let w7 = window(days, &[], now, 7);
    let w28 = window(days, &[], now, 28);
    let Some(m) = latest else {
        return json!({"available": false, "tdee_7d_kcal": w7["total_kcal_mean"], "tdee_28d_kcal": w28["total_kcal_mean"]});
    };
    let d = derived(m, person);
    let ree = d["bmr"]["katch_mcardle_kcal"].as_f64();
    let tdee = w28["total_kcal_mean"].as_f64().or(w7["total_kcal_mean"].as_f64());
    let weight = m.v("weight_kg");
    let ffm = m.ffm();
    json!({
        "available": true,
        "ree_kcal": ree, "ree_model": "katch_mcardle_bia",
        "bmr": d["bmr"].clone(),
        "tdee_7d_kcal": w7["total_kcal_mean"], "tdee_28d_kcal": w28["total_kcal_mean"],
        "active_7d_kcal": w7["active_kcal_mean"], "active_28d_kcal": w28["active_kcal_mean"],
        "tdee_per_kg": tdee.zip(weight).map(|(t, w)| round(t / w, 1)),
        "tdee_per_kg_ffm": tdee.zip(ffm).map(|(t, f)| round(t / f, 1)),
        "pal": tdee.zip(ree).map(|(t, r)| round(t / r, 2)),
        "note": "Daily energy: resting from the latest prior measurement (Katch-McArdle on measured fat-free mass, else Mifflin-St Jeor), active from heart rate (Keytel 2005) minus the resting share of the same minutes.",
    })
}

#[derive(Debug, Deserialize)]
pub struct SummaryArgs {
    pub database_path: String,
    #[serde(default)]
    pub person: Person,
    #[serde(default)]
    pub utc_offset_seconds: i64,
    #[serde(default)]
    pub now_unix: Option<i64>,
}

pub fn summary(args: &SummaryArgs) -> GooseResult<Value> {
    let conn = Connection::open(&args.database_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let now = args.now_unix.unwrap_or_else(now_s);
    let all = load(&conn)?;
    let ms = usable(&all);
    let days = load_days(&conn, args.utc_offset_seconds)?;
    let sessions = load_sessions(&conn)?;
    let list: Vec<Value> = all
        .iter()
        .map(|m| {
            json!({"id": m.id, "ts": m.ts, "local_time": m.local_time, "source": m.source, "device": m.device, "excluded": m.excluded, "note": m.note,
                   "revision": m.revision, "values": m.values, "derived": derived(m, &args.person),
                   "context": {"d7": window(&days, &sessions, m.ts, 7), "d28": window(&days, &sessions, m.ts, 28), "d56": window(&days, &sessions, m.ts, 56), "d84": window(&days, &sessions, m.ts, 84)}})
        })
        .collect();
    let latest = ms.last().copied();
    let previous = latest.and_then(|l| ms.iter().rev().skip(1).find(|p| comparability(p, l)["comparable"] == true).copied()).or_else(|| ms.iter().rev().nth(1).copied());
    let change = match (previous, latest) {
        (Some(a), Some(b)) => json!({
            "from_id": a.id, "to_id": b.id, "days": round((b.ts - a.ts) as f64 / 86_400.0, 0), "comparability": comparability(a, b),
            "weight_kg": delta(a, b, "weight_kg"), "fat_mass_kg": delta(a, b, "fat_mass_kg"), "smm_kg": delta(a, b, "smm_kg"),
            "ffm_kg": delta(a, b, "ffm_kg"), "pbf_pct": delta(a, b, "pbf_pct"), "tbw_l": delta(a, b, "tbw_l"),
            "note": "A single pair of BIA measurements is not read as tissue change unless it exceeds the minimal detectable change (MDC95).",
        }),
        _ => Value::Null,
    };
    // cardiometabolic trends: observed points only (body composition) + monthly strap means
    let series = |k: &str| -> Vec<Value> { ms.iter().filter_map(|m| value_of(m, k).map(|v| json!([m.ts, round(v, 2)]))).collect() };
    let mut monthly: BTreeMap<String, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for d in &days {
        if let Some((key, _)) = inbody_time_from_unix(d.day_start + args.utc_offset_seconds) {
            let e = monthly.entry(key).or_default();
            if let Some(v) = d.rhr { e.0.push(v) }
            if let Some(v) = d.rmssd { e.1.push(v) }
        }
    }
    Ok(json!({
        "algorithm_version": ALGORITHM_VERSION, "parser_version": PARSER_VERSION,
        "count": all.len(), "usable": ms.len(),
        "latest": list.iter().rev().find(|m| m["excluded"] == false).cloned(),
        "change": change,
        "recomposition": recomposition(&ms),
        "segmental": segmental(&ms),
        "metabolism": metabolism(latest, &args.person, &days, now, args.utc_offset_seconds),
        "adaptation": adaptation(&ms, &days, &sessions, now),
        "series": {
            "weight_kg": series("weight_kg"), "fat_mass_kg": series("fat_mass_kg"), "smm_kg": series("smm_kg"), "ffm_kg": series("ffm_kg"),
            "pbf_pct": series("pbf_pct"), "whr": series("whr"), "vfa_cm2": series("vfa_cm2"), "tbw_l": series("tbw_l"), "icw_l": series("icw_l"),
            "ecw_l": series("ecw_l"), "ecw_ratio": series("ecw_ratio"),
            "strap_monthly": monthly.into_iter().map(|(k, (r, h))| json!({"month": k, "rhr_bpm": mean(&r).map(|v| round(v, 1)), "rmssd_ms": mean(&h).map(|v| round(v, 1))})).collect::<Vec<_>>(),
        },
        "measurements": list,
        "mdc95": SEM.iter().map(|(k, _)| (k.to_string(), json!(round(mdc95(k), 2)))).collect::<Map<_, _>>(),
        "catalog": catalog(),
    }))
}

/// "YYYY-MM" of a local unix time
fn inbody_time_from_unix(local: i64) -> Option<(String, ())> {
    let days = local.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some((format!("{y:04}-{m:02}"), ()))
}

/// Anchor for the daily energy model: the latest usable measurement taken before `before` (end
/// of the day being analysed) and at most 120 days old. Only past measurements: a day analysed
/// before the first measurement keeps the profile-based estimate.
#[derive(Debug, Clone)]
pub struct EnergyAnchor {
    pub id: String,
    pub ts: i64,
    pub weight_kg: f64,
    pub ffm_kg: f64,
}

pub fn energy_anchor(conn: &Connection, before: i64) -> GooseResult<Option<EnergyAnchor>> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='body_comp_measurements'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !exists {
        return Ok(None);
    }
    let mut st = conn.prepare_cached("SELECT id, ts, local_time, source, device, values_json, excluded, note, revision FROM body_comp_measurements WHERE excluded = 0 AND ts < ?1 AND ts >= ?2 ORDER BY ts DESC LIMIT 5")?;
    let rows = st.query_map(params![before, before - 120 * 86_400], |r| {
        let vals: String = r.get(5)?;
        Ok(Measurement {
            id: r.get(0)?, ts: r.get(1)?, local_time: r.get(2)?, source: r.get(3)?, device: r.get(4)?,
            values: serde_json::from_str::<Value>(&vals).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default(),
            excluded: false, note: r.get(7)?, revision: r.get(8)?,
        })
    })?;
    for m in rows.flatten() {
        if let (Some(w), Some(f)) = (m.v("weight_kg"), m.ffm()) {
            if (25.0..300.0).contains(&w) && f > 15.0 && f < w {
                return Ok(Some(EnergyAnchor { id: m.id.clone(), ts: m.ts, weight_kg: w, ffm_kg: f }));
            }
        }
    }
    Ok(None)
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/InBody-20261115.csv");

    fn db() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.sqlite").to_string_lossy().to_string();
        (dir, p)
    }

    #[test]
    fn parses_every_column_of_the_inbody_export() {
        let (rows, unknown) = parse_inbody_csv(FIXTURE, 3600).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(unknown.is_empty(), "{unknown:?}");
        let r = &rows[0];
        assert_eq!(r.raw.len(), 45);
        assert_eq!(r.local_time, "2026-11-15T12:30:39");
        assert_eq!(r.ts, 1_794_742_239); // 11:30:39 UTC
        assert_eq!(r.device.as_deref(), Some("970S"));
        assert_eq!(r.id, "inbody:970S:2026-11-15T12:30:39");
        let v = |k: &str| r.values.get(k).cloned().unwrap();
        assert_eq!(v("weight_kg"), json!(81.3));
        assert_eq!(v("smm_kg"), json!(40.4));
        assert_eq!(v("fat_mass_kg"), json!(10.7));
        assert_eq!(v("pbf_pct"), json!(13.2));
        assert_eq!(v("bmr_device_kcal"), json!(1894.0));
        assert_eq!(v("ecw_ratio"), json!(0.37));
        assert_eq!(v("phase_angle_deg"), json!(6.7));
        assert_eq!(v("lean_left_leg_kg"), json!(10.05));
        // "-" is missing, never zero
        assert_eq!(v("slm_kg"), Value::Null);
        assert_eq!(v("waist_cm"), Value::Null);
        assert_eq!(v("leg_muscle_level"), Value::Null);
        // 43 mapped columns + date + device
        assert_eq!(r.values.len(), 43);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    #[test]
    fn derived_indices_and_bmr_equations() {
        let (rows, _) = parse_inbody_csv(FIXTURE, 3600).unwrap();
        let m = Measurement { id: rows[0].id.clone(), ts: rows[0].ts, local_time: Some(rows[0].local_time.clone()), source: "inbody_csv".into(), device: rows[0].device.clone(), values: rows[0].values.clone(), excluded: false, note: None, revision: 1 };
        let d = derived(&m, &Person { height_cm: None, age: Some(30.0), sex: Some("male".into()) });
        assert_eq!(d["ffm_kg"], 70.6);
        assert_eq!(d["height_source"], "from_device_bmi");
        let h = d["height_m"].as_f64().unwrap();
        assert!((h - 1.829).abs() < 0.002, "{h}");
        assert!((d["ffmi"].as_f64().unwrap() - 21.1).abs() < 0.1);
        assert!((d["fmi"].as_f64().unwrap() - 3.2).abs() < 0.1);
        assert_eq!(d["smm_to_fat"], 3.78);
        // the device BMR is Katch-McArdle on its own fat-free mass: 370 + 21.6 x 70.6 = 1895
        assert_eq!(d["bmr"]["katch_mcardle_kcal"], 1895.0);
        assert_eq!(d["bmr"]["device_is_katch_mcardle"], true);
        assert_eq!(d["arms"]["larger"], "left");
        assert!((d["legs"]["pct"].as_f64().unwrap() - 0.7).abs() < 0.05);
        assert_eq!(d["ecw_tbw"], 0.37);
        assert!(d["quality"].as_array().unwrap().is_empty(), "{}", d["quality"]);
        // with a profile height, the profile wins
        let d2 = derived(&m, &Person { height_cm: Some(180.0), age: None, sex: None });
        assert_eq!(d2["height_source"], "profile");
        assert_eq!(d2["bmr"]["mifflin_st_jeor_kcal"], Value::Null);
    }

    #[test]
    fn import_is_idempotent_and_revisions_corrections() {
        let (_d, path) = db();
        let args = |csv: &str| ImportCsvArgs { database_path: path.clone(), csv: csv.into(), file_name: Some("InBody-20261115.csv".into()), utc_offset_seconds: 3600, now_unix: Some(1) };
        let a = import_csv(&args(FIXTURE)).unwrap();
        assert_eq!((a["added"].as_i64(), a["unchanged"].as_i64()), (Some(1), Some(0)));
        let b = import_csv(&args(FIXTURE)).unwrap();
        assert_eq!((b["added"].as_i64(), b["unchanged"].as_i64()), (Some(0), Some(1)));
        let corrected = FIXTURE.replacen(",81.3,", ",81.4,", 1);
        let c = import_csv(&args(&corrected)).unwrap();
        assert_eq!(c["updated"], 1);
        let conn = Connection::open(&path).unwrap();
        let (n, rev): (i64, i64) = conn.query_row("SELECT COUNT(*), MAX(revision) FROM body_comp_measurements", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((n, rev), (1, 2));
        // a HealthKit copy of the same weighing (written by the InBody app) is not a new measurement
        let hk = import_healthkit(&ImportHealthKitArgs { database_path: path.clone(), now_unix: Some(2), samples: vec![
            HealthKitSample { uuid: "A".into(), ts: 1_794_742_300, kind: "weight_kg".into(), value: 81.3, source_name: Some("InBody".into()), source_bundle: None },
            HealthKitSample { uuid: "B".into(), ts: 1_794_742_300, kind: "pbf_pct".into(), value: 0.132, source_name: Some("InBody".into()), source_bundle: None },
            HealthKitSample { uuid: "C".into(), ts: 1_794_832_000, kind: "weight_kg".into(), value: 81.0, source_name: Some("Balance".into()), source_bundle: None },
        ]}).unwrap();
        assert_eq!(hk["skipped_same_weighing_as_csv"], 1);
        assert_eq!(hk["added"], 1);
    }

    fn synth(day: i64, smm: f64, fat: f64, tbw: f64) -> Measurement {
        let mut v = Map::new();
        let w = 81.0 + (smm - 40.0) * 1.4 + (fat - 10.7);
        v.insert("weight_kg".into(), json!(w));
        v.insert("smm_kg".into(), json!(smm));
        v.insert("fat_mass_kg".into(), json!(fat));
        v.insert("tbw_l".into(), json!(tbw));
        v.insert("ecw_ratio".into(), json!(0.37));
        Measurement { id: format!("m{day}"), ts: 1_790_000_000 + day * 86_400, local_time: Some("2026-01-01T08:00:00".into()), source: "inbody_csv".into(), device: Some("970S".into()), values: v, excluded: false, note: None, revision: 1 }
    }

    #[test]
    fn recomposition_needs_several_measurements_and_beats_the_error() {
        let one = [synth(0, 40.4, 10.7, 51.6)];
        assert_eq!(recomposition(&one.iter().collect::<Vec<_>>())["class"], "insufficient_data");
        // two close measurements with a small change: still insufficient (and below MDC95)
        let two = [synth(0, 40.4, 10.7, 51.6), synth(14, 40.7, 10.4, 51.8)];
        assert_eq!(recomposition(&two.iter().collect::<Vec<_>>())["class"], "insufficient_data");
        assert_eq!(delta(&two[0], &two[1], "smm_kg")["beyond_error"], false);
        // 12 weeks, muscle +1.6 kg, fat -2.4 kg, steady: real recomposition
        let gain: Vec<Measurement> = (0..7).map(|i| synth(i * 14, 40.4 + 0.27 * i as f64, 10.7 - 0.4 * i as f64, 51.6 + 0.2 * i as f64)).collect();
        assert_eq!(recomposition(&gain.iter().collect::<Vec<_>>())["class"], "muscle_gain_fat_loss");
        // noise around a constant: indeterminate
        let noise: Vec<Measurement> = (0..6).map(|i| synth(i * 10, 40.4 + if i % 2 == 0 { 0.3 } else { -0.3 }, 10.7 + if i % 3 == 0 { 0.5 } else { -0.4 }, 51.6)).collect();
        assert_eq!(recomposition(&noise.iter().collect::<Vec<_>>())["class"], "indeterminate");
        // muscle down
        let loss: Vec<Measurement> = (0..5).map(|i| synth(i * 14, 40.4 - 0.4 * i as f64, 10.7, 51.6 - 0.5 * i as f64)).collect();
        let r = recomposition(&loss.iter().collect::<Vec<_>>());
        assert_eq!(r["class"], "possible_muscle_loss");
    }

    #[test]
    fn asymmetry_is_persistent_only_when_repeated() {
        let mk = |i: i64, r: f64, l: f64| {
            let mut m = synth(i * 20, 40.0, 10.0, 51.0);
            m.values.insert("lean_right_arm_kg".into(), json!(r));
            m.values.insert("lean_left_arm_kg".into(), json!(l));
            m
        };
        let one = [mk(0, 4.6, 4.2)];
        assert_eq!(segmental(&one.iter().collect::<Vec<_>>())["arms"]["persistent_asymmetry"], false);
        let three = [mk(0, 4.6, 4.2), mk(1, 4.7, 4.3), mk(2, 4.6, 4.25)];
        assert_eq!(segmental(&three.iter().collect::<Vec<_>>())["arms"]["persistent_asymmetry"], true);
    }

    #[test]
    fn context_windows_use_only_the_past() {
        let days: Vec<StrapDay> = (0..40).map(|i| StrapDay { day_start: 1_790_000_000 + i * 86_400, sleep_min: Some(420.0 + i as f64), rmssd: Some(50.0), rhr: Some(50.0), strain_pct: Some(30.0), total_kcal: Some(2800.0), active_kcal: Some(800.0), coverage_h: Some(23.0) }).collect();
        let t = 1_790_000_000 + 28 * 86_400;
        let w = window(&days, &[], t, 28);
        assert_eq!(w["days_with_data"], 28);
        // days 0..27: mean sleep 420 + 13.5 min, nothing from day 28 on
        assert_eq!(w["sleep_h_mean"], round((420.0 + 13.5) / 60.0, 2));
    }

    #[test]
    fn energy_anchor_is_never_from_the_future() {
        let (_d, path) = db();
        import_csv(&ImportCsvArgs { database_path: path.clone(), csv: FIXTURE.into(), file_name: None, utc_offset_seconds: 3600, now_unix: Some(1) }).unwrap();
        let conn = Connection::open(&path).unwrap();
        assert!(energy_anchor(&conn, 1_794_742_239).unwrap().is_none(), "the measurement itself is not before itself");
        let a = energy_anchor(&conn, 1_794_742_239 + 3600).unwrap().unwrap();
        assert!((a.ffm_kg - 70.6).abs() < 1e-9);
        assert!(energy_anchor(&conn, 1_794_742_239 + 121 * 86_400).unwrap().is_none(), "older than 120 days");
    }

    #[test]
    fn spearman_and_month_keys() {
        assert_eq!(spearman(&[1.0, 2.0, 3.0, 4.0, 5.0], &[2.0, 4.0, 6.0, 8.0, 10.0]), Some(1.0));
        assert_eq!(spearman(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), None);
        assert_eq!(inbody_time_from_unix(1_794_742_239 + 3600).unwrap().0, "2026-11");
    }

    /// GOOSE_FLA = forme_last_analysis.json pulled from the phone: real strap days + the fixture
    #[test]
    #[ignore]
    fn real_summary() {
        let Ok(fla) = std::env::var("GOOSE_FLA") else { return };
        let (_d, path) = db();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE whoop_lab_days (date_key TEXT PRIMARY KEY, input_count INTEGER NOT NULL, input_last_ts INTEGER NOT NULL, algorithm_version TEXT NOT NULL, result_json TEXT NOT NULL);").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(fla).unwrap()).unwrap();
        for d in v["days"].as_array().unwrap() {
            conn.execute("INSERT INTO whoop_lab_days VALUES (?1, 0, 0, 'x', ?2)", params![d["date"].as_str().unwrap(), d.to_string()]).unwrap();
        }
        import_csv(&ImportCsvArgs { database_path: path.clone(), csv: FIXTURE.into(), file_name: None, utc_offset_seconds: 3600, now_unix: Some(1) }).unwrap();
        let s = summary(&SummaryArgs { database_path: path, person: Person { height_cm: None, age: Some(30.0), sex: Some("male".into()) }, utc_offset_seconds: 3600, now_unix: Some(1_794_760_000) }).unwrap();
        println!("derived {}", s["latest"]["derived"]);
        println!("context d7 {}", s["latest"]["context"]["d7"]);
        println!("metabolism {}", s["metabolism"]);
        println!("recomposition {}", s["recomposition"]);
    }
}
