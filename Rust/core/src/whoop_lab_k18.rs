//! whoop-lab: nightly recovery values computed from decoded WHOOP 5.0 R18 history records.
//!
//! R18 carries 1 Hz heart rate, up to four R-R intervals per second, the band motion/rest state
//! and a skin-temperature channel (see `protocol::R18HistoryFields`). These helpers compute:
//!
//! - `hrv_rmssd_ms`: RMSSD over truly successive beats while the band reports STILL or SLEEP.
//!   R18 does not carry every beat, so successive differences are only taken inside runs of
//!   consecutive seconds that all carry R-R intervals, and a run is split at any interval
//!   outside 300–2000 ms or differing from the previous accepted beat by more than 20 %.
//! - `skin_temperature_delta_c`: median skin temperature during SLEEP minus the median of the
//!   previous nights' medians (at least three nights, up to thirty).
//!
//! No value is produced when the inputs are insufficient; callers report the blocker instead.

use serde::Deserialize;

use crate::{GooseResult, store::GooseStore};

pub const K18_HRV_SOURCE: &str = "whoop_lab.k18_sleep_rr_rmssd";
pub const K18_SKIN_TEMPERATURE_SOURCE: &str = "whoop_lab.k18_sleep_skin_temperature_delta";

const MIN_SUCCESSIVE_DIFFERENCES: usize = 100;
const MIN_SEGMENT_BEATS: usize = 5;
const MAX_RELATIVE_CHANGE: f64 = 0.20;
const MIN_SLEEP_TEMPERATURE_SAMPLES: usize = 1_800;
const MIN_BASELINE_NIGHTS: usize = 3;
const MAX_BASELINE_NIGHTS: i64 = 30;
const DAY_SECONDS: i64 = 86_400;

#[derive(Debug, Clone, Deserialize)]
struct R18Json {
    #[serde(default)]
    rr_intervals_ms: Vec<u16>,
    #[serde(default)]
    skin_temperature_centi_c: Option<u16>,
    #[serde(default)]
    band_state: String,
}

#[derive(Debug, Clone)]
struct Record {
    seconds: i64,
    frame_id: String,
    r18: R18Json,
}

#[derive(Debug, Clone, PartialEq)]
pub struct K18RecoveryValue {
    pub value: f64,
    pub sample_count: usize,
    pub value_source: &'static str,
    /// First and last contributing frame ids (the full list would be too large to store).
    pub input_ids: Vec<String>,
}

fn records(store: &GooseStore, start_seconds: i64, end_seconds: i64) -> GooseResult<Vec<Record>> {
    Ok(store
        .k18_history_records(start_seconds, end_seconds)?
        .into_iter()
        .filter_map(|(seconds, frame_id, json)| {
            serde_json::from_str::<R18Json>(&json)
                .ok()
                .map(|r18| Record { seconds, frame_id, r18 })
        })
        .collect())
}

fn is_rest_state(state: &str) -> bool {
    state == "still" || state == "sleep"
}

fn bounding_ids(ids: &[&str]) -> Vec<String> {
    match (ids.first(), ids.last()) {
        (Some(first), Some(last)) if first != last => vec![first.to_string(), last.to_string()],
        (Some(first), _) => vec![first.to_string()],
        _ => Vec::new(),
    }
}

/// RMSSD (ms) over successive beats recorded while the band was at rest, or None if fewer than
/// `MIN_SUCCESSIVE_DIFFERENCES` clean successive differences are available.
fn rest_rmssd(records: &[Record]) -> Option<(f64, usize, Vec<String>)> {
    let mut squared_sum = 0.0;
    let mut difference_count = 0usize;
    let mut used_ids = Vec::new();
    let mut segment: Vec<f64> = Vec::new();
    let mut previous_second: Option<i64> = None;

    let flush = |segment: &mut Vec<f64>, squared_sum: &mut f64, count: &mut usize| {
        if segment.len() >= MIN_SEGMENT_BEATS {
            for pair in segment.windows(2) {
                let delta = pair[1] - pair[0];
                *squared_sum += delta * delta;
                *count += 1;
            }
        }
        segment.clear();
    };

    for record in records {
        let continuous = previous_second.is_some_and(|previous| record.seconds == previous + 1);
        if !is_rest_state(&record.r18.band_state) || record.r18.rr_intervals_ms.is_empty() || !continuous {
            flush(&mut segment, &mut squared_sum, &mut difference_count);
        }
        previous_second = Some(record.seconds);
        if !is_rest_state(&record.r18.band_state) {
            continue;
        }
        let mut contributed = false;
        for &interval in &record.r18.rr_intervals_ms {
            let interval = f64::from(interval);
            let plausible = (300.0..=2000.0).contains(&interval);
            let stable = segment
                .last()
                .is_none_or(|last| ((interval - last) / last).abs() <= MAX_RELATIVE_CHANGE);
            if !plausible {
                flush(&mut segment, &mut squared_sum, &mut difference_count);
                continue;
            }
            if !stable {
                flush(&mut segment, &mut squared_sum, &mut difference_count);
            }
            segment.push(interval);
            contributed = true;
        }
        if contributed {
            used_ids.push(record.frame_id.as_str());
        }
    }
    flush(&mut segment, &mut squared_sum, &mut difference_count);

    if difference_count < MIN_SUCCESSIVE_DIFFERENCES {
        return None;
    }
    let rmssd = (squared_sum / difference_count as f64).sqrt();
    Some((rmssd, difference_count, bounding_ids(&used_ids)))
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn sleep_skin_temperature(records: &[Record]) -> Option<(f64, usize, Vec<String>)> {
    let samples = records
        .iter()
        .filter(|record| record.r18.band_state == "sleep")
        .filter_map(|record| {
            record
                .r18
                .skin_temperature_centi_c
                .map(|raw| (f64::from(raw) / 100.0, record.frame_id.as_str()))
        })
        .collect::<Vec<_>>();
    if samples.len() < MIN_SLEEP_TEMPERATURE_SAMPLES {
        return None;
    }
    let ids = samples.iter().map(|(_, id)| *id).collect::<Vec<_>>();
    let count = samples.len();
    median(samples.iter().map(|(value, _)| *value).collect()).map(|value| (value, count, bounding_ids(&ids)))
}

/// Value for `metric_id` in the day window [start_ms, end_ms), or Ok(None) when R18 inputs are
/// insufficient. Only `hrv_rmssd_ms` and `skin_temperature_delta_c` are handled.
pub fn k18_recovery_value(
    store: &GooseStore,
    metric_id: &str,
    start_ms: i64,
    end_ms: i64,
) -> GooseResult<Option<K18RecoveryValue>> {
    let start_seconds = start_ms.div_euclid(1000);
    let end_seconds = end_ms.div_euclid(1000);
    match metric_id {
        "hrv_rmssd_ms" => {
            let day = records(store, start_seconds, end_seconds)?;
            Ok(rest_rmssd(&day).map(|(rmssd, count, ids)| K18RecoveryValue {
                value: (rmssd * 10.0).round() / 10.0,
                sample_count: count,
                value_source: K18_HRV_SOURCE,
                input_ids: ids,
            }))
        }
        "skin_temperature_delta_c" => {
            let day = records(store, start_seconds, end_seconds)?;
            let Some((tonight, count, ids)) = sleep_skin_temperature(&day) else {
                return Ok(None);
            };
            let mut baseline_nights = Vec::new();
            for nights_back in 1..=MAX_BASELINE_NIGHTS {
                let night_start = start_seconds - nights_back * DAY_SECONDS;
                let night = records(store, night_start, night_start + DAY_SECONDS)?;
                if let Some((value, _, _)) = sleep_skin_temperature(&night) {
                    baseline_nights.push(value);
                }
            }
            if baseline_nights.len() < MIN_BASELINE_NIGHTS {
                return Ok(None);
            }
            let baseline = median(baseline_nights).expect("baseline nights checked");
            Ok(Some(K18RecoveryValue {
                value: ((tonight - baseline) * 100.0).round() / 100.0,
                sample_count: count,
                value_source: K18_SKIN_TEMPERATURE_SOURCE,
                input_ids: ids,
            }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seconds: i64, rr: &[u16], state: &str) -> Record {
        Record {
            seconds,
            frame_id: format!("f{seconds}"),
            r18: R18Json {
                rr_intervals_ms: rr.to_vec(),
                skin_temperature_centi_c: Some(3300),
                band_state: state.to_string(),
            },
        }
    }

    #[test]
    fn rmssd_uses_only_successive_beats_at_rest() {
        // 120 consecutive seconds alternating 1000/1040 ms: every successive difference is 40 ms.
        let mut records = (0..120)
            .map(|second| record(second, &[if second % 2 == 0 { 1000 } else { 1040 }], "sleep"))
            .collect::<Vec<_>>();
        // A gap, then an artifact and awake data that must not contribute.
        records.push(record(200, &[600], "sleep"));
        records.push(record(201, &[1000], "wake"));
        let (rmssd, count, _) = rest_rmssd(&records).expect("enough beats");
        assert_eq!(count, 119);
        assert!((rmssd - 40.0).abs() < 1e-9);
    }

    #[test]
    fn rmssd_requires_enough_differences() {
        let records = (0..50).map(|second| record(second, &[1000], "still")).collect::<Vec<_>>();
        assert!(rest_rmssd(&records).is_none());
    }

    #[test]
    fn awake_data_gives_no_resting_rmssd() {
        let records = (0..500).map(|second| record(second, &[1000], "wake")).collect::<Vec<_>>();
        assert!(rest_rmssd(&records).is_none());
    }
}
