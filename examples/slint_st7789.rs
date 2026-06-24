//! Slint UI display example on ST7789 LCD screen
//!
//! This example demonstrates how to use the Slint UI framework on ESP32-C6,
//! driving the ST7789 LCD screen (240x320) via SPI interface to display UI.
//!
//! Hardware connections:
//! - GPIO6  -> SCL (SPI Clock)
//! - GPIO7  -> SDA (SPI MOSI)
//! - GPIO20 -> CS  (Chip Select)
//! - GPIO21 -> DC  (Data/Command)
//! - GPIO22 -> RST (Reset)
//! - GPIO23 -> BLK (Backlight)

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;

use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use panic_rtt_target as _;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

// Import display module from the library
use radio::display::{DISPLAY_HEIGHT, DISPLAY_WIDTH, DisplayLineBuffer, EspPlatform};

slint::include_modules!();

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();
  info!("Slint ST7789 Demo starting...");

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
  // Slint software renderer with line-by-line mode needs minimal heap (state + font cache)
  esp_alloc::heap_allocator!(size: 48 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

  info!("Embassy initialized!");

  // Configure GPIO pins
  let sclk = peripherals.GPIO6;
  let mosi = peripherals.GPIO7;
  let cs = Output::new(peripherals.GPIO20, Level::High, OutputConfig::default());
  let dc = Output::new(peripherals.GPIO21, Level::Low, OutputConfig::default());
  let rst = Output::new(peripherals.GPIO22, Level::High, OutputConfig::default());
  let blk = Output::new(peripherals.GPIO23, Level::High, OutputConfig::default());

  // Initialize SPI
  let spi_config = SpiConfig::default()
    .with_frequency(Rate::from_mhz(40))
    .with_mode(SpiMode::_0);

  let spi_bus = Spi::new(peripherals.SPI2, spi_config)
    .expect("SPI initialization failed")
    .with_sck(sclk)
    .with_mosi(mosi);

  // Wrap SpiBus as SpiDevice using ExclusiveDevice
  let spi_device = ExclusiveDevice::new_no_delay(spi_bus, cs).expect("SPI Device creation failed");

  // Create mipidsi SPI interface (requires a buffer)
  static mut SPI_BUF: [u8; 960] = [0u8; 960]; // 240 * 2 * 2 = 960 bytes buffer
  let spi_buf = unsafe { &mut *core::ptr::addr_of_mut!(SPI_BUF) };
  let spi_interface = SpiInterface::new(spi_device, dc, spi_buf);

  // Turn on backlight
  let _ = blk;

  // Initialize ST7789 display
  let mut delay = embassy_time::Delay;
  let mut display = Builder::new(ST7789, spi_interface)
    .display_size(DISPLAY_WIDTH, DISPLAY_HEIGHT)
    .display_offset(0, 0)
    .orientation(Orientation::new().rotate(Rotation::Deg0))
    .color_order(ColorOrder::Rgb)
    .invert_colors(ColorInversion::Inverted)
    .reset_pin(rst)
    .init(&mut delay)
    .expect("Display initialization failed");

  info!("ST7789 display initialized!");

  // Clear screen to black
  display.clear(Rgb565::BLACK).expect("Clear screen failed");

  // Initialize Slint platform
  let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
  window.set_size(slint::PhysicalSize::new(
    DISPLAY_WIDTH as u32,
    DISPLAY_HEIGHT as u32,
  ));

  let platform = EspPlatform::new(window.clone());
  slint::platform::set_platform(Box::new(platform)).expect("Set Slint platform failed");

  // Create Slint UI instance
  let ui = ExampleWindow::new().expect("Create UI failed");
  let ui_weak = ui.as_weak();

  info!("Slint UI initialized!");

  // Main loop: update UI and render to screen
  let mut counter: i32 = 0;
  loop {
    // Update counter
    counter = counter.wrapping_add(1);
    if let Some(ui) = ui_weak.upgrade() {
      ui.set_counter(counter);
    }

    // Let Slint process events and animations
    slint::platform::update_timers_and_animations();

    // Render to display using the library's line buffer provider
    window.draw_if_needed(|renderer| {
      let line_buffer = DisplayLineBuffer::new(&mut display);
      renderer.render_by_line(line_buffer);
    });

    Timer::after(Duration::from_millis(100)).await;
  }
}
