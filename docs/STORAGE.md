# Local storage, data flow and CPU (17 Nov)

## What the database held

3.0 GB on 17 Nov, after 8 days:

| Table | Size | Content |
|---|---|---|
| `decoded_frames` | 1.6 GB (+ 94 MB index) | every frame as hex **and** as parsed JSON |
| `raw_evidence` | 0.75 GB (+ 85 MB index) | the same bytes as hex |
| `whoop5_r18` | 218 MB | 1 Hz history, typed (what the app reads) |
| `sport_imu` | 95 MB | live IMU during sport watches, binary |
| rest | ~150 MB | analyses, sessions, Smart Wake, steps |

The capture layer grew by about **430 MB a day**:
- live IMU frames: 175 MB (already in `sport_imu`);
- R18 history: 116 MB (already in `whoop5_r18`);
- raw bytes: 130 MB.

## Retention (`db_maintenance.rs`, `DatabaseMaintenance.swift`)

| Data | Kept |
|---|---|
| decoded frames | 48 h (the legacy metric features read since yesterday 00:00), and never a frame the typed ingests have not consumed |
| live sensor frames (packet 43) | 6 h |
| raw bytes | 12 h |
| sport IMU | 14 days inside sessions, 2 days outside |
| live HR, Smart Wake nights, legacy step samples | 30 days |
| typed tables (`whoop5_*`, analyses, sessions) | kept |

**How the purge runs:**
- **Bounded passes:** deletes go by rowid ranges, 15–40k rows per pass. The two capture tables have no index between
  them, so a cascading delete would scan 1.6 GB per row.
- **Schedule:** a pass every 30 s while a backlog remains (3 min in the background), then every 10–20 min.
- **Space returned:** each pass gives back up to 16 MB of free pages (`incremental_vacuum`).
- **One-time full VACUUM:** switches the file to incremental mode. It runs in a background processing task while the
  phone charges, with history syncs held for 10 min, and never during a sport session or a Smart Wake night.

**Decoder change:** typed rows are re-derived only from the frames still held. Older typed rows are kept as they are.

**Expected size:** about 0.6–0.9 GB.

**Live IMU frames** no longer go through the capture layer or the generic parse, except in a debug physiology
capture. The sport and Smart Wake engines receive them directly.

**Measured on the phone on 19 Nov:**
- **File size:** 3.0 GB → 810 MB after the night's full VACUUM, now in incremental mode.
- **Capture layer:** frames kept from 48 h back.
- **Daily intake:** no live IMU frames any more; raw bytes 29 MB a day instead of 130 MB.
- **Light pass:** 0.2 s in steady state.

## Transfer

- **After a relaunch by iOS**, every history sync now waits for the strap's notifications to be subscribed again
  (15 s at most), whatever its trigger (restore, periodic or screen). Before, GET_DATA_RANGE went out before them:
  3 timeouts, then 20 / 60 / 180 s of back-off at every relaunch. 19 Nov: the sync starts 1.5 s after the reconnect
  and ends in 4–20 s.
- **FD4B0007** carries unframed diagnostic chunks every hour (~300 B). They are dropped as before, logged at debug
  level.

## CPU (iOS kills a background app above 80 % CPU over 60 s)

| Cause (profiles on the phone) | Share | Change |
|---|---|---|
| Heart-rate series written as JSON every second | 56 % | every 5 min and on leaving the foreground |
| Legacy step discovery + counter ingest after every sync | 70 % of the next profile | discovery removed, ingest every 6 h at most |
| DateFormatter created per call on the Forme screens | 25 % of rendering | cached |
| Sport watch (live IMU) 2–7 h a day, all dropped | stream + parse | ends after 8 min without anything promising, 15 min cool-down: −31 % IMU time on a 9–17 Nov replay, same sessions |
| Live IMU frames parsed to JSON and stored three times | per frame | engines only |
| Legacy packet inputs (step counter ingest > 48 s) started on screen, still running in the background (kill 19 Nov 09:18) | — | ingest removed; every step refuses to start in the background |
| Hourly heart-rate ranges rebuilt over 100k points every 1–2 s, redrawing Aujourd'hui in the background | 24 % of a core in the background | once a minute, on screen only |

Background CPU on 19 Nov (Time Profiler, 90 s, app behind Settings): 21,805 samples → **1,339 (≈1.5 % of a core)**.
The engines (sport, Smart Wake, night optics, database maintenance) now write to `goose-ble-live.log`.

## Development

- `gooseswift://debug-export?hours=6`: time slice of the database in `Documents/GooseSwift/slice.sqlite`.
- `gooseswift://debug-stats?hours=24`: volumes per packet type in `db-stats.json`.
