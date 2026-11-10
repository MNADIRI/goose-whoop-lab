"""Import raw WHOOP 5.0 frames exported by the patched goose app (WhoopLabAutoExport) into the lab database.

Chunks are `raw/<stamp>-r<first>-<last>.tsv.deflate` files: raw DEFLATE of TSV lines
`rowid \t evidence_id \t captured_at \t payload_hex`. Importing is idempotent (keyed on evidence_id),
so the same chunk or overlapping chunks can be imported any number of times.

Usage:
    python3 import_raw.py [--source DIR] [--db PATH]

DIR defaults to ~/Library/Mobile Documents/com~apple~CloudDocs/whoop-lab (the iCloud Drive folder
picked in goose → Connection → whoop-lab). PATH defaults to data/whoop-lab.sqlite next to this script.
"""

from __future__ import annotations

import argparse
import sqlite3
import sys
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_SOURCE = Path.home() / "Library/Mobile Documents/com~apple~CloudDocs/whoop-lab"
DEFAULT_DB = HERE / "data" / "whoop-lab.sqlite"

SCHEMA = """
CREATE TABLE IF NOT EXISTS raw_frames (
    evidence_id TEXT PRIMARY KEY,
    source_rowid INTEGER NOT NULL,
    captured_at TEXT NOT NULL,
    packet_type INTEGER,
    frame BLOB NOT NULL,
    chunk TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS raw_frames_type_time ON raw_frames (packet_type, captured_at);
CREATE TABLE IF NOT EXISTS imported_chunks (
    name TEXT PRIMARY KEY,
    rows INTEGER NOT NULL,
    inserted INTEGER NOT NULL,
    imported_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
"""


def open_db(path: Path) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(path)
    db.executescript(SCHEMA)
    return db


def read_chunk(path: Path) -> list[tuple[int, str, str, bytes]]:
    text = zlib.decompress(path.read_bytes(), -zlib.MAX_WBITS).decode("utf-8")
    rows = []
    for line in text.splitlines():
        if not line:
            continue
        rowid, evidence_id, captured_at, payload_hex = line.split("\t")
        rows.append((int(rowid), evidence_id, captured_at, bytes.fromhex(payload_hex)))
    return rows


def import_chunk(db: sqlite3.Connection, path: Path) -> tuple[int, int]:
    rows = read_chunk(path)
    before = db.total_changes
    db.executemany(
        "INSERT OR IGNORE INTO raw_frames (evidence_id, source_rowid, captured_at, packet_type, frame, chunk)"
        " VALUES (?, ?, ?, ?, ?, ?)",
        [(eid, rid, at, frame[8] if len(frame) > 8 else None, frame, path.name) for rid, eid, at, frame in rows],
    )
    inserted = db.total_changes - before
    db.execute(
        "INSERT OR REPLACE INTO imported_chunks (name, rows, inserted) VALUES (?, ?, ?)",
        (path.name, len(rows), inserted),
    )
    db.commit()
    return len(rows), inserted


def import_all(source: Path, db_path: Path) -> dict:
    raw_dir = source / "raw"
    if not raw_dir.is_dir():
        raise SystemExit(f"no raw/ folder in {source}")
    db = open_db(db_path)
    done = {name for (name,) in db.execute("SELECT name FROM imported_chunks")}
    # iCloud keeps not-yet-downloaded files as hidden ".name.icloud" placeholders.
    pending_download = sorted(p.name for p in raw_dir.glob(".*.icloud"))
    summary = {"chunks": 0, "rows": 0, "inserted": 0, "skipped": 0, "pending_download": len(pending_download)}
    for path in sorted(raw_dir.glob("*.tsv.deflate")):
        if path.name in done:
            summary["skipped"] += 1
            continue
        rows, inserted = import_chunk(db, path)
        summary["chunks"] += 1
        summary["rows"] += rows
        summary["inserted"] += inserted
    summary["total_frames"] = db.execute("SELECT COUNT(*) FROM raw_frames").fetchone()[0]
    summary["history_frames"] = db.execute("SELECT COUNT(*) FROM raw_frames WHERE packet_type = 47").fetchone()[0]
    db.close()
    return summary


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", type=Path, default=DEFAULT_SOURCE)
    parser.add_argument("--db", type=Path, default=DEFAULT_DB)
    args = parser.parse_args(argv)
    summary = import_all(args.source, args.db)
    print(" ".join(f"{key}={value}" for key, value in summary.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
