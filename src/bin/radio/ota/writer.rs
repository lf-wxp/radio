//! Sector-buffered writer that streams an OTA image into the inactive slot
//! and activates it on success.
//!
//! # Pipeline at a glance
//!
//! ```text
//!  caller (HTTP downloader, future)
//!     │   .write_chunk(&[u8]) — any size, async
//!     ▼
//!  ┌─────────────────────────────────────────┐
//!  │ OtaWriter                               │
//!  │  ┌──────────────┐                       │
//!  │  │ sector_buf   │  4 KiB heap buffer    │
//!  │  │ [0..buf_used)│  ◄── caller's bytes   │
//!  │  └──────────────┘                       │
//!  │  on full → erase + write one sector,    │
//!  │  yield to executor, repeat              │
//!  └─────────────────────────────────────────┘
//!     │   .finalize() — pads tail, activates slot
//!     ▼
//!  bootloader picks new image on next reboot
//! ```
//!
//! # Why sector buffering?
//!
//! The underlying `esp-storage::FlashStorage` exposes two trait impls:
//!
//! - `embedded_storage::Storage::write` does **read-modify-erase-write** on
//!   each call (one full 4 KiB cycle, even for a 16-byte payload).
//! - `embedded_storage::nor_flash::NorFlash::{erase,write}` are raw — caller
//!   is responsible for pre-erasing and respecting `WRITE_SIZE` alignment.
//!
//! HTTP chunks arrive at unpredictable boundaries (TLS record size, TCP MSS,
//! application framing). To get exactly **one erase + one program per
//! sector**, we accumulate caller-provided bytes into a 4 KiB sector buffer
//! and flush only when full (or on `finalize`). This keeps flash wear and
//! programming time at the theoretical minimum (~30–50 ms per sector).
//!
//! # Yielding & WDT
//!
//! Each sector flush blocks for ~30–50 ms (erase + program). We `await` an
//! immediate `Timer::after` between sectors so the embassy executor can
//! service WiFi / HTTP / WDT-feed tasks. A 1.5 MiB image flushes ~384
//! sectors → roughly 12–20 s wall-clock, fully cooperative.

use alloc::boxed::Box;
use alloc::vec;

use defmt::{Format, debug, info, warn};
use embassy_time::Timer;
use embedded_storage::nor_flash::NorFlash;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
  AppPartitionSubType, Error as PartitionError, PARTITION_TABLE_MAX_LEN, PartitionType,
  read_partition_table,
};
use esp_storage::{FlashStorage, FlashStorageError};

/// Flash sector size (also the erase granularity) on the ESP32-C6.
const SECTOR_SIZE: usize = 4096;

/// Bytes of ESP image header we inspect before accepting the rest of the
/// stream. The on-flash header is 24 bytes; we only need the first 14 to
/// validate magic + chip id, but reading the full block keeps us aligned
/// with the ESP-IDF layout if we later want to surface entry-point or
/// segment count.
const HEADER_LEN: usize = 24;

/// First byte of every ESP image — see ESP-IDF `esp_image_format.h`.
const ESP_IMAGE_MAGIC: u8 = 0xE9;

/// `chip_id` value the bootloader writes into byte offsets 0x0C..0x0E for
/// ESP32-C6 firmware. Matches `ESP_CHIP_ID_ESP32C6` in ESP-IDF. We hard-code
/// this because the crate is built exclusively for the C6 target (see the
/// `esp32c6` feature pinned in `Cargo.toml`); flashing a binary built for a
/// different chip would brick the radio on next boot.
const EXPECTED_CHIP_ID: u16 = 0x000D;

/// Errors that can occur while staging an OTA image.
#[derive(Debug, Format, Clone, Copy, PartialEq, Eq)]
pub enum OtaError {
  /// Image bytes exceed the target slot capacity.
  ImageTooLarge { image_size: u32, slot_size: u32 },
  /// Caller declared an `expected_size` in `begin` but `finalize` saw a
  /// different number of bytes. Indicates a truncated download.
  SizeMismatch { expected: u32, received: u32 },
  /// First [`HEADER_LEN`] bytes do not look like an ESP image (bad magic
  /// or chip-id mismatch). Caught before any flash sectors are activated
  /// in OTA-data, so the running image stays untouched.
  BadImageHeader { magic: u8, chip_id: u16 },
  /// `OtaUpdater::next_partition` could not find an inactive OTA slot.
  /// Usually means the partition table lacks `ota_0`/`ota_1`/`otadata`.
  SlotNotFound,
  /// Underlying SPI flash error (read/erase/program).
  Flash(FlashStorageError),
  /// Partition-table parsing or OTA-data manipulation error.
  Partition(PartitionError),
}

impl From<FlashStorageError> for OtaError {
  fn from(e: FlashStorageError) -> Self {
    Self::Flash(e)
  }
}

impl From<PartitionError> for OtaError {
  fn from(e: PartitionError) -> Self {
    Self::Partition(e)
  }
}

/// Streams an OTA image into the inactive slot, sector-by-sector.
///
/// Lifecycle:
///
/// 1. [`OtaWriter::begin`] — locates the inactive slot, allocates a 4 KiB
///    sector buffer, returns a writer.
/// 2. [`OtaWriter::write_chunk`] — call any number of times with any chunk
///    size. Bytes are buffered; full sectors are erased+programmed inline.
/// 3. [`OtaWriter::finalize`] — pads the trailing partial sector, activates
///    the new slot in OTA-data, marks state `New`. Returns the released
///    [`FlashStorage`] handle so the caller can resume `PresetStore`.
/// 4. [`OtaWriter::abort`] — drop without activating; the inactive slot is
///    left in whatever (partial / 0xFF) state we had reached.
///
/// `OtaWriter` owns the `FlashStorage` for its lifetime; presets must be
/// `pause()`d before [`OtaWriter::begin`] and `resume()`d on the handle
/// returned by [`OtaWriter::finalize`] / [`OtaWriter::abort`].
pub struct OtaWriter<'d> {
  flash: FlashStorage<'d>,
  /// Absolute flash offset where the target slot starts.
  slot_base: u32,
  /// Slot capacity in bytes.
  slot_size: u32,
  /// Inactive slot the bootloader will switch to on `finalize`.
  target_slot: AppPartitionSubType,
  /// 4 KiB on-heap accumulator. Heap is preferred over a stack array
  /// to keep `main`'s stack frame within the existing
  /// `clippy::large_stack_frames` budget.
  sector_buf: Box<[u8; SECTOR_SIZE]>,
  /// Bytes currently buffered in `sector_buf` (`0..=SECTOR_SIZE`).
  buf_used: usize,
  /// Number of full sectors already programmed into flash.
  flushed_sectors: u32,
  /// Total caller-visible bytes accepted (`write_chunk` arg lengths).
  /// Used for progress reporting and the `expected_size` cross-check.
  accepted_bytes: u32,
  /// Optional caller-declared image size for progress / cross-check.
  expected_size: Option<u32>,
  /// `true` once the first [`HEADER_LEN`] bytes have been validated.
  /// Prevents activating an image with the wrong magic / chip id.
  header_verified: bool,
}

impl<'d> OtaWriter<'d> {
  /// Begin staging an OTA image into the inactive slot.
  ///
  /// `expected_size`, when supplied, lets [`progress`](Self::progress) report
  /// a percentage and lets [`finalize`](Self::finalize) detect truncated
  /// downloads. Pass `None` if the size is not known up-front (e.g.
  /// chunked-transfer-encoded HTTP).
  ///
  /// # Errors
  /// - [`OtaError::SlotNotFound`] if no inactive OTA partition exists.
  /// - [`OtaError::ImageTooLarge`] if `expected_size > slot_size`.
  /// - [`OtaError::Partition`] for partition-table parse failures.
  pub fn begin(mut flash: FlashStorage<'d>, expected_size: Option<u32>) -> Result<Self, OtaError> {
    let (target_slot, slot_base, slot_size) = determine_target(&mut flash)?;

    if let Some(size) = expected_size
      && size > slot_size
    {
      return Err(OtaError::ImageTooLarge {
        image_size: size,
        slot_size,
      });
    }

    info!(
      "OTA writer begin: slot={=?}, base=0x{:x}, size={} bytes, expected={=?}",
      target_slot, slot_base, slot_size, expected_size
    );

    Ok(Self {
      flash,
      slot_base,
      slot_size,
      target_slot,
      // Erased flash reads as 0xFF; pre-fill the buffer to match so a
      // partial trailing sector flushes with valid padding.
      //
      // Built via `vec!` → `into_boxed_slice` → `try_into` to keep the
      // 4 KiB allocation off the stack (would otherwise trip the
      // crate-wide `deny(clippy::large_stack_frames)`).
      sector_buf: sector_buffer(),
      buf_used: 0,
      flushed_sectors: 0,
      accepted_bytes: 0,
      expected_size,
      header_verified: false,
    })
  }

  /// Append `chunk` bytes to the staged image. Yields to the executor after
  /// each completed sector flush.
  ///
  /// The first [`HEADER_LEN`] bytes are inspected against
  /// [`ESP_IMAGE_MAGIC`] / [`EXPECTED_CHIP_ID`] before any sector is
  /// activated; a bad header causes [`OtaError::BadImageHeader`] and the
  /// caller can [`abort`](Self::abort) without touching OTA-data.
  ///
  /// # Errors
  /// - [`OtaError::BadImageHeader`] if the first 24 bytes are not a valid
  ///   ESP image header for this chip.
  /// - [`OtaError::ImageTooLarge`] if the cumulative bytes would overflow
  ///   the target slot.
  /// - [`OtaError::Flash`] for SPI flash erase/program failures.
  pub async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), OtaError> {
    let mut remaining = chunk;
    while !remaining.is_empty() {
      let space = SECTOR_SIZE - self.buf_used;
      let take = remaining.len().min(space);
      self.sector_buf[self.buf_used..self.buf_used + take].copy_from_slice(&remaining[..take]);
      self.buf_used += take;
      self.accepted_bytes = self.accepted_bytes.saturating_add(take as u32);
      remaining = &remaining[take..];

      // Validate the image header as soon as we've accumulated enough
      // bytes, but BEFORE the first sector flush. We only ever flush full
      // sectors (4096 ≫ 24), so the header always lands inside the first
      // sector and stays available in `sector_buf`.
      if !self.header_verified && self.buf_used >= HEADER_LEN {
        verify_image_header(&self.sector_buf[..HEADER_LEN])?;
        self.header_verified = true;
        info!("OTA image header OK (magic=0xE9, chip=ESP32-C6)");
      }

      if self.buf_used == SECTOR_SIZE {
        self.flush_sector()?;
        // Cooperative yield so embassy can run the WiFi driver / WDT feeder.
        Timer::after_micros(0).await;
      }
    }
    Ok(())
  }

  /// Bytes accepted so far (`write_chunk` lengths summed). Useful for UI
  /// progress.
  #[allow(
    dead_code,
    reason = "Currently superseded by OtaProgress state machine; kept as a stable \
      API for future external consumers (CLI tooling, integration tests)"
  )]
  pub fn bytes_written(&self) -> u32 {
    self.accepted_bytes
  }

  /// Progress as 0..=100 if `expected_size` was given to [`begin`](Self::begin).
  #[allow(
    dead_code,
    reason = "Same rationale as `bytes_written`: convenience accessor exposed for \
      callers that don't subscribe to the OtaProgress state machine"
  )]
  pub fn progress_percent(&self) -> Option<u8> {
    let expected = self.expected_size?;
    if expected == 0 {
      return Some(100);
    }
    let pct = (u64::from(self.accepted_bytes) * 100 / u64::from(expected)).min(100);
    Some(pct as u8)
  }

  /// Pad-and-flush the trailing partial sector, then mark the new slot as
  /// the bootloader's next target. Returns the released `FlashStorage`
  /// handle.
  ///
  /// # Errors
  /// - [`OtaError::SizeMismatch`] if `expected_size` was given and does not
  ///   match `accepted_bytes`.
  /// - [`OtaError::Flash`] for the final program / OTA-data write.
  /// - [`OtaError::Partition`] for OTA-data parse failures.
  pub fn finalize(mut self) -> Result<FlashStorage<'d>, OtaError> {
    // Defence in depth: a stream that ends before HEADER_LEN bytes is
    // certainly not a valid image. `verify_image_header` is normally hit
    // inside `write_chunk`; this branch covers truncated downloads.
    if !self.header_verified {
      return Err(OtaError::BadImageHeader {
        magic: self.sector_buf.first().copied().unwrap_or(0),
        chip_id: 0,
      });
    }

    if let Some(expected) = self.expected_size
      && expected != self.accepted_bytes
    {
      return Err(OtaError::SizeMismatch {
        expected,
        received: self.accepted_bytes,
      });
    }

    // Tail bytes (if any) get flushed with their 0xFF padding; the
    // bootloader image format allows trailing 0xFF.
    if self.buf_used > 0 {
      self.flush_sector()?;
    }

    info!(
      "OTA writer finalize: activating slot {=?} ({} bytes, {} sectors)",
      self.target_slot, self.accepted_bytes, self.flushed_sectors
    );

    // Re-open OtaUpdater so `next_partition` recomputes the same slot and
    // we can flip OTA-data atomically. The 3 KiB partition-table buffer
    // is heap-allocated so we don't blow the deny(large_stack_frames)
    // budget that's enforced crate-wide.
    let mut pt_buf = pt_buffer();
    let pt_arr = pt_buf_as_array(&mut pt_buf);
    let mut updater = OtaUpdater::new(&mut self.flash, pt_arr)?;
    updater.activate_next_partition()?;
    // Mark the freshly activated image as `New` so the bootloader's
    // rollback machinery (if enabled) knows to flip to `PendingVerify`
    // on first boot.
    updater.set_current_ota_state(OtaImageState::New)?;
    // (no explicit drop: OtaUpdater holds only borrows.)

    Ok(self.flash)
  }

  /// Abort without activating. The partial bytes already programmed remain
  /// in the inactive slot but OTA-data is untouched, so the bootloader keeps
  /// running the current image.
  pub fn abort(self) -> FlashStorage<'d> {
    warn!(
      "OTA writer abort: {} bytes / {} sectors discarded",
      self.accepted_bytes, self.flushed_sectors
    );
    self.flash
  }

  /// Erase + program one full sector. Caller guarantees `buf_used`
  /// is `SECTOR_SIZE` for normal flushes; tail flush in `finalize` may
  /// pass through with `buf_used < SECTOR_SIZE` (the unused tail is
  /// already 0xFF).
  fn flush_sector(&mut self) -> Result<(), OtaError> {
    let abs_offset = self.slot_base + self.flushed_sectors * SECTOR_SIZE as u32;
    let next_end = abs_offset
      .checked_add(SECTOR_SIZE as u32)
      .ok_or(OtaError::ImageTooLarge {
        image_size: self.accepted_bytes,
        slot_size: self.slot_size,
      })?;
    if next_end > self.slot_base + self.slot_size {
      return Err(OtaError::ImageTooLarge {
        image_size: self.accepted_bytes,
        slot_size: self.slot_size,
      });
    }

    debug!(
      "OTA flush sector #{}: erase+write at 0x{:x}",
      self.flushed_sectors, abs_offset
    );

    NorFlash::erase(&mut self.flash, abs_offset, next_end)?;
    NorFlash::write(&mut self.flash, abs_offset, self.sector_buf.as_ref())?;

    self.flushed_sectors += 1;
    self.buf_used = 0;
    // Reset to 0xFF so the next partial flush has correct padding.
    self.sector_buf.fill(0xFF);
    Ok(())
  }
}

/// Resolve the inactive OTA slot (subtype, absolute offset, capacity).
///
/// Re-uses the same 3 KiB partition-table buffer across the two sequential
/// scans (first for `OtaUpdater::next_partition`, then for `find_partition`)
/// to keep stack pressure flat.
fn determine_target(
  flash: &mut FlashStorage<'_>,
) -> Result<(AppPartitionSubType, u32, u32), OtaError> {
  // Heap-allocate the 3 KiB partition-table buffer. A stack array would
  // exceed the crate-wide `deny(clippy::large_stack_frames)` budget.
  let mut buf = pt_buffer();

  // 1) Ask OtaUpdater which slot is inactive (handles current/booted
  //    partition skew correctly).
  let target = {
    let pt_arr = pt_buf_as_array(&mut buf);
    let mut updater = OtaUpdater::new(flash, pt_arr)?;
    let (region, slot) = updater.next_partition()?;
    // We only need the subtype; FlashRegion is dropped here.
    let _ = region;
    slot
  };

  // 2) Re-parse the partition table to extract the target slot's absolute
  //    offset/length (FlashRegion does not expose these as `pub`).
  let pt_arr = pt_buf_as_array(&mut buf);
  let pt = read_partition_table(flash, pt_arr)?;
  let entry = pt
    .find_partition(PartitionType::App(target))?
    .ok_or(OtaError::SlotNotFound)?;
  Ok((target, entry.offset(), entry.len()))
}

/// Allocate the 3 KiB scratch buffer that `read_partition_table` /
/// `OtaUpdater::new` need. Lives on the heap to keep callers' stack
/// frames small.
fn pt_buffer() -> Box<[u8]> {
  vec![0u8; PARTITION_TABLE_MAX_LEN].into_boxed_slice()
}

/// Allocate the 4 KiB sector accumulator pre-filled with 0xFF (the value
/// erased flash reads back as, so a partial flush pads correctly).
fn sector_buffer() -> Box<[u8; SECTOR_SIZE]> {
  let boxed: Box<[u8]> = vec![0xFFu8; SECTOR_SIZE].into_boxed_slice();
  // Length matches by construction; the conversion never fails.
  boxed
    .try_into()
    .unwrap_or_else(|_| unreachable!("sector_buffer length matches SECTOR_SIZE"))
}

/// Borrow the heap buffer as the fixed-size array reference the
/// `esp-bootloader-esp-idf` API expects.
fn pt_buf_as_array(buf: &mut Box<[u8]>) -> &mut [u8; PARTITION_TABLE_MAX_LEN] {
  // Length is set by `pt_buffer()` and cannot drift; the conversion is
  // infallible in practice.
  (&mut **buf)
    .try_into()
    .expect("pt_buffer length matches PARTITION_TABLE_MAX_LEN")
}

/// Validate the first [`HEADER_LEN`] bytes of an OTA stream.
///
/// The ESP image format starts with:
///
/// | Offset | Size | Field         |
/// |--------|------|---------------|
/// | 0x00   | 1    | `magic` = 0xE9|
/// | 0x01   | 1    | segment count |
/// | 0x02   | 1    | spi mode      |
/// | 0x03   | 1    | spi speed/size|
/// | 0x04   | 4    | entry addr    |
/// | 0x08   | 1    | wp pin        |
/// | 0x09   | 3    | spi pin drv   |
/// | 0x0C   | 2    | `chip_id` (LE)|
///
/// We deliberately keep the check minimal — full app-descriptor parsing
/// (project name, version, IDF magic) lives behind the same buffer and
/// will be wired up in a follow-up if/when needed.
fn verify_image_header(header: &[u8]) -> Result<(), OtaError> {
  // Caller is responsible for slicing exactly HEADER_LEN bytes.
  debug_assert_eq!(header.len(), HEADER_LEN);

  let magic = header[0];
  let chip_id = u16::from_le_bytes([header[0x0C], header[0x0D]]);

  if magic != ESP_IMAGE_MAGIC || chip_id != EXPECTED_CHIP_ID {
    warn!(
      "OTA image header rejected: magic=0x{:02x} (want 0xE9), chip_id=0x{:04x} (want 0x{:04x})",
      magic, chip_id, EXPECTED_CHIP_ID
    );
    return Err(OtaError::BadImageHeader { magic, chip_id });
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Build a 24-byte buffer with the supplied `magic` and `chip_id`.
  fn synthetic_header(magic: u8, chip_id: u16) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0] = magic;
    h[1] = 4; // segment count, arbitrary
    let cid = chip_id.to_le_bytes();
    h[0x0C] = cid[0];
    h[0x0D] = cid[1];
    h
  }

  #[test]
  fn accepts_valid_c6_header() {
    let h = synthetic_header(ESP_IMAGE_MAGIC, EXPECTED_CHIP_ID);
    assert!(verify_image_header(&h).is_ok());
  }

  #[test]
  fn rejects_bad_magic() {
    let h = synthetic_header(0xAB, EXPECTED_CHIP_ID);
    assert!(matches!(
      verify_image_header(&h),
      Err(OtaError::BadImageHeader { magic: 0xAB, .. })
    ));
  }

  #[test]
  fn rejects_wrong_chip_id() {
    // 0x0009 is ESP32-S3; flashing it onto a C6 would brick the radio.
    let h = synthetic_header(ESP_IMAGE_MAGIC, 0x0009);
    assert!(matches!(
      verify_image_header(&h),
      Err(OtaError::BadImageHeader {
        chip_id: 0x0009,
        ..
      })
    ));
  }
}
