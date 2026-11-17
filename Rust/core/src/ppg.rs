//! PPG beyond the pulse rate (docs/SENSORS.md §4c): optical channel audit, night pulse-wave
//! template from ensemble-averaged beats, and respiratory rate from the pulse modulations.
//!
//! Inputs are what the strap already stores: R26 compact optical windows (one channel, LED 1 ->
//! photodiode 3, bursts of ~40 s every ~20 min, all day) and R20 optical frames when they were
//! recorded (all channels, 25 Hz). Nothing here estimates blood pressure or SpO2 from the green
//! channel; the audit reports whether the red / infrared pair needed for SpO2 was ever sampled.
//!
//! Pulse template: at 25 Hz one sample per 40 ms cannot show the wave shape of a single beat, but
//! the signal is band-limited (< 12.5 Hz) and the beats fall at random sampling phases. Each still
//! burst is reconstructed at 200 Hz (Lanczos interpolation), beats are aligned on their maximal
//! upstroke slope and median-averaged: the template has a 5 ms grid and its split-half agreement is
//! reported. Features (rise time, width, decay, second-derivative b/a, diastolic shoulder) are
//! tracked night to night, never read as absolute vascular values.
//!
//! Respiration: per burst, beat amplitude (RIAV), baseline (RIIV) and interval (RIFV) series,
//! 9-30 breaths/min band (the 0.1 Hz Mayer wave of blood-pressure regulation sits below it and
//! dominates the slow content); a burst counts when two of the three agree within 2 rpm.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::dsp::{filtfilt, median, round, std_sample, Biquad};
use crate::optical::{bursts, Burst};
use crate::whoop5_records::R26Record;
use crate::GooseResult;

pub const ALGORITHM_VERSION: &str = "ppg-3";
pub(crate) const FS: f64 = 25.0;
const UP: usize = 8;
const FU: f64 = FS * UP as f64;
/// template window around the maximal upstroke slope
const PRE_MS: f64 = 250.0;
const POST_MS: f64 = 850.0;
const MOTION_LIMIT: f64 = 0.20;

// ------------------------------------------------------------------ signal helpers

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let p = std::f64::consts::PI * x;
        p.sin() / p
    }
}

/// Band-limited reconstruction at `factor` x the rate (Lanczos, a = 4).
pub fn lanczos_upsample(x: &[f64], factor: usize) -> Vec<f64> {
    const A: i64 = 4;
    let n = x.len() as i64;
    (0..x.len() * factor)
        .map(|j| {
            let t = j as f64 / factor as f64;
            let c = t.floor() as i64;
            let mut s = 0.0;
            let mut w = 0.0;
            for k in (c - A + 1)..=(c + A) {
                if k < 0 || k >= n {
                    continue;
                }
                let d = t - k as f64;
                let l = sinc(d) * sinc(d / A as f64);
                s += x[k as usize] * l;
                w += l;
            }
            if w.abs() > 1e-9 { s / w } else { 0.0 }
        })
        .collect()
}

fn highpass(x: &[f64], fs: f64, fc: f64) -> Vec<f64> {
    filtfilt(&[Biquad::highpass(fs, fc)], x)
}

fn percentile(v: &[f64], p: f64) -> f64 {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return 0.0;
    }
    s.sort_by(f64::total_cmp);
    let k = (p / 100.0 * (s.len() - 1) as f64).round() as usize;
    s[k.min(s.len() - 1)]
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 3 {
        return None;
    }
    let ma = a[..n].iter().sum::<f64>() / n as f64;
    let mb = b[..n].iter().sum::<f64>() / n as f64;
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        sab += (a[i] - ma) * (b[i] - mb);
        saa += (a[i] - ma).powi(2);
        sbb += (b[i] - mb).powi(2);
    }
    (saa > 0.0 && sbb > 0.0).then(|| sab / (saa * sbb).sqrt())
}

/// Dominant frequency (Hz) of an evenly sampled series within [lo, hi], zero-padded DFT.
fn dominant(s: &[f64], fs: f64, lo: f64, hi: f64) -> Option<(f64, f64)> {
    let n = s.len();
    if n < 16 {
        return None;
    }
    let pad = n * 8;
    let w: Vec<f64> = s.iter().enumerate().map(|(i, v)| v * (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos())).collect();
    let (k0, k1) = ((lo * pad as f64 / fs).ceil() as usize, (hi * pad as f64 / fs).floor() as usize);
    let power = |k: usize| {
        let (mut re, mut im) = (0.0, 0.0);
        for (i, v) in w.iter().enumerate() {
            let ph = -2.0 * std::f64::consts::PI * (k * i) as f64 / pad as f64;
            re += v * ph.cos();
            im += v * ph.sin();
        }
        re * re + im * im
    };
    let ps: Vec<(usize, f64)> = (k0..=k1).map(|k| (k, power(k))).collect();
    let total: f64 = ps.iter().map(|p| p.1).sum();
    let (k, p) = ps.into_iter().max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some((k as f64 * fs / pad as f64, if total > 0.0 { p / total } else { 0.0 }))
}

pub(crate) fn bandpass(x: &[f64], fs: f64, lo: f64, hi: f64) -> Vec<f64> {
    filtfilt(&[Biquad::highpass(fs, lo), Biquad::lowpass(fs, hi)], x)
}

// ------------------------------------------------------------------ beats of one burst

#[derive(Debug, Clone)]
pub struct BurstBeats {
    pub start: i64,
    /// upsampled, high-passed, polarity-corrected signal (blood volume up = up)
    pub z: Vec<f64>,
    /// raw samples at 25 Hz (baseline modulation)
    pub raw: Vec<f64>,
    /// maximal-upstroke-slope indices in `z` (FU Hz)
    pub fid: Vec<usize>,
    pub hr: f64,
    pub inverted: bool,
}

fn still(b: &Burst) -> bool {
    b.windows.len() >= 20 && b.windows.iter().all(|w| w.motion.is_some_and(|m| f64::from(m) <= MOTION_LIMIT) && w.acceptance_raw != 0)
}

/// Beats of a still burst whose pulse rate matches the strap heart rate (+- 6 bpm), else None.
pub fn burst_beats(b: &Burst, hr_at: &dyn Fn(i64) -> Option<f64>) -> Option<BurstBeats> {
    if !still(b) {
        return None;
    }
    let hrs: Vec<f64> = b.windows.iter().filter_map(|w| hr_at(w.ts)).filter(|h| *h > 25.0).collect();
    let hr = median(&hrs)?;
    let raw: Vec<f64> = b.windows.iter().flat_map(|w| w.samples.iter().map(|v| *v as f64)).collect();
    let m = raw.iter().sum::<f64>() / raw.len() as f64;
    let y = highpass(&raw.iter().map(|v| v - m).collect::<Vec<_>>(), FS, 0.25);
    let up = lanczos_upsample(&y, UP);
    // the systolic upstroke is steeper than the diastolic decay (reflectance PPG: more blood, less light)
    let d: Vec<f64> = (0..up.len()).map(|i| if i == 0 || i + 1 >= up.len() { 0.0 } else { (up[i + 1] - up[i - 1]) / 2.0 }).collect();
    let inverted = -percentile(&d, 0.5) > percentile(&d, 99.5);
    let z: Vec<f64> = if inverted { up.iter().map(|v| -v).collect() } else { up };
    let d: Vec<f64> = if inverted { d.iter().map(|v| -v).collect() } else { d };
    let period = 60.0 / hr;
    let dist = (0.6 * period * FU) as usize;
    let mut order: Vec<usize> = (1..d.len().saturating_sub(1)).collect();
    order.sort_by(|a, b| d[*b].total_cmp(&d[*a]));
    let mut fid: Vec<usize> = Vec::new();
    for i in order.into_iter().take(d.len() / 8) {
        if d[i] <= 0.0 {
            break;
        }
        if fid.iter().all(|f| f.abs_diff(i) >= dist) {
            fid.push(i);
        }
    }
    fid.sort_unstable();
    // drop the filter edges
    let edge = (1.0 * FU) as usize;
    fid.retain(|i| *i > edge && *i + edge < z.len());
    if fid.len() < 8 {
        return None;
    }
    let ibi: Vec<f64> = fid.windows(2).map(|p| (p[1] - p[0]) as f64 / FU).collect();
    let pr = 60.0 / median(&ibi)?;
    if (pr - hr).abs() > 6.0 {
        return None;
    }
    Some(BurstBeats { start: b.windows[0].ts, z, raw, fid, hr, inverted })
}

// ------------------------------------------------------------------ pulse template

/// Normalised beats (foot 0, systolic peak 1) on the 5 ms grid [-PRE_MS, +POST_MS].
fn beat_shapes(bb: &BurstBeats) -> Vec<Vec<f64>> {
    let pre = (PRE_MS / 1000.0 * FU) as usize;
    let len = ((PRE_MS + POST_MS) / 1000.0 * FU) as usize;
    let period = 60.0 / bb.hr;
    let mut out = Vec::new();
    for k in 1..bb.fid.len().saturating_sub(1) {
        let i = bb.fid[k];
        let per = (bb.fid[k + 1] - i) as f64 / FU;
        if !(0.7 * period..=1.3 * period).contains(&per) || i < pre || i - pre + len > bb.z.len() {
            continue;
        }
        let seg = &bb.z[i - pre..i - pre + len];
        let foot = seg[..pre].iter().copied().fold(f64::INFINITY, f64::min);
        let peak = seg[pre..pre + (0.4 * FU) as usize].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if peak - foot <= 0.0 {
            continue;
        }
        out.push(seg.iter().map(|v| (v - foot) / (peak - foot)).collect());
    }
    out
}

fn median_shape(beats: &[&Vec<f64>]) -> Vec<f64> {
    let len = beats.first().map_or(0, |b| b.len());
    (0..len).map(|j| median(&beats.iter().map(|b| b[j]).collect::<Vec<_>>()).unwrap_or(f64::NAN)).collect()
}

fn smooth(x: &[f64], half: usize) -> Vec<f64> {
    (0..x.len()).map(|i| {
        let (a, b) = (i.saturating_sub(half), (i + half + 1).min(x.len()));
        x[a..b].iter().sum::<f64>() / (b - a) as f64
    }).collect()
}

/// Shape features of a template (ms relative to the maximal upstroke slope). The systolic peak is
/// the first maximum after the upstroke; at the wrist the diastolic (reflected) wave is often almost
/// as high, so it is reported separately (a global maximum would jump from one to the other).
pub fn features(t: &[f64]) -> Value {
    let dt = 1000.0 / FU;
    let pre = (PRE_MS / dt) as usize;
    let ms = |i: usize| (i as f64 - pre as f64) * dt;
    let s = smooth(t, 2);
    let d1: Vec<f64> = (0..s.len()).map(|i| if i == 0 || i + 1 >= s.len() { 0.0 } else { (s[i + 1] - s[i - 1]) / 2.0 }).collect();
    let d1s = smooth(&d1, 2);
    let d2: Vec<f64> = (0..d1s.len()).map(|i| if i == 0 || i + 1 >= d1s.len() { 0.0 } else { (d1s[i + 1] - d1s[i - 1]) / 2.0 }).collect();
    let foot = (0..pre).min_by(|a, b| t[*a].total_cmp(&t[*b])).unwrap_or(0);
    let limit = (pre + (0.45 * FU) as usize).min(t.len() - 1);
    let Some(sys) = (pre + 1..limit).find(|i| d1s[*i - 1] > 0.0 && d1s[*i] <= 0.0) else {
        return json!({"foot_ms": round(ms(foot), 0), "systolic_peak": Value::Null});
    };
    let amp = (s[sys] - s[foot]).max(1e-9);
    let level = |i: usize| (s[i] - s[foot]) / amp;
    let up50 = (foot..sys).find(|i| level(*i) >= 0.5);
    let down50 = (sys..s.len()).find(|i| level(*i) <= 0.5);
    let a_i = (foot..sys).max_by(|x, y| d2[*x].total_cmp(&d2[*y]));
    let b_i = a_i.and_then(|a| (a..=sys).min_by(|x, y| d2[*x].total_cmp(&d2[*y])));
    let b_over_a = match (a_i, b_i) {
        (Some(a), Some(b)) if d2[a] > 0.0 => Some(d2[b] / d2[a]),
        _ => None,
    };
    // diastolic wave: next maximum within 450 ms, else the inflection (the decay slows most)
    let hi = (sys + (0.45 * FU) as usize).min(d1s.len() - 2);
    let lo = sys + (0.06 * FU) as usize;
    let dia_peak = (lo..hi).find(|i| d1s[*i - 1] > 0.0 && d1s[*i] <= 0.0);
    let dia = dia_peak.map(|i| (i, "pic")).or_else(|| {
        (lo..hi).filter(|i| d1s[*i] > d1s[*i - 1] && d1s[*i] >= d1s[*i + 1]).max_by(|a, b| d1s[*a].total_cmp(&d1s[*b])).map(|i| (i, "inflexion"))
    });
    json!({
        "foot_ms": round(ms(foot), 0),
        "systolic_peak_ms": round(ms(sys), 0),
        "rise_time_ms": round((sys - foot) as f64 * dt, 0),
        "half_width_ms": match (up50, down50) { (Some(a), Some(b)) => json!(round((b - a) as f64 * dt, 0)), _ => Value::Null },
        "decay_50_ms": down50.map(|b| round((b - sys) as f64 * dt, 0)),
        "b_over_a": b_over_a.map(|v| round(v, 2)),
        "diastolic": dia.map(|(i, kind)| json!({"kind": kind, "delay_ms": round((i - sys) as f64 * dt, 0), "reflection_index": round(level(i), 2)})),
    })
}

/// Night template from the still bursts of the sleep period.
pub fn pulse_template(windows: &[R26Record], hr_at: &dyn Fn(i64) -> Option<f64>) -> Value {
    let bs = bursts(windows);
    let beats: Vec<BurstBeats> = bs.iter().filter_map(|b| burst_beats(b, hr_at)).collect();
    let shapes: Vec<Vec<f64>> = beats.iter().flat_map(beat_shapes).collect();
    if shapes.len() < 60 {
        return json!({"available": false, "beats": shapes.len(), "bursts_used": beats.len(), "reason": "fewer than 60 clean beats (still, accepted, pulse rate = strap heart rate)"});
    }
    let all: Vec<&Vec<f64>> = shapes.iter().collect();
    let tmpl = median_shape(&all);
    let even: Vec<&Vec<f64>> = shapes.iter().step_by(2).collect();
    let odd: Vec<&Vec<f64>> = shapes.iter().skip(1).step_by(2).collect();
    let (h0, h1) = (median_shape(&even), median_shape(&odd));
    let split_r = pearson(&h0, &h1);
    let split_max = h0.iter().zip(&h1).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    let step = (20.0 / (1000.0 / FU)) as usize; // 20 ms grid for display
    let grid: Vec<Value> = (0..tmpl.len()).step_by(step).map(|i| json!([round(i as f64 * 1000.0 / FU - PRE_MS, 0), round(tmpl[i], 3)])).collect();
    json!({
        "available": true,
        "beats": shapes.len(),
        "bursts_used": beats.len(),
        "bursts_total": bs.len(),
        "inverted_bursts": beats.iter().filter(|b| b.inverted).count(),
        "grid_ms": 1000.0 / FU,
        "split_half_r": split_r.map(|v| round(v, 4)),
        "split_half_max_diff": round(split_max, 3),
        "features": features(&tmpl),
        "template": grid,
        "method": "R26 still bursts in the sleep period, 200 Hz Lanczos reconstruction, beats aligned on the maximal upstroke slope, median of normalised beats",
    })
}

// ------------------------------------------------------------------ respiration from the PPG

/// Breaths per minute of one burst: amplitude, baseline and interval modulations, 9-30 rpm band.
pub fn burst_respiration(bb: &BurstBeats) -> Value {
    let t: Vec<f64> = bb.fid.iter().map(|i| *i as f64 / FU).collect();
    let w_pre = (0.3 * FU) as usize;
    let w_post = (0.4 * FU) as usize;
    let amp: Vec<f64> = bb.fid.iter().map(|&i| {
        let foot = bb.z[i.saturating_sub(w_pre)..=i].iter().copied().fold(f64::INFINITY, f64::min);
        let pk = bb.z[i..(i + w_post).min(bb.z.len())].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        pk - foot
    }).collect();
    let base: Vec<f64> = bb.fid.iter().map(|&i| bb.raw[(i / UP).min(bb.raw.len() - 1)]).collect();
    let mut iv: Vec<f64> = t.windows(2).map(|p| p[1] - p[0]).collect();
    iv.insert(0, iv.first().copied().unwrap_or(1.0));
    let fs4 = 4.0;
    let grid: Vec<f64> = (0..((t[t.len() - 1] - t[0]) * fs4) as usize).map(|k| t[0] + k as f64 / fs4).collect();
    let resample = |v: &[f64]| -> Vec<f64> {
        grid.iter().map(|g| {
            let j = t.partition_point(|x| x <= g).clamp(1, t.len() - 1);
            let (t0, t1) = (t[j - 1], t[j]);
            let f = if t1 > t0 { (g - t0) / (t1 - t0) } else { 0.0 };
            v[j - 1] + f * (v[j] - v[j - 1])
        }).collect()
    };
    let mut est: Vec<f64> = Vec::new();
    let mut each = serde_json::Map::new();
    for (name, series) in [("amplitude", &amp), ("baseline", &base), ("interval", &iv)] {
        let s = resample(series);
        if s.len() < 40 {
            continue;
        }
        let f = bandpass(&s, fs4, 0.15, 0.6);
        if let Some((hz, share)) = dominant(&f, fs4, 0.15, 0.5) {
            each.insert(name.into(), json!({"rpm": round(hz * 60.0, 1), "peak_share": round(share, 2)}));
            // a maximum on the band edge is the tail of the Mayer wave (or of the pulse), not a breath
            if (9.6..=29.4).contains(&(hz * 60.0)) {
                est.push(hz * 60.0);
            }
        }
    }
    let agree: Vec<f64> = est.iter().copied().filter(|v| est.iter().filter(|w| (*v - **w).abs() <= 2.0).count() >= 2).collect();
    json!({"start": bb.start, "rpm": median(&agree).map(|v| round(v, 1)), "modulations": each})
}

/// Respiratory rate per still burst of a period, with the median and its spread.
pub fn respiration(windows: &[R26Record], hr_at: &dyn Fn(i64) -> Option<f64>) -> Value {
    let bs = bursts(windows);
    let per: Vec<Value> = bs.iter().filter_map(|b| burst_beats(b, hr_at)).map(|bb| burst_respiration(&bb)).collect();
    let vals: Vec<f64> = per.iter().filter_map(|p| p["rpm"].as_f64()).collect();
    json!({
        // at least 3 agreeing bursts (~2 min of still signal)
        "rpm": (vals.len() >= 3).then(|| median(&vals)).flatten().map(|v| round(v, 1)),
        "rpm_sd": std_sample(&vals).map(|v| round(v, 1)),
        "bursts": per.len(),
        "bursts_agreeing": vals.len(),
        "series": per.iter().filter_map(|p| p["rpm"].as_f64().map(|r| json!([p["start"], r]))).collect::<Vec<_>>(),
        "method": "PPG amplitude / baseline / interval modulations of still R26 bursts, 9-30 rpm, >= 2 of 3 within 2 rpm",
    })
}

// ------------------------------------------------------------------ channel audit (R20)

/// One photodiode channel of one R20 time slot (block). LEDs listed in a block fire together and
/// both photodiodes of the block integrate their light: block 4 slot B ("LED 4, drive 0") sees 36 x
/// the ambient level, i.e. LED 2's light from slot A.
#[derive(Debug, Clone, Default)]
struct Channel {
    block: i64,
    slot: String,
    detector: i64,
    /// active LEDs of the block: (LED, mA)
    leds: Vec<(i64, f64)>,
    /// per second: samples (ADC codes), range (uA full scale), offset (10 nA units)
    seconds: BTreeMap<i64, (Vec<f64>, f64, f64)>,
    empty_seconds: usize,
}

/// 19-bit ADC (max 524 287 = saturation); `range` = full-scale photocurrent in uA (32 / 16).
pub(crate) const ADC_FULL: f64 = 524_288.0;
/// offset DAC step (10 nA): 800 removed 8.06 uA on 12 Nov (frame after the change)
pub(crate) const OFFSET_UA: f64 = 0.01;
/// LED drive code step (10 uA): 3350 = 33.5 mA
const DRIVE_MA: f64 = 0.01;

pub(crate) fn i32s(b: &[u8]) -> Vec<f64> {
    b.chunks_exact(4).map(|c| f64::from(i32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect()
}

/// Wavelength of each LED from the evidence of the frames (docs/SENSORS.md §4d).
pub fn led_roles() -> Value {
    json!([
        {"led": 1, "role": "vert", "evidence": "fort taux d'absorption (0,18 µA/mA avec la LED 4), pulsatilité ~1 % : canal de pouls principal, salves R26"},
        {"led": 4, "role": "vert", "evidence": "s'allume avec la LED 1 dans le même créneau, même profil"},
        {"led": 2, "role": "infrarouge probable", "evidence": "≈ 3 µA/mA (16 x le créneau vert), pulsatilité 0,2-0,3 %, allumée en continu à 2 mA comme capteur de contact, et dans la paire SpO₂"},
        {"led": 3, "role": "rouge probable", "evidence": "seulement dans le créneau SpO₂, au courant le plus fort (127,5 mA) et sur la gamme la plus sensible (16 µA) : profil d'une LED rouge, plus absorbée ; jamais transmise"},
    ])
}

/// Per channel: absolute photocurrent (codes x range / 2^19 + offset of the previous frame), light
/// per mA of LED drive, pulsatility on the absolute level, pulse rate vs the strap, class; the R26
/// channel is identified by sample identity; the SpO2 slots are reported as sampled or not.
pub fn audit(conn: &Connection, hr_at: &dyn Fn(i64) -> Option<f64>) -> GooseResult<Value> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='whoop5_r20_slot'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    if !exists {
        return Ok(json!({"r20_seconds": 0, "channels": [], "led_roles": led_roles(), "conclusions": ["Aucune trame optique complète (R20) enregistrée."]}));
    }
    // (ts, block) -> active LEDs
    let mut rows: Vec<(i64, i64, String, i64, i64, i64, i64, i64, i64, Vec<u8>)> = Vec::new();
    {
        let mut st = conn.prepare("SELECT ts, block, slot, source_raw, drive_raw, detector_raw, range_raw, offset_raw, count, samples FROM whoop5_r20_slot ORDER BY ts")?;
        let it = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)))?;
        for row in it {
            rows.push(row?);
        }
    }
    let mut leds_at: BTreeMap<(i64, i64), Vec<(i64, f64)>> = BTreeMap::new();
    for r in &rows {
        if r.4 > 0 {
            let e = leds_at.entry((r.0, r.1)).or_default();
            if !e.iter().any(|l| l.0 == r.3) {
                e.push((r.3, r.4 as f64 * DRIVE_MA));
            }
        }
    }
    let mut chans: BTreeMap<(i64, String, i64, String), Channel> = BTreeMap::new();
    for (ts, block, slot, _src, _drive, det, range, offset, count, samples) in rows {
        let leds = leds_at.get(&(ts, block)).cloned().unwrap_or_default();
        let lk = leds.iter().map(|l| format!("{}@{}", l.0, l.1)).collect::<Vec<_>>().join("+");
        let c = chans.entry((block, slot.clone(), det, lk)).or_insert_with(|| Channel { block, slot, detector: det, leds, ..Default::default() });
        if count > 0 {
            c.seconds.insert(ts, (i32s(&samples), range as f64, offset as f64));
        } else {
            c.empty_seconds += 1;
        }
    }
    let r20_seconds: usize = chans.values().map(|c| c.seconds.len() + c.empty_seconds).max().unwrap_or(0);
    let (lo, hi) = chans.values().flat_map(|c| c.seconds.keys().copied()).fold((i64::MAX, i64::MIN), |(a, b), t| (a.min(t), b.max(t)));
    let r26: BTreeMap<i64, Vec<f64>> = if lo <= hi {
        crate::whoop5_store::load_r26(conn, lo, hi + 1)?.into_iter().map(|w| (w.ts, w.samples.iter().map(|v| *v as f64).collect())).collect()
    } else {
        BTreeMap::new()
    };
    let mut out = Vec::new();
    for c in chans.values() {
        let mut runs: Vec<Vec<i64>> = Vec::new();
        for &t in c.seconds.keys() {
            match runs.last_mut() {
                Some(r) if *r.last().unwrap() + 1 == t => r.push(t),
                _ => runs.push(vec![t]),
            }
        }
        // 60 s chunks, at most 3 per run: enough for level / pulsatility, cheap on the phone
        let chunks: Vec<Vec<i64>> = runs.iter().flat_map(|r| r.chunks(60).filter(|c| c.len() >= 20).take(3).map(|c| c.to_vec()).collect::<Vec<_>>()).collect();
        let (mut dcs, mut acs, mut pis, mut prs, mut errs, mut sat, mut n) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0usize, 0usize);
        for r in chunks.iter() {
            // photocurrent in uA; the offset reported in a frame applies from the next one
            let mut x: Vec<f64> = Vec::new();
            for (k, t) in r.iter().enumerate() {
                let (s, range, _) = &c.seconds[t];
                let off = if k == 0 { c.seconds[t].2 } else { c.seconds[&r[k - 1]].2 };
                sat += s.iter().filter(|v| **v >= ADC_FULL - 1.0).count();
                n += s.len();
                x.extend(s.iter().map(|v| v * range / ADC_FULL + off * OFFSET_UA));
            }
            let dc = median(&x).unwrap_or(0.0);
            let m = x.iter().sum::<f64>() / x.len() as f64;
            let y = bandpass(&x.iter().map(|v| v - m).collect::<Vec<_>>(), FS, 0.5, 4.0);
            let y = &y[25..y.len() - 25];
            let pp: Vec<f64> = y.chunks(38).filter(|w| w.len() == 38).map(|w| w.iter().copied().fold(f64::NEG_INFINITY, f64::max) - w.iter().copied().fold(f64::INFINITY, f64::min)).collect();
            let ac = median(&pp).unwrap_or(0.0);
            dcs.push(dc);
            acs.push(ac);
            if dc > 0.05 {
                pis.push(100.0 * ac / dc);
            }
            if let Some((hz, share)) = dominant(&y.iter().step_by(2).copied().collect::<Vec<_>>(), FS / 2.0, 0.7, 3.0).filter(|_| !c.leds.is_empty()) {
                let hr = median(&r.iter().filter_map(|t| hr_at(*t)).collect::<Vec<_>>());
                if share > 0.0 {
                    prs.push(hz * 60.0);
                    if let Some(h) = hr {
                        errs.push(hz * 60.0 - h);
                    }
                }
            }
        }
        let secs = c.seconds.len();
        let dc = median(&dcs);
        let pi = median(&pis);
        let ma: f64 = c.leds.iter().map(|l| l.1).sum();
        let per_ma = dc.filter(|_| ma > 0.0).map(|d| d / ma);
        let leds: Vec<i64> = c.leds.iter().map(|l| l.0).collect();
        let class = if secs == 0 {
            "non échantillonné"
        } else if leds.is_empty() {
            "lumière ambiante + courant d'obscurité"
        } else if leds == [2] {
            "infrarouge probable"
        } else if leds.iter().all(|l| *l == 1 || *l == 4) {
            "vert"
        } else {
            "indéterminé"
        };
        let common: Vec<i64> = c.seconds.keys().copied().filter(|t| r26.contains_key(t)).collect();
        let r26_match = (common.len() >= 10).then(|| {
            let a: Vec<f64> = common.iter().flat_map(|t| c.seconds[t].0.iter().copied()).collect();
            let b: Vec<f64> = common.iter().flat_map(|t| r26[t].iter().copied()).collect();
            let identical = a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 0.5);
            json!({"seconds": common.len(), "identical": identical, "r": pearson(&a, &b).map(|v| round(v, 3))})
        });
        let first = c.seconds.values().next();
        out.push(json!({
            "block": c.block, "slot": c.slot, "photodiode": c.detector,
            "leds": c.leds.iter().map(|l| json!({"led": l.0, "ma": l.1})).collect::<Vec<_>>(),
            "led": leds.first(), "led_ma_total": round(ma, 1),
            "range_ua": first.map(|f| f.1), "offset_ua": first.map(|f| round(f.2 * OFFSET_UA, 2)),
            "seconds": secs, "empty_seconds": c.empty_seconds,
            "samples_per_second": if secs > 0 { json!(round(c.seconds.values().map(|v| v.0.len()).sum::<usize>() as f64 / secs as f64, 1)) } else { Value::Null },
            "photocurrent_ua": dc.map(|v| round(v, 2)),
            "pulse_ua": median(&acs).map(|v| round(v, 3)),
            "ua_per_ma": per_ma.map(|v| round(v, 3)),
            "pi_pct": pi.map(|v| round(v, 3)),
            "saturated_pct": if n > 0 { json!(round(100.0 * sat as f64 / n as f64, 2)) } else { Value::Null },
            "pulse_rate_bpm": median(&prs).map(|v| round(v, 1)),
            "pulse_matches_strap_hr": (!errs.is_empty()).then(|| errs.iter().filter(|e| e.abs() <= 6.0).count() * 2 >= errs.len()),
            "class": class,
            "r26": r26_match,
        }));
    }
    let find = |pred: &dyn Fn(&Value) -> bool| out.iter().find(|c| pred(c)).cloned();
    let green = find(&|c| c["block"] == 0 && c["slot"] == "a");
    let ir = find(&|c| c["class"] == "infrarouge probable" && c["photodiode"] == 3);
    let spo2_sampled = out.iter().any(|c| (c["block"] == 1 || c["block"] == 2) && c["seconds"].as_u64().unwrap_or(0) > 0);
    let r26_channel = out.iter().find(|c| c["r26"]["identical"] == true).map(|c| format!("créneau 0, photodiode {} (LED 1 + LED 4)", c["photodiode"]));
    let mut conclusions = vec![format!("{r20_seconds} s de trames optiques complètes (R20) : 5 créneaux, 2 photodiodes par créneau, 25 échantillons par seconde, convertisseur 19 bits, gamme 16 ou 32 µA, compensation par pas de 10 nA.")];
    if let Some(ch) = &r26_channel {
        conclusions.push(format!("Les salves de nuit (R26) sont le {ch} : échantillons identiques sur les secondes communes."));
    }
    if let (Some(g), Some(i)) = (&green, &ir) {
        if let (Some(a), Some(b)) = (g["ua_per_ma"].as_f64(), i["ua_per_ma"].as_f64()) {
            conclusions.push(format!("LED 2 renvoie {:.0} fois plus de courant par mA que les LED vertes (pulsatilité {:.2} % contre {:.2} %), et reste allumée à 2 mA pour détecter le contact : infrarouge probable. LED 3 n'apparaît que dans le créneau SpO₂, au courant maximal et sur la gamme la plus sensible : rouge probable.",
                b / a, i["pi_pct"].as_f64().unwrap_or(0.0), g["pi_pct"].as_f64().unwrap_or(0.0)));
        }
    }
    if !spo2_sampled {
        conclusions.push("Les créneaux SpO₂ (LED 3 rouge + LED 2 infrarouge sur la photodiode 1) ne sont jamais transmis : pas de paire rouge + infrarouge simultanée, donc pas de SpO₂ continue. La SpO₂ reste celle du bracelet (mesures ponctuelles la nuit).".into());
    }
    Ok(json!({"algorithm_version": ALGORITHM_VERSION, "r20_seconds": r20_seconds, "channels": out, "led_roles": led_roles(), "r26_channel": r26_channel, "spo2_pair_sampled": spo2_sampled, "conclusions": conclusions}))
}

/// Audit with the strap heart rate of the R20 seconds only.
pub fn audit_db(conn: &Connection) -> GooseResult<Value> {
    let exists: bool = conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='whoop5_r20_slot'", [], |_| Ok(true)).optional()?.unwrap_or(false);
    let (lo, hi): (Option<i64>, Option<i64>) = if exists { conn.query_row("SELECT MIN(ts), MAX(ts) FROM whoop5_r20_slot", [], |r| Ok((r.get(0)?, r.get(1)?)))? } else { (None, None) };
    let n: i64 = if exists { conn.query_row("SELECT COUNT(*) FROM whoop5_r20_slot", [], |r| r.get(0))? } else { 0 };
    // cached: recomputed only when new optical frames arrived
    conn.execute("CREATE TABLE IF NOT EXISTS ppg_audit_cache (version TEXT NOT NULL, rows INTEGER NOT NULL, result TEXT NOT NULL)", [])?;
    let cached: Option<String> = conn.query_row("SELECT result FROM ppg_audit_cache WHERE version = ?1 AND rows = ?2", params![ALGORITHM_VERSION, n], |r| r.get(0)).optional()?;
    if let Some(v) = cached.and_then(|j| serde_json::from_str::<Value>(&j).ok()) {
        return Ok(v);
    }
    let hr = match (lo, hi) {
        (Some(a), Some(b)) => hr_map(conn, a, b + 1)?,
        _ => Default::default(),
    };
    let v = audit(conn, &|t| hr.get(&t).copied())?;
    conn.execute("DELETE FROM ppg_audit_cache", [])?;
    conn.execute("INSERT INTO ppg_audit_cache (version, rows, result) VALUES (?1, ?2, ?3)", params![ALGORITHM_VERSION, n, v.to_string()])?;
    Ok(v)
}

/// R18 heart rate per second over a range (for the audit's pulse check).
pub fn hr_map(conn: &Connection, from: i64, to: i64) -> GooseResult<std::collections::HashMap<i64, f64>> {
    let mut st = conn.prepare("SELECT ts, hr FROM whoop5_r18 WHERE ts >= ?1 AND ts < ?2 AND hr > 0")?;
    let rows = st.query_map(params![from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as f64)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Synthetic reflectance PPG: light falls quickly at systole (rise 120 ms) and recovers slowly,
    /// with a diastolic bump, amplitude modulated by breathing.
    fn synth(seconds: usize, bpm: f64, rpm: f64) -> Vec<R26Record> {
        let period = 60.0 / bpm;
        let wave = |ph: f64| -> f64 {
            let tr = 0.12 / period;
            if ph < tr { (ph / tr * PI / 2.0).sin() } else { let u = (ph - tr) / (1.0 - tr); (1.0 - u).powf(1.6) + 0.12 * (-((u - 0.35) / 0.06).powi(2)).exp() }
        };
        (0..seconds)
            .map(|s| {
                let samples: Vec<i64> = (0..25)
                    .map(|i| {
                        let t = (s * 25 + i) as f64 / FS;
                        let breath = 1.0 + 0.15 * (2.0 * PI * rpm / 60.0 * t).sin();
                        300_000 - (3000.0 * breath * wave((t / period).fract())) as i64 + (500.0 * (2.0 * PI * rpm / 60.0 * t).cos()) as i64
                    })
                    .collect();
                R26Record { ts: 1_794_000_000 + s as i64, record_index: s as u32, time_word: 0, burst: 1, base: samples[0] as i32, deltas: vec![], samples, motion: Some(0.05), status_raw: 0, on_wrist_raw: 1, acceptance_raw: 1 }
            })
            .collect()
    }

    #[test]
    fn upsampling_reproduces_a_band_limited_signal() {
        let f = 2.3;
        let x: Vec<f64> = (0..250).map(|i| (2.0 * PI * f * i as f64 / FS).sin()).collect();
        let u = lanczos_upsample(&x, UP);
        let err = (400..1600).map(|j| (u[j] - (2.0 * PI * f * j as f64 / FU).sin()).abs()).fold(0.0, f64::max);
        assert!(err < 0.03, "max error {err}");
    }

    #[test]
    fn template_recovers_rise_time_and_polarity() {
        // three bursts of 40 s at 57 bpm: inverted reflectance signal must be turned upright
        let mut w = Vec::new();
        for k in 0..3 {
            let mut b = synth(40, 57.0, 15.0);
            for r in &mut b { r.ts += k * 1200; }
            w.extend(b);
        }
        let t = pulse_template(&w, &|_| Some(57.0));
        assert_eq!(t["available"], true, "{t}");
        assert_eq!(t["inverted_bursts"], 3);
        let rise = t["features"]["rise_time_ms"].as_f64().unwrap();
        assert!((90.0..=170.0).contains(&rise), "rise {rise}");
        assert!(t["split_half_r"].as_f64().unwrap() > 0.99);
    }

    #[test]
    fn respiration_from_modulations_and_mayer_band_excluded() {
        let mut w = Vec::new();
        for k in 0..3 {
            let mut b = synth(40, 60.0, 15.0);
            for r in &mut b { r.ts += k * 1200; }
            w.extend(b);
        }
        let r = respiration(&w, &|_| Some(60.0));
        let rpm = r["rpm"].as_f64().expect("rpm");
        assert!((rpm - 15.0).abs() <= 1.5, "{r}");
        // a moving burst is never used
        let mut m = synth(40, 60.0, 15.0);
        m[10].motion = Some(0.5);
        assert_eq!(respiration(&m, &|_| Some(60.0))["bursts"], 0);
    }

    #[test]
    #[ignore]
    fn real_audit_and_nights() {
        // GOOSE_PPG_DB: sqlite with whoop5_r20_slot, whoop5_r26 and whoop5_r18 (ts, hr)
        let Ok(db) = std::env::var("GOOSE_PPG_DB") else { return };
        let conn = Connection::open(db).unwrap();
        let hr = hr_map(&conn, 0, i64::MAX).unwrap();
        let t0 = std::time::Instant::now();
        let a = audit(&conn, &|t| hr.get(&t).copied()).unwrap();
        println!("audit in {:?}", t0.elapsed());
        println!("{}", serde_json::to_string_pretty(&a["conclusions"]).unwrap());
        for c in a["channels"].as_array().unwrap() {
            println!("{} {} LEDs {} PD{} {} s, {} uA, {} uA/mA, PI {} %, sat {} %, pr {} -> {}", c["block"], c["slot"], c["leds"], c["photodiode"], c["seconds"], c["photocurrent_ua"], c["ua_per_ma"], c["pi_pct"], c["saturated_pct"], c["pulse_rate_bpm"], c["class"]);
        }
        // nights: 22:30 -> 09:30 local (UTC+1)
        for night in 0..8 {
            let n0 = 1_794_349_800 + night * 86_400;
            let w = crate::whoop5_store::load_r26(&conn, n0, n0 + 11 * 3600).unwrap();
            let t = pulse_template(&w, &|ts| hr.get(&ts).copied());
            let r = respiration(&w, &|ts| hr.get(&ts).copied());
            println!("night {night}: beats {} split {} features {} | resp {} ({}/{})", t["beats"], t["split_half_r"], t["features"], r["rpm"], r["bursts_agreeing"], r["bursts"]);
        }
    }
}
