//! ESP-Radio: A complete FM radio application for ESP32-C6.
//!
//! This binary integrates all the project's modules into a single
//! production-ready firmware:
//!
//! - **WiFi**: Provisioned via SoftAP captive portal, persisted in Flash
//!   (see [`radio::wifi_provision`]).
//! - **Display**: 240x320 ST7789 LCD over SPI rendering a Material-Design
//!   Slint UI (see [`radio::display`] and `ui/radio_ui.slint`).
//! - **Tuner**: Si4703 FM receiver over I2C with auto-tune to the
//!   strongest station on boot (see [`radio::si4703`]).
//! - **Input**: KY-040 rotary encoder for tuning + push button for seek
//!   / mute (see [`radio::rotary_encoder`]).
//!
//! # Hardware connections (ESP32-C6)
//!
//! | Function          | GPIO   |
//! |-------------------|--------|
//! | ST7789 SCK        | GPIO3  |
//! | ST7789 MOSI       | GPIO0  |
//! | ST7789 CS         | GPIO1  |
//! | ST7789 DC         | GPIO2  |
//! | ST7789 RST        | GPIO22 |
//! | ST7789 BLK        | GPIO23 |
//! | Si4703 SDA (SDIO) | GPIO6  |
//! | Si4703 SCL (SCLK) | GPIO7  |
//! | Si4703 RST        | GPIO10 |
//! | Encoder S1 (CLK)  | GPIO11 |
//! | Encoder S2 (DT)   | GPIO18 |
//! | Encoder KEY       | GPIO19 |
//!
//! # User interaction
//!
//! - **Rotate encoder**: tune up/down by 0.1 MHz steps (with acceleration).
//! - **Short press button**: cycle to the next saved preset; falls back to
//!   `seek-up` when no presets have been saved yet.
//! - **Long press button (>= 800 ms)**: save the current frequency into
//!   the next preset slot (FIFO eviction when all 8 slots are full).
//! - **Ultra-long press (>= 2.5 s)**: toggle mute.
//!
//! # Architecture
//!
//! State is shared across three concurrent activities via `embassy-sync`
//! primitives:
//!
//! - [`state::INPUT_CMDS`] (Channel<8>): input task -> radio control task.
//! - [`state::RADIO_STATE`] (Mutex): radio control task -> UI render loop.

#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
// picoserve's `Router` type is a fluent chain of generics — each
// `.route(...)` call wraps the previous one, so the layout-of query
// for `web_task`'s task pool nests once per route. With #9 we now
// have ten routes; the default 128-step recursion limit is too
// shallow for rustc's layout solver in release mode.
#![recursion_limit = "512"]
#![deny(
  clippy::mem_forget,
  reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

mod clock;
mod diagnostics;
mod hardware;
mod listening_log;
mod mdns;
mod ntp;
mod ota;
mod presets;
mod state;
mod tasks;
mod ui;
mod web;

use defmt::info;
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::pcnt::Pcnt;
use esp_hal::timer::timg::TimerGroup;
use esp_storage::FlashStorage;
use panic_rtt_target as _;
use static_cell::StaticCell;

use radio::rotary_encoder::handle_pcnt_overflow;
use radio::si4703::{Station, format_freq};
use radio::wifi_provision::{ConnectionConfig, ProvisioningConfig, WifiProvisioner};

use crate::diagnostics::{PostResult, check_heap, check_i2c_bus, check_si4703_device_id};

use crate::hardware::{DisplayPins, EncoderPins, TunerPins};
use crate::presets::PresetStore;
use crate::state::{
  DEFAULT_FREQ_X10, MAX_SCAN_STATIONS, PRESET_EMPTY, RADIO_STATE, SPECTRUM_LEN, pick_strongest,
  publish_freq, publish_presets, publish_spectrum, set_status,
};

slint::include_modules!();

esp_bootloader_esp_idf::esp_app_desc!();

/// Static storage for the embassy-net stack resources.
///
/// Capacity 4: SoftAP captive portal (during provisioning) drops to
/// 0 once we move to STA. In STA mode we use 1 socket for the web
/// console (TCP) and 1 socket for the mDNS responder (UDP), leaving
/// 2 free for future features (NTP, OTA).
static STACK_RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

/// PCNT interrupt handler for rotary-encoder overflow accumulation.
///
/// Must live in `main.rs` because the `#[esp_hal::handler]` attribute
/// macro requires a top-level binary `fn`.
#[esp_hal::handler]
fn pcnt_interrupt_handler() {
  handle_pcnt_overflow(0, 100, -100);
}

#[allow(
  clippy::large_stack_frames,
  reason = "main allocates large peripheral wrappers and SPI/UI buffers"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();
  info!("=== ESP-Radio: starting ===");
  diagnostics::record_boot_time();

  // ------------------------------------------------------------------------
  // Core init: clocks, allocator, embassy
  // ------------------------------------------------------------------------
  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  // Two heap regions are registered with esp-alloc; both feed a single
  // global allocator and dispatch is handled by esp-alloc internally.
  //   1. 64 KiB in *reclaimed* RAM (e.g. boot stack region freed after
  //      `esp_hal::init`). Available immediately and best for transient
  //      buffers.
  //   2. 96 KiB in regular DRAM, used by Slint, WiFi/BLE, and Rust
  //      `String`/`Vec` allocations.
  // Total = 160 KiB. Adjust if `oom` panics appear during heavy WiFi
  // load or large Slint scenes.
  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 65536);
  esp_alloc::heap_allocator!(size: 96 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);
  info!("Embassy initialized");

  // ------------------------------------------------------------------------
  // Display + Slint platform
  // ------------------------------------------------------------------------
  let display_bundle = hardware::init_display(
    peripherals.SPI2,
    DisplayPins {
      sck: peripherals.GPIO3,
      mosi: peripherals.GPIO0,
      cs: peripherals.GPIO1,
      dc: peripherals.GPIO2,
      rst: peripherals.GPIO22,
      blk: peripherals.GPIO23,
    },
  );
  let mut display = display_bundle.display;
  let window = display_bundle.window;
  info!("ST7789 ready");

  let ui_root = RadioWindow::new().expect("create UI failed");
  let ui_weak = ui_root.as_weak();
  set_status("Connecting WiFi...").await;
  ui::render_once(&window, &ui_root, &mut display).await;

  // ------------------------------------------------------------------------
  // WiFi provisioning + connection
  // ------------------------------------------------------------------------
  let flash = FlashStorage::new(peripherals.FLASH);
  let mut provisioner = WifiProvisioner::new(flash);
  let conn_config = ConnectionConfig::default();
  let prov_config = ProvisioningConfig::default();
  let stack_resources = STACK_RESOURCES.init(StackResources::new());

  // Captured for the radio control task so its OTA branch can open a
  // TCP socket to the firmware host. `None` when WiFi failed at boot;
  // OTA requests in that case fail fast with `OtaProgress::Failed("offline")`.
  let mut wifi_stack: Option<embassy_net::Stack<'static>> = None;

  match provisioner
    .provision_and_connect(
      &spawner,
      peripherals.WIFI,
      &conn_config,
      &prov_config,
      stack_resources,
    )
    .await
  {
    Ok(connected) => {
      info!("WiFi connected: {}", connected.ssid.as_str());
      let mut state = RADIO_STATE.lock().await;
      state.wifi_connected = true;
      state.wifi_ssid = connected.ssid.clone();
      state.status = "WiFi OK";
      state.dirty = true;
      drop(state);
      let app: &'static picoserve::AppRouter<web::AppProps> = picoserve::make_static!(
        picoserve::AppRouter<web::AppProps>,
        <web::AppProps as picoserve::AppBuilder>::build_app(web::AppProps)
      );
      let config: &'static picoserve::Config = picoserve::make_static!(
        picoserve::Config,
        picoserve::Config::new(picoserve::Timeouts {
          start_read_request: embassy_time::Duration::from_secs(5),
          persistent_start_read_request: embassy_time::Duration::from_secs(5),
          read_request: embassy_time::Duration::from_secs(5),
          write: embassy_time::Duration::from_secs(5),
        })
      );
      let stack = connected.stack;
      // We intentionally drop `connected` here: the embassy-net stack and
      // controller continue running through the spawned task. Future
      // features (NTP, internet radio) can take ownership of the stack
      // before that.
      drop(connected);
      wifi_stack = Some(stack);
      match web::web_task(stack, app, config) {
        Ok(token) => {
          spawner.spawn(token);
          info!("Web console listening on :80");
        }
        Err(_e) => {
          defmt::error!("Failed to spawn web task: task arena full");
        }
      }
      // mDNS responder so the user can reach the console at
      // `http://esp-radio.local/` instead of the dynamic DHCP IP.
      match mdns::mdns_task(stack) {
        Ok(token) => {
          spawner.spawn(token);
          info!("mDNS responder online: esp-radio.local");
        }
        Err(_e) => {
          defmt::error!("Failed to spawn mDNS task: task arena full");
        }
      }
      // SNTP client so the web console can render real timestamps
      // once the LAN can reach the public NTP anycast IPs.
      match ntp::ntp_task(stack) {
        Ok(token) => {
          spawner.spawn(token);
          info!("NTP client armed (Cloudflare anycast)");
        }
        Err(_e) => {
          defmt::error!("Failed to spawn ntp_task: task arena full");
        }
      }
    }
    Err(e) => {
      info!("WiFi failed: {} - continuing offline", e);
      let mut state = RADIO_STATE.lock().await;
      state.wifi_connected = false;
      state.status = "WiFi failed";
      state.dirty = true;
    }
  }

  // ------------------------------------------------------------------------
  // Preset store: take the flash handle back from the provisioner and
  // load any previously saved favourites + last-tuned frequency.
  //
  // Only one subsystem may own `FlashStorage` at a time (esp-storage
  // singleton). Doing the hand-off here keeps the design lock-free in
  // the steady state — once the radio task owns the store, it's the
  // sole writer.
  // ------------------------------------------------------------------------
  let mut flash = provisioner.into_flash();
  // Anti-rollback latch: now that WiFi + display have come up cleanly,
  // tell the bootloader to commit the running image. Failing this is
  // non-fatal (older partition layouts without `otadata` simply skip
  // the write), so we keep the `flash` handle for the preset store
  // regardless. See `ota::mark_current_app_valid` for the rationale.
  ota::mark_current_app_valid(&mut flash);
  let preset_store = PresetStore::open(flash);
  let stored_presets = preset_store.snapshot();
  info!(
    "Presets loaded: {} saved, last_tuned={}",
    stored_presets.used(),
    stored_presets.last_tuned
  );

  // ------------------------------------------------------------------------
  // Si4703 FM tuner init
  // ------------------------------------------------------------------------
  set_status("Init tuner...").await;
  let (mut radio_chip, mut i2c) = hardware::init_tuner(
    peripherals.I2C0,
    TunerPins {
      sda: peripherals.GPIO6,
      scl: peripherals.GPIO7,
      rst: peripherals.GPIO10,
    },
  )
  .await;
  info!("I2C ready");

  // --------------------------------------------------------------------------
  // Power-On Self-Test (POST)
  // --------------------------------------------------------------------------
  set_status("POST...").await;
  ui::render_once(&window, &ui_root, &mut display).await;

  // Check 1: Heap allocator
  let heap_check = check_heap();
  info!("POST: heap = {:?}", heap_check.is_pass());

  // Check 2: I²C bus + Si4703 device ID (requires first register read)
  let (i2c_check, si4703_id_check) = if radio_chip.init(&mut i2c).await.is_ok() {
    let dev_id = radio_chip.device_id();
    let bus = check_i2c_bus(dev_id);
    let chip = check_si4703_device_id(dev_id);
    info!(
      "POST: I2C={:?}, Si4703 dev_id=0x{:04X} ({:?})",
      bus.is_pass(),
      dev_id,
      chip.is_pass()
    );
    (bus, chip)
  } else {
    info!("POST: Si4703 init FAILED");
    (
      diagnostics::CheckStatus::Fail(diagnostics::error_codes::I2C_BUS),
      diagnostics::CheckStatus::Fail(diagnostics::error_codes::SI4703_INIT),
    )
  };

  // Check 3: Encoder (validated later when PCNT is initialised)
  let encoder_check = diagnostics::CheckStatus::Skipped;

  // Assemble POST result and store as 'static for the health endpoint.
  let post_result = PostResult {
    i2c_bus: i2c_check,
    si4703_id: si4703_id_check,
    heap_alloc: heap_check,
    encoder: encoder_check,
  };

  static POST_RESULT: StaticCell<PostResult> = StaticCell::new();
  let post_ref: &'static PostResult = POST_RESULT.init(post_result);
  diagnostics::set_post_result(post_ref);

  if !post_result.all_pass() {
    let msg = post_result.status_message();
    info!("POST FAILED: {}", msg);
    set_status(msg).await;
    ui::render_once(&window, &ui_root, &mut display).await;
    // If the tuner itself failed, stay in the error screen.
    if si4703_id_check.is_fail() || i2c_check.is_fail() {
      ui::run_loop(&window, &mut display, &ui_weak).await
    }
  } else {
    info!("POST: all checks passed");
  }

  info!(
    "Si4703 ready (dev=0x{:04X}, chip=0x{:04X})",
    radio_chip.device_id(),
    radio_chip.chip_id()
  );
  let _ = radio_chip.set_volume(&mut i2c, 8);

  // ------------------------------------------------------------------------
  // Boot-time tuning target: prefer the last-tuned frequency restored
  // from flash; fall back to a band scan when there's no saved value
  // (first boot, wiped flash, etc.).
  // ------------------------------------------------------------------------
  let initial_freq = if stored_presets.last_tuned != PRESET_EMPTY {
    info!("Restoring last_tuned freq: {}", stored_presets.last_tuned);
    stored_presets.last_tuned
  } else {
    set_status("Scanning...").await;
    ui::render_once(&window, &ui_root, &mut display).await;
    let mut stations = [Station::empty(); MAX_SCAN_STATIONS];
    match radio_chip.scan_stations(&mut i2c, &mut stations).await {
      Ok(count) if count > 0 => pick_strongest(&stations[..count]).unwrap_or(DEFAULT_FREQ_X10),
      _ => DEFAULT_FREQ_X10,
    }
  };

  // Boot-time RSSI sweep across the whole FM band. Runs once, before we
  // commit to the playback frequency, and seeds the on-screen spectrum
  // bar that the UI shows from then on. Failure is non-fatal — the UI
  // simply renders a flat baseline if the sweep does not complete.
  set_status("Sweep...").await;
  ui::render_once(&window, &ui_root, &mut display).await;
  let mut spectrum = [0u8; SPECTRUM_LEN];
  if radio_chip.sweep_rssi(&mut i2c, &mut spectrum).await.is_ok() {
    publish_spectrum(&spectrum).await;
  }

  let (mhz, dec) = format_freq(initial_freq);
  info!("Tuning to {}.{} MHz", mhz, dec);
  let _ = radio_chip.tune(&mut i2c, initial_freq).await;
  publish_freq(initial_freq).await;
  publish_presets(stored_presets, initial_freq).await;
  set_status("Ready").await;

  // ------------------------------------------------------------------------
  // Rotary encoder init (PCNT0)
  // ------------------------------------------------------------------------
  let mut pcnt = Pcnt::new(peripherals.PCNT);
  pcnt.set_interrupt_handler(pcnt_interrupt_handler);

  let encoder = hardware::init_encoder(
    pcnt,
    EncoderPins {
      a: peripherals.GPIO11,
      b: peripherals.GPIO18,
      key: peripherals.GPIO19,
    },
  );
  info!("Rotary encoder ready");

  // ------------------------------------------------------------------------
  // Spawn input + radio control tasks; render UI in main
  // ------------------------------------------------------------------------
  spawner.spawn(tasks::input_task(encoder).expect("create input_task token"));
  spawner.spawn(
    tasks::radio_control_task(radio_chip, i2c, preset_store, wifi_stack)
      .expect("create radio_control_task token"),
  );
  // Listening-log sampler: pure software task that snapshots
  // RADIO_STATE every 10 s into the in-RAM ring buffer for the
  // web console's replay panel.
  spawner.spawn(tasks::logger_task().expect("create logger_task token"));

  // Tiny pause to let tasks initialise before we monopolise the executor.
  Timer::after(Duration::from_millis(10)).await;

  info!("All systems running. Entering UI render loop.");
  ui::run_loop(&window, &mut display, &ui_weak).await
}
