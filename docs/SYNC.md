# History sync

## How it works

The strap records everything in flash pages (R18 every second, events, R26, R20…). A sync is:

1. `GET_DATA_RANGE` (34) → pointers, as u32 words from body offset 1:
   `[w0, read, newest, acked, tag, …]`. Healthy: `read == acked`.
2. `SEND_HISTORICAL_DATA` (22) → the strap streams pages: `HistoryStart`, ~45 packets, `HistoryEnd`.
3. After each `HistoryEnd` the app sends `HISTORICAL_DATA_RESULT` (23) `01 <page u32> <tag u32>`;
   the strap answers with the next page, then `HistoryComplete`.

Syncs run every 5 min while connected (`WhoopLabAutoExport`), every 60 s during a sport watch,
every 45-300 s around a Smart Wake window, and on foreground when the data is over 3 min old.

Throughput (20 Nov, 10.5 h backlog): 40 862 packets in 7 min, 900 pages of ~45 packets,
0.32 s per page, 38 ms between pages, i.e. about 40 s per hour of data. A 5 min sync in the
background takes ~35 s (the strap sends ~10 packets/s on a relaxed connection).

## The stuck transfer (13 Nov, 19→20 Nov)

| night | start | end | failures | ended by |
|---|---|---|---|---|
| 12→13 Nov | 00:17 | 09:34 | 52 | strap, by itself |
| 17 Nov | 11:41 | 12:58 | 79 | app relaunch |
| 19→20 Nov | 23:42 | 10:34 | 632 | app relaunch (new notifications + hello) |

Signature: `GET_DATA_RANGE` is answered (or times out for a few minutes), `SEND_HISTORICAL_DATA`
is answered SUCCESS, then at most a `HistoryStart` and nothing; `read = acked + 1` and stays there
while `newest` keeps growing. On 19 Nov it began 4 s after a `HistoryComplete`, when a sport watch
asked for a new sync immediately. The old code retried every minute for ten hours: no data, the
phone (already at 20 %, Low Power Mode) fell to 1 %, and the whole night arrived at 10:34.

## Recovery (from 20 Nov)

`GooseBLEClient+SyncRecovery.swift`, by failures in a row (repeating every 8):

1. retry in 20 s
2. re-acknowledge the page the strap holds as acknowledged (`01 <acked> <tag>`, idempotent)
3. fresh session: WHOOP notifications off/on + client hello
4. reconnect (the sync runs as soon as the link is ready)
5. to 8: retries spaced 2 / 5 / 10 / 10 min

Periodic callers wait during the back-off (`canStartPeriodicSync`) and never start a sync less
than 15 s after the previous one. Back in the foreground, a failing strap gets a fresh session and
a sync at once. Each recovery is logged as `historical_sync.recovered … steps=…`, which tells
which step clears the strap the next time it happens.

## Diagnostic log

`Documents/GooseSwift/goose-ble-live.log` is written in 64 KB / 5 s batches through one open
handle (it used to open, write, fsync and close per line, which drew an iOS `diskwrites_resource`
report) and rotates at 16 MB to `goose-ble-live.prev.log`. A device copy stops at 40 MB, so the
old unbounded file could no longer be read.

## Background CPU, memory and writes (20 Nov)

Measured on the iPhone, app in the background, phone locked, 6 min with two syncs:

| | before | after |
|---|---|---|
| CPU | 8.2 s / 330 s, killed at 90 % for 53 s (11:03) | 3.4 s / 360 s (~1 %) |
| memory footprint | 390-667 MB (largest process in jetsam events) | 62-75 MB |
| writes per sync (~370 packets) | ~10 MB (1.07 GB in 296 s for the morning backlog) | ~0.2 MB |

What changed:
- the recovery rollup read R18 by `json_extract` over every decoded frame, once per baseline
  night (48 s of CPU that continued into the background): it reads `whoop5_r18` by `ts` now;
- analysis requests are interruptible: their SQLite queries stop when the app reports the
  background (`app.set_background`, `Rust/core/src/interrupt.rs`); ingest is never interrupted;
- windowed decoded-frame reads use two new indexes (`raw_evidence(captured_at)`,
  `decoded_frames(evidence_id)`) instead of a full scan and an on-disk sort of every row, and the
  window is cached 60 s while no frame arrives (about ten reports read it per refresh);
- the R18 re-parse runs once; the schema migration once per file and process;
- frames are committed in 1 s batches (one transaction per frame before);
- the heart-rate / HRV series files (10 + 2 MB) are rewritten every 30 min instead of 5, the prune
  no longer scans or shifts 100k samples per beat, the resting-HR window is sorted once a minute;
- the SwiftUI tree is replaced by an empty view 3 s after going to the background (live values kept
  re-rendering the Today screen); the sport engine runs every 2 min when idle in the background.

## Foreground metrics refresh (20 Nov)

The packet-derived inputs refresh (19 bridge requests) logs its time and the memory each step adds
(`health.packet_inputs run … memory: …` in the live log). On the iPhone:

| | before | after |
|---|---|---|
| duration | 26.1 s | 13.8 s |
| peak footprint | 1 398 MB (vital events +861 MB, heart rate +273 MB) | 370 MB |

- the vital-event report kept two inputs per R18 second (~90k objects): the lists now hold the
  latest 256 plus every resolved and verified input, with counts, flags and signals computed over
  all of them (`VitalInputSummary`);
- the decoded window and the capture correlation are shared (`Arc`) between the reports of a run
  and dropped when it ends (`cache.clear`);
- arrays over 200 entries are cut to their first and last 50 before crossing the bridge (the app
  reads counts, the first feature and short daily lists);
- the heart-rate feature report, read by no screen, is no longer requested;
- the refresh after a sync runs at most every 15 min (was 5).

The first sync after a launch still writes ~10-15 MB (catch-up work run once per launch); later
background syncs ~0.2 MB.
