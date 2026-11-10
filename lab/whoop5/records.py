"""Decoders for the WHOOP 5.0 / MG records the strap emits.

Offsets follow the NOOP WHOOP 5/MG sensor contract (docs/PROTOCOL_SENSORS.md, firmware 50.42.1.0)
and were checked against frames from firmware 50.38.1.0. Fields whose physical meaning is not
established are kept raw and named `*_raw`; nothing is silently turned into a measurement.

Record layouts carried by packet type 47 (historical data), selected by frame byte 9:
    18  R18 biometric summary (124 B, 1 Hz): HR, up to 4 R-R, gravity, steps, temperature, band state
    20  R20 optical blocks (2140 B): 5 blocks x 2 slots x <=50 raw optical counts
    21  R21 six-axis IMU (1244 B): 100 Hz accelerometer + gyroscope
    26  R26 compact optical window (88 B): base + 24 deltas

Other packet types: 40 live HR/R-R, 48 strap events, 49 history metadata, 36 command responses,
50 console logs, 51/52 dedicated IMU streams (structure only).
"""

from __future__ import annotations

import struct
from dataclasses import asdict, dataclass, field
from typing import Any

PACKET_COMMAND = 35
PACKET_COMMAND_RESPONSE = 36
PACKET_REALTIME = 40
PACKET_HISTORY = 47
PACKET_EVENT = 48
PACKET_METADATA = 49
PACKET_CONSOLE = 50
PACKET_IMU_LIVE = 51
PACKET_IMU_HISTORY = 52

LAYOUT_R18 = 18
LAYOUT_R20 = 20
LAYOUT_R21 = 21
LAYOUT_R26 = 26

EVENT_NAMES = {
    0: "UNDEFINED", 1: "ERROR", 2: "CONSOLE_OUTPUT", 3: "BATTERY_LEVEL", 4: "SYSTEM_CONTROL",
    7: "CHARGING_ON", 8: "CHARGING_OFF", 9: "WRIST_ON", 10: "WRIST_OFF", 11: "BLE_CONNECTION_UP",
    12: "BLE_CONNECTION_DOWN", 13: "RTC_LOST", 14: "DOUBLE_TAP", 15: "BOOT", 16: "SET_RTC",
    17: "TEMPERATURE_LEVEL", 18: "PAIRING_MODE", 28: "FLASH_INIT_COMPLETE", 29: "STRAP_CONDITION_REPORT",
    33: "BLE_REALTIME_HR_ON", 34: "BLE_REALTIME_HR_OFF", 56: "STRAP_DRIVEN_ALARM_SET",
    57: "STRAP_DRIVEN_ALARM_EXECUTED", 58: "APP_DRIVEN_ALARM_EXECUTED", 59: "STRAP_DRIVEN_ALARM_DISABLED",
    60: "HAPTICS_FIRED", 63: "EXTENDED_BATTERY_INFORMATION", 96: "HIGH_FREQ_SYNC_PROMPT",
    97: "HIGH_FREQ_SYNC_ENABLED", 98: "HIGH_FREQ_SYNC_DISABLED", 100: "HAPTICS_TERMINATED",
    109: "BATTERY_PACK_INFO", 123: "GENERIC_FIRMWARE_EVENT",
}

METADATA_KINDS = {1: "HISTORY_START", 2: "HISTORY_END", 3: "HISTORY_COMPLETE"}
BAND_STATES = ("wake", "still", "sleep", "up")
ACTIVITY_CLASSES = {0: "still", 1: "walk", 2: "run"}

# Earliest device timestamp accepted as a real clock reading (2020-01-01).
MIN_PLAUSIBLE_TS = 1_577_836_800


def rr_ticks_to_ms(ticks: int) -> int:
    return (ticks * 1000 + 512) // 1024


def _u8(b: bytes, o: int) -> int:
    return b[o]


def _u16(b: bytes, o: int) -> int:
    return struct.unpack_from("<H", b, o)[0]


def _i16(b: bytes, o: int) -> int:
    return struct.unpack_from("<h", b, o)[0]


def _u32(b: bytes, o: int) -> int:
    return struct.unpack_from("<I", b, o)[0]


def _i32(b: bytes, o: int) -> int:
    return struct.unpack_from("<i", b, o)[0]


def _f32(b: bytes, o: int) -> float:
    return struct.unpack_from("<f", b, o)[0]


@dataclass
class R18:
    """One second of the biometric summary stream."""

    ts: int
    record_index: int
    hr: int
    rr_ms: list[int]
    cardiac_flags_raw: int
    hr_quality_raw: int
    alt_hr: int
    rr_adjacent_raw: int
    dyn_accel_g: float | None
    grav_x: float | None
    grav_y: float | None
    grav_z: float | None
    steps: int
    steps_hw: int | None
    cadence_raw: int
    activity: str | None
    activity_raw: int
    counter_override: bool
    aux_temp1_c: float | None
    aux_temp2_c: float | None
    skin_temp_c: float | None
    status_raw: tuple[int, int, int]
    wrist_bits: int
    wake_quality_bits: int
    band_state: str
    extra_state_bits: int
    sleep_adjacent_raw: int
    optical_baseline_raw: tuple[int, int]
    optical_amplitude_raw: tuple[int, int]
    unknown_f32: float | None
    warnings: list[str] = field(default_factory=list)

    def as_row(self) -> dict[str, Any]:
        row = asdict(self)
        row.pop("warnings")
        row["rr_ms"] = ",".join(str(v) for v in self.rr_ms)
        for key in ("status_raw", "optical_baseline_raw", "optical_amplitude_raw"):
            row[key] = ",".join(str(v) for v in row[key])
        return row


def _finite(value: float) -> float | None:
    return value if value == value and abs(value) != float("inf") else None


def decode_r18(frame: bytes) -> R18:
    if len(frame) != 124 or frame[9] != LAYOUT_R18:
        raise ValueError("not a 124-byte R18 frame")
    warnings: list[str] = []
    rr_count = frame[23]
    if rr_count > 4:
        warnings.append(f"rr_count_{rr_count}_clamped_to_4")
    rr_ms = [rr_ticks_to_ms(_u16(frame, 24 + 2 * i)) for i in range(min(rr_count, 4))]
    rr_ms = [v for v in rr_ms if v > 0]
    override = frame[64] == 0x10
    dyn = _finite(_f32(frame, 41))
    if dyn is not None and not 0 <= dyn <= 8:
        warnings.append("dyn_accel_out_of_range")
        dyn = None
    aux1 = _i16(frame, 69) / 10
    aux2 = _i16(frame, 71) / 10
    skin_raw = _u16(frame, 73)
    skin = skin_raw / 100 if 500 <= skin_raw <= 4500 else None
    activity_raw = frame[63]
    bits = frame[81]
    return R18(
        ts=_u32(frame, 15),
        record_index=_u32(frame, 11),
        hr=frame[22],
        rr_ms=rr_ms,
        cardiac_flags_raw=frame[33],
        hr_quality_raw=frame[36],
        alt_hr=frame[37],
        rr_adjacent_raw=_u16(frame, 38),
        dyn_accel_g=dyn,
        grav_x=_finite(_f32(frame, 45)),
        grav_y=_finite(_f32(frame, 49)),
        grav_z=_finite(_f32(frame, 53)),
        steps=_u16(frame, 57),
        steps_hw=_u16(frame, 61) if override else None,
        cadence_raw=_u16(frame, 59),
        activity=ACTIVITY_CLASSES.get(activity_raw),
        activity_raw=activity_raw,
        counter_override=override,
        aux_temp1_c=aux1 if 0 <= aux1 <= 60 else None,
        aux_temp2_c=aux2 if 0 <= aux2 <= 60 else None,
        skin_temp_c=skin,
        status_raw=(_u16(frame, 75), _u16(frame, 77), _u16(frame, 79)),
        wrist_bits=bits & 3,
        wake_quality_bits=(bits >> 2) & 3,
        band_state=BAND_STATES[(bits >> 4) & 3],
        extra_state_bits=(bits >> 6) & 3,
        sleep_adjacent_raw=frame[82],
        optical_baseline_raw=(frame[106], frame[107]),
        optical_amplitude_raw=(frame[108], frame[109]),
        unknown_f32=_finite(_f32(frame, 113)),
        warnings=warnings,
    )


@dataclass
class R26:
    """Compact optical window: an absolute first code and 24 adjacent deltas."""

    ts: int
    record_index: int
    time_word_raw: int
    burst: int
    samples: list[int]
    motion_f32: float | None
    status_raw: int
    on_wrist_raw: int
    acceptance_raw: int

    def as_row(self) -> dict[str, Any]:
        row = asdict(self)
        row["samples"] = ",".join(str(v) for v in self.samples)
        return row


def decode_r26(frame: bytes) -> R26:
    if len(frame) != 88 or frame[9] != LAYOUT_R26:
        raise ValueError("not an 88-byte R26 frame")
    samples = [_i32(frame, 23)]  # signed base (a real burst had 0xffff8731 = -30929)
    for i in range(24):
        samples.append(samples[-1] + _i16(frame, 27 + 2 * i))
    return R26(
        ts=_u32(frame, 15),
        record_index=_u32(frame, 11),
        time_word_raw=_u16(frame, 19),
        burst=_u16(frame, 21),
        samples=samples,
        motion_f32=_finite(_f32(frame, 75)),
        status_raw=_u16(frame, 79),
        on_wrist_raw=frame[81],
        acceptance_raw=frame[82],
    )


@dataclass
class R20Block:
    count: int
    source_a_raw: int
    drive_a_raw: int
    source_b_raw: int
    drive_b_raw: int
    detector_a_raw: int
    range_a_raw: int
    offset_a_raw: int
    detector_b_raw: int
    range_b_raw: int
    offset_b_raw: int
    slot_a: list[int]
    slot_b: list[int]


@dataclass
class R20:
    ts: int
    record_index: int
    marker_raw: int
    blocks: list[R20Block]


def decode_r20(frame: bytes) -> R20:
    if len(frame) != 2140 or frame[9] != LAYOUT_R20:
        raise ValueError("not a 2140-byte R20 frame")
    blocks = []
    for base in (26, 448, 870, 1292, 1714):
        count = frame[base]
        if count > 50:
            raise ValueError(f"R20 block count {count} exceeds 50")
        blocks.append(
            R20Block(
                count=count,
                source_a_raw=frame[base + 1],
                drive_a_raw=_u16(frame, base + 2),
                source_b_raw=frame[base + 4],
                drive_b_raw=_u16(frame, base + 5),
                detector_a_raw=frame[base + 7],
                range_a_raw=_u32(frame, base + 8),
                offset_a_raw=_i16(frame, base + 12),
                detector_b_raw=frame[base + 14],
                range_b_raw=_u32(frame, base + 15),
                offset_b_raw=_i16(frame, base + 19),
                slot_a=[_i32(frame, base + 21 + 4 * i) for i in range(count)],
                slot_b=[_i32(frame, base + 221 + 4 * i) for i in range(count)],
            )
        )
    return R20(ts=_u32(frame, 15), record_index=_u32(frame, 11), marker_raw=frame[10], blocks=blocks)


@dataclass
class R21:
    """Six-axis IMU, 100 Hz buffer. accel in raw/4096 g, gyro in raw*2000/32768 deg/s (earlier convention)."""

    ts: float
    record_index: int
    marker_raw: int
    accel_raw: list[tuple[int, int, int]]
    gyro_raw: list[tuple[int, int, int]]

    @property
    def accel_g(self) -> list[tuple[float, float, float]]:
        return [(x / 4096, y / 4096, z / 4096) for x, y, z in self.accel_raw]

    @property
    def gyro_dps(self) -> list[tuple[float, float, float]]:
        k = 2000 / 32768
        return [(x * k, y * k, z * k) for x, y, z in self.gyro_raw]


def decode_r21(frame: bytes) -> R21:
    if len(frame) != 1244 or frame[9] != LAYOUT_R21:
        raise ValueError("not a 1244-byte R21 frame")
    a_count = min(_u16(frame, 24), 100)
    g_count = min(_u16(frame, 630), 100)
    accel = [(_i16(frame, 28 + 2 * i), _i16(frame, 228 + 2 * i), _i16(frame, 428 + 2 * i)) for i in range(a_count)]
    gyro = [(_i16(frame, 640 + 2 * i), _i16(frame, 840 + 2 * i), _i16(frame, 1040 + 2 * i)) for i in range(g_count)]
    return R21(
        ts=_u32(frame, 15) + _u16(frame, 19) / 32768,
        record_index=_u32(frame, 11),
        marker_raw=frame[10],
        accel_raw=accel,
        gyro_raw=gyro,
    )


@dataclass
class LiveHR:
    ts: int
    time_word_raw: int
    hr: int
    rr_ms: list[int]


def decode_live_hr(frame: bytes) -> LiveHR:
    if frame[8] != PACKET_REALTIME or len(frame) < 22:
        raise ValueError("not a packet-40 frame")
    declared = frame[17]
    available = (len(frame) - 4 - 18) // 2
    rr = [rr_ticks_to_ms(_u16(frame, 18 + 2 * i)) for i in range(min(declared, available))]
    return LiveHR(ts=_u32(frame, 10), time_word_raw=_u16(frame, 14), hr=frame[16], rr_ms=[v for v in rr if v > 0])


@dataclass
class StrapEvent:
    ts: int
    subseconds_raw: int
    event_id: int
    name: str
    data: bytes
    decoded: dict[str, Any]


def decode_event(frame: bytes) -> StrapEvent:
    if frame[8] != PACKET_EVENT or len(frame) < 24:
        raise ValueError("not a packet-48 frame")
    event_id = _u16(frame, 10)
    data = frame[20:-4]
    decoded: dict[str, Any] = {}
    if event_id == 3 and len(data) >= 9:
        # Observed as 1 status byte then two u32 words; the second tracks the pack voltage in mV.
        decoded = {"status_raw": data[0], "word1_raw": _u32(data, 1), "voltage_mv_candidate": _u32(data, 5)}
    elif event_id == 17 and len(data) >= 2:
        decoded = {"level_raw": _u16(data, 0)}
    return StrapEvent(
        ts=_u32(frame, 12),
        subseconds_raw=_u16(frame, 16),
        event_id=event_id,
        name=EVENT_NAMES.get(event_id, f"EVENT_{event_id}"),
        data=bytes(data),
        decoded=decoded,
    )


@dataclass
class HistoryMetadata:
    kind_raw: int
    kind: str
    body: bytes


def decode_metadata(frame: bytes) -> HistoryMetadata:
    if frame[8] != PACKET_METADATA:
        raise ValueError("not a packet-49 frame")
    kind = frame[10]
    return HistoryMetadata(kind_raw=kind, kind=METADATA_KINDS.get(kind, f"METADATA_{kind}"), body=bytes(frame[11:-4]))


@dataclass
class ConsoleLog:
    text: str
    body: bytes


def decode_console(frame: bytes) -> ConsoleLog:
    body = bytes(frame[10:-4])
    text = body.split(b"\x00", 1)[0].decode("ascii", errors="replace")
    return ConsoleLog(text=text, body=body)


def decode_history_record(frame: bytes) -> R18 | R20 | R21 | R26 | None:
    """Decode a type-47 frame to its record, or None for an unsupported layout."""
    layout = frame[9]
    if layout == LAYOUT_R18 and len(frame) == 124:
        return decode_r18(frame)
    if layout == LAYOUT_R26 and len(frame) == 88:
        return decode_r26(frame)
    if layout == LAYOUT_R20 and len(frame) == 2140:
        return decode_r20(frame)
    if layout == LAYOUT_R21 and len(frame) == 1244:
        return decode_r21(frame)
    return None
