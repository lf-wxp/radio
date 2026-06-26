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

extern crate alloc;

use alloc::string::String;

use embassy_time::{Duration, Timer};

// ============================================================================
// Public Types
// ============================================================================

/// RDS data block types: (Block A, Block B, Block C, Block D)
pub type RdsBlocks = (u16, u16, u16, u16);

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
/// STATUSRSSI bit 8: receive mode (1 = stereo, 0 = mono).
const STATUS_ST: u16 = 1 << 8;

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
    let stc_ok = self.wait_stc(i2c).await?;
    if !stc_ok {
      defmt::warn!(
        "tune: STC timeout, frequency {} may not be tuned successfully",
        freq_mhz_x10
      );
    }

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

  /// Read RSSI **and** the stereo-receive flag in a single I2C transaction.
  ///
  /// Equivalent to calling [`Si4703::rssi`] followed by [`Si4703::is_stereo`]
  /// but cuts the I2C traffic in half — important on the 200 ms refresh
  /// loop where every `read_registers` is ~3 ms of blocking I/O.
  ///
  /// Returns `(rssi, stereo)` where `rssi` is in the range 0..=75 and
  /// `stereo` is `true` when the receiver is currently locked to a
  /// stereo pilot tone.
  pub fn rssi_stereo<I2C>(&mut self, i2c: &mut I2C) -> Result<(u8, bool), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    let rssi = (self.regs[REG_STATUS_RSSI] & 0x00FF) as u8;
    let stereo = (self.regs[REG_STATUS_RSSI] & STATUS_ST) != 0;
    Ok((rssi, stereo))
  }

  /// Whether the receiver is currently locked onto a stereo pilot tone.
  ///
  /// Triggers an I2C read of the STATUSRSSI register; prefer
  /// [`Si4703::rssi_stereo`] when both values are needed.
  pub fn is_stereo<I2C>(&mut self, i2c: &mut I2C) -> Result<bool, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    self.read_registers(i2c)?;
    Ok((self.regs[REG_STATUS_RSSI] & STATUS_ST) != 0)
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
    let stc_ok = self.wait_stc(i2c).await?;

    // Check if seek failed (SF/BL bit) or timed out
    let seek_failed = !stc_ok || (self.regs[REG_STATUS_RSSI] & STATUS_SF_BL) != 0;

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

  /// Sweep the entire band, sampling RSSI at evenly-spaced points.
  ///
  /// Unlike [`scan_stations`], which uses the chip's hardware seek to
  /// jump between detected carriers, this method **forces a tune to
  /// every bucket**, giving a dense RSSI map suitable for drawing a
  /// spectrum view of the FM band.
  ///
  /// The output buffer length determines the resolution: with `N`
  /// buckets the band is divided into `N` equal slots from
  /// `bottom_freq` to `top_freq`, and each `out[i]` receives the RSSI
  /// reported by the chip immediately after tuning the centre of slot
  /// `i`. RSSI is in the range `0..=75` (Si4703 hardware contract).
  ///
  /// # Cost
  ///
  /// Each bucket costs one `tune` (~60 ms STC wait) plus one
  /// `read_registers` (~3 ms). For `N = 52` buckets that is about
  /// 3.3 s of blocking I²C/I/O, which is fine for a one-shot boot-time
  /// sweep but **not** suitable for a hot loop.
  ///
  /// The function never panics on a short buffer; it simply skips the
  /// work when `out.is_empty()`.
  ///
  /// [`scan_stations`]: Si4703::scan_stations
  pub async fn sweep_rssi<I2C>(&mut self, i2c: &mut I2C, out: &mut [u8]) -> Result<(), I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    if out.is_empty() {
      return Ok(());
    }

    let bottom = self.band.bottom_freq_mhz_x10();
    let top = self.band.top_freq_mhz_x10();
    // Inclusive span in 0.1 MHz units. Both bounds are < 1100, so the
    // u16 subtraction is safe and the product fits in u32 well below
    // overflow even for `out.len() == u16::MAX`.
    let span_x10 = u32::from(top.saturating_sub(bottom));
    let n = out.len() as u32;

    for (i, slot) in out.iter_mut().enumerate() {
      // Centre frequency of bucket `i`, computed as
      //     bottom + span * (2i + 1) / (2N)
      // i.e. the midpoint of slot `i` in floating-point space, but
      // implemented in u32 arithmetic to avoid pulling in a softfloat
      // library on a no_std target. The numerator stays under
      // `span_x10 * 2N` < 2^17 for any reasonable N, so no overflow.
      let mid = u32::from(bottom) + (span_x10 * (2 * i as u32 + 1)) / (2 * n);
      let freq = (mid.min(u32::from(top))) as u16;
      // A failed tune leaves the chip on the previous channel; record
      // 0 for that bucket and continue so a single I²C glitch doesn't
      // abort the whole sweep.
      match self.tune(i2c, freq).await {
        Ok(()) => {
          *slot = self.rssi(i2c).unwrap_or(0);
        }
        Err(_) => *slot = 0,
      }
    }
    Ok(())
  }

  /// Read RDS data if available.
  ///
  /// Returns the four RDS blocks (A, B, C, D) if new data is ready,
  /// or `None` if no new RDS data is available.
  pub fn read_rds<I2C>(&mut self, i2c: &mut I2C) -> Result<Option<RdsBlocks>, I2C::Error>
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

  /// Wait for Seek/Tune Complete (STC) flag with timeout (5 seconds).
  ///
  /// Returns `Ok(true)` if STC was set within the timeout,
  /// `Ok(false)` if the operation timed out without STC being set.
  async fn wait_stc<I2C>(&mut self, i2c: &mut I2C) -> Result<bool, I2C::Error>
  where
    I2C: embedded_hal::i2c::I2c,
  {
    for _ in 0..100 {
      Timer::after(Duration::from_millis(50)).await;
      self.read_registers(i2c)?;
      if (self.regs[REG_STATUS_RSSI] & STATUS_STC) != 0 {
        return Ok(true);
      }
    }
    // Timeout: STC flag was not set within 5 seconds
    defmt::warn!("wait_stc: STC timeout (5s)");
    Ok(false)
  }
}

// ============================================================================
// RDS Decoder
// ============================================================================

/// Maximum number of bytes a Group 2A RadioText message can carry
/// (16 segments * 4 bytes = 64). Group 2B is shorter (32) but fits the
/// same buffer.
pub const RT_MAX_LEN: usize = 64;

/// Decoded RDS Clock-Time payload (Group 4A).
///
/// The wire protocol carries UTC hour/minute together with a *local*
/// timezone offset measured in half-hours (so e.g. China = `+16` →
/// UTC+8h). The MJD (Modified Julian Date) part is intentionally not
/// surfaced here: the UI only displays `HH:MM`, and skipping the date
/// math keeps this MCU-friendly.
#[derive(Clone, Copy, Debug, defmt::Format, PartialEq, Eq)]
pub struct RdsClockTime {
  /// UTC hour, range `0..=23`.
  pub utc_hour: u8,
  /// UTC minute, range `0..=59`.
  pub utc_minute: u8,
  /// Local-time offset in half-hours, range `-24..=24`
  /// (sign bit + 5-bit magnitude as transmitted).
  pub local_offset_half_hours: i8,
}

impl RdsClockTime {
  /// Return the local hour after applying [`local_offset_half_hours`],
  /// wrapped into `0..24`. Minutes are also returned because a 30-minute
  /// offset (e.g. India `+11`, Newfoundland `-7`) can roll the minute
  /// component into the next hour.
  pub fn local_hh_mm(self) -> (u8, u8) {
    // Convert UTC time + half-hour offset to total minutes since midnight,
    // then wrap into a single day. Doing the math in i32 avoids any
    // overflow/underflow when the offset is negative.
    let utc_minutes = i32::from(self.utc_hour) * 60 + i32::from(self.utc_minute);
    let offset_minutes = i32::from(self.local_offset_half_hours) * 30;
    let total = (utc_minutes + offset_minutes).rem_euclid(24 * 60);
    ((total / 60) as u8, (total % 60) as u8)
  }
}

/// RDS (Radio Data System) decoder for PS (station name) and RT (Radio Text).
///
/// Supports:
/// - Programme Service name (Group 0A/0B): 8-character station name
/// - RadioText (Group 2A/2B): up to 64 chars of free-form text broadcast by
///   the station, terminated by `0x0D`. The decoder maintains a stable
///   `radio_text` buffer that is only swapped when the broadcaster flips the
///   A/B flag (signalling a new message).
///
/// The decoder does **not** convert character sets — it simply collects raw
/// RDS bytes. Callers should run [`decode_rds_text`] on the resulting slice
/// to produce a `String` suitable for display (handles UTF-8 / GB2312 / Latin-1).
pub struct RdsDecoder {
  /// Programme Service name (8 characters)
  ps_name: [u8; 8],
  /// Bitmask of which PS segments have been received
  ps_valid: u8,

  /// RadioText buffer (raw RDS bytes, terminator `0x0D` excluded).
  rt_buf: [u8; RT_MAX_LEN],
  /// Effective length of `rt_buf` (truncated at terminator or fully filled).
  rt_len: usize,
  /// Bitmask of which RT segments have been received (max 16 segments).
  rt_valid: u32,
  /// Most recently observed A/B flag; `None` until first RT group seen.
  rt_ab_flag: Option<bool>,
  /// True once the RT message has been observed in full (terminator seen or
  /// all 16 segments collected).
  rt_complete: bool,

  /// Most recently decoded Clock-Time frame, awaiting consumption via
  /// [`RdsDecoder::take_clock_time`]. `None` until the first valid 4A
  /// group is decoded after construction or [`RdsDecoder::reset`].
  ct_pending: Option<RdsClockTime>,

  /// Most recently observed Programme Type code (RBDS / RDS standard,
  /// 0..=31). Lifted from bits 10..=6 of every RDS Block B, so it
  /// stabilises within the first ~100 ms of tuning. `None` until the
  /// first RDS group is seen for the current station.
  pty: Option<u8>,

  /// Most recently observed Programme Identification (PI) code, taken
  /// straight from RDS Block A. `None` until the first RDS group is
  /// processed on the current station. Used by the AF (alternative
  /// frequency) follower to verify that a candidate frequency carries
  /// the same programme before we commit to switching.
  pi: Option<u16>,

  /// Alternative-frequency (AF) list accumulated from group 0A block C.
  ///
  /// Stored as 0.1 MHz units (matching `Si4703::tune`'s argument), so
  /// `965` means 96.5 MHz. The 25-slot ceiling matches the RDS "method A"
  /// maximum AF list length (count code 224..=249 → up to 25 entries).
  af_freqs: [u16; AF_LIST_MAX],
  /// Number of valid entries currently in [`Self::af_freqs`].
  af_count: u8,
  /// Expected list length announced by the most recent leading AF code
  /// (224..=249). Used purely as a debug / diagnostic hint; the decoder
  /// stops accepting new entries once `af_count` matches this.
  af_expected: u8,
}

/// Maximum length of an RDS "method A" AF list (count code 249 = 25 entries).
const AF_LIST_MAX: usize = 25;

/// Sentinel for "AF list follows, expected length = N" (codes 224..=249).
const AF_LIST_LEAD_BASE: u8 = 224;
const AF_LIST_LEAD_MAX: u8 = 249;

/// First valid AF "frequency code" (n = 1 → 87.6 MHz at 0.1 MHz steps).
const AF_FREQ_MIN: u8 = 1;
/// Last valid AF "frequency code" (n = 204 → 107.9 MHz).
const AF_FREQ_MAX: u8 = 204;
/// Base frequency offset for AF codes, in 0.1 MHz units (`87.5 MHz × 10`).
///
/// Spec: AF code `n ∈ 1..=204` represents `(87.5 + n × 0.1) MHz`.
const AF_FREQ_BASE_X10: u16 = 875;

impl RdsDecoder {
  /// Create a new RDS decoder instance
  pub fn new() -> Self {
    Self {
      ps_name: [b' '; 8],
      ps_valid: 0,
      rt_buf: [b' '; RT_MAX_LEN],
      rt_len: 0,
      rt_valid: 0,
      rt_ab_flag: None,
      rt_complete: false,
      ct_pending: None,
      pty: None,
      pi: None,
      af_freqs: [0; AF_LIST_MAX],
      af_count: 0,
      af_expected: 0,
    }
  }

  /// Process an RDS data block.
  ///
  /// Feed the four RDS blocks obtained from [`Si4703::read_rds`] into this method.
  /// Returns `true` if the PS (station) name is now complete.
  pub fn process(&mut self, block_a: u16, block_b: u16, block_c: u16, block_d: u16) -> bool {
    // Group type is in bits 15-12 of block B; bit 11 is the version (0 = A, 1 = B)
    let group_type = (block_b >> 12) & 0x0F;
    let version_b = (block_b & 0x0800) != 0;

    // Programme Identification sits in *every* Block A. Caching the most
    // recent value lets the AF follower confirm a candidate frequency
    // belongs to the same programme before committing to the switch.
    self.pi = Some(block_a);

    // Programme Type sits in bits 10..=6 of *every* Block B regardless of
    // group, so the cheapest place to harvest it is right here.
    self.pty = Some(((block_b >> 5) & 0x1F) as u8);

    match group_type {
      // Group 0A / 0B: Programme Service name (always 4 chars * 2 bytes)
      0 => {
        let segment = (block_b & 0x03) as usize;
        self.ps_name[segment * 2] = (block_d >> 8) as u8;
        self.ps_name[segment * 2 + 1] = (block_d & 0xFF) as u8;
        self.ps_valid |= 1 << segment;
        // Group 0A also carries two AF "frequency codes" in block C
        // (high byte first). 0B repeats PI in block C and carries no AF.
        if !version_b {
          self.process_af_codes((block_c >> 8) as u8, (block_c & 0xFF) as u8);
        }
      }
      // Group 2A / 2B: RadioText
      2 => self.process_rt(block_b, block_c, block_d, version_b),
      // Group 4A: Clock-Time (Group 4B is reserved by the spec).
      4 if !version_b => self.process_ct(block_b, block_c, block_d),
      _ => {}
    }

    // All 4 PS segments received
    self.ps_valid == 0x0F
  }

  /// Internal RT decode (Group 2A: 4 chars/segment, Group 2B: 2 chars/segment).
  fn process_rt(&mut self, block_b: u16, block_c: u16, block_d: u16, version_b: bool) {
    // Bit 4 of block B is the Text A/B flag — when it toggles, the
    // broadcaster has started transmitting a new RT message.
    let ab = (block_b & 0x0010) != 0;
    if self.rt_ab_flag != Some(ab) {
      self.rt_buf = [b' '; RT_MAX_LEN];
      self.rt_len = 0;
      self.rt_valid = 0;
      self.rt_complete = false;
      self.rt_ab_flag = Some(ab);
    }

    let segment = (block_b & 0x0F) as usize;
    let bytes_per_segment = if version_b { 2 } else { 4 };
    let base = segment * bytes_per_segment;
    if base + bytes_per_segment > RT_MAX_LEN {
      return;
    }

    let chars = if version_b {
      // Group 2B: only block D carries 2 chars; block C is repeat of PI.
      [(block_d >> 8) as u8, (block_d & 0xFF) as u8, 0, 0]
    } else {
      [
        (block_c >> 8) as u8,
        (block_c & 0xFF) as u8,
        (block_d >> 8) as u8,
        (block_d & 0xFF) as u8,
      ]
    };

    let mut terminator_at: Option<usize> = None;
    for (i, &byte) in chars.iter().take(bytes_per_segment).enumerate() {
      // 0x0D ('\r') terminates the RT message early.
      if byte == 0x0D {
        terminator_at = Some(base + i);
        break;
      }
      self.rt_buf[base + i] = byte;
    }

    self.rt_valid |= 1 << segment;

    if let Some(end) = terminator_at {
      self.rt_len = end;
      self.rt_complete = true;
      // Clear bytes past the terminator so they don't leak into the output.
      for slot in &mut self.rt_buf[end..] {
        *slot = b' ';
      }
    } else {
      // Track the rightmost byte we've populated.
      let candidate = base + bytes_per_segment;
      if candidate > self.rt_len {
        self.rt_len = candidate;
      }
      // Mark as complete when all theoretically-available segments are in.
      let all_segments_mask = if version_b { 0x00FF } else { 0xFFFF };
      if self.rt_valid == all_segments_mask {
        self.rt_complete = true;
      }
    }
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

  /// Get the station name decoded into a heap [`String`], handling
  /// UTF-8 / GB2312 / Latin-1 input. See [`decode_rds_text`].
  pub fn station_name_string(&self) -> String {
    decode_rds_text(&self.ps_name)
  }

  /// Get the raw RadioText bytes received so far (length = [`Self::radio_text_len`]).
  pub fn radio_text_bytes(&self) -> &[u8] {
    &self.rt_buf[..self.rt_len]
  }

  /// Length in bytes of the RT message accumulated so far.
  pub fn radio_text_len(&self) -> usize {
    self.rt_len
  }

  /// Decode RadioText into a heap [`String`], handling UTF-8 / GB2312 / Latin-1.
  pub fn radio_text_string(&self) -> String {
    decode_rds_text(self.radio_text_bytes())
  }

  /// Convenience: human-readable Programme Type for the current station.
  ///
  /// Returns `None` until at least one RDS group has been decoded on the
  /// current station, or when the broadcaster reports PTY 0 ("None").
  pub fn pty_label(&self) -> Option<&'static str> {
    let code = self.pty?;
    if code == 0 {
      return None;
    }
    Some(pty_label(code))
  }

  /// True once the RT message terminator (`0x0D`) has been observed or all
  /// 16 segments have been received.
  pub fn radio_text_complete(&self) -> bool {
    self.rt_complete
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
    self.rt_buf = [b' '; RT_MAX_LEN];
    self.rt_len = 0;
    self.rt_valid = 0;
    self.rt_ab_flag = None;
    self.rt_complete = false;
    self.ct_pending = None;
    self.pty = None;
    self.pi = None;
    self.af_freqs = [0; AF_LIST_MAX];
    self.af_count = 0;
    self.af_expected = 0;
  }

  /// Most recently observed Programme Type (PTY) code, 0..=31.
  ///
  /// `None` until the first RDS group has been processed on the current
  /// station. Use [`pty_label`] to map to a human-readable string.
  pub fn pty(&self) -> Option<u8> {
    self.pty
  }

  /// Most recently observed Programme Identification (PI) code from
  /// Block A. `None` until the first RDS group is processed on the
  /// current station.
  ///
  /// PI is the primary handle for verifying that an alternative
  /// frequency carries the same programme: the AF follower compares
  /// the candidate's PI to the original station's PI and refuses to
  /// switch on a mismatch.
  pub fn pi(&self) -> Option<u16> {
    self.pi
  }

  /// Alternative-frequency (AF) list collected from group 0A.
  ///
  /// Each entry is in 0.1 MHz units (e.g. `965` = 96.5 MHz), matching
  /// the argument format of [`Si4703::tune`]. The slice is empty until
  /// at least one valid AF code has been decoded; entries are
  /// deduplicated and the original station's own frequency is *not*
  /// filtered out (callers are expected to skip it).
  pub fn alt_freqs(&self) -> &[u16] {
    &self.af_freqs[..self.af_count as usize]
  }

  /// Process the two AF codes carried in group 0A block C.
  ///
  /// Encoding (RBDS standard, AF method A):
  /// - `0`        — filler.
  /// - `1..=204`  — frequency: `87.5 MHz + n × 0.1 MHz`.
  /// - `205..=223`— reserved / filler.
  /// - `224..=249`— "AF list follows", count = code − 224 (0..=25).
  /// - `250`      — LF/MF list start (we treat as filler; we're FM-only).
  /// - `251..=255`— reserved.
  ///
  /// Method B (paired) is intentionally **not** parsed: it interleaves
  /// the tuned frequency with each AF and would require additional
  /// state to disentangle. Method A covers the vast majority of real
  /// broadcasts; an unparsed B-method list simply leaves `af_count = 0`
  /// and the AF follower stays dormant on that station.
  fn process_af_codes(&mut self, code_a: u8, code_b: u8) {
    self.process_single_af_code(code_a);
    self.process_single_af_code(code_b);
  }

  fn process_single_af_code(&mut self, code: u8) {
    if (AF_LIST_LEAD_BASE..=AF_LIST_LEAD_MAX).contains(&code) {
      // Leading code: reset the list and remember the announced length.
      self.af_freqs = [0; AF_LIST_MAX];
      self.af_count = 0;
      self.af_expected = code - AF_LIST_LEAD_BASE;
      return;
    }

    if !(AF_FREQ_MIN..=AF_FREQ_MAX).contains(&code) {
      // Filler / reserved / LF-MF marker — ignore.
      return;
    }

    let freq_x10 = AF_FREQ_BASE_X10 + u16::from(code);

    // Deduplicate: AF lists are tiny (≤25), so a linear scan is fine
    // and avoids pulling in a hash set on a no_std target.
    if self.af_freqs[..self.af_count as usize].contains(&freq_x10) {
      return;
    }
    if (self.af_count as usize) < AF_LIST_MAX {
      self.af_freqs[self.af_count as usize] = freq_x10;
      self.af_count += 1;
    }
  }

  /// Internal Clock-Time decode (Group 4A).
  ///
  /// Wire format (per RBDS Annex G / IEC 62106):
  /// - Block B (low 2 bits) + Block C (high 15 bits) = 17-bit MJD (unused).
  /// - Block C low 1 bit + Block D bits 15..12 = 5-bit UTC hour (0..=23).
  /// - Block D bits 11..6 = 6-bit UTC minute (0..=59).
  /// - Block D bit 5 = sign of local-time offset (1 = negative).
  /// - Block D bits 4..0 = magnitude of local-time offset in half-hours.
  ///
  /// Out-of-range fields (e.g. hour=27 from a corrupted frame) are
  /// silently dropped — the next valid CT will overwrite the stash.
  fn process_ct(&mut self, _block_b: u16, block_c: u16, block_d: u16) {
    let hour = (((block_c & 0x0001) << 4) | (block_d >> 12)) as u8;
    let minute = ((block_d >> 6) & 0x3F) as u8;
    let offset_mag = (block_d & 0x1F) as i8;
    let sign_negative = (block_d & 0x0020) != 0;
    let offset = if sign_negative {
      -offset_mag
    } else {
      offset_mag
    };

    // Spec-mandated ranges; reject obvious corruption rather than show
    // a bogus clock to the user.
    if hour > 23 || minute > 59 || offset_mag > 24 {
      return;
    }

    self.ct_pending = Some(RdsClockTime {
      utc_hour: hour,
      utc_minute: minute,
      local_offset_half_hours: offset,
    });
  }

  /// Take the most recently decoded Clock-Time frame, if any.
  ///
  /// Consumed on read so callers see each broadcast CT (which arrives
  /// approximately once per minute, on the minute boundary) exactly once.
  pub fn take_clock_time(&mut self) -> Option<RdsClockTime> {
    self.ct_pending.take()
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

/// Map an RBDS / RDS Programme Type code (0..=31) to a short label.
///
/// Strings are kept short (≤ 8 ASCII chars) so they fit on the station
/// card without truncation. Source: RBDS standard NRSC-4-B Annex F /
/// IEC 62106 Table 9. Codes outside `0..=31` are clamped to `"--"` so
/// callers can pass a raw `u8` without panicking on corrupted input.
///
/// PTY 0 is the broadcaster's way of saying "no programme type" and is
/// therefore reported as `"None"`; UI code typically suppresses the
/// badge entirely in that case.
pub fn pty_label(code: u8) -> &'static str {
  // RBDS labels (United States / Europe both use this table since 1998).
  match code {
    0 => "None",
    1 => "News",
    2 => "Info",
    3 => "Sports",
    4 => "Talk",
    5 => "Rock",
    6 => "ClsRock",
    7 => "AdltHit",
    8 => "SoftRck",
    9 => "Top 40",
    10 => "Country",
    11 => "Oldies",
    12 => "Soft",
    13 => "Nostlga",
    14 => "Jazz",
    15 => "Classic",
    16 => "R&B",
    17 => "SoftR&B",
    18 => "Lang",
    19 => "RelMusc",
    20 => "RelTalk",
    21 => "Persnly",
    22 => "Public",
    23 => "College",
    24..=28 => "Unassgn",
    29 => "Weather",
    30 => "Test",
    31 => "Alert!",
    _ => "--",
  }
}

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

// ============================================================================
// RDS text decoding (UTF-8 / GB2312 / Latin-1)
// ============================================================================

/// Decode raw RDS bytes into a printable [`String`].
///
/// FM RDS broadcasters use one of three character encodings in practice:
/// - **UTF-8** (modern stations, especially outside Europe).
/// - **GB2312 / GBK** (Chinese stations using the RBDS Chinese extension).
/// - **Latin-1 / ASCII** (default RDS character set, EBU Tech 3667).
///
/// The wire protocol does not signal which encoding is in use, so we apply
/// a small heuristic:
///
/// 1. If the entire byte slice is valid UTF-8, decode it as such.
/// 2. Otherwise scan byte-by-byte: if a byte falls in the GB2312 lead range
///    (`0xA1..=0xFE`) and the next byte is a valid trail byte, treat the
///    pair as a single CJK glyph and emit a placeholder `?` (we can't ship
///    a 7000-entry GB2312 table on a 320 KB MCU). All other bytes outside
///    the printable ASCII range (`0x20..=0x7E`) are also emitted as `?`.
///
/// Trailing whitespace (spaces and `0x00`) is stripped so the UI doesn't
/// show a half-empty buffer when the message is shorter than the maximum.
///
/// This means:
/// - English / European stations render correctly verbatim.
/// - Chinese stations show one `?` per CJK character (UI still scrolls a
///   meaningful length so the user knows RT is present).
/// - UTF-8 stations (including emoji) render correctly.
pub fn decode_rds_text(bytes: &[u8]) -> String {
  // Strip trailing 0x00 / space padding before decoding so UTF-8 detection
  // and length tracking aren't fooled by buffer-fill bytes.
  let mut end = bytes.len();
  while end > 0 {
    let last = bytes[end - 1];
    if last == b' ' || last == 0 {
      end -= 1;
    } else {
      break;
    }
  }
  let trimmed = &bytes[..end];

  if trimmed.is_empty() {
    return String::new();
  }

  // Fast path: pure ASCII or valid UTF-8 (covers UTF-8 RDS extensions).
  if let Ok(s) = core::str::from_utf8(trimmed) {
    return String::from(s);
  }

  // Fallback: scan byte-by-byte, recognising GB2312 lead/trail pairs.
  let mut out = String::with_capacity(trimmed.len());
  let mut i = 0;
  while i < trimmed.len() {
    let byte = trimmed[i];
    if (0x20..=0x7E).contains(&byte) {
      // Printable ASCII passes through.
      out.push(byte as char);
      i += 1;
    } else if (0xA1..=0xFE).contains(&byte) && i + 1 < trimmed.len() {
      // Looks like a GB2312 lead byte. Validate the trail byte.
      let trail = trimmed[i + 1];
      let is_valid_gb_trail =
        (0xA1..=0xFE).contains(&trail) || (0x40..=0x7E).contains(&trail) || trail == 0x80;
      if is_valid_gb_trail {
        out.push('?');
        i += 2;
        continue;
      }
      out.push('?');
      i += 1;
    } else {
      // Anything else (control chars, isolated high bytes) -> placeholder.
      out.push('?');
      i += 1;
    }
  }
  out
}
