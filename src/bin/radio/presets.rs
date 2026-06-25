//! Flash-backed persistence for the user's favourite stations.
//!
//! Mirrors the design of [`radio::wifi_provision::storage`] but with a
//! payload tailored to [`crate::state::PresetSet`]: a compact, fixed-size
//! record protected by a magic number + CRC32 so corrupted sectors can
//! be detected and recycled silently.
//!
//! # Storage layout (within one 4KB sector)
//!
//! | Offset | Size | Description                                    |
//! |--------|------|------------------------------------------------|
//! | 0x00   | 4    | Magic number (`b"RPST"` little-endian)         |
//! | 0x04   | 1    | Format version (currently `1`)                 |
//! | 0x05   | 1    | Slot count `MAX_PRESETS` (sanity check)        |
//! | 0x06   | 2    | Reserved (zeros)                               |
//! | 0x08   | 4    | CRC32 of the payload                           |
//! | 0x0C   | 16   | Preset frequencies — `MAX_PRESETS` × `u16` LE  |
//! | 0x1C   | 2    | `last_tuned` frequency (MHz × 10)              |
//! | 0x1E   | …    | Reserved for future fields                     |
//!
//! The whole record is 32 bytes; we still erase the whole 4 KB sector on
//! every write because that's the smallest unit the underlying NOR flash
//! supports. With a 30 s tune-debounce the erase rate stays well below
//! the chip's 100k cycle endurance.

use defmt::Format;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;

use crate::state::{MAX_PRESETS, PRESET_EMPTY, PresetSet};

/// "RPST" — Radio PreSeT. Little-endian on flash.
const MAGIC: u32 = u32::from_le_bytes(*b"RPST");

/// Bumped whenever the on-flash layout changes incompatibly.
const FORMAT_VERSION: u8 = 1;

/// Bytes from the start of the sector to the CRC field.
const HEADER_SIZE: usize = 12;
/// Bytes in the payload (the part covered by CRC).
/// 16 bytes of preset freqs + 2 bytes of last_tuned = 18 bytes.
const PAYLOAD_SIZE: usize = MAX_PRESETS * 2 + 2;
/// Total record size, padded up to a multiple of 4 for word-aligned writes.
const RECORD_SIZE: usize = round_up_4(HEADER_SIZE + PAYLOAD_SIZE);

const fn round_up_4(n: usize) -> usize {
  n.div_ceil(4) * 4
}

/// Default flash offset: first sector of the partition-table `storage`
/// region (0x3E_0000) — well clear of the bootloader, app code and the
/// last-sector slot reserved for the WiFi credential store at `0x3F_F000`
/// (see [`radio::wifi_provision::storage::DEFAULT_STORAGE_OFFSET`]).
///
/// # Flash partition layout (4 MB chip)
///
/// | Region       | Offset       | Size   | Owner                        |
/// |--------------|--------------|--------|------------------------------|
/// | Preset store | `0x3E_0000`  | 4 KB   | [`PresetStore`]              |
/// | …            | `0x3E_1000`  | ~124KB | (reserved / unused)          |
/// | WiFi creds   | `0x3F_F000`  | 4 KB   | `CredentialStorage`          |
const DEFAULT_PRESET_OFFSET: u32 = 0x3E_0000;

/// Errors that can occur while reading or writing the preset store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Format)]
pub enum PresetStoreError {
  /// Underlying flash read / write / erase failed.
  Flash,
  /// Stored data has the wrong magic — interpreted as "no record yet".
  InvalidMagic,
  /// Stored data failed CRC32 — sector is corrupt and was wiped.
  CrcMismatch,
  /// Format version on flash isn't supported by this build.
  VersionMismatch,
}

/// Owns the [`FlashStorage`] handle for the preset partition and the
/// last-known good [`PresetSet`] in RAM.
///
/// The cached snapshot lets the radio task answer "what's saved?"
/// without re-reading flash on every tune.
pub struct PresetStore<'d> {
  flash: FlashStorage<'d>,
  offset: u32,
  cached: PresetSet,
}

impl<'d> PresetStore<'d> {
  /// Open the store at the default offset, loading any existing record.
  ///
  /// On any error (no record, CRC fail, version mismatch) the cached
  /// set falls back to [`PresetSet::empty`]; the caller can still call
  /// [`PresetStore::save_set`] afterwards to write a fresh record.
  #[must_use]
  pub fn open(flash: FlashStorage<'d>) -> Self {
    Self::open_at(flash, DEFAULT_PRESET_OFFSET)
  }

  /// Open the store at a custom sector-aligned offset.
  ///
  /// # Panics
  ///
  /// Panics if `offset` isn't a multiple of [`FlashStorage::SECTOR_SIZE`].
  #[must_use]
  pub fn open_at(flash: FlashStorage<'d>, offset: u32) -> Self {
    assert!(
      offset.is_multiple_of(FlashStorage::SECTOR_SIZE),
      "preset store offset must be sector-aligned"
    );
    let mut store = Self {
      flash,
      offset,
      cached: PresetSet::empty(),
    };
    // Best effort: missing / corrupt records just leave the cache empty.
    if let Ok(set) = store.read_record() {
      store.cached = set;
    }
    store
  }

  /// Latest in-memory snapshot. O(1), no flash I/O.
  #[must_use]
  pub fn snapshot(&self) -> PresetSet {
    self.cached
  }

  /// Persist `set` and update the cached snapshot atomically (as far as
  /// a single-sector erase + write can be).
  ///
  /// # Errors
  ///
  /// Returns [`PresetStoreError::Flash`] if the underlying erase or
  /// write transaction fails.
  pub fn save_set(&mut self, set: PresetSet) -> Result<(), PresetStoreError> {
    self.write_record(&set)?;
    self.cached = set;
    Ok(())
  }

  /// Save the current frequency into the next free slot.
  ///
  /// Returns `Ok(Some(idx))` with the slot index used, or `Ok(None)` if
  /// `freq_x10` is the empty sentinel (no-op). Matches [`PresetSet::save`]
  /// semantics for the slot index.
  ///
  /// # Errors
  ///
  /// Returns [`PresetStoreError::Flash`] if the flash write fails; the
  /// in-memory snapshot is left untouched in that case.
  pub fn save_freq(&mut self, freq_x10: u16) -> Result<Option<usize>, PresetStoreError> {
    if freq_x10 == PRESET_EMPTY {
      return Ok(None);
    }
    let mut next = self.cached;
    let idx = next.save(freq_x10);
    self.save_set(next)?;
    Ok(Some(idx))
  }

  /// Update only the `last_tuned` field (and persist it).
  ///
  /// # Errors
  ///
  /// Returns [`PresetStoreError::Flash`] on flash failure.
  pub fn record_last_tuned(&mut self, freq_x10: u16) -> Result<(), PresetStoreError> {
    if self.cached.last_tuned == freq_x10 {
      return Ok(());
    }
    let mut next = self.cached;
    next.last_tuned = freq_x10;
    self.save_set(next)
  }

  // ---------------------------------------------------------------------
  // Internal: flash codec
  // ---------------------------------------------------------------------

  fn read_record(&mut self) -> Result<PresetSet, PresetStoreError> {
    let mut buf = [0u8; RECORD_SIZE];
    self
      .flash
      .read(self.offset, &mut buf)
      .map_err(|_| PresetStoreError::Flash)?;

    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != MAGIC {
      return Err(PresetStoreError::InvalidMagic);
    }
    let version = buf[4];
    let slot_count = buf[5];
    if version != FORMAT_VERSION || usize::from(slot_count) != MAX_PRESETS {
      return Err(PresetStoreError::VersionMismatch);
    }
    let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let payload = &buf[HEADER_SIZE..HEADER_SIZE + PAYLOAD_SIZE];
    if stored_crc != crc32(payload) {
      return Err(PresetStoreError::CrcMismatch);
    }

    let mut freqs = [PRESET_EMPTY; MAX_PRESETS];
    for (idx, slot) in freqs.iter_mut().enumerate() {
      let off = idx * 2;
      *slot = u16::from_le_bytes([payload[off], payload[off + 1]]);
    }
    let last_tuned_off = MAX_PRESETS * 2;
    let last_tuned = u16::from_le_bytes([payload[last_tuned_off], payload[last_tuned_off + 1]]);

    Ok(PresetSet { freqs, last_tuned })
  }

  fn write_record(&mut self, set: &PresetSet) -> Result<(), PresetStoreError> {
    let mut buf = [0u8; RECORD_SIZE];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4] = FORMAT_VERSION;
    buf[5] = MAX_PRESETS as u8;
    // buf[6..8] = reserved zeros

    // Build payload first so we can CRC it.
    let payload_start = HEADER_SIZE;
    let payload_end = payload_start + PAYLOAD_SIZE;
    {
      let payload = &mut buf[payload_start..payload_end];
      for (idx, freq) in set.freqs.iter().enumerate() {
        let off = idx * 2;
        payload[off..off + 2].copy_from_slice(&freq.to_le_bytes());
      }
      let last_tuned_off = MAX_PRESETS * 2;
      payload[last_tuned_off..last_tuned_off + 2].copy_from_slice(&set.last_tuned.to_le_bytes());
    }
    let crc = crc32(&buf[payload_start..payload_end]);
    buf[8..12].copy_from_slice(&crc.to_le_bytes());

    self
      .flash
      .erase(self.offset, self.offset + FlashStorage::SECTOR_SIZE)
      .map_err(|_| PresetStoreError::Flash)?;
    self
      .flash
      .write(self.offset, &buf)
      .map_err(|_| PresetStoreError::Flash)?;
    Ok(())
  }
}

/// Standard CRC-32/ISO-HDLC. Inlined here (rather than reaching into
/// `wifi_provision::storage`) to keep the two persistence subsystems
/// independent — they're free to switch to a hardware CRC peripheral
/// at different times.
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
