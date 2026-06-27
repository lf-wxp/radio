//! Flash-backed persistence for the user's favourite stations.
//!
//! Mirrors the design of [`radio::wifi_provision::storage`] but with a
//! payload tailored to [`crate::state::PresetSet`]: a compact, fixed-size
//! record protected by a magic number + CRC32 so corrupted sectors can
//! be detected and recycled silently.
//!
//! # Storage layout (within one 4KB sector)
//!
//! Common header (covers v1 and v2):
//!
//! | Offset | Size | Description                                    |
//! |--------|------|------------------------------------------------|
//! | 0x00   | 4    | Magic number (`b"RPST"` little-endian)         |
//! | 0x04   | 1    | Format version (`1` = legacy, `2` = current)   |
//! | 0x05   | 1    | Slot count `MAX_PRESETS` (sanity check)        |
//! | 0x06   | 2    | Reserved (zeros)                               |
//! | 0x08   | 4    | CRC32 of the payload                           |
//!
//! ## v1 payload (18 bytes, read-only on this build)
//!
//! | Offset | Size | Description                                    |
//! |--------|------|------------------------------------------------|
//! | 0x0C   | 16   | Preset frequencies — `MAX_PRESETS` × `u16` LE  |
//! | 0x1C   | 2    | `last_tuned` frequency (MHz × 10)              |
//!
//! ## v2 payload (98 bytes, written by current builds)
//!
//! | Offset | Size | Description                                    |
//! |--------|------|------------------------------------------------|
//! | 0x0C   | 16   | Preset frequencies — `MAX_PRESETS` × `u16` LE  |
//! | 0x1C   | 2    | `last_tuned` frequency (MHz × 10)              |
//! | 0x1E   | 16   | Cached RDS PI codes — `MAX_PRESETS` × `u16` LE |
//! | 0x2E   | 64   | Cached RDS PS names — `MAX_PRESETS` × 8 bytes  |
//!
//! v2 records read by older firmware will fail the version check and
//! be treated as "no presets stored"; downgrading therefore wipes the
//! preset table on the next save. This is documented in the README
//! release notes for the v2 schema bump.
//!
//! Records are padded up to a 4-byte multiple for word-aligned writes
//! and stored alongside a CRC32; the whole 4 KB sector is erased on
//! every write because that's the smallest unit the underlying NOR
//! flash supports. With the existing 30 s tune-debounce + the new
//! 30 s metadata-fill debounce in [`crate::tasks`], the erase rate
//! stays well below the chip's 100k cycle endurance.

use defmt::Format;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;

use crate::state::{MAX_PRESETS, PRESET_EMPTY, PresetSet};

/// "RPST" — Radio PreSeT. Little-endian on flash.
const MAGIC: u32 = u32::from_le_bytes(*b"RPST");

/// Legacy format: 18-byte payload, no RDS metadata cache.
const FORMAT_VERSION_V1: u8 = 1;
/// Current format: 98-byte payload with PI + PS metadata.
const FORMAT_VERSION_V2: u8 = 2;
/// Always written by [`PresetStore::write_record`].
const FORMAT_VERSION_CURRENT: u8 = FORMAT_VERSION_V2;

/// Bytes from the start of the sector to the CRC field.
const HEADER_SIZE: usize = 12;
/// v1 payload size: 16 bytes of freqs + 2 bytes last_tuned.
const PAYLOAD_V1_SIZE: usize = MAX_PRESETS * 2 + 2;
/// v2 payload extension: 16 bytes PI + 64 bytes PS.
const PAYLOAD_V2_EXTRA: usize = MAX_PRESETS * 2 + MAX_PRESETS * 8;
/// v2 payload size (the part covered by CRC).
const PAYLOAD_V2_SIZE: usize = PAYLOAD_V1_SIZE + PAYLOAD_V2_EXTRA;
/// Total record size for the *current* (v2) format, padded to 4 bytes.
const RECORD_SIZE_V2: usize = round_up_4(HEADER_SIZE + PAYLOAD_V2_SIZE);
/// Maximum buffer size used for reads — sized for v2; v1 records
/// occupy only the first `HEADER_SIZE + PAYLOAD_V1_SIZE` bytes.
const READ_BUF_SIZE: usize = RECORD_SIZE_V2;

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
  #[allow(
    dead_code,
    reason = "thin wrapper kept for public API symmetry; \
                                live callers use save_freq_with_meta to \
                                also capture RDS PI/PS atomically."
  )]
  pub fn save_freq(&mut self, freq_x10: u16) -> Result<Option<usize>, PresetStoreError> {
    self.save_freq_with_meta(freq_x10, None, None)
  }

  /// Save `freq_x10` and seed its RDS metadata in one shot.
  ///
  /// Convenience wrapper letting callers atomically capture the
  /// station identity at the moment of "save". Pass `None` for either
  /// argument to leave the corresponding cache slot at its default
  /// (unknown) — the background metadata-fill task will populate it
  /// on the next listen.
  ///
  /// # Errors
  ///
  /// Returns [`PresetStoreError::Flash`] if the flash write fails; the
  /// in-memory snapshot is left untouched in that case.
  pub fn save_freq_with_meta(
    &mut self,
    freq_x10: u16,
    pi: Option<u16>,
    ps: Option<[u8; 8]>,
  ) -> Result<Option<usize>, PresetStoreError> {
    if freq_x10 == PRESET_EMPTY {
      return Ok(None);
    }
    let mut next = self.cached;
    let idx = next.save_with_meta(freq_x10, pi, ps);
    self.save_set(next)?;
    Ok(Some(idx))
  }

  /// Update the cached PI / PS for `freq_x10` if it's currently saved.
  ///
  /// Returns `Ok(true)` when something actually changed (and was
  /// flushed to flash), `Ok(false)` when the frequency isn't a saved
  /// preset *or* the metadata was already up to date — both of which
  /// the caller usually wants to ignore. This keeps the background
  /// fill loop allocation- and flash-free in the common case where
  /// nothing has changed since the last tick.
  ///
  /// # Errors
  ///
  /// Returns [`PresetStoreError::Flash`] if the flash write fails.
  pub fn update_meta(
    &mut self,
    freq_x10: u16,
    pi: Option<u16>,
    ps: Option<[u8; 8]>,
  ) -> Result<bool, PresetStoreError> {
    let Some(idx) = self.cached.position(freq_x10) else {
      return Ok(false);
    };
    let mut next = self.cached;
    // Only commit fields that are both "known" (Some) and "new"
    // information; otherwise we'd waste a flash erase cycle every
    // time RDS re-emitted the same PI / PS.
    let pi_changed = matches!(pi, Some(new) if new != 0 && next.pi[idx] != new);
    let ps_changed = matches!(ps, Some(new) if new.iter().any(|&b| b != 0) && next.ps[idx] != new);
    if !pi_changed && !ps_changed {
      return Ok(false);
    }
    next.set_meta(
      idx,
      if pi_changed { pi } else { None },
      if ps_changed { ps } else { None },
    );
    self.save_set(next)?;
    Ok(true)
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
  // OTA flash hand-off
  // ---------------------------------------------------------------------

  /// Surrender the underlying [`FlashStorage`] handle so another
  /// subsystem (currently OTA) can borrow it for the duration of an
  /// update.
  ///
  /// Returns the live flash handle plus a [`PausedPresetStore`] token
  /// that remembers the cached snapshot and on-flash offset; pair it
  /// with [`PausedPresetStore::resume`] to put the store back together
  /// once the borrower is done.
  ///
  /// Rationale: `esp-storage`'s `FlashStorage` is effectively a
  /// singleton that can only have one live owner at a time, and the
  /// OTA writer needs raw flash access to populate the inactive app
  /// partition. Rather than wrapping the whole thing in a
  /// `Mutex<FlashStorage>` (which would force every preset write to
  /// acquire a lock just to support a once-per-month OTA), we model
  /// the rare hand-off explicitly. The radio task is expected to
  /// suspend `last_tuned` debounce flushes (`RadioState.ota_in_progress`)
  /// while paused so it doesn't accidentally hold a stale handle.
  #[must_use]
  pub fn pause(self) -> (FlashStorage<'d>, PausedPresetStore) {
    let token = PausedPresetStore {
      offset: self.offset,
      cached: self.cached,
    };
    (self.flash, token)
  }
}

/// Opaque handle returned by [`PresetStore::pause`] used to reattach a
/// previously surrendered [`FlashStorage`] back into a working store.
///
/// Holds only `Copy` data (offset + cached snapshot) so the OTA flow
/// can shove it onto the stack while it owns the flash handle without
/// any allocation.
#[derive(Debug, Clone, Copy)]
pub struct PausedPresetStore {
  offset: u32,
  cached: PresetSet,
}

impl PausedPresetStore {
  /// Reattach the flash handle and return a ready-to-use store.
  ///
  /// The cached snapshot is preserved verbatim — no flash read happens
  /// here, so callers should call [`PresetStore::save_set`] (or one of
  /// its convenience wrappers) to overwrite the on-flash record only
  /// when they actually want to mutate it.
  ///
  /// We deliberately do *not* re-read from flash on resume: the OTA
  /// writer never touches the `storage` partition (it only writes to
  /// the inactive app slot via [`esp_bootloader_esp_idf::ota_updater`]),
  /// so the cached snapshot remains authoritative. Skipping the read
  /// also means resume is allocation-free and cannot fail.
  #[must_use]
  pub fn resume<'d>(self, flash: FlashStorage<'d>) -> PresetStore<'d> {
    PresetStore {
      flash,
      offset: self.offset,
      cached: self.cached,
    }
  }
}

impl<'d> PresetStore<'d> {
  // ---------------------------------------------------------------------
  // Internal: flash codec
  // ---------------------------------------------------------------------

  fn read_record(&mut self) -> Result<PresetSet, PresetStoreError> {
    let mut buf = [0u8; READ_BUF_SIZE];
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
    if usize::from(slot_count) != MAX_PRESETS {
      return Err(PresetStoreError::VersionMismatch);
    }
    // Pick the payload window matching the on-flash version. Anything
    // we don't know about is rejected outright so a future v3 record
    // doesn't get partially decoded by an older firmware.
    let payload_size = match version {
      FORMAT_VERSION_V1 => PAYLOAD_V1_SIZE,
      FORMAT_VERSION_V2 => PAYLOAD_V2_SIZE,
      _ => return Err(PresetStoreError::VersionMismatch),
    };
    let stored_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let payload = &buf[HEADER_SIZE..HEADER_SIZE + payload_size];
    if stored_crc != crc32(payload) {
      return Err(PresetStoreError::CrcMismatch);
    }

    // ----- common (v1+) fields ---------------------------------------
    let mut freqs = [PRESET_EMPTY; MAX_PRESETS];
    for (idx, slot) in freqs.iter_mut().enumerate() {
      let off = idx * 2;
      *slot = u16::from_le_bytes([payload[off], payload[off + 1]]);
    }
    let last_tuned_off = MAX_PRESETS * 2;
    let last_tuned = u16::from_le_bytes([payload[last_tuned_off], payload[last_tuned_off + 1]]);

    // ----- v2-only metadata ------------------------------------------
    // For v1 records the PI / PS arrays start out as the all-zero
    // sentinel, which is exactly the "unknown" encoding documented on
    // [`PresetSet`]; the background metadata-fill loop will populate
    // them on the next listen.
    let mut pi = [0u16; MAX_PRESETS];
    let mut ps = [[0u8; 8]; MAX_PRESETS];
    if version == FORMAT_VERSION_V2 {
      let pi_off = PAYLOAD_V1_SIZE;
      for (idx, slot) in pi.iter_mut().enumerate() {
        let off = pi_off + idx * 2;
        *slot = u16::from_le_bytes([payload[off], payload[off + 1]]);
      }
      let ps_off = pi_off + MAX_PRESETS * 2;
      for (idx, slot) in ps.iter_mut().enumerate() {
        let off = ps_off + idx * 8;
        slot.copy_from_slice(&payload[off..off + 8]);
      }
    }

    Ok(PresetSet {
      freqs,
      last_tuned,
      pi,
      ps,
    })
  }

  fn write_record(&mut self, set: &PresetSet) -> Result<(), PresetStoreError> {
    let mut buf = [0u8; RECORD_SIZE_V2];
    buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    buf[4] = FORMAT_VERSION_CURRENT;
    buf[5] = MAX_PRESETS as u8;
    // buf[6..8] = reserved zeros

    // Build payload first so we can CRC it.
    let payload_start = HEADER_SIZE;
    let payload_end = payload_start + PAYLOAD_V2_SIZE;
    {
      let payload = &mut buf[payload_start..payload_end];
      // freqs
      for (idx, freq) in set.freqs.iter().enumerate() {
        let off = idx * 2;
        payload[off..off + 2].copy_from_slice(&freq.to_le_bytes());
      }
      // last_tuned
      let last_tuned_off = MAX_PRESETS * 2;
      payload[last_tuned_off..last_tuned_off + 2].copy_from_slice(&set.last_tuned.to_le_bytes());
      // PI cache (v2)
      let pi_off = PAYLOAD_V1_SIZE;
      for (idx, pi) in set.pi.iter().enumerate() {
        let off = pi_off + idx * 2;
        payload[off..off + 2].copy_from_slice(&pi.to_le_bytes());
      }
      // PS cache (v2)
      let ps_off = pi_off + MAX_PRESETS * 2;
      for (idx, ps) in set.ps.iter().enumerate() {
        let off = ps_off + idx * 8;
        payload[off..off + 8].copy_from_slice(ps);
      }
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
