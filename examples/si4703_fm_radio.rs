//! Si4703 FM Radio Receiver Example
//!
//! This example demonstrates how to use the Si4703 FM radio module (MCU-470 board)
//! on ESP32-C6 via I2C (2-wire mode) to implement practical FM radio features:
//! - Scan and discover available stations
//! - Tune to a specific frequency
//! - Volume control
//! - Read signal strength (RSSI)
//! - RDS (Radio Data System) basic support
//!
//! Hardware connections (MCU-470 Si4703 board):
//! - 3.3V  -> 3.3V power supply
//! - GND   -> Ground
//! - SDIO  -> GPIO6 (I2C SDA)
//! - SCLK  -> GPIO7 (I2C SCL)
//! - SEN   -> GND (select I2C mode, address 0x10)
//! - RST   -> GPIO5 (Reset pin)
//! - GPIO1 -> Not connected (or interrupt)
//! - GPIO2 -> GPIO4 (STC/RDS interrupt, optional)
//!
//! Note: Si4703 uses a non-standard I2C initialization sequence.
//! The SDIO pin must be pulled low while RST transitions from low to high
//! to enter 2-wire (I2C) mode.

#![no_std]
#![no_main]

extern crate alloc;

use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use panic_rtt_target as _;

// Import the Si4703 driver from the library
use radio::si4703::{
  ChannelSpacing, FmBand, RdsDecoder, SeekDirection, Si4703, Station, format_freq,
};

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();
  info!("=== Si4703 FM Radio Demo ===");

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
  esp_alloc::heap_allocator!(size: 64 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

  info!("Embassy initialized!");

  // ========================================================================
  // Hardware Reset Sequence for Si4703 (enter 2-wire/I2C mode)
  // ========================================================================
  // The Si4703 requires a specific sequence to enter I2C mode:
  // 1. Set SDIO (SDA) low
  // 2. Pulse RST from low to high
  // 3. Release SDIO to be used as I2C SDA

  // Configure RST pin as output (GPIO5)
  let mut rst_pin = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());

  // Configure SDIO (GPIO6) as output temporarily for reset sequence
  let mut sdio_pin = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());

  // Step 1: Hold SDIO low and RST low
  sdio_pin.set_low();
  rst_pin.set_low();
  Timer::after(Duration::from_millis(10)).await;

  // Step 2: Release RST (set high) while SDIO is still low
  rst_pin.set_high();
  Timer::after(Duration::from_millis(10)).await;

  // Step 3: Release SDIO - now the chip is in I2C mode
  drop(sdio_pin);

  info!("Si4703 reset sequence complete, entering I2C mode");

  // ========================================================================
  // Initialize I2C bus
  // ========================================================================
  // SAFETY: GPIO6 was released by dropping sdio_pin above.
  // We steal it back to pass to the I2C peripheral.
  let sda_pin = unsafe { esp_hal::peripherals::GPIO6::steal() };
  let i2c_config = I2cConfig::default().with_frequency(Rate::from_khz(100));

  let mut i2c = I2c::new(peripherals.I2C0, i2c_config)
    .expect("I2C initialization failed")
    .with_sda(sda_pin)
    .with_scl(peripherals.GPIO7);

  info!("I2C bus initialized (100 kHz)");

  // ========================================================================
  // Initialize Si4703
  // ========================================================================
  let mut radio = Si4703::new(FmBand::UsEurope, ChannelSpacing::Spacing100K);

  match radio.init(&mut i2c).await {
    Ok(()) => {
      info!("Si4703 initialized successfully!");
      info!(
        "  Device ID: 0x{:04X}, Chip ID: 0x{:04X}",
        radio.device_id(),
        radio.chip_id()
      );
    }
    Err(_) => {
      info!("ERROR: Failed to initialize Si4703! Check wiring.");
      loop {
        Timer::after(Duration::from_secs(1)).await;
      }
    }
  }

  // ========================================================================
  // Set initial volume
  // ========================================================================
  let _ = radio.set_volume(&mut i2c, 8);
  info!("Volume set to 8/15");

  // ========================================================================
  // Scan for available stations
  // ========================================================================
  info!("Scanning for FM stations...");
  let mut stations = [Station::empty(); 20];

  let station_count = match radio.scan_stations(&mut i2c, &mut stations).await {
    Ok(count) => {
      info!("Scan complete! Found {} stations:", count);
      for i in 0..count {
        let (mhz, dec) = format_freq(stations[i].freq_mhz_x10);
        info!("  [{}] {}.{} MHz (RSSI: {})", i, mhz, dec, stations[i].rssi);
      }
      count
    }
    Err(_) => {
      info!("ERROR: Station scan failed!");
      0
    }
  };

  // ========================================================================
  // Tune to the strongest station (or default 101.5 MHz)
  // ========================================================================
  let target_freq = if station_count > 0 {
    // Find station with highest RSSI
    let mut best_idx = 0;
    let mut best_rssi = 0u8;
    for i in 0..station_count {
      if stations[i].rssi > best_rssi {
        best_rssi = stations[i].rssi;
        best_idx = i;
      }
    }
    stations[best_idx].freq_mhz_x10
  } else {
    1015 // Default: 101.5 MHz
  };

  let (mhz, dec) = format_freq(target_freq);
  info!("Tuning to {}.{} MHz...", mhz, dec);

  if radio.tune(&mut i2c, target_freq).await.is_ok() {
    let rssi = radio.rssi(&mut i2c).unwrap_or(0);
    info!("Tuned successfully! RSSI: {}", rssi);
  } else {
    info!("ERROR: Tune failed!");
  }

  // ========================================================================
  // Main loop: monitor signal and read RDS data
  // ========================================================================
  info!("Entering main loop - monitoring signal and RDS...");
  info!("(In a real application, buttons would control seek/volume)");

  let mut rds_decoder = RdsDecoder::new();
  let mut loop_counter: u32 = 0;

  loop {
    loop_counter += 1;

    // Read RDS data every iteration
    if let Ok(Some((a, b, c, d))) = radio.read_rds(&mut i2c) {
      if rds_decoder.process(a, b, c, d) {
        info!("RDS Station Name: {:a}", rds_decoder.station_name_str());
      }
    }

    // Print status every 5 seconds
    if loop_counter % 50 == 0 {
      if let Ok(freq) = radio.current_frequency(&mut i2c) {
        let rssi = radio.rssi(&mut i2c).unwrap_or(0);
        let (mhz, dec) = format_freq(freq);
        info!(
          "Status: {}.{} MHz | RSSI: {} | Vol: {}/15",
          mhz,
          dec,
          rssi,
          radio.volume()
        );
      }
    }

    // Demonstrate seek every 30 seconds (seek to next station)
    if loop_counter % 300 == 0 {
      info!("Seeking next station...");
      match radio.seek(&mut i2c, SeekDirection::Up).await {
        Ok(Some(freq)) => {
          let (mhz, dec) = format_freq(freq);
          info!("Found station: {}.{} MHz", mhz, dec);
          rds_decoder.reset();
        }
        Ok(None) => {
          info!("No more stations found (end of band)");
        }
        Err(_) => {
          info!("Seek error!");
        }
      }
    }

    Timer::after(Duration::from_millis(100)).await;
  }
}
