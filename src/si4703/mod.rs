//! Si4703 FM Radio Receiver Driver
//!
//! A `no_std` compatible driver for the Si4703 FM radio receiver chip,
//! communicating via I2C (2-wire mode).
//!
//! # Features
//! - Tune to specific FM frequencies
//! - Seek/scan for available stations
//! - Volume control (0-15)
//! - Mute/unmute
//! - Mono/stereo mode switching
//! - RSSI (signal strength) reading
//! - RDS (Radio Data System) decoding
//!
//! # Hardware Setup
//! The Si4703 uses a non-standard I2C initialization sequence.
//! The SDIO pin must be pulled low while RST transitions from low to high
//! to enter 2-wire (I2C) mode.
//!
//! Typical connections (MCU-470 board):
//! - SDIO -> I2C SDA
//! - SCLK -> I2C SCL
//! - SEN  -> GND (I2C address 0x10)
//! - RST  -> GPIO (reset control)
//!
//! # Example
//! ```no_run
//! use radio::si4703::{Si4703, FmBand, ChannelSpacing};
//!
//! let mut radio = Si4703::new(FmBand::UsEurope, ChannelSpacing::Spacing100K);
//! // After hardware reset sequence:
//! // radio.init(&mut i2c).await.unwrap();
//! // radio.set_volume(&mut i2c, 8).unwrap();
//! // radio.tune(&mut i2c, 1015).await.unwrap(); // 101.5 MHz
//! ```

use embassy_time::{Duration, Timer};

// ============================================================================
// Constants
// ============================================================================

/// Si4703 I2C address (when SEN pin is tied to GND)
const SI4703_ADDR: u8 = 0x10;

// Register indices
const REG_DEVICE_ID: usize = 0x00;
const REG_CHIP_ID: usize = 0x01;
const REG_POWER_CFG: usize = 0x02;
const REG_CHANNEL: usize = 0x03;
const REG_SYS_CFG1: usize = 0x04;
const REG_SYS_CFG2: usize = 0x05;
const REG_SYS_CFG3: usize = 0x06;
const REG_TEST1: usize = 0x07;
const REG_STATUS_RSSI: usize = 0x0A;
const REG_READ_CHAN: usize = 0x0B;
const REG_RDS_A: usize = 0x0C;
const REG_RDS_B: usize = 0x0D;
const REG_RDS_C: usize = 0x0E;
const REG_RDS_D: usize = 0x0F;

// POWERCFG register bits
const PWR_DSMUTE: u16 = 1 << 15;
const PWR_DMUTE: u16 = 1 << 14;
const PWR_MONO: u16 = 1 << 13;
const PWR_SEEKUP: u16 = 1 << 9;
const PWR_SEEK: u16 = 1 << 8;
const PWR_ENABLE: u16 = 1 << 0;

// CHANNEL register bits
const CHAN_TUNE: u16 = 1 << 15;

// SYSCFG1 register bits
const SYS_RDS_EN: u16 = 1 << 12;

// SYSCFG2 register bits - band and spacing masks
const SYS2_BAND_MASK: u16 = 0x00C0;
const SYS2_SPACE_MASK: u16 = 0x0030;

// STATUS_RSSI register bits
const STATUS_RDSR: u16 = 1 << 15;
const STATUS_STC: u16 = 1 << 14;
const STATUS_SF_BL: u16 = 1 << 13;

// READ_CHAN register bits
const READCHAN_MASK: u16 = 0x03FF;

// TEST1 register bits
const TEST1_XOSCEN: u16 = 1 << 15;

// ============================================================================
// Public Types
// ============================================================================

/// FM band configuration
#[derive(Clone, Copy, Debug, defmt::Format)]
pub enum FmBand {
  /// US/Europe: 87.5 - 108.0 MHz
  UsEurope,
  /// Japan Wide: 76.0 - 108.0 MHz
  JapanWide,
  /// Japan: 76.0 - 90.0 MHz
  Japan,
}

impl FmBand {
  /// Get the bottom frequency of this band (in MHz * 10)
  pub fn bottom_freq_mhz_x10(self) -> u16 {
    match self {
      FmBand::UsEurope => 875,
      FmBand::JapanWide | FmBand::Japan => 760,
    }
  }

  /// Get the top frequency of this band (in MHz * 10)
  pub fn top_freq_mhz_x10(self) -> u16 {
    match self {
      FmBand::UsEurope | FmBand::JapanWide => 1080,
      FmBand::Japan => 900,
    }
  }

  fn register_value(self) -> u16 {
    match self {
      FmBand::UsEurope => 0x0000,
      FmBand::JapanWide => 0x0040,
      FmBand::Japan => 0x0080,
    }
  }
}

/// Channel spacing configuration
#[derive(Clone, Copy, Debug, defmt::Format)]
pub enum ChannelSpacing {
  /// 200 kHz (US/Australia)
  Spacing200K,
  /// 100 kHz (Europe/Japan)
  Spacing100K,
  /// 50 kHz
  Spacing50K,
}

impl ChannelSpacing {
  /// Get the spacing in kHz
  pub fn khz(self) -> u16 {
    match self {
      ChannelSpacing::Spacing200K => 200,
      ChannelSpacing::Spacing100K => 100,
      ChannelSpacing::Spacing50K => 50,
    }
  }

  fn register_value(self) -> u16 {
    match self {
      ChannelSpacing::Spacing200K => 0x0000,
      ChannelSpacing::Spacing100K => 0x0010,
      ChannelSpacing::Spacing50K => 0x0020,
    }
  }
}

/// Station information discovered during scan
#[derive(Clone, Copy, Debug, defmt::Format)]
pub struct Station {
  /// Frequency in MHz * 10 (e.g., 1015 = 101.5 MHz)
  pub freq_mhz_x10: u16,
  /// Signal strength (RSSI) 0-75
  pub rssi: u8,
}

impl Station {
  /// Create a default (empty) station entry
  pub const fn empty() -> Self {
    Self {
      freq_mhz_x10: 0,
      rssi: 0,
    }
  }

  /// Get the frequency as a formatted tuple (integer_part, decimal_part)
  /// e.g., 1015 -> (101, 5)
  pub fn freq_parts(self) -> (u16, u16) {
    (self.freq_mhz_x10 / 10, self.freq_mhz_x10 % 10)
  }
}

/// Seek direction
#[derive(Clone, Copy, Debug, defmt::Format)]
pub enum SeekDirection {
  /// Seek towards higher frequencies
  Up,
  /// Seek towards lower frequencies
  Down,
}

// ============================================================================
// Si4703 Driver
// ============================================================================

/// Si4703 FM radio receiver driver.
///
/// This driver communicates with the Si4703 chip via I2C and provides
/// high-level methods for controlling the radio.
pub struct Si4703 {
  regs: [u16; 16],
  band: FmBand,
  spacing: ChannelSpacing,
}

impl Si4703 {
  /// Create a new Si4703 driver instance.
  ///
  /// # Arguments
  /// - `band`: FM band to use (determines frequency range)
  /// - `spacing`: Channel spacing (determines tuning step size)
  pub fn new(band: FmBand, spacing: ChannelSpacing) -> Self {
    Self {
      regs: [0u16; 16],
      band,
      spacing,
    }
  }

  /// Get the configured FM band
  pub fn band(&self) -> FmBand {
    self.band
  }

  /// Get the configured channel spacing
  pub fn spacing(&self) -> ChannelSpacing {
    self.spacing
  }

  /// Initialize the Si4703 chip.
  ///
  /// This must be called after the hardware reset sequence has been performed
  /// (SDIO held low while RST transitions low -> high).
  ///
  /// The initialization sequence:
  /// 1. Enable crystal oscillator
  /// 2. Power up the device
  /// 3. Enable RDS
  /// 4. Configure band, spacing, and seek thresholds
  pub async fn init<I2C>(&mut self, i2c: &mut I2C) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    // Read current register state
    self.read_registers(i2c)?;

    // Enable the oscillator (TEST1 register)
    self.regs[REG_TEST1] |= TEST1_XOSCEN;
    self.write_registers(i2c)?;

    // Wait for oscillator to stabilize (500ms per datasheet)
    Timer::after(Duration::from_millis(500)).await;

    // Read registers again
    self.read_registers(i2c)?;

    // Power up the device
    self.regs[REG_POWER_CFG] = PWR_DSMUTE | PWR_DMUTE | PWR_ENABLE;
    self.write_registers(i2c)?;

    // Wait for powerup (110ms per datasheet)
    Timer::after(Duration::from_millis(110)).await;

    // Read registers again
    self.read_registers(i2c)?;

    // Enable RDS
    self.regs[REG_SYS_CFG1] |= SYS_RDS_EN;

    // Configure band and spacing
    self.regs[REG_SYS_CFG2] &= !(SYS2_BAND_MASK | SYS2_SPACE_MASK);
    self.regs[REG_SYS_CFG2] |= self.band.register_value() | self.spacing.register_value();

    // Set volume to a moderate level (0-15)
    self.regs[REG_SYS_CFG2] &= 0xFFF0;
    self.regs[REG_SYS_CFG2] |= 0x0005; // Volume = 5

    // Set seek threshold (SEEKTH = 25, lower = more sensitive)
    self.regs[REG_SYS_CFG2] &= 0x00FF;
    self.regs[REG_SYS_CFG2] |= 0x1900;

    // Configure SYSCFG3 for seek SNR and impulse thresholds
    self.regs[REG_SYS_CFG3] = 0x0004; // SKSNR=0, SKCNT=4

    self.write_registers(i2c)?;

    Timer::after(Duration::from_millis(110)).await;

    Ok(())
  }

  /// Set volume level (0-15).
  ///
  /// Values above 15 will be clamped to 15.
  pub fn set_volume<I2C>(&mut self, i2c: &mut I2C, volume: u8) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    let vol = volume.min(15) as u16;
    self.read_registers(i2c)?;
    self.regs[REG_SYS_CFG2] &= 0xFFF0;
    self.regs[REG_SYS_CFG2] |= vol;
    self.write_registers(i2c)
  }

  /// Get current volume level (0-15)
  pub fn volume(&self) -> u8 {
    (self.regs[REG_SYS_CFG2] & 0x000F) as u8
  }

  /// Mute or unmute the audio output.
  ///
  /// When muted, the audio output is silenced but the radio continues to operate.
  pub fn set_mute<I2C>(&mut self, i2c: &mut I2C, mute: bool) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    if mute {
      self.regs[REG_POWER_CFG] &= !PWR_DMUTE;
    } else {
      self.regs[REG_POWER_CFG] |= PWR_DMUTE;
    }
    self.write_registers(i2c)
  }

  /// Set mono or stereo mode.
  ///
  /// Mono mode can improve reception in weak signal areas.
  pub fn set_mono<I2C>(&mut self, i2c: &mut I2C, mono: bool) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    if mono {
      self.regs[REG_POWER_CFG] |= PWR_MONO;
    } else {
      self.regs[REG_POWER_CFG] &= !PWR_MONO;
    }
    self.write_registers(i2c)
  }

  /// Tune to a specific frequency.
  ///
  /// # Arguments
  /// - `freq_mhz_x10`: Frequency in MHz * 10 (e.g., 1015 = 101.5 MHz)
  ///
  /// This method blocks (async) until the tune operation completes or times out.
  pub async fn tune<I2C>(&mut self, i2c: &mut I2C, freq_mhz_x10: u16) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    let bottom = self.band.bottom_freq_mhz_x10();
    let spacing_khz = self.spacing.khz();

    // Calculate channel number
    // channel = (freq_mhz_x10 - bottom) * 100 / spacing_khz
    let channel = ((freq_mhz_x10 - bottom) as u32 * 100 / spacing_khz as u32) as u16;

    self.read_registers(i2c)?;

    // Set channel and TUNE bit
    self.regs[REG_CHANNEL] &= 0xFE00;
    self.regs[REG_CHANNEL] |= channel | CHAN_TUNE;
    self.write_registers(i2c)?;

    // Wait for Seek/Tune Complete (STC) flag
    self.wait_stc(i2c).await?;

    // Clear TUNE bit
    self.read_registers(i2c)?;
    self.regs[REG_CHANNEL] &= !CHAN_TUNE;
    self.write_registers(i2c)?;

    Ok(())
  }

  /// Get the currently tuned frequency (in MHz * 10).
  ///
  /// Returns the frequency that the radio is currently tuned to.
  pub fn current_frequency<I2C>(&mut self, i2c: &mut I2C) -> Result<u16, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    let channel = self.regs[REG_READ_CHAN] & READCHAN_MASK;
    let bottom = self.band.bottom_freq_mhz_x10();
    let spacing_khz = self.spacing.khz();

    let freq = bottom + (channel as u32 * spacing_khz as u32 / 100) as u16;
    Ok(freq)
  }

  /// Get the current RSSI (Received Signal Strength Indicator) value.
  ///
  /// Returns a value between 0 and 75, where higher values indicate stronger signals.
  pub fn rssi<I2C>(&mut self, i2c: &mut I2C) -> Result<u8, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    Ok((self.regs[REG_STATUS_RSSI] & 0x00FF) as u8)
  }

  /// Seek to the next station in the specified direction.
  ///
  /// Returns the frequency found (MHz * 10) or `None` if seek failed
  /// (reached band limit without finding a station).
  pub async fn seek<I2C>(
    &mut self,
    i2c: &mut I2C,
    direction: SeekDirection,
  ) -> Result<Option<u16>, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;

    // Configure seek direction and start seek
    self.regs[REG_POWER_CFG] &= !(PWR_SEEKUP | PWR_SEEK);
    if matches!(direction, SeekDirection::Up) {
      self.regs[REG_POWER_CFG] |= PWR_SEEKUP;
    }
    self.regs[REG_POWER_CFG] |= PWR_SEEK;
    self.write_registers(i2c)?;

    // Wait for STC
    self.wait_stc(i2c).await?;

    // Check if seek failed (SF/BL bit)
    let seek_failed = (self.regs[REG_STATUS_RSSI] & STATUS_SF_BL) != 0;

    // Clear SEEK bit
    self.regs[REG_POWER_CFG] &= !PWR_SEEK;
    self.write_registers(i2c)?;

    if seek_failed {
      return Ok(None);
    }

    // Read the found frequency
    let freq = self.current_frequency(i2c)?;
    Ok(Some(freq))
  }

  /// Scan the entire band and return a list of found stations.
  ///
  /// Fills the provided `stations` slice with discovered stations and returns
  /// the number of stations found.
  ///
  /// # Arguments
  /// - `stations`: Buffer to store discovered stations
  ///
  /// # Returns
  /// The number of stations found (up to `stations.len()`)
  pub async fn scan_stations<I2C>(
    &mut self,
    i2c: &mut I2C,
    stations: &mut [Station],
  ) -> Result<usize, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    let mut count = 0;
    let max_stations = stations.len();

    // Start from the bottom of the band
    let start_freq = self.band.bottom_freq_mhz_x10();
    self.tune(i2c, start_freq).await?;

    // Seek up through the entire band
    loop {
      if count >= max_stations {
        break;
      }

      match self.seek(i2c, SeekDirection::Up).await? {
        Some(freq) => {
          // Check if we've wrapped around to the beginning
          if count > 0 && freq <= stations[0].freq_mhz_x10 {
            break;
          }

          let rssi = self.rssi(i2c)?;
          stations[count] = Station {
            freq_mhz_x10: freq,
            rssi,
          };
          count += 1;
        }
        None => {
          // Seek failed - reached end of band
          break;
        }
      }
    }

    Ok(count)
  }

  /// Read RDS data if available.
  ///
  /// Returns the four RDS blocks (A, B, C, D) if new data is ready,
  /// or `None` if no new RDS data is available.
  pub fn read_rds<I2C>(&mut self, i2c: &mut I2C) -> Result<Option<(u16, u16, u16, u16)>, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;

    if (self.regs[REG_STATUS_RSSI] & STATUS_RDSR) != 0 {
      Ok(Some((
        self.regs[REG_RDS_A],
        self.regs[REG_RDS_B],
        self.regs[REG_RDS_C],
        self.regs[REG_RDS_D],
      )))
    } else {
      Ok(None)
    }
  }

  /// Get device ID for chip verification.
  ///
  /// Expected value for Si4703: 0x1242
  pub fn device_id(&self) -> u16 {
    self.regs[REG_DEVICE_ID]
  }

  /// Get chip ID for chip verification.
  pub fn chip_id(&self) -> u16 {
    self.regs[REG_CHIP_ID]
  }

  // ========================================================================
  // Private methods
  // ========================================================================

  /// Read all 16 registers from Si4703.
  /// Si4703 returns data starting from register 0x0A, wrapping around to 0x09.
  fn read_registers<I2C>(&mut self, i2c: &mut I2C) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    let mut buf = [0u8; 32];
    i2c.read(SI4703_ADDR, &mut buf)?;

    // Data comes in order: 0x0A, 0x0B, ..., 0x0F, 0x00, 0x01, ..., 0x09
    let mut idx = 0;
    for reg in 0x0A..=0x0F {
      self.regs[reg] = u16::from_be_bytes([buf[idx], buf[idx + 1]]);
      idx += 2;
    }
    for reg in 0x00..=0x09 {
      self.regs[reg] = u16::from_be_bytes([buf[idx], buf[idx + 1]]);
      idx += 2;
    }
    Ok(())
  }

  /// Write registers 0x02 through 0x07 to Si4703.
  fn write_registers<I2C>(&self, i2c: &mut I2C) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    let mut buf = [0u8; 12]; // 6 registers * 2 bytes
    for i in 0..6 {
      let reg = self.regs[0x02 + i];
      buf[i * 2] = (reg >> 8) as u8;
      buf[i * 2 + 1] = reg as u8;
    }
    i2c.write(SI4703_ADDR, &buf)
  }

  /// Wait for Seek/Tune Complete (STC) flag with timeout (5 seconds)
  async fn wait_stc<I2C>(&mut self, i2c: &mut I2C) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    for _ in 0..100 {
      Timer::after(Duration::from_millis(50)).await;
      self.read_registers(i2c)?;
      if (self.regs[REG_STATUS_RSSI] & STATUS_STC) != 0 {
        return Ok(());
      }
    }
    // Timeout - return anyway
    Ok(())
  }
}

// ============================================================================
// RDS Decoder
// ============================================================================

/// Simple RDS (Radio Data System) decoder.
///
/// Currently supports decoding the Programme Service (PS) name,
/// which is the 8-character station name broadcast by FM stations.
pub struct RdsDecoder {
  /// Programme Service name (8 characters)
  ps_name: [u8; 8],
  /// Bitmask of which PS segments have been received
  ps_valid: u8,
}

impl RdsDecoder {
  /// Create a new RDS decoder instance
  pub fn new() -> Self {
    Self {
      ps_name: [b' '; 8],
      ps_valid: 0,
    }
  }

  /// Process an RDS data block.
  ///
  /// Feed the four RDS blocks obtained from [`Si4703::read_rds`] into this method.
  /// Returns `true` if the PS (station) name is now complete.
  pub fn process(&mut self, _block_a: u16, block_b: u16, _block_c: u16, block_d: u16) -> bool {
    // Group type is in bits 15-12 of block B
    let group_type = (block_b >> 12) & 0x0F;

    // Group 0A/0B contains Programme Service name
    if group_type == 0 {
      let segment = (block_b & 0x03) as usize;
      self.ps_name[segment * 2] = (block_d >> 8) as u8;
      self.ps_name[segment * 2 + 1] = (block_d & 0xFF) as u8;
      self.ps_valid |= 1 << segment;
    }

    // All 4 segments received
    self.ps_valid == 0x0F
  }

  /// Get the decoded station name bytes (may be incomplete).
  ///
  /// The name is 8 characters. Unreceived segments will contain spaces.
  pub fn station_name(&self) -> &[u8; 8] {
    &self.ps_name
  }

  /// Get the station name as a string slice (best effort, replaces invalid UTF-8 with spaces)
  pub fn station_name_str(&self) -> &str {
    core::str::from_utf8(&self.ps_name).unwrap_or("        ")
  }

  /// Check if the station name is complete (all 4 segments received)
  pub fn is_complete(&self) -> bool {
    self.ps_valid == 0x0F
  }

  /// Reset the decoder for a new station.
  ///
  /// Call this when tuning to a different frequency.
  pub fn reset(&mut self) {
    self.ps_name = [b' '; 8];
    self.ps_valid = 0;
  }
}

impl Default for RdsDecoder {
  fn default() -> Self {
    Self::new()
  }
}

// ============================================================================
// Utility Functions
// ============================================================================

/// Format a frequency value (MHz * 10) into integer and decimal parts.
///
/// # Example
/// ```
/// let (integer, decimal) = radio::si4703::format_freq(1015);
/// // integer = 101, decimal = 5 -> "101.5 MHz"
/// ```
pub fn format_freq(freq_mhz_x10: u16) -> (u16, u16) {
  (freq_mhz_x10 / 10, freq_mhz_x10 % 10)
}
