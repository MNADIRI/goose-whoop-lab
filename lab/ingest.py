"""Ingest WHOOP 5.0 raw frames into typed tables in the lab database.

Pipeline (idempotent, incremental):
    iCloud Drive/whoop-lab/raw/*.tsv.deflate  --import_raw-->  raw_frames
    raw_frames (rowid > decode cursor)         --whoop5------>  typed tables below

Tables written here:
    biometric_1hz     one row per second from R18 (HR, R-R, gravity, steps, temperature, band state)
    rr_intervals      every R-R interval with its device second, from R18 (history) and packet 40 (live)
    live_hr           packet-40 live heart-rate frames
    optical_windows   R26 compact optical windows (25 reconstructed samples)
    optical_r20       R20 raw optical counts (one row per block/slot/sample) when the strap sends them
    imu_100hz         R21 accelerometer/gyroscope samples when the strap sends them
    events            strap events (wrist on/off, charging, battery, taps, BLE, firmware)
    history_metadata  HISTORY_START/END/COMPLETE markers
    frame_log         frames that failed CRC or have an unknown layout (kept for later decoders)
    decode_state      cursor and counters

Usage:
    python3 ingest.py [--source DIR] [--db PATH] [--no-import] [--full]
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import import_raw  # noqa: E402
from whoop5 import (  # noqa: E402
    PACKET_COMMAND_RESPONSE,
    PACKET_CONSOLE,
    PACKET_EVENT,
    PACKET_HISTORY,
    PACKET_METADATA,
    PACKET_REALTIME,
    R18,
    R20,
    R21,
    R26,
    check_frame,
    decode_event,
    decode_history_record,
    decode_live_hr,
    decode_metadata,
)
from whoop5.records import MIN_PLAUSIBLE_TS  # noqa: E402

SCHEMA = """
CREATE TABLE IF NOT EXISTS biometric_1hz (
    ts INTEGER PRIMARY KEY,
    record_index INTEGER,
    hr INTEGER,
    alt_hr INTEGER,
    hr_quality_raw INTEGER,
    cardiac_flags_raw INTEGER,
    rr_count INTEGER,
    rr_ms TEXT,
    rr_adjacent_raw INTEGER,
    dyn_accel_g REAL,
    grav_x REAL, grav_y REAL, grav_z REAL,
    steps INTEGER,
    steps_hw INTEGER,
    cadence_raw INTEGER,
    activity TEXT,
    activity_raw INTEGER,
    counter_override INTEGER,
    aux_temp1_c REAL,
    aux_temp2_c REAL,
    skin_temp_c REAL,
    status_raw TEXT,
    wrist_bits INTEGER,
    wake_quality_bits INTEGER,
    band_state TEXT,
    extra_state_bits INTEGER,
    sleep_adjacent_raw INTEGER,
    optical_baseline_raw TEXT,
    optical_amplitude_raw TEXT,
    unknown_f32 REAL,
    evidence_id TEXT
);
CREATE INDEX IF NOT EXISTS biometric_band ON biometric_1hz (band_state, ts);
CREATE TABLE IF NOT EXISTS rr_intervals (
    ts INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    source TEXT NOT NULL,
    rr_ms INTEGER NOT NULL,
    PRIMARY KEY (ts, idx, source)
);
CREATE TABLE IF NOT EXISTS live_hr (
    evidence_id TEXT PRIMARY KEY,
    captured_at TEXT NOT NULL,
    ts INTEGER,
    time_word_raw INTEGER,
    hr INTEGER,
    rr_ms TEXT
);
CREATE INDEX IF NOT EXISTS live_hr_ts ON live_hr (ts);
CREATE TABLE IF NOT EXISTS optical_windows (
    ts INTEGER NOT NULL,
    record_index INTEGER NOT NULL,
    burst INTEGER,
    time_word_raw INTEGER,
    samples TEXT,
    amplitude INTEGER,
    motion_f32 REAL,
    status_raw INTEGER,
    on_wrist_raw INTEGER,
    acceptance_raw INTEGER,
    evidence_id TEXT,
    PRIMARY KEY (ts, record_index)
);
CREATE TABLE IF NOT EXISTS optical_r20 (
    ts INTEGER NOT NULL,
    record_index INTEGER NOT NULL,
    block INTEGER NOT NULL,
    slot TEXT NOT NULL,
    idx INTEGER NOT NULL,
    value INTEGER NOT NULL,
    PRIMARY KEY (ts, record_index, block, slot, idx)
);
CREATE TABLE IF NOT EXISTS optical_r20_blocks (
    ts INTEGER NOT NULL,
    record_index INTEGER NOT NULL,
    block INTEGER NOT NULL,
    count INTEGER,
    config_json TEXT,
    PRIMARY KEY (ts, record_index, block)
);
CREATE TABLE IF NOT EXISTS imu_100hz (
    ts REAL NOT NULL,
    record_index INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    ax INTEGER, ay INTEGER, az INTEGER,
    gx INTEGER, gy INTEGER, gz INTEGER,
    PRIMARY KEY (record_index, idx)
);
CREATE INDEX IF NOT EXISTS imu_ts ON imu_100hz (ts);
CREATE TABLE IF NOT EXISTS events (
    evidence_id TEXT PRIMARY KEY,
    captured_at TEXT NOT NULL,
    ts INTEGER,
    subseconds_raw INTEGER,
    event_id INTEGER,
    name TEXT,
    data_hex TEXT,
    decoded_json TEXT
);
CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
CREATE TABLE IF NOT EXISTS history_metadata (
    evidence_id TEXT PRIMARY KEY,
    captured_at TEXT NOT NULL,
    kind TEXT,
    body_hex TEXT
);
CREATE TABLE IF NOT EXISTS frame_log (
    evidence_id TEXT PRIMARY KEY,
    captured_at TEXT NOT NULL,
    packet_type INTEGER,
    layout INTEGER,
    length INTEGER,
    reason TEXT
);
CREATE TABLE IF NOT EXISTS decode_state (
    key TEXT PRIMARY KEY,
    value TEXT
);
"""

BIOMETRIC_COLUMNS = (
    "ts", "record_index", "hr", "alt_hr", "hr_quality_raw", "cardiac_flags_raw", "rr_count", "rr_ms",
    "rr_adjacent_raw", "dyn_accel_g", "grav_x", "grav_y", "grav_z", "steps", "steps_hw", "cadence_raw",
    "activity", "activity_raw", "counter_override", "aux_temp1_c", "aux_temp2_c", "skin_temp_c", "status_raw",
    "wrist_bits", "wake_quality_bits", "band_state", "extra_state_bits", "sleep_adjacent_raw",
    "optical_baseline_raw", "optical_amplitude_raw", "unknown_f32", "evidence_id",
)


def open_db(path: Path) -> sqlite3.Connection:
    db = import_raw.open_db(path)
    db.executescript(SCHEMA)
    db.execute("PRAGMA journal_mode = WAL")
    return db


def get_state(db: sqlite3.Connection, key: str, default: str) -> str:
    row = db.execute("SELECT value FROM decode_state WHERE key = ?", (key,)).fetchone()
    return row[0] if row else default


def set_state(db: sqlite3.Connection, key: str, value: str) -> None:
    db.execute("INSERT OR REPLACE INTO decode_state (key, value) VALUES (?, ?)", (key, value))


class Decoder:
    def __init__(self, db: sqlite3.Connection):
        self.db = db
        self.counts: dict[str, int] = {}

    def bump(self, key: str, n: int = 1) -> None:
        self.counts[key] = self.counts.get(key, 0) + n

    def log_frame(self, evidence_id: str, captured_at: str, frame: bytes, reason: str) -> None:
        self.db.execute(
            "INSERT OR IGNORE INTO frame_log VALUES (?, ?, ?, ?, ?, ?)",
            (evidence_id, captured_at, frame[8] if len(frame) > 8 else None, frame[9] if len(frame) > 9 else None, len(frame), reason),
        )
        self.bump(f"log:{reason}")

    def handle(self, evidence_id: str, captured_at: str, frame: bytes) -> None:
        chk = check_frame(frame)
        if not chk.ok:
            self.log_frame(evidence_id, captured_at, frame, "crc_or_length_invalid")
            return
        t = chk.packet_type
        if t == PACKET_HISTORY:
            record = decode_history_record(frame)
            if record is None:
                self.log_frame(evidence_id, captured_at, frame, f"unknown_history_layout_{frame[9]}_len_{len(frame)}")
            elif isinstance(record, R18):
                self.store_r18(record, evidence_id)
            elif isinstance(record, R26):
                self.store_r26(record, evidence_id)
            elif isinstance(record, R20):
                self.store_r20(record)
            elif isinstance(record, R21):
                self.store_r21(record)
        elif t == PACKET_REALTIME:
            live = decode_live_hr(frame)
            self.db.execute(
                "INSERT OR IGNORE INTO live_hr VALUES (?, ?, ?, ?, ?, ?)",
                (evidence_id, captured_at, live.ts, live.time_word_raw, live.hr, ",".join(map(str, live.rr_ms))),
            )
            if live.ts >= MIN_PLAUSIBLE_TS:
                self.db.executemany(
                    "INSERT OR IGNORE INTO rr_intervals VALUES (?, ?, 'live', ?)",
                    [(live.ts, i, rr) for i, rr in enumerate(live.rr_ms)],
                )
            self.bump("live_hr")
        elif t == PACKET_EVENT:
            event = decode_event(frame)
            self.db.execute(
                "INSERT OR IGNORE INTO events VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (evidence_id, captured_at, event.ts, event.subseconds_raw, event.event_id, event.name,
                 event.data.hex(), json.dumps(event.decoded) if event.decoded else None),
            )
            self.bump("events")
        elif t == PACKET_METADATA:
            meta = decode_metadata(frame)
            self.db.execute(
                "INSERT OR IGNORE INTO history_metadata VALUES (?, ?, ?, ?)",
                (evidence_id, captured_at, meta.kind, meta.body.hex()),
            )
            self.bump("metadata")
        elif t in (PACKET_COMMAND_RESPONSE, PACKET_CONSOLE):
            self.bump("command_or_console")
        else:
            self.log_frame(evidence_id, captured_at, frame, f"unhandled_packet_type_{t}")

    def store_r18(self, r: R18, evidence_id: str) -> None:
        if r.ts < MIN_PLAUSIBLE_TS:
            self.bump("r18_implausible_ts")
            return
        row = r.as_row()
        row["rr_count"] = len(r.rr_ms)
        row["evidence_id"] = evidence_id
        row["counter_override"] = int(r.counter_override)
        self.db.execute(
            f"INSERT OR IGNORE INTO biometric_1hz ({', '.join(BIOMETRIC_COLUMNS)}) VALUES ({', '.join('?' * len(BIOMETRIC_COLUMNS))})",
            [row[c] for c in BIOMETRIC_COLUMNS],
        )
        self.db.executemany(
            "INSERT OR IGNORE INTO rr_intervals VALUES (?, ?, 'history', ?)",
            [(r.ts, i, rr) for i, rr in enumerate(r.rr_ms)],
        )
        self.bump("r18")

    def store_r26(self, r: R26, evidence_id: str) -> None:
        self.db.execute(
            "INSERT OR IGNORE INTO optical_windows VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (r.ts, r.record_index, r.burst, r.time_word_raw, ",".join(map(str, r.samples)),
             max(r.samples) - min(r.samples), r.motion_f32, r.status_raw, r.on_wrist_raw, r.acceptance_raw, evidence_id),
        )
        self.bump("r26")

    def store_r20(self, r: R20) -> None:
        for index, block in enumerate(r.blocks):
            config = {k: v for k, v in block.__dict__.items() if k not in ("slot_a", "slot_b")}
            self.db.execute(
                "INSERT OR IGNORE INTO optical_r20_blocks VALUES (?, ?, ?, ?, ?)",
                (r.ts, r.record_index, index, block.count, json.dumps(config)),
            )
            self.db.executemany(
                "INSERT OR IGNORE INTO optical_r20 VALUES (?, ?, ?, ?, ?, ?)",
                [(r.ts, r.record_index, index, "a", i, v) for i, v in enumerate(block.slot_a)]
                + [(r.ts, r.record_index, index, "b", i, v) for i, v in enumerate(block.slot_b)],
            )
        self.bump("r20")

    def store_r21(self, r: R21) -> None:
        n = max(len(r.accel_raw), len(r.gyro_raw))
        rows = []
        for i in range(n):
            a = r.accel_raw[i] if i < len(r.accel_raw) else (None, None, None)
            g = r.gyro_raw[i] if i < len(r.gyro_raw) else (None, None, None)
            rows.append((r.ts + i / 100, r.record_index, i, *a, *g))
        self.db.executemany("INSERT OR IGNORE INTO imu_100hz VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", rows)
        self.bump("r21")


def decode_new_frames(db: sqlite3.Connection, full: bool = False) -> dict[str, int]:
    cursor = 0 if full else int(get_state(db, "raw_frames_cursor", "0"))
    decoder = Decoder(db)
    last = cursor
    batch = db.execute(
        "SELECT rowid, evidence_id, captured_at, frame FROM raw_frames WHERE rowid > ? ORDER BY rowid", (cursor,)
    ).fetchall()
    for rowid, evidence_id, captured_at, frame in batch:
        decoder.handle(evidence_id, captured_at, bytes(frame))
        last = rowid
    set_state(db, "raw_frames_cursor", str(last))
    db.commit()
    decoder.counts["frames"] = len(batch)
    return decoder.counts


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", type=Path, default=import_raw.DEFAULT_SOURCE)
    parser.add_argument("--db", type=Path, default=import_raw.DEFAULT_DB)
    parser.add_argument("--no-import", action="store_true", help="skip the iCloud chunk import")
    parser.add_argument("--full", action="store_true", help="re-decode every raw frame")
    args = parser.parse_args(argv)
    if not args.no_import:
        summary = import_raw.import_all(args.source, args.db)
        print("import:", " ".join(f"{k}={v}" for k, v in summary.items()))
    db = open_db(args.db)
    counts = decode_new_frames(db, full=args.full)
    print("decode:", " ".join(f"{k}={v}" for k, v in sorted(counts.items())))
    for table in ("biometric_1hz", "rr_intervals", "live_hr", "optical_windows", "events", "imu_100hz", "optical_r20", "frame_log"):
        print(f"  {table}: {db.execute(f'SELECT COUNT(*) FROM {table}').fetchone()[0]}")
    db.close()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
