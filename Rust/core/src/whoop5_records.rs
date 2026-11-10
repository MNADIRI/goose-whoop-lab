//! WHOOP 5.0 record decoders (Layer B, decoded) and the field catalog.
//!
//! Input is the frame *payload* as stored in `decoded_frames.payload_hex`: the bytes after the
//! 8-byte header (0xaa 0x01, len u16, 0x00 0x01, CRC16-Modbus) and before the CRC32 trailer.
//! payload[0] = packet type, payload[1] = record layout (R18 = 18 ...). All multi-byte values are
//! little-endian. Catalog offsets are given as FRAME offsets (= payload offset + 8) because that
//! is how they appear in captures and in `lab/whoop5/records.py`.
//!
//! Nothing here interprets a byte whose meaning is not established: such bytes are kept raw and
//! flagged `DECODED_UNVALIDATED` (layout known, physiology unknown) or exposed in `unmapped_hex`.

use serde::Serialize;
use serde_json::{json, Value};

pub const DECODER_VERSION: &str = "whoop5-records.v3"; // v2: signed R26 base; v3: console text offset, battery %
pub const HEADER_LEN: usize = 8;

pub const PACKET_REALTIME: u8 = 40;
/// Realtime raw stream; with TOGGLE_IMU_MODE (106) it carries the R21 IMU layout at 1 record/s.
pub const PACKET_REALTIME_RAW: u8 = 43;
pub const PACKET_HISTORY: u8 = 47;
pub const PACKET_EVENT: u8 = 48;
pub const PACKET_METADATA: u8 = 49;
pub const PACKET_CONSOLE: u8 = 50;

pub const LAYOUT_R18: u8 = 18;
pub const LAYOUT_R20: u8 = 20;
pub const LAYOUT_R21: u8 = 21;
pub const LAYOUT_R26: u8 = 26;

/// Frame sizes (header + payload + CRC32) of each history layout seen on firmware 50.38.x.
pub const R18_FRAME_LEN: usize = 124;
pub const R20_FRAME_LEN: usize = 2140;
pub const R21_FRAME_LEN: usize = 1244;
pub const R26_FRAME_LEN: usize = 88;

pub const MIN_PLAUSIBLE_TS: i64 = 1_577_836_800; // 2020-01-01

/// Tentative IMU conversions (verified by NOOP on the WHOOP 4.0 raw stream, NOT yet on 5.0 R21).
pub const ACCEL_LSB_PER_G: f64 = 4096.0;
pub const GYRO_DPS_PER_LSB: f64 = 2000.0 / 32768.0;

/// Confidence attached to every decoded or derived value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    /// Checked against an independent reference (other device, physics, published value).
    Validated,
    /// Byte layout is known and stable, meaning or scale not independently confirmed.
    DecodedUnvalidated,
    /// Algorithmic output whose method is reasonable but not validated on this device.
    Experimental,
    /// Estimated from other signals (e.g. respiration from R-R).
    Inferred,
    /// Came from an external export (WHOOP / Apple Health), never mixed with local values.
    Imported,
    /// No usable input.
    Unavailable,
    /// Input failed a validity check.
    Invalid,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Validated => "VALIDATED",
            Status::DecodedUnvalidated => "DECODED_UNVALIDATED",
            Status::Experimental => "EXPERIMENTAL",
            Status::Inferred => "INFERRED",
            Status::Imported => "IMPORTED",
            Status::Unavailable => "UNAVAILABLE",
            Status::Invalid => "INVALID",
        }
    }
}

// ------------------------------------------------------------------ byte readers

fn u16_at(p: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(p.get(o..o + 2)?.try_into().ok()?))
}
fn i16_at(p: &[u8], o: usize) -> Option<i16> {
    Some(i16::from_le_bytes(p.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(p: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?))
}
fn i32_at(p: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?))
}
fn f32_at(p: &[u8], o: usize) -> Option<f32> {
    let v = f32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?);
    v.is_finite().then_some(v)
}

/// R-R words are 1/1024 s ticks; rounded to the nearest millisecond.
pub fn rr_ticks_to_ms(ticks: u16) -> u32 {
    (u32::from(ticks) * 1000 + 512) / 1024
}

// ------------------------------------------------------------------ field catalog

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FieldSpec {
    pub record: &'static str,
    pub name: &'static str,
    /// Offset in the full frame (payload offset + 8).
    pub frame_offset: usize,
    pub len: usize,
    /// u8/u16/i16/u32/i32/f32 (all little-endian), or "bits" for a bit field.
    pub dtype: &'static str,
    pub scale: &'static str,
    pub unit: &'static str,
    pub validity: &'static str,
    pub status: Status,
    pub note: &'static str,
}

macro_rules! f {
    ($r:expr, $n:expr, $o:expr, $l:expr, $t:expr, $s:expr, $u:expr, $v:expr, $st:ident, $note:expr) => {
        FieldSpec { record: $r, name: $n, frame_offset: $o, len: $l, dtype: $t, scale: $s, unit: $u, validity: $v, status: Status::$st, note: $note }
    };
}

/// Every byte range we know about, per record. Bytes not listed are preserved in `unmapped_hex`.
pub const FIELD_CATALOG: &[FieldSpec] = &[
    // R18: one-second biometric summary (124 B)
    f!("R18", "record_index", 11, 4, "u32", "1", "count", "monotonic per strap", DecodedUnvalidated, "history read pointer; gaps = missing records"),
    f!("R18", "timestamp", 15, 4, "u32", "1", "s (unix)", ">= 2020-01-01", Validated, "matches phone clock to the second during live sync"),
    f!("R18", "subseconds", 19, 2, "u16", "1/32768", "s", "", DecodedUnvalidated, ""),
    f!("R18", "heart_rate", 22, 1, "u8", "1", "bpm", "30-230, 0 = no lock", Validated, "agrees with live packet 40 HR"),
    f!("R18", "rr_count", 23, 1, "u8", "1", "count", "0-4", Validated, ""),
    f!("R18", "rr_intervals", 24, 8, "u16[4]", "1000/1024", "ms", "300-2000 ms physiological", Validated, "1/1024 s ticks; successive beats only within consecutive seconds"),
    f!("R18", "cardiac_flags_raw", 33, 1, "bits", "", "flags", "", DecodedUnvalidated, "bit5 (88 %) = still / HR locked; bits 0, 4, 6 = movement (higher dyn accel, R-R rarely present): quality mask candidate"),
    f!("R18", "byte34 / byte35", 34, 2, "u8+u8", "1", "raw", "", DecodedUnvalidated, "byte34 tracks HR over 5 min (r 0.76); byte35 bits 5-7 = band state x32 (0 wake, 32 still, 64 sleep, 96 up), low bits change every second (unknown)"),
    f!("R18", "hr_quality_raw", 36, 1, "u8", "1", "raw", "", DecodedUnvalidated, "HR confidence candidate: higher with R-R present (r 0.45), ~IBI/9 ms"),
    f!("R18", "alt_heart_rate", 37, 1, "u8", "1", "bpm?", "", DecodedUnvalidated, "secondary HR candidate, tracks HR loosely"),
    f!("R18", "rr_adjacent_raw", 38, 2, "u8+u8", "1", "bpm / ?", "", DecodedUnvalidated, "low byte = copy of HR; high byte = 0 when active, ~48-53 at rest/sleep, tracks HR (r 0.84 in sleep): firmware resting-HR estimate candidate"),
    f!("R18", "byte_40", 40, 1, "u8", "1", "raw", "", DecodedUnvalidated, "inverse of HR over 5 min (r -0.66), correlates with hr_quality: interval-like value"),
    f!("R18", "dynamic_accel", 41, 4, "f32", "1", "g", "0-8 g", DecodedUnvalidated, "firmware motion intensity; ~0 at rest"),
    f!("R18", "gravity_x", 45, 4, "f32", "1", "g", "|g| ~ 1 at rest", Validated, "|(x,y,z)| = 1.00 g at rest"),
    f!("R18", "gravity_y", 49, 4, "f32", "1", "g", "", Validated, ""),
    f!("R18", "gravity_z", 53, 4, "f32", "1", "g", "", Validated, ""),
    f!("R18", "step_counter", 57, 2, "u16", "1", "steps (cumulative)", "wraps at 65536", DecodedUnvalidated, "increments while walking; resets on reboot"),
    f!("R18", "cadence_raw", 59, 2, "u16", "1/256 s?", "step period", "", DecodedUnvalidated, "still 138, walk 128, run 81: run/walk ratio 0.63 matches step periods; cadence ~ 60*256/raw spm (unconfirmed unit)"),
    f!("R18", "step_counter_hw", 61, 2, "u16", "1", "steps (cumulative)", "only when byte 64 = 0x10", DecodedUnvalidated, "software override counter"),
    f!("R18", "activity_class", 63, 1, "u8", "1", "enum", "0 still, 1 walk, 2 run", DecodedUnvalidated, ""),
    f!("R18", "counter_override_flag", 64, 1, "u8", "1", "flag", "0x10 = override", DecodedUnvalidated, ""),
    f!("R18", "bytes_65_68", 65, 4, "raw", "1", "raw", "", DecodedUnvalidated, "always 0 on fw 50.38.1 (reserved)"),
    f!("R18", "aux_temp_1", 69, 2, "i16", "0.1", "degC", "0-60", DecodedUnvalidated, "board/ambient candidate, not skin"),
    f!("R18", "aux_temp_2", 71, 2, "i16", "0.1", "degC", "0-60", DecodedUnvalidated, "board/ambient candidate, not skin"),
    f!("R18", "skin_temp", 73, 2, "u16", "0.01", "degC", "5-45", DecodedUnvalidated, "33-35 degC worn at night, falls off-wrist; skin, NOT core"),
    f!("R18", "status_words", 75, 6, "u16[3]", "1", "raw", "", DecodedUnvalidated, "meaning unknown"),
    f!("R18", "state_bits", 81, 1, "bits", "", "enum", "", DecodedUnvalidated, "b0-1 wrist, b2-3 wake quality, b4-5 band state (wake/still/sleep/up), b6-7 extra"),
    f!("R18", "spo2_or_status (sleep_adjacent_raw)", 82, 1, "u8", "1", "% or code", "70-100 = SpO2 %, else status", DecodedUnvalidated, "on-strap SpO2: 30 s windows every 20 min during sleep (xx:16/36/56); codes 2/8/16/32/128/144/160/168 = no valid reading"),
    f!("R18", "bytes_83_105", 83, 23, "raw", "1", "raw", "", DecodedUnvalidated, "meaning unknown"),
    f!("R18", "optical_baseline_raw", 106, 2, "u8[2]", "1", "raw", "", DecodedUnvalidated, "candidate optical baseline"),
    f!("R18", "optical_amplitude_raw", 108, 2, "u8[2]", "1", "raw", "", DecodedUnvalidated, "candidate optical amplitude"),
    f!("R18", "bytes_110_112", 110, 3, "raw", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R18", "unknown_f32", 113, 4, "f32", "1", "?", "", DecodedUnvalidated, ""),
    // R26: compact optical window (88 B)
    f!("R26", "record_index", 11, 4, "u32", "1", "count", "", DecodedUnvalidated, ""),
    f!("R26", "timestamp", 15, 4, "u32", "1", "s (unix)", ">= 2020-01-01", DecodedUnvalidated, ""),
    f!("R26", "time_word", 19, 2, "u16", "1", "raw", "", DecodedUnvalidated, "sub-second or sequence word"),
    f!("R26", "burst_counter", 21, 2, "u16", "1", "count", "", DecodedUnvalidated, ""),
    f!("R26", "base_value", 23, 4, "i32", "1", "ADC code", "", DecodedUnvalidated, "absolute first sample of the window"),
    f!("R26", "deltas", 27, 48, "i16[24]", "1", "ADC code", "", DecodedUnvalidated, "x[i+1] = x[i] + delta[i]; wavelength unknown"),
    f!("R26", "motion", 75, 4, "f32", "1", "g?", "", DecodedUnvalidated, "motion during the window"),
    f!("R26", "status_raw", 79, 2, "u16", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R26", "on_wrist_raw", 81, 1, "u8", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R26", "acceptance_raw", 82, 1, "u8", "1", "raw", "nonzero = accepted?", DecodedUnvalidated, "signal acceptance flag candidate"),
    // R20: raw optical, 5 blocks x 2 slots x <=50 samples (2140 B)
    f!("R20", "marker", 10, 1, "u8", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R20", "record_index", 11, 4, "u32", "1", "count", "", DecodedUnvalidated, ""),
    f!("R20", "timestamp", 15, 4, "u32", "1", "s (unix)", "", DecodedUnvalidated, "record-level only; per-sample timing unknown"),
    f!("R20", "block[k].count", 26, 1, "u8", "1", "samples", "0-50; blocks at 26, 448, 870, 1292, 1714", DecodedUnvalidated, "samples beyond count are padding"),
    f!("R20", "block[k].source_a / drive_a", 27, 3, "u8+u16", "1", "raw", "", DecodedUnvalidated, "LED source + drive current code, unit unknown"),
    f!("R20", "block[k].source_b / drive_b", 30, 3, "u8+u16", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R20", "block[k].detector_a / range_a / offset_a", 33, 7, "u8+u32+i16", "1", "raw", "", DecodedUnvalidated, "photodiode routing / TIA range / offset"),
    f!("R20", "block[k].detector_b / range_b / offset_b", 40, 7, "u8+u32+i16", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R20", "block[k].slot_a", 47, 200, "i32[50]", "1", "ADC code", "", DecodedUnvalidated, "unidentified optical channel (no wavelength label); 25 samples/s observed (1 record = 1 s)"),
    f!("R20", "observed roles (10 Nov, fw 50.38.1)", 26, 0, "note", "", "", "", Experimental, "block 0: source 1/4, AC/DC ~0.7 %, clean pulse (green-like); block 3: drive 0 = ambient/dark; block 4: source 2 drive 200, AC/DC 0.04-0.16 %, pulse (red- or IR-like); blocks 1-2 configured (source 3 drive 12750) but empty"),
    f!("R20", "block[k].slot_b", 247, 200, "i32[50]", "1", "ADC code", "", DecodedUnvalidated, "unidentified optical channel"),
    // R21: six-axis IMU (1244 B)
    f!("R21", "marker", 10, 1, "u8", "1", "raw", "", DecodedUnvalidated, ""),
    f!("R21", "record_index", 11, 4, "u32", "1", "count", "", DecodedUnvalidated, ""),
    f!("R21", "timestamp", 15, 6, "u32+u16", "1 + 1/32768", "s (unix)", "", DecodedUnvalidated, ""),
    f!("R21", "accel_count", 24, 2, "u16", "1", "samples", "0-100", DecodedUnvalidated, ""),
    f!("R21", "accel_x/y/z", 28, 600, "i16[100] x3", "1/4096 (tentative)", "g", "", DecodedUnvalidated, "planar blocks at 28, 228, 428; scale verified on 4.0 only"),
    f!("R21", "gyro_count", 630, 2, "u16", "1", "samples", "0-100", DecodedUnvalidated, ""),
    f!("R21", "gyro_x/y/z", 640, 600, "i16[100] x3", "2000/32768 (tentative)", "deg/s", "", DecodedUnvalidated, "planar blocks at 640, 840, 1040"),
    // packet 40: live HR
    f!("LIVE40", "timestamp", 10, 4, "u32", "1", "s (unix)", "", Validated, ""),
    f!("LIVE40", "time_word", 14, 2, "u16", "1", "raw", "", DecodedUnvalidated, ""),
    f!("LIVE40", "heart_rate", 16, 1, "u8", "1", "bpm", "", Validated, ""),
    f!("LIVE40", "rr_count", 17, 1, "u8", "1", "count", "", Validated, ""),
    f!("LIVE40", "rr_intervals", 18, 8, "u16[n]", "1000/1024", "ms", "", Validated, ""),
    // packet 48: events
    f!("EVENT48", "event_id", 10, 2, "u16", "1", "enum", "", DecodedUnvalidated, "names from goose/NOOP tables"),
    f!("EVENT48", "timestamp", 12, 4, "u32", "1", "s (unix)", "", DecodedUnvalidated, ""),
    f!("EVENT48", "subseconds", 16, 2, "u16", "1", "raw", "", DecodedUnvalidated, ""),
    f!("EVENT48", "data", 20, 0, "raw", "1", "raw", "", DecodedUnvalidated, "BATTERY_LEVEL: status u8, word u32, voltage candidate u32 (mV)"),
    // packet 49: history metadata
    f!("META49", "kind", 10, 1, "u8", "1", "enum", "1 start, 2 end, 3 complete", Validated, ""),
    f!("META49", "body", 11, 0, "raw", "1", "raw", "", DecodedUnvalidated, "contains the trim cursor"),
];

pub fn field_catalog_json() -> Value {
    json!({
        "decoder_version": DECODER_VERSION,
        "endianness": "little",
        "offsets": "frame offsets (payload offset + 8)",
        "statuses": ["VALIDATED", "DECODED_UNVALIDATED", "EXPERIMENTAL", "INFERRED", "IMPORTED", "UNAVAILABLE", "INVALID"],
        "fields": FIELD_CATALOG,
    })
}

// ------------------------------------------------------------------ records

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R18Record {
    pub ts: i64,
    pub subseconds: u16,
    pub record_index: u32,
    pub hr: u8,
    pub rr_count_raw: u8,
    pub rr_ms: Vec<u32>,
    pub cardiac_flags_raw: u8,
    pub hr_quality_raw: u8,
    pub alt_hr: u8,
    pub rr_adjacent_raw: u16,
    pub dyn_accel_g: Option<f32>,
    pub gravity_g: Option<[f32; 3]>,
    pub steps: u16,
    pub cadence_raw: u16,
    pub steps_hw: Option<u16>,
    pub counter_override: bool,
    pub activity_raw: u8,
    pub activity: Option<&'static str>,
    pub aux_temp1_c: Option<f32>,
    pub aux_temp2_c: Option<f32>,
    pub skin_temp_c: Option<f32>,
    pub skin_temp_raw: u16,
    pub status_raw: [u16; 3],
    pub wrist_bits: u8,
    pub wake_quality_bits: u8,
    pub band_state: &'static str,
    pub extra_state_bits: u8,
    pub sleep_adjacent_raw: u8,
    pub optical_baseline_raw: [u8; 2],
    pub optical_amplitude_raw: [u8; 2],
    pub unknown_f32: Option<f32>,
    /// Payload bytes not covered by a named field (frame offsets 34-35, 40, 65-68, 83-105, 110-112).
    pub unmapped_hex: String,
    pub warnings: Vec<String>,
}

const BAND_STATES: [&str; 4] = ["wake", "still", "sleep", "up"];

fn activity_name(raw: u8) -> Option<&'static str> {
    match raw {
        0 => Some("still"),
        1 => Some("walk"),
        2 => Some("run"),
        _ => None,
    }
}

/// payload offset of a frame offset
const fn p(frame_offset: usize) -> usize {
    frame_offset - HEADER_LEN
}

pub fn decode_r18(pl: &[u8]) -> Option<R18Record> {
    if pl.len() != R18_FRAME_LEN - 12 || pl[0] != PACKET_HISTORY || pl[1] != LAYOUT_R18 {
        return None;
    }
    let mut warnings = Vec::new();
    let ts = i64::from(u32_at(pl, p(15))?);
    if ts < MIN_PLAUSIBLE_TS {
        warnings.push("timestamp_before_2020".into());
    }
    let rr_count_raw = pl[p(23)];
    if rr_count_raw > 4 {
        warnings.push(format!("rr_count_{rr_count_raw}_clamped_to_4"));
    }
    let rr_ms: Vec<u32> = (0..usize::from(rr_count_raw.min(4)))
        .filter_map(|i| u16_at(pl, p(24) + 2 * i).map(rr_ticks_to_ms))
        .filter(|v| *v > 0)
        .collect();
    let dyn_accel_g = f32_at(pl, p(41)).filter(|v| {
        let ok = (0.0..=8.0).contains(v);
        if !ok {
            warnings.push("dynamic_accel_out_of_range".into());
        }
        ok
    });
    let gravity_g = match (f32_at(pl, p(45)), f32_at(pl, p(49)), f32_at(pl, p(53))) {
        (Some(x), Some(y), Some(z)) => Some([x, y, z]),
        _ => None,
    };
    let counter_override = pl[p(64)] == 0x10;
    let aux = |o| i16_at(pl, p(o)).map(|v| f32::from(v) / 10.0).filter(|v| (0.0..=60.0).contains(v));
    let skin_temp_raw = u16_at(pl, p(73))?;
    let bits = pl[p(81)];
    let activity_raw = pl[p(63)];
    let mut unmapped = Vec::new();
    for (a, b) in [(34, 36), (40, 41), (65, 69), (83, 106), (110, 113)] {
        unmapped.extend_from_slice(&pl[p(a)..p(b)]);
    }
    Some(R18Record {
        ts,
        subseconds: u16_at(pl, p(19))?,
        record_index: u32_at(pl, p(11))?,
        hr: pl[p(22)],
        rr_count_raw,
        rr_ms,
        cardiac_flags_raw: pl[p(33)],
        hr_quality_raw: pl[p(36)],
        alt_hr: pl[p(37)],
        rr_adjacent_raw: u16_at(pl, p(38))?,
        dyn_accel_g,
        gravity_g,
        steps: u16_at(pl, p(57))?,
        cadence_raw: u16_at(pl, p(59))?,
        steps_hw: counter_override.then(|| u16_at(pl, p(61))).flatten(),
        counter_override,
        activity_raw,
        activity: activity_name(activity_raw),
        aux_temp1_c: aux(69),
        aux_temp2_c: aux(71),
        skin_temp_c: (500..=4500).contains(&skin_temp_raw).then(|| f32::from(skin_temp_raw) / 100.0),
        skin_temp_raw,
        status_raw: [u16_at(pl, p(75))?, u16_at(pl, p(77))?, u16_at(pl, p(79))?],
        wrist_bits: bits & 3,
        wake_quality_bits: (bits >> 2) & 3,
        band_state: BAND_STATES[usize::from((bits >> 4) & 3)],
        extra_state_bits: (bits >> 6) & 3,
        sleep_adjacent_raw: pl[p(82)],
        optical_baseline_raw: [pl[p(106)], pl[p(107)]],
        optical_amplitude_raw: [pl[p(108)], pl[p(109)]],
        unknown_f32: f32_at(pl, p(113)),
        unmapped_hex: hex::encode(unmapped),
        warnings,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R26Record {
    pub ts: i64,
    pub record_index: u32,
    pub time_word: u16,
    pub burst: u16,
    pub base: i32,
    pub deltas: Vec<i16>,
    /// Reconstructed 25-sample sequence (i64: cannot overflow, i32 base + 24 x i16).
    pub samples: Vec<i64>,
    pub motion: Option<f32>,
    pub status_raw: u16,
    pub on_wrist_raw: u8,
    pub acceptance_raw: u8,
}

pub fn decode_r26(pl: &[u8]) -> Option<R26Record> {
    if pl.len() != R26_FRAME_LEN - 12 || pl[0] != PACKET_HISTORY || pl[1] != LAYOUT_R26 {
        return None;
    }
    // signed: a real burst (10 Nov) had 0xffff8731 = -30 929, not 4 294 936 367
    let base = i32_at(pl, p(23))?;
    let deltas: Vec<i16> = (0..24).map(|i| i16_at(pl, p(27) + 2 * i)).collect::<Option<_>>()?;
    let mut samples = Vec::with_capacity(25);
    samples.push(i64::from(base));
    for d in &deltas {
        samples.push(samples[samples.len() - 1] + i64::from(*d));
    }
    Some(R26Record {
        ts: i64::from(u32_at(pl, p(15))?),
        record_index: u32_at(pl, p(11))?,
        time_word: u16_at(pl, p(19))?,
        burst: u16_at(pl, p(21))?,
        base,
        deltas,
        samples,
        motion: f32_at(pl, p(75)),
        status_raw: u16_at(pl, p(79))?,
        on_wrist_raw: pl[p(81)],
        acceptance_raw: pl[p(82)],
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R20Slot {
    /// "a" or "b" within the block.
    pub slot: &'static str,
    pub source_raw: u8,
    pub drive_raw: u16,
    pub detector_raw: u8,
    pub range_raw: u32,
    pub offset_raw: i16,
    /// Only the first `count` positions; the rest of the 50 is padding.
    pub samples: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R20Block {
    pub index: u8,
    pub count: u8,
    pub slots: [R20Slot; 2],
    /// True when bytes past `count` are not all zero (would mean the count/layout is wrong).
    pub padding_nonzero: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R20Record {
    pub ts: i64,
    pub record_index: u32,
    pub marker_raw: u8,
    pub blocks: Vec<R20Block>,
    pub status: Status,
    pub warnings: Vec<String>,
}

pub const R20_BLOCK_FRAME_OFFSETS: [usize; 5] = [26, 448, 870, 1292, 1714];

pub fn decode_r20(pl: &[u8]) -> Option<R20Record> {
    // history (47, TOGGLE_PERSISTENT_R20) or live (43, ENABLE_OPTICAL_DATA + TOGGLE_OPTICAL_MODE): same layout
    if pl.len() != R20_FRAME_LEN - 12 || (pl[0] != PACKET_HISTORY && pl[0] != PACKET_REALTIME_RAW) || pl[1] != LAYOUT_R20 {
        return None;
    }
    let mut warnings = Vec::new();
    let mut blocks = Vec::new();
    for (k, &fo) in R20_BLOCK_FRAME_OFFSETS.iter().enumerate() {
        let b = p(fo);
        let raw_count = pl[b];
        if raw_count > 50 {
            warnings.push(format!("block_{k}_count_{raw_count}_exceeds_50"));
        }
        let count = raw_count.min(50);
        let read = |base: usize| -> Vec<i32> { (0..usize::from(count)).filter_map(|i| i32_at(pl, base + 4 * i)).collect() };
        let padding_nonzero = [b + 21, b + 221]
            .iter()
            .any(|base| pl[base + 4 * usize::from(count)..base + 200].iter().any(|v| *v != 0));
        blocks.push(R20Block {
            index: k as u8,
            count,
            slots: [
                R20Slot { slot: "a", source_raw: pl[b + 1], drive_raw: u16_at(pl, b + 2)?, detector_raw: pl[b + 7], range_raw: u32_at(pl, b + 8)?, offset_raw: i16_at(pl, b + 12)?, samples: read(b + 21) },
                R20Slot { slot: "b", source_raw: pl[b + 4], drive_raw: u16_at(pl, b + 5)?, detector_raw: pl[b + 14], range_raw: u32_at(pl, b + 15)?, offset_raw: i16_at(pl, b + 19)?, samples: read(b + 221) },
            ],
            padding_nonzero,
        });
    }
    if blocks.iter().any(|b| b.padding_nonzero) {
        warnings.push("nonzero_padding_after_count".into());
    }
    Some(R20Record {
        ts: i64::from(u32_at(pl, p(15))?),
        record_index: u32_at(pl, p(11))?,
        marker_raw: pl[p(10)],
        status: if warnings.is_empty() { Status::DecodedUnvalidated } else { Status::Invalid },
        blocks,
        warnings,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct R21Record {
    /// Unix seconds + subseconds/32768.
    pub ts: f64,
    pub record_index: u32,
    pub marker_raw: u8,
    pub accel_count_raw: u16,
    pub gyro_count_raw: u16,
    /// Raw int16 per axis, planar (x[], y[], z[]), only the first `count` samples.
    pub accel_raw: [Vec<i16>; 3],
    pub gyro_raw: [Vec<i16>; 3],
    pub warnings: Vec<String>,
}

impl R21Record {
    pub fn accel_g(&self) -> [Vec<f64>; 3] {
        self.accel_raw.clone().map(|axis| axis.iter().map(|v| f64::from(*v) / ACCEL_LSB_PER_G).collect())
    }
    pub fn gyro_dps(&self) -> [Vec<f64>; 3] {
        self.gyro_raw.clone().map(|axis| axis.iter().map(|v| f64::from(*v) * GYRO_DPS_PER_LSB).collect())
    }
}

pub fn decode_r21(pl: &[u8]) -> Option<R21Record> {
    // history (47) or live (43, TOGGLE_IMU_MODE): same 1244-byte layout, verified on fw 50.38.1
    if pl.len() != R21_FRAME_LEN - 12 || (pl[0] != PACKET_HISTORY && pl[0] != PACKET_REALTIME_RAW) || pl[1] != LAYOUT_R21 {
        return None;
    }
    let mut warnings = Vec::new();
    let accel_count_raw = u16_at(pl, p(24))?;
    let gyro_count_raw = u16_at(pl, p(630))?;
    if accel_count_raw > 100 || gyro_count_raw > 100 {
        warnings.push("imu_count_exceeds_100".into());
    }
    let block = |fo: usize, n: u16| -> Vec<i16> { (0..usize::from(n.min(100))).filter_map(|i| i16_at(pl, p(fo) + 2 * i)).collect() };
    Some(R21Record {
        ts: f64::from(u32_at(pl, p(15))?) + f64::from(u16_at(pl, p(19))?) / 32768.0,
        record_index: u32_at(pl, p(11))?,
        marker_raw: pl[p(10)],
        accel_count_raw,
        gyro_count_raw,
        accel_raw: [block(28, accel_count_raw), block(228, accel_count_raw), block(428, accel_count_raw)],
        gyro_raw: [block(640, gyro_count_raw), block(840, gyro_count_raw), block(1040, gyro_count_raw)],
        warnings,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LiveHr {
    pub ts: i64,
    pub time_word: u16,
    pub hr: u8,
    pub rr_ms: Vec<u32>,
}

pub fn decode_live_hr(pl: &[u8]) -> Option<LiveHr> {
    if pl.first() != Some(&PACKET_REALTIME) || pl.len() < p(18) {
        return None;
    }
    let declared = usize::from(pl[p(17)]);
    let available = (pl.len() - p(18)) / 2;
    Some(LiveHr {
        ts: i64::from(u32_at(pl, p(10))?),
        time_word: u16_at(pl, p(14))?,
        hr: pl[p(16)],
        rr_ms: (0..declared.min(available).min(4)).filter_map(|i| u16_at(pl, p(18) + 2 * i)).map(rr_ticks_to_ms).filter(|v| *v > 0).collect(),
    })
}

pub fn event_name(id: u16) -> String {
    let name = match id {
        0 => "UNDEFINED", 1 => "ERROR", 2 => "CONSOLE_OUTPUT", 3 => "BATTERY_LEVEL", 4 => "SYSTEM_CONTROL",
        5 => "EXTERNAL_5V_ON", 6 => "EXTERNAL_5V_OFF", 7 => "CHARGING_ON", 8 => "CHARGING_OFF",
        9 => "WRIST_ON", 10 => "WRIST_OFF", 11 => "BLE_CONNECTION_UP", 12 => "BLE_CONNECTION_DOWN",
        13 => "RTC_LOST", 14 => "DOUBLE_TAP", 15 => "BOOT", 16 => "SET_RTC", 17 => "TEMPERATURE_LEVEL",
        18 => "PAIRING_MODE", 19 => "SERIAL_HEAD_CONNECTED", 20 => "SERIAL_HEAD_REMOVED",
        21 => "BATTERY_PACK_CONNECTED", 22 => "BATTERY_PACK_REMOVED", 23 => "BLE_BONDED",
        24 => "BLE_HR_PROFILE_ENABLED", 25 => "BLE_HR_PROFILE_DISABLED", 26 => "TRIM_ALL_DATA",
        27 => "TRIM_ALL_DATA_ENDED", 28 => "FLASH_INIT_COMPLETE", 29 => "STRAP_CONDITION_REPORT",
        30 => "BOOT_REPORT", 31 => "EXIT_VIRGIN_MODE", 32 => "CAPTOUCH_AUTOTHRESHOLD_ACTION",
        33 => "BLE_REALTIME_HR_ON", 34 => "BLE_REALTIME_HR_OFF", 35 => "ACCELEROMETER_RESET",
        36 => "AFE_RESET", 37 => "SHIP_MODE_ENABLED", 38 => "SHIP_MODE_DISABLED", 39 => "SHIP_MODE_BOOT",
        40 => "CH1_SATURATION_DETECTED", 41 => "CH2_SATURATION_DETECTED", 42 => "ACCELEROMETER_SATURATION_DETECTED",
        43 => "BLE_SYSTEM_RESET", 44 => "BLE_SYSTEM_ON", 45 => "BLE_SYSTEM_INITIALIZED",
        46 => "RAW_DATA_COLLECTION_ON", 47 => "RAW_DATA_COLLECTION_OFF", 56 => "STRAP_DRIVEN_ALARM_SET",
        57 => "STRAP_DRIVEN_ALARM_EXECUTED", 58 => "APP_DRIVEN_ALARM_EXECUTED", 59 => "STRAP_DRIVEN_ALARM_DISABLED",
        60 => "HAPTICS_FIRED", 63 => "EXTENDED_BATTERY_INFORMATION", 96 => "HIGH_FREQ_SYNC_PROMPT",
        97 => "HIGH_FREQ_SYNC_ENABLED", 98 => "HIGH_FREQ_SYNC_DISABLED", 100 => "HAPTICS_TERMINATED",
        109 => "BATTERY_PACK_INFO", 123 => "GENERIC_FIRMWARE_EVENT",
        _ => return format!("EVENT_{id}"),
    };
    name.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StrapEvent {
    pub ts: i64,
    pub subseconds: u16,
    pub event_id: u16,
    pub name: String,
    pub data_hex: String,
    pub decoded: Value,
}

pub fn decode_event(pl: &[u8]) -> Option<StrapEvent> {
    if pl.first() != Some(&PACKET_EVENT) || pl.len() < p(20) {
        return None;
    }
    let event_id = u16_at(pl, p(10))?;
    let data = &pl[p(20)..];
    let decoded = match event_id {
        // goose observation on 5.0: status byte + two u32 words, the second tracks pack voltage.
        // NOOP's 4.0 layout (soc@17/10, mV@21) does not match 5.0 frames, so it is not applied.
        // BATTERY_LEVEL: u16@1 / 10 = state of charge (max 99.7 % matches the firmware log
        // "SUPERVISOR: SOC report: 99.70"), u16@5 = pack voltage in mV (3442-4421 observed)
        3 if data.len() >= 9 => {
            let soc = u16_at(data, 1).map(|v| f64::from(v) / 10.0).filter(|v| *v <= 100.0);
            let mv = u16_at(data, 5).unwrap_or(0);
            json!({"status_raw": data[0], "battery_pct": soc, "voltage_mv": (3000..=4500).contains(&mv).then_some(mv),
                   "status": Status::DecodedUnvalidated})
        }
        // EXTENDED_BATTERY_INFORMATION: u16@5 / 10 looks like a battery temperature (20-44 degC)
        63 if data.len() >= 7 => json!({"battery_temp_c_candidate": u16_at(data, 5).map(|v| f64::from(v) / 10.0), "status": Status::Experimental}),
        17 if data.len() >= 2 => json!({"level_raw": u16_at(data, 0), "status": Status::DecodedUnvalidated}),
        _ => Value::Null,
    };
    Some(StrapEvent {
        ts: i64::from(u32_at(pl, p(12))?),
        subseconds: u16_at(pl, p(16))?,
        event_id,
        name: event_name(event_id),
        data_hex: hex::encode(data),
        decoded,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HistoryMeta {
    pub kind_raw: u8,
    pub kind: String,
    pub body_hex: String,
}

pub fn decode_metadata(pl: &[u8]) -> Option<HistoryMeta> {
    if pl.first() != Some(&PACKET_METADATA) || pl.len() < p(11) {
        return None;
    }
    let kind_raw = pl[p(10)];
    let kind = match kind_raw {
        1 => "HISTORY_START".into(),
        2 => "HISTORY_END".into(),
        3 => "HISTORY_COMPLETE".into(),
        k => format!("METADATA_{k}"),
    };
    Some(HistoryMeta { kind_raw, kind, body_hex: hex::encode(&pl[p(11)..]) })
}

pub fn decode_console(pl: &[u8]) -> Option<String> {
    if pl.first() != Some(&PACKET_CONSOLE) || pl.len() < p(21) {
        return None;
    }
    // text starts at frame byte 21 (frame 8-20: header, timestamp, counters)
    let body = pl.get(p(21)..).unwrap_or(&[]);
    let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
    Some(String::from_utf8_lossy(&body[..end]).chars().take(2048).collect())
}

/// Every record family, for callers that dispatch on the payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "family")]
pub enum Record {
    R18(R18Record),
    R20(R20Record),
    R21(R21Record),
    R26(R26Record),
    LiveHr(LiveHr),
    Event(StrapEvent),
    Metadata(HistoryMeta),
    Console { text: String },
    /// Known packet type but unknown layout/size: kept for later decoders.
    Unknown { packet_type: u8, layout: u8, payload_len: usize },
}

pub fn decode_payload(pl: &[u8]) -> Option<Record> {
    let (&ty, layout) = (pl.first()?, pl.get(1).copied().unwrap_or(0));
    let rec = match ty {
        PACKET_HISTORY => match layout {
            LAYOUT_R18 => decode_r18(pl).map(Record::R18),
            LAYOUT_R20 => decode_r20(pl).map(Record::R20),
            LAYOUT_R21 => decode_r21(pl).map(Record::R21),
            LAYOUT_R26 => decode_r26(pl).map(Record::R26),
            _ => None,
        },
        PACKET_REALTIME => decode_live_hr(pl).map(Record::LiveHr),
        PACKET_REALTIME_RAW if layout == LAYOUT_R21 => decode_r21(pl).map(Record::R21),
        PACKET_REALTIME_RAW if layout == LAYOUT_R20 => decode_r20(pl).map(Record::R20),
        PACKET_EVENT => decode_event(pl).map(Record::Event),
        PACKET_METADATA => decode_metadata(pl).map(Record::Metadata),
        PACKET_CONSOLE => decode_console(pl).map(|text| Record::Console { text }),
        _ => None,
    };
    Some(rec.unwrap_or(Record::Unknown { packet_type: ty, layout, payload_len: pl.len() }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::{build_v5_payload_frame, parse_frame, DeviceType};

    /// Builds a full frame from a payload template and returns the payload as parse_frame sees it.
    pub fn roundtrip(payload: &[u8]) -> Vec<u8> {
        let frame = build_v5_payload_frame(payload);
        let parsed = parse_frame(DeviceType::Goose, &frame).expect("frame");
        assert!(parsed.header_crc_valid && parsed.payload_crc_valid);
        hex::decode(parsed.payload_hex).unwrap()
    }

    fn put(buf: &mut [u8], frame_off: usize, bytes: &[u8]) {
        buf[frame_off - 8..frame_off - 8 + bytes.len()].copy_from_slice(bytes);
    }

    pub fn r18_payload(ts: u32, hr: u8, rr_ticks: &[u16], steps: u16, band: u8) -> Vec<u8> {
        let mut pl = vec![0u8; 112];
        pl[0] = PACKET_HISTORY;
        pl[1] = LAYOUT_R18;
        put(&mut pl, 11, &7u32.to_le_bytes());
        put(&mut pl, 15, &ts.to_le_bytes());
        pl[22 - 8] = hr;
        pl[23 - 8] = rr_ticks.len() as u8;
        for (i, t) in rr_ticks.iter().enumerate() {
            put(&mut pl, 24 + 2 * i, &t.to_le_bytes());
        }
        put(&mut pl, 41, &0.02f32.to_le_bytes());
        put(&mut pl, 45, &0.0f32.to_le_bytes());
        put(&mut pl, 49, &0.6f32.to_le_bytes());
        put(&mut pl, 53, &(-0.8f32).to_le_bytes());
        put(&mut pl, 57, &steps.to_le_bytes());
        put(&mut pl, 73, &3412u16.to_le_bytes());
        pl[81 - 8] = band << 4 | 1;
        pl
    }

    #[test]
    fn r18_decodes_every_field() {
        let pl = roundtrip(&r18_payload(1_794_000_000, 58, &[1024, 1000], 1234, 2));
        let r = decode_r18(&pl).unwrap();
        assert_eq!(r.ts, 1_794_000_000);
        assert_eq!(r.hr, 58);
        assert_eq!(r.rr_ms, vec![1000, 977]);
        assert_eq!(r.steps, 1234);
        assert_eq!(r.band_state, "sleep");
        assert_eq!(r.wrist_bits, 1);
        assert_eq!(r.skin_temp_c, Some(34.12));
        assert_eq!(r.gravity_g, Some([0.0, 0.6, -0.8]));
        assert_eq!(r.unmapped_hex.len(), 2 * (2 + 1 + 4 + 23 + 3));
        // agrees with the decoder the app already uses
        let legacy = crate::protocol::parse_r18_history_fields(&pl).unwrap();
        assert_eq!(legacy.heart_rate_bpm as u8, r.hr);
        assert_eq!(legacy.rr_intervals_ms.iter().map(|v| *v as u32).collect::<Vec<_>>(), r.rr_ms);
    }

    #[test]
    fn r18_rejects_wrong_size_and_flags_out_of_range() {
        let mut pl = r18_payload(1_000, 0, &[], 0, 0);
        assert!(decode_r18(&pl[..100]).is_none());
        put(&mut pl, 41, &50.0f32.to_le_bytes());
        let r = decode_r18(&pl).unwrap();
        assert!(r.dyn_accel_g.is_none());
        assert!(r.warnings.iter().any(|w| w == "timestamp_before_2020"));
        assert!(r.warnings.iter().any(|w| w == "dynamic_accel_out_of_range"));
    }

    #[test]
    fn r26_reconstructs_cumulative_deltas_without_overflow() {
        let mut pl = vec![0u8; 76];
        pl[0] = PACKET_HISTORY;
        pl[1] = LAYOUT_R26;
        put(&mut pl, 15, &1_794_000_000u32.to_le_bytes());
        put(&mut pl, 23, &i32::MIN.to_le_bytes());
        for i in 0..24 {
            let d: i16 = if i % 2 == 0 { i16::MAX } else { i16::MIN };
            put(&mut pl, 27 + 2 * i, &d.to_le_bytes());
        }
        pl[82 - 8] = 1;
        let r = decode_r26(&roundtrip(&pl)).unwrap();
        assert_eq!(r.samples.len(), 25);
        assert_eq!(r.samples[0], i64::from(i32::MIN));
        assert_eq!(r.samples[1], i64::from(i32::MIN) + 32767);
        assert_eq!(r.samples[2], i64::from(i32::MIN) - 1);
        assert_eq!(r.acceptance_raw, 1);
    }

    pub fn r20_payload(counts: [u8; 5]) -> Vec<u8> {
        let mut pl = vec![0u8; 2128];
        pl[0] = PACKET_HISTORY;
        pl[1] = LAYOUT_R20;
        put(&mut pl, 15, &1_794_000_000u32.to_le_bytes());
        for (k, &fo) in R20_BLOCK_FRAME_OFFSETS.iter().enumerate() {
            pl[fo - 8] = counts[k];
            pl[fo - 8 + 1] = k as u8 + 1;
            for i in 0..usize::from(counts[k]) {
                put(&mut pl, fo + 21 + 4 * i, &(-(i as i32) * 1000).to_le_bytes());
                put(&mut pl, fo + 221 + 4 * i, &((i as i32) * 7).to_le_bytes());
            }
        }
        pl
    }

    #[test]
    fn r20_keeps_counted_samples_only() {
        let r = decode_r20(&roundtrip(&r20_payload([50, 25, 0, 1, 50]))).unwrap();
        assert_eq!(r.blocks.len(), 5);
        assert_eq!(r.blocks[1].slots[0].samples.len(), 25);
        assert_eq!(r.blocks[1].slots[0].samples[3], -3000);
        assert_eq!(r.blocks[2].slots[1].samples.len(), 0);
        assert_eq!(r.blocks[0].slots[0].source_raw, 1);
        assert_eq!(r.status, Status::DecodedUnvalidated);
        // a count that lies about the data is caught
        let mut bad = r20_payload([10, 0, 0, 0, 0]);
        bad[26 - 8] = 5;
        let r = decode_r20(&bad).unwrap();
        assert!(r.blocks[0].padding_nonzero);
        assert_eq!(r.status, Status::Invalid);
    }

    pub fn r21_payload(n: u16, accel: (i16, i16, i16), gyro: (i16, i16, i16)) -> Vec<u8> {
        let mut pl = vec![0u8; 1232];
        pl[0] = PACKET_HISTORY;
        pl[1] = LAYOUT_R21;
        put(&mut pl, 15, &1_794_000_000u32.to_le_bytes());
        put(&mut pl, 19, &16384u16.to_le_bytes());
        put(&mut pl, 24, &n.to_le_bytes());
        put(&mut pl, 630, &n.to_le_bytes());
        for i in 0..usize::from(n) {
            put(&mut pl, 28 + 2 * i, &accel.0.to_le_bytes());
            put(&mut pl, 228 + 2 * i, &accel.1.to_le_bytes());
            put(&mut pl, 428 + 2 * i, &accel.2.to_le_bytes());
            put(&mut pl, 640 + 2 * i, &gyro.0.to_le_bytes());
            put(&mut pl, 840 + 2 * i, &gyro.1.to_le_bytes());
            put(&mut pl, 1040 + 2 * i, &gyro.2.to_le_bytes());
        }
        pl
    }

    #[test]
    fn r21_converts_with_tentative_scales_and_keeps_raw() {
        let r = decode_r21(&roundtrip(&r21_payload(100, (0, 0, 4096), (164, -164, 0)))).unwrap();
        assert_eq!(r.ts, 1_794_000_000.5);
        assert_eq!(r.accel_raw[2].len(), 100);
        assert_eq!(r.accel_g()[2][0], 1.0);
        assert!((r.gyro_dps()[0][0] - 10.009).abs() < 0.01);
        assert_eq!(r.gyro_raw[1][0], -164);
    }

    #[test]
    fn reassembler_skips_false_frame_start_inside_payload() {
        use crate::protocol::FrameAccumulator;
        let good = build_v5_payload_frame(&r18_payload(1_794_000_000, 60, &[1024], 1, 0));
        // garbage holding 0xaa with a plausible but fake header, split across notifications
        let mut stream = vec![0x12, 0xaa, 0x01, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00];
        stream.extend_from_slice(&good);
        stream.extend_from_slice(&good);
        let mut acc = FrameAccumulator::new(DeviceType::Goose);
        let mut frames = Vec::new();
        for chunk in stream.chunks(20) {
            frames.extend(acc.feed(chunk).frames);
        }
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f == &good));
    }

    #[test]
    fn dispatch_keeps_unknown_layouts() {
        let pl = vec![PACKET_HISTORY, 99, 0, 0];
        assert_eq!(decode_payload(&pl), Some(Record::Unknown { packet_type: 47, layout: 99, payload_len: 4 }));
    }

    #[test]
    fn event_names_and_battery_candidate() {
        let mut pl = vec![0u8; 24];
        pl[0] = PACKET_EVENT;
        put(&mut pl, 10, &3u16.to_le_bytes());
        put(&mut pl, 12, &1_794_000_000u32.to_le_bytes());
        put(&mut pl, 21, &611u16.to_le_bytes());
        put(&mut pl, 25, &3900u16.to_le_bytes());
        let e = decode_event(&pl).unwrap();
        assert_eq!(e.name, "BATTERY_LEVEL");
        assert_eq!(e.decoded["battery_pct"], 61.1);
        assert_eq!(e.decoded["voltage_mv"], 3900);
        assert_eq!(event_name(200), "EVENT_200");
    }

    #[test]
    fn console_text_starts_at_frame_byte_21() {
        // real payload prefix (10 Nov): "...BLE Conn: ..."
        let pl = hex::decode("3250020082a01a6ac21534000120424c4520436f6e6e3a20").unwrap();
        assert_eq!(decode_console(&pl).unwrap(), " BLE Conn: ");
    }
}
