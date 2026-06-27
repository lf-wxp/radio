//! Host-side mirror of the [`crate::presets`] flash codec.
//!
//! The firmware's `read_record` / `write_record` pair reads and
//! writes a tightly-packed binary record into a NOR-flash sector. The
//! `esp-storage` dependency is target-only and we don't want a flash
//! emulator just to sanity-check the byte layout, so this module
//! re-implements the codec in pure Rust against `Vec<u8>` buffers.
//!
//! What we *want* to catch with these tests:
//!
//! * The v1 → v2 lazy migration path actually recovers freqs +
//!   `last_tuned` from a v1 record while leaving the PI / PS metadata
//!   at the all-zero "unknown" sentinel.
//! * v2 records round-trip through encode + decode unchanged, including
//!   PS buffers that contain ASCII spaces (RDS pad characters).
//! * The CRC32 covers the right window — flipping a single payload
//!   byte invalidates the record on read.
//! * Header sanity checks (`MAGIC`, version, slot count) reject the
//!   right things.
//!
//! What we explicitly *don't* try to test here:
//!
//! * Wear-levelling, sector erasure, or any actual flash interaction.
//! * The OTA hand-off (`PausedPresetStore::resume`) — that lives on
//!   top of the flash handle which only exists on-target.

/// Preset slot count must match `crate::state::MAX_PRESETS` on the
/// firmware side. Hard-coded here because host-tests deliberately
/// don't link the firmware crate (it's `no_std` and depends on
/// `esp-hal`); a future bump in `MAX_PRESETS` should be reflected
/// here and in the test fixtures below.
pub const MAX_PRESETS: usize = 8;

/// `b"RPST"` little-endian — must match `presets::MAGIC`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"RPST");

pub const FORMAT_VERSION_V1: u8 = 1;
pub const FORMAT_VERSION_V2: u8 = 2;

const HEADER_SIZE: usize = 12;
const PAYLOAD_V1_SIZE: usize = MAX_PRESETS * 2 + 2;
const PAYLOAD_V2_EXTRA: usize = MAX_PRESETS * 2 + MAX_PRESETS * 8;
const PAYLOAD_V2_SIZE: usize = PAYLOAD_V1_SIZE + PAYLOAD_V2_EXTRA;

/// Decoded preset record, minus everything that's only meaningful on
/// the device (the `last_tuned` debounce instant etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetRecord {
  pub freqs: [u16; MAX_PRESETS],
  pub last_tuned: u16,
  pub pi: [u16; MAX_PRESETS],
  pub ps: [[u8; 8]; MAX_PRESETS],
}

impl PresetRecord {
  pub const fn empty() -> Self {
    Self {
      freqs: [0; MAX_PRESETS],
      last_tuned: 0,
      pi: [0; MAX_PRESETS],
      ps: [[0; 8]; MAX_PRESETS],
    }
  }
}

/// What went wrong when decoding a record. Mirrors the relevant
/// variants of `presets::PresetStoreError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
  InvalidMagic,
  VersionMismatch,
  CrcMismatch,
  TooShort,
}

/// Encode a [`PresetRecord`] into the v2 wire format.
///
/// Returns the exact bytes the firmware would write to flash (minus
/// the trailing 4-byte alignment pad, which is irrelevant on read).
pub fn encode_v2(rec: &PresetRecord) -> Vec<u8> {
  let mut buf = vec![0u8; HEADER_SIZE + PAYLOAD_V2_SIZE];
  buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
  buf[4] = FORMAT_VERSION_V2;
  buf[5] = MAX_PRESETS as u8;
  // buf[6..8] reserved zero
  // CRC at buf[8..12] filled in below.

  let payload_start = HEADER_SIZE;
  let payload_end = payload_start + PAYLOAD_V2_SIZE;
  let payload = &mut buf[payload_start..payload_end];
  // freqs
  for (i, freq) in rec.freqs.iter().enumerate() {
    let off = i * 2;
    payload[off..off + 2].copy_from_slice(&freq.to_le_bytes());
  }
  // last_tuned
  let last_tuned_off = MAX_PRESETS * 2;
  payload[last_tuned_off..last_tuned_off + 2].copy_from_slice(&rec.last_tuned.to_le_bytes());
  // PI
  let pi_off = PAYLOAD_V1_SIZE;
  for (i, pi) in rec.pi.iter().enumerate() {
    let off = pi_off + i * 2;
    payload[off..off + 2].copy_from_slice(&pi.to_le_bytes());
  }
  // PS
  let ps_off = pi_off + MAX_PRESETS * 2;
  for (i, ps) in rec.ps.iter().enumerate() {
    let off = ps_off + i * 8;
    payload[off..off + 8].copy_from_slice(ps);
  }

  let crc = crc32(&buf[payload_start..payload_end]);
  buf[8..12].copy_from_slice(&crc.to_le_bytes());
  buf
}

/// Encode a [`PresetRecord`] into the legacy v1 wire format.
///
/// Used only by tests to construct fixtures that exercise the
/// migration path; the firmware never writes v1 records anymore.
pub fn encode_v1(rec: &PresetRecord) -> Vec<u8> {
  let mut buf = vec![0u8; HEADER_SIZE + PAYLOAD_V1_SIZE];
  buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
  buf[4] = FORMAT_VERSION_V1;
  buf[5] = MAX_PRESETS as u8;

  let payload_start = HEADER_SIZE;
  let payload_end = payload_start + PAYLOAD_V1_SIZE;
  let payload = &mut buf[payload_start..payload_end];
  for (i, freq) in rec.freqs.iter().enumerate() {
    let off = i * 2;
    payload[off..off + 2].copy_from_slice(&freq.to_le_bytes());
  }
  let last_tuned_off = MAX_PRESETS * 2;
  payload[last_tuned_off..last_tuned_off + 2].copy_from_slice(&rec.last_tuned.to_le_bytes());

  let crc = crc32(&buf[payload_start..payload_end]);
  buf[8..12].copy_from_slice(&crc.to_le_bytes());
  buf
}

/// Decode a record using the same dispatch logic as the firmware:
/// look at the version byte, then pick the matching payload window.
///
/// v1 records succeed with PI / PS at the all-zero sentinel — that's
/// the whole point of the lazy migration.
pub fn decode(buf: &[u8]) -> Result<PresetRecord, DecodeError> {
  if buf.len() < HEADER_SIZE {
    return Err(DecodeError::TooShort);
  }
  let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
  if magic != MAGIC {
    return Err(DecodeError::InvalidMagic);
  }
  let version = buf[4];
  let slot_count = buf[5];
  if usize::from(slot_count) != MAX_PRESETS {
    return Err(DecodeError::VersionMismatch);
  }
  let payload_size = match version {
    FORMAT_VERSION_V1 => PAYLOAD_V1_SIZE,
    FORMAT_VERSION_V2 => PAYLOAD_V2_SIZE,
    _ => return Err(DecodeError::VersionMismatch),
  };
  if buf.len() < HEADER_SIZE + payload_size {
    return Err(DecodeError::TooShort);
  }
  let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
  let payload = &buf[HEADER_SIZE..HEADER_SIZE + payload_size];
  if stored_crc != crc32(payload) {
    return Err(DecodeError::CrcMismatch);
  }

  let mut rec = PresetRecord::empty();
  for i in 0..MAX_PRESETS {
    let off = i * 2;
    rec.freqs[i] = u16::from_le_bytes([payload[off], payload[off + 1]]);
  }
  let last_tuned_off = MAX_PRESETS * 2;
  rec.last_tuned = u16::from_le_bytes([payload[last_tuned_off], payload[last_tuned_off + 1]]);
  if version == FORMAT_VERSION_V2 {
    let pi_off = PAYLOAD_V1_SIZE;
    for i in 0..MAX_PRESETS {
      let off = pi_off + i * 2;
      rec.pi[i] = u16::from_le_bytes([payload[off], payload[off + 1]]);
    }
    let ps_off = pi_off + MAX_PRESETS * 2;
    for i in 0..MAX_PRESETS {
      let off = ps_off + i * 8;
      rec.ps[i].copy_from_slice(&payload[off..off + 8]);
    }
  }
  Ok(rec)
}

/// Standard CRC-32/ISO-HDLC. Bit-identical to `presets::crc32`.
fn crc32(data: &[u8]) -> u32 {
  let mut crc: u32 = 0xFFFF_FFFF;
  for &byte in data {
    crc ^= u32::from(byte);
    for _ in 0..8 {
      crc = if crc & 1 != 0 {
        (crc >> 1) ^ 0xEDB8_8320
      } else {
        crc >> 1
      };
    }
  }
  !crc
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_record() -> PresetRecord {
    let mut rec = PresetRecord::empty();
    rec.freqs[0] = 881; // 88.1 MHz
    rec.freqs[1] = 935; // 93.5 MHz
    rec.freqs[2] = 1015; // 101.5 MHz
    rec.last_tuned = 1015;
    rec.pi[0] = 0xC2_01;
    rec.pi[2] = 0x10_5E;
    rec.ps[0] = *b"BBC R1  ";
    rec.ps[2] = *b"KQED    ";
    rec
  }

  #[test]
  fn v2_round_trip_preserves_all_fields() {
    let original = sample_record();
    let bytes = encode_v2(&original);
    let decoded = decode(&bytes).expect("v2 round-trip should decode");
    assert_eq!(decoded, original);
  }

  #[test]
  fn v2_round_trip_preserves_blank_ps_buffers() {
    // Stations without RDS PS leave the slot's PS as all-zeros;
    // make sure the blank state survives the encode → decode trip
    // and is *not* helpfully turned into spaces.
    let mut rec = PresetRecord::empty();
    rec.freqs[3] = 977;
    let bytes = encode_v2(&rec);
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.ps[3], [0u8; 8]);
  }

  #[test]
  fn v1_record_decodes_with_blank_metadata() {
    // The whole point of the version dispatch: a v1 record on flash
    // (e.g. produced by the previous firmware build, then read by
    // the current one after upgrade) must yield the right freqs +
    // last_tuned with PI / PS at the unknown sentinel.
    let mut rec = sample_record();
    // PI / PS get dropped on the way through v1 — match that.
    rec.pi = [0; MAX_PRESETS];
    rec.ps = [[0; 8]; MAX_PRESETS];
    let bytes = encode_v1(&rec);
    let decoded = decode(&bytes).expect("v1 record should decode");
    assert_eq!(decoded, rec);
    // Buffer length is the smaller v1 size — guard against accidental
    // payload-window confusion in the encoder.
    assert_eq!(bytes.len(), HEADER_SIZE + PAYLOAD_V1_SIZE);
  }

  #[test]
  fn invalid_magic_is_rejected() {
    let mut bytes = encode_v2(&sample_record());
    bytes[0] ^= 0xFF;
    assert_eq!(decode(&bytes), Err(DecodeError::InvalidMagic));
  }

  #[test]
  fn unknown_version_is_rejected() {
    let mut bytes = encode_v2(&sample_record());
    bytes[4] = 99; // hypothetical future v99
    assert_eq!(decode(&bytes), Err(DecodeError::VersionMismatch));
  }

  #[test]
  fn wrong_slot_count_is_rejected() {
    let mut bytes = encode_v2(&sample_record());
    bytes[5] = (MAX_PRESETS as u8) + 1; // guard against a host build with a different MAX_PRESETS
    assert_eq!(decode(&bytes), Err(DecodeError::VersionMismatch));
  }

  #[test]
  fn payload_corruption_fails_crc() {
    // Flip a freq byte. The decoder must notice via CRC instead of
    // happily returning a half-wrong record.
    let mut bytes = encode_v2(&sample_record());
    bytes[HEADER_SIZE + 1] ^= 0xFF;
    assert_eq!(decode(&bytes), Err(DecodeError::CrcMismatch));
  }

  #[test]
  fn truncated_buffer_is_rejected() {
    let bytes = encode_v2(&sample_record());
    // Drop the last byte so the v2 payload window can't be filled.
    let short = &bytes[..bytes.len() - 1];
    assert_eq!(decode(short), Err(DecodeError::TooShort));
  }

  #[test]
  fn ps_with_high_bit_bytes_round_trips_verbatim() {
    // Some broadcasters emit Latin-1 high-bit bytes inside PS. The
    // codec must store them as-is — any UTF-8 conversion happens at
    // a higher layer (`web::decode_preset_ps` on the JSON path or
    // the LCD label decoder).
    let mut rec = PresetRecord::empty();
    rec.freqs[0] = 1037;
    rec.ps[0] = [b'C', b'a', b'f', 0xE9, b' ', b' ', b' ', b' ']; // "Café"
    let bytes = encode_v2(&rec);
    let decoded = decode(&bytes).unwrap();
    assert_eq!(decoded.ps[0], rec.ps[0]);
  }
}
