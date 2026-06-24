//! WiFi Provisioning + Station Connection Example
//!
//! This example demonstrates the simplified WiFi provisioning flow
//! using the `wifi_provision` module's high-level API:
//!
//! 1. Create a `WifiProvisioner` instance
//! 2. Call `provision_and_connect()` - it handles everything:
//!    - Check Flash for saved credentials
//!    - If none, start SoftAP captive portal
//!    - Save credentials to Flash
//!    - Connect to target WiFi in Station mode
//!    - Wait for DHCP IP assignment
//! 3. Use the returned `ConnectedWifi` for your application
//!
//! # Clearing saved credentials
//! To force re-provisioning, call `provisioner.clear_credentials()` or erase the Flash sector.

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use panic_rtt_target as _;
use radio::wifi_provision::{ConnectionConfig, ProvisioningConfig, WifiProvisioner};
use static_cell::StaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

/// Network stack resources.
static STACK_RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();
  info!("=== WiFi Provisioning Example (Simplified) ===");

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
  // One-line WiFi: provision (if needed) + connect
  // ========================================================================
  let flash = FlashStorage::new(peripherals.FLASH);
  let mut provisioner = WifiProvisioner::new(flash);

  let conn_config = ConnectionConfig::default();
  let prov_config = ProvisioningConfig::default();
  let stack_resources = STACK_RESOURCES.init(StackResources::new());

  let wifi = provisioner
    .provision_and_connect(
      &spawner,
      peripherals.WIFI,
      &conn_config,
      &prov_config,
      stack_resources,
    )
    .await;

  match wifi {
    Ok(connected) => {
      info!(
        "=== WiFi connected! SSID: \"{}\" ===",
        connected.ssid.as_str()
      );
      info!("Application is now running with network access.");

      // Application main loop - use connected.stack for TCP/UDP
      loop {
        if connected.controller.is_connected() {
          info!("WiFi OK");
        } else {
          info!("WARNING: WiFi disconnected!");
        }
        Timer::after(Duration::from_secs(10)).await;
      }
    }
    Err(e) => {
      info!("WiFi connection failed: {}", e);
      info!("Please reboot the device to retry.");
      loop {
        Timer::after(Duration::from_secs(5)).await;
      }
    }
  }
}
