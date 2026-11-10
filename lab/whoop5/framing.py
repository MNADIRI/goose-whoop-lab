"""WHOOP 5.0 / MG BLE frame integrity and splitting.

Frame layout (all offsets absolute in the reassembled frame, little-endian):

    0      0xAA   start of frame
    1      0x01   format 1
    2..3   u16    declared length = len(frame) - 8
    4..5   u16    CRC16-Modbus over frame[0:6]... stored at 6..7
    8      u8     packet type
    9      u8     layout (for type 47 history records) / sub-type
    ...
    -4..   u32    CRC32 over frame[8:-4]

The header CRC16 covers bytes [0,6) and is stored at [6,8). The body CRC32 covers
[8, len-4) and is stored in the last four bytes.
"""

from __future__ import annotations

import struct
import zlib
from dataclasses import dataclass

SOF = 0xAA
FORMAT_1 = 0x01
HEADER_LEN = 8
TRAILER_LEN = 4


def crc16_modbus(data: bytes) -> int:
    crc = 0xFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            if crc & 1:
                crc = (crc >> 1) ^ 0xA001
            else:
                crc >>= 1
    return crc


@dataclass(frozen=True)
class FrameCheck:
    frame: bytes
    packet_type: int | None
    layout: int | None
    declared_len: int | None
    header_crc_ok: bool
    body_crc_ok: bool
    length_ok: bool

    @property
    def ok(self) -> bool:
        return self.header_crc_ok and self.body_crc_ok and self.length_ok


def check_frame(frame: bytes) -> FrameCheck:
    if len(frame) < HEADER_LEN + 1 + TRAILER_LEN or frame[0] != SOF or frame[1] != FORMAT_1:
        return FrameCheck(frame, None, None, None, False, False, False)
    declared = struct.unpack_from("<H", frame, 2)[0]
    header_ok = crc16_modbus(frame[:6]) == struct.unpack_from("<H", frame, 6)[0]
    length_ok = declared == len(frame) - HEADER_LEN
    body_ok = False
    if length_ok:
        body_ok = zlib.crc32(frame[HEADER_LEN:-TRAILER_LEN]) == struct.unpack_from("<I", frame, len(frame) - 4)[0]
    return FrameCheck(frame, frame[8], frame[9], declared, header_ok, body_ok, length_ok)


def split_frames(data: bytes) -> list[bytes]:
    """Split a buffer holding one or more back-to-back frames (BLE notifications may concatenate)."""
    frames: list[bytes] = []
    offset = 0
    while offset + HEADER_LEN <= len(data):
        if data[offset] != SOF or data[offset + 1] != FORMAT_1:
            offset += 1
            continue
        declared = struct.unpack_from("<H", data, offset + 2)[0]
        end = offset + HEADER_LEN + declared
        if end > len(data):
            break
        frames.append(bytes(data[offset:end]))
        offset = end
    return frames
