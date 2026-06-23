//! WiFi Credentials Persistent Storage
//!
//! Provides Flash-based persistent storage for WiFi credentials using `esp-storage`.
//! Credentials are stored in a dedicated Flash sector with magic number and CRC32
//! validation to ensure data integrity.
//!
//! # Storage Layout (within one 4KB sector)
//!
//! | Offset | Size | Description |
//! |--------|------|-------------|
//! | 0x00   | 4    | Magic number (0xCAFE_F00D) |
//! | 0x04   | 4    | CRC32 of payload |
//! | 0x08   | 1    | SSID length (max 32) |
//! | 0x09   | 32   | SSID bytes |
//! | 0x29   | 1    | Password length (max 64) |
//! | 0x2A   | 64   | Password bytes |
//! | 0x6A   | ...  | Reserved |

use alloc::string::String;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_storage::FlashStorage;

use super::WifiCredentials;

extern crate alloc;

/// Magic number to identify valid stored credentials.
const MAGIC: u32 = 0xCAFE_F00D;

/// Maximum SSID length in bytes.
const MAX_SSID_LEN: usize = 32;

/// Maximum password length in bytes.
const MAX_PASSWORD_LEN: usize = 64;

/// Total payload size: ssid_len(1) + ssid(32) + pass_len(1) + pass(64) = 98 bytes.
const PAYLOAD_SIZE: usize = 1 + MAX_SSID_LEN + 1 + MAX_PASSWORD_LEN;

/// Header size: magic(4) + crc(4) = 8 bytes.
const HEADER_SIZE: usize = 8;

/// Total record size (must be word-aligned, round up to 4-byte boundary).
/// 8 + 98 = 106, round up to 108.
const RECORD_SIZE: usize = 108;

/// Default storage offset: use the last 4KB sector before the 2MB boundary.
/// This avoids conflicting with application code and bootloader.
/// For a 4MB flash, this is at offset 0x3FF000 (last sector).
/// Users can customize this via `CredentialStorage::with_offset`.
const DEFAULT_STORAGE_OFFSET: u32 = 0x3F_F000;

/// Errors that can occur during credential storage operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, defmt::Format)]
pub enum StorageError {
  /// Flash read/write/erase operation failed.
  FlashError,
  /// Stored data has invalid magic number (no credentials saved).
  InvalidMagic,
  /// Stored data failed CRC validation (corrupted).
  CrcMismatch,
  /// SSID length exceeds maximum.
  SsidTooLong,
  /// Password length exceeds maximum.
  PasswordTooLong,
}

/// Persistent storage for WiFi credentials.
///
/// Uses a single Flash sector to store SSID and password with
/// magic number and CRC32 integrity checking.
pub struct CredentialStorage<'d> {
  flash: FlashStorage<'d>,
  offset: u32,
}

impl<'d> CredentialStorage<'d> {
  /// Create a new credential storage with default offset.
  ///
  /// The default offset is the last 4KB sector of a 4MB flash (0x3FF000).
  /// Make sure this sector is not used by your application or bootloader.
  pub fn new(flash: FlashStorage<'d>) -> Self {
    Self {
      flash,
      offset: DEFAULT_STORAGE_OFFSET,
    }
  }

  /// Create a new credential storage with a custom Flash offset.
  ///
  /// The offset must be sector-aligned (multiple of 4096).
  ///
  /// # Panics
  ///
  /// Panics if offset is not sector-aligned.
  pub fn with_offset(flash: FlashStorage<'d>, offset: u32) -> Self {
    assert!(
      offset % FlashStorage::SECTOR_SIZE == 0,
      "storage offset must be sector-aligned"
    );
    Self { flash, offset }
  }

  /// Load WiFi credentials from Flash.
  ///
  /// Returns `Ok(Some(credentials))` if valid credentials are stored,
  /// `Ok(None)` if no credentials are stored (empty/erased flash),
  /// or `Err(StorageError)` if an error occurs.
  pub fn load(&mut self) -> Result<Option<WifiCredentials>, StorageError> {
    // Read the full record from flash
    let mut buf = [0u8; RECORD_SIZE];
    self
      .flash
      .read(self.offset, &mut buf)
      .map_err(|_| StorageError::FlashError)?;

    // Check magic number
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != MAGIC {
      // No valid data stored (flash is erased = 0xFF, or never written)
      return Ok(None);
    }

    // Extract stored CRC
    let stored_crc = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);

    // Calculate CRC of payload
    let payload = &buf[HEADER_SIZE..HEADER_SIZE + PAYLOAD_SIZE];
    let calculated_crc = crc32(payload);

    if stored_crc != calculated_crc {
      return Err(StorageError::CrcMismatch);
    }

    // Parse SSID
    let ssid_len = payload[0] as usize;
    if ssid_len > MAX_SSID_LEN {
      return Err(StorageError::CrcMismatch);
    }
    let ssid_bytes = &payload[1..1 + ssid_len];

    // Parse password
    let pass_offset = 1 + MAX_SSID_LEN;
    let pass_len = payload[pass_offset] as usize;
    if pass_len > MAX_PASSWORD_LEN {
      return Err(StorageError::CrcMismatch);
    }
    let pass_bytes = &payload[pass_offset + 1..pass_offset + 1 + pass_len];

    // Convert to strings
    let ssid = core::str::from_utf8(ssid_bytes).map_err(|_| StorageError::CrcMismatch)?;
    let password = core::str::from_utf8(pass_bytes).map_err(|_| StorageError::CrcMismatch)?;

    Ok(Some(WifiCredentials {
      ssid: String::from(ssid),
      password: String::from(password),
    }))
  }

  /// Save WiFi credentials to Flash.
  ///
  /// This will erase the storage sector and write the new credentials.
  pub fn save(&mut self, credentials: &WifiCredentials) -> Result<(), StorageError> {
    let ssid_bytes = credentials.ssid.as_bytes();
    let pass_bytes = credentials.password.as_bytes();

    if ssid_bytes.len() > MAX_SSID_LEN {
      return Err(StorageError::SsidTooLong);
    }
    if pass_bytes.len() > MAX_PASSWORD_LEN {
      return Err(StorageError::PasswordTooLong);
    }

    // Build payload
    let mut payload = [0u8; PAYLOAD_SIZE];
    payload[0] = ssid_bytes.len() as u8;
    payload[1..1 + ssid_bytes.len()].copy_from_slice(ssid_bytes);

    let pass_offset = 1 + MAX_SSID_LEN;
    payload[pass_offset] = pass_bytes.len() as u8;
    payload[pass_offset + 1..pass_offset + 1 + pass_bytes.len()].copy_from_slice(pass_bytes);

    // Calculate CRC
    let crc = crc32(&payload);

    // Build full record (word-aligned buffer)
    let mut record = [0u8; RECORD_SIZE];
    record[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    record[4..8].copy_from_slice(&crc.to_le_bytes());
    record[HEADER_SIZE..HEADER_SIZE + PAYLOAD_SIZE].copy_from_slice(&payload);

    // Erase the sector first
    self
      .flash
      .erase(self.offset, self.offset + FlashStorage::SECTOR_SIZE)
      .map_err(|_| StorageError::FlashError)?;

    // Write the record
    self
      .flash
      .write(self.offset, &record)
      .map_err(|_| StorageError::FlashError)?;

    Ok(())
  }

  /// Clear stored credentials by erasing the storage sector.
  pub fn clear(&mut self) -> Result<(), StorageError> {
    self
      .flash
      .erase(self.offset, self.offset + FlashStorage::SECTOR_SIZE)
      .map_err(|_| StorageError::FlashError)?;
    Ok(())
  }

  /// Check if valid credentials are stored without fully parsing them.
  pub fn has_credentials(&mut self) -> bool {
    let mut buf = [0u8; HEADER_SIZE];
    if self.flash.read(self.offset, &mut buf).is_err() {
      return false;
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    magic == MAGIC
  }
}

/// Simple CRC32 implementation (no lookup table, suitable for embedded).
///
/// Uses the standard CRC-32/ISO-HDLC polynomial (0xEDB88320).
fn crc32(data: &[u8]) -> u32 {
  let mut crc: u32 = 0xFFFF_FFFF;
  for &byte in data {
    crc ^= byte as u32;
    for _ in 0..8 {
      if crc & 1 != 0 {
        crc = (crc >> 1) ^ 0xEDB8_8320;
      } else {
        crc >>= 1;
      }
    }
  }
  !crc
}
