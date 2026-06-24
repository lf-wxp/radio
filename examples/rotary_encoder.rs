//! Rotary Encoder Example
//!
//! Demonstrates how to use the PCNT hardware peripheral to read rotation direction and button events from a rotary encoder.
//!
//! # Hardware Connection (ESP32-C6)
//! - S1 (CLK/A) -> GPIO4
//! - S2 (DT/B)  -> GPIO5
//! - KEY         -> GPIO6
//! - GND         -> GND
//! - 5V/VCC      -> 3.3V

#![no_std]
#![no_main]

extern crate alloc;

use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::pcnt::Pcnt;
use esp_hal::timer::timg::TimerGroup;
use panic_rtt_target as _;
use radio::rotary_encoder::{EncoderConfig, RotaryEncoder, handle_pcnt_overflow};

esp_bootloader_esp_idf::esp_app_desc!();

// PCNT interrupt handler
#[esp_hal::handler]
fn pcnt_interrupt_handler() {
  // Handle overflow with default configuration limits
  handle_pcnt_overflow(0, 100, -100);
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();
  info!("Rotary encoder example starting...");

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
  esp_alloc::heap_allocator!(size: 32 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

  info!("Embassy initialized!");

  // Initialize PCNT peripheral
  let mut pcnt = Pcnt::new(peripherals.PCNT);
  pcnt.set_interrupt_handler(pcnt_interrupt_handler);

  // Configure encoder input pins (with pull-up)
  let input_config = InputConfig::default().with_pull(Pull::Up);
  let pin_a = Input::new(peripherals.GPIO4, input_config);
  let pin_b = Input::new(peripherals.GPIO5, input_config);
  let pin_key = Input::new(peripherals.GPIO6, input_config);

  // Create rotary encoder instance
  let encoder_config = EncoderConfig::default();
  let encoder = RotaryEncoder::new(pcnt.unit0, pin_a, pin_b, pin_key, encoder_config)
    .expect("Failed to initialize encoder");

  info!("Rotary encoder initialized!");
  info!("  - S1(CLK) -> GPIO4");
  info!("  - S2(DT)  -> GPIO5");
  info!("  - KEY     -> GPIO6");
  info!("Rotary encoder is now running, please rotate or press the button...");

  // Start heartbeat task
  spawner.spawn(heartbeat_task().unwrap());

  // Main loop: poll rotation value
  let mut last_value: i32 = 0;
  loop {
    let current_value = encoder.value();

    if current_value != last_value {
      let delta = current_value - last_value;
      let direction = if delta > 0 {
        "Clockwise ↻"
      } else {
        "Counter-clockwise ↺"
      };
      info!(
        "Rotation: {} | Delta: {} | Total: {}",
        direction, delta, current_value
      );
      last_value = current_value;
    }

    // Check button state
    if encoder.is_button_pressed() {
      info!("Button pressed! Current value: {}", current_value);
      // Wait for release to avoid repeated triggers
      while encoder.is_button_pressed() {
        Timer::after(Duration::from_millis(10)).await;
      }
      info!("Button released!");
    }

    Timer::after(Duration::from_millis(20)).await;
  }
}

/// Heartbeat task
///
/// Periodically prints status information to confirm the system is running normally.
#[embassy_executor::task]
async fn heartbeat_task() {
  info!("Heartbeat task started");
  loop {
    Timer::after(Duration::from_secs(5)).await;
    info!("Encoder running... (heartbeat every 5 seconds)");
  }
}
