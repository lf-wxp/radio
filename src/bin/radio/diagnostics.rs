//! Power-On Self-Test (POST) and runtime diagnostics.
//!
//! Provides boot-time hardware verification and runtime health monitoring
//! for the ESP-Radio system. The POST sequence validates all critical
//! peripherals before the main application loop starts, giving the user
//! immediate visual feedback if something is wrong.
//!
//! # POST checks (run once at boot)
//!
//! | Check           | Method                                  | Pass criteria              |
//! |-----------------|-----------------------------------------|----------------------------|
//! | I²C bus         | Read Si4703 device ID register          | No I2C NAK / timeout       |
//! | Si4703 chip ID  | Compare `device_id()` against `0x1242`  | Exact match                |
//! | Heap allocator  | Attempt a small allocation + free       | No OOM panic               |
//! | PCNT encoder    | Read initial counter value              | Returns without error      |
//!
//! # Runtime health (polled by `GET /api/health`)
//!
//! Exposes cumulative counters and instantaneous gauges that the web
//! console can display for remote troubleshooting.

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_time::Instant;
use serde::Serialize;

// ============================================================================
// POST result types
// ============================================================================

/// Individual POST check outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
  /// Check passed.
  Pass,
  /// Check failed with an error code.
  Fail(u8),
  /// Check was skipped (peripheral not present or not applicable).
  Skipped,
}

impl CheckStatus {
  pub fn is_pass(self) -> bool {
    matches!(self, Self::Pass)
  }

  pub fn is_fail(self) -> bool {
    matches!(self, Self::Fail(_))
  }
}

/// Error codes for POST failures.
///
/// Each code maps to a short mnemonic displayed on the LCD when the
/// corresponding check fails. Codes are intentionally small (u8) so
/// they fit in a single status-bar character slot.
pub mod error_codes {
  /// I²C bus communication failure (NAK or timeout).
  pub const I2C_BUS: u8 = 0x01;
  /// Si4703 device ID mismatch (wrong chip or dead bus).
  pub const SI4703_DEV_ID: u8 = 0x02;
  /// Si4703 chip initialisation failure.
  pub const SI4703_INIT: u8 = 0x03;
  /// Heap allocator test failure (OOM on small allocation).
  pub const HEAP_ALLOC: u8 = 0x04;
  /// PCNT (rotary encoder) read failure.
  #[allow(dead_code)]
  pub const PCNT_READ: u8 = 0x05;
}

/// Aggregate result of the full POST sequence.
#[derive(Clone, Copy, Debug)]
pub struct PostResult {
  pub i2c_bus: CheckStatus,
  pub si4703_id: CheckStatus,
  pub heap_alloc: CheckStatus,
  pub encoder: CheckStatus,
}

impl PostResult {
  /// Returns `true` when all checks passed (or were skipped).
  pub fn all_pass(&self) -> bool {
    !self.i2c_bus.is_fail()
      && !self.si4703_id.is_fail()
      && !self.heap_alloc.is_fail()
      && !self.encoder.is_fail()
  }

  /// Return the first failure code, if any.
  pub fn first_failure_code(&self) -> Option<u8> {
    for check in [self.i2c_bus, self.si4703_id, self.heap_alloc, self.encoder] {
      if let CheckStatus::Fail(code) = check {
        return Some(code);
      }
    }
    None
  }

  /// Format a human-readable summary for the LCD status line.
  ///
  /// Examples: `"POST OK"`, `"POST FAIL: 0x02"`.
  pub fn status_message(&self) -> &'static str {
    if self.all_pass() {
      "POST OK"
    } else {
      match self.first_failure_code() {
        Some(0x01) => "ERR: I2C bus",
        Some(0x02) => "ERR: chip ID",
        Some(0x03) => "ERR: Si4703",
        Some(0x04) => "ERR: heap",
        Some(0x05) => "ERR: encoder",
        _ => "ERR: unknown",
      }
    }
  }
}

// ============================================================================
// POST execution
// ============================================================================

/// Expected Si4703 device ID (register 0x00).
///
/// All genuine Si4703 / Si4702 chips report `0x1242` in this register.
const EXPECTED_DEVICE_ID: u16 = 0x1242;

/// Run the heap allocator self-test.
///
/// Attempts a small allocation (64 bytes), writes a pattern, verifies
/// it, then frees. This catches a misconfigured `esp-alloc` or a
/// corrupted heap early — before the Slint UI or WiFi stack try to
/// allocate and panic with an unhelpful OOM message.
pub fn check_heap() -> CheckStatus {
  // Attempt a 64-byte allocation.
  let mut buf: Vec<u8> = Vec::with_capacity(64);
  if buf.capacity() < 64 {
    return CheckStatus::Fail(error_codes::HEAP_ALLOC);
  }
  // Write a known pattern and verify.
  for i in 0..64u8 {
    buf.push(i);
  }
  for (i, &val) in buf.iter().enumerate() {
    if val != i as u8 {
      return CheckStatus::Fail(error_codes::HEAP_ALLOC);
    }
  }
  // Allocation will be freed when `buf` drops.
  CheckStatus::Pass
}

/// Verify Si4703 device ID after registers have been read.
///
/// Call this *after* `hardware::init_tuner` has completed the reset
/// sequence and the first `read_registers` has populated the register
/// shadow. The device ID register (0x00) is read-only and always
/// available, even before `Si4703::init()` powers up the chip.
pub fn check_si4703_device_id(device_id: u16) -> CheckStatus {
  if device_id == EXPECTED_DEVICE_ID {
    CheckStatus::Pass
  } else {
    CheckStatus::Fail(error_codes::SI4703_DEV_ID)
  }
}

/// Verify that the I²C bus is functional by checking if the device ID
/// read returned a non-zero value (a zero read typically indicates a
/// bus fault where SDA is stuck low).
pub fn check_i2c_bus(device_id: u16) -> CheckStatus {
  if device_id != 0 {
    CheckStatus::Pass
  } else {
    CheckStatus::Fail(error_codes::I2C_BUS)
  }
}

// ============================================================================
// Runtime health monitoring
// ============================================================================

/// Global reference to the POST result, set once during boot.
///
/// Uses `core::sync::atomic::AtomicPtr` so the health endpoint can
/// read it without a mutex. The pointer is `null` until `set_post_result`
/// is called from `main`.
static POST_RESULT_PTR: core::sync::atomic::AtomicPtr<PostResult> =
  core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Store the POST result reference globally (call once from `main`).
///
/// # Safety invariant
///
/// This function must only be called **once** during the program's
/// lifetime, with a reference that truly has `'static` lifetime
/// (e.g. obtained from `StaticCell::init`). Calling it more than once
/// would silently replace the pointer, potentially leaving stale
/// references in flight on the health endpoint.
pub fn set_post_result(result: &'static PostResult) {
  POST_RESULT_PTR.store(
    result as *const PostResult as *mut PostResult,
    Ordering::Release,
  );
}

/// Retrieve the stored POST result reference.
pub fn get_post_result() -> Option<&'static PostResult> {
  let ptr = POST_RESULT_PTR.load(Ordering::Acquire);
  if ptr.is_null() {
    None
  } else {
    // SAFETY: the pointer was set from a `&'static PostResult` in
    // `set_post_result`, so it's valid for the program's lifetime.
    Some(unsafe { &*ptr })
  }
}

/// Boot timestamp captured once at startup for uptime calculation.
static BOOT_INSTANT: AtomicU32 = AtomicU32::new(0);

/// Cumulative I²C error counter, incremented by the radio control task.
///
/// Exposed as an atomic so the health endpoint can read it without
/// acquiring the radio-state mutex (which would block the UI).
static I2C_ERROR_TOTAL: AtomicU32 = AtomicU32::new(0);

// ============================================================================
// Software watchdog
// ============================================================================

/// Timeout threshold for the software watchdog, in seconds.
///
/// If `radio_control_task` hasn't called [`watchdog_feed`] within this
/// window, the health endpoint reports the task as stalled. 5 s is
/// generous: the task ticks at 5 Hz (200 ms), so even a single missed
/// tick wouldn't trip the watchdog — only a genuine hang (deadlock,
/// infinite loop, stuck I²C STC wait) would.
pub const WATCHDOG_TIMEOUT_SECS: u32 = 5;

/// Timestamp (boot-relative seconds) of the most recent watchdog feed.
///
/// Updated atomically by [`watchdog_feed`]; read by [`watchdog_ok`].
static WATCHDOG_LAST_FEED: AtomicU32 = AtomicU32::new(0);

/// Feed the software watchdog. Call from `radio_control_task` on every
/// tick (both command-handling and periodic-refresh arms) to prove the
/// task is still making progress.
pub fn watchdog_feed() {
  let now = Instant::now().as_secs() as u32;
  // Relaxed is sufficient: ESP32-C6 is single-core RISC-V, so all
  // stores are immediately visible to all readers without fencing.
  WATCHDOG_LAST_FEED.store(now, Ordering::Relaxed);
}

/// Check whether the radio control task is still alive.
///
/// Returns `true` if the last feed was within [`WATCHDOG_TIMEOUT_SECS`]
/// of the current time, or if the watchdog has never been fed yet
/// (boot grace period — the task hasn't started its main loop).
pub fn watchdog_ok() -> bool {
  let last = WATCHDOG_LAST_FEED.load(Ordering::Relaxed);
  if last == 0 {
    // Never fed yet — task hasn't entered its main loop. Don't
    // report a false alarm during the boot sequence.
    return true;
  }
  let now = Instant::now().as_secs() as u32;
  now.saturating_sub(last) <= WATCHDOG_TIMEOUT_SECS
}

/// Seconds since the last watchdog feed. Returns `None` if the
/// watchdog has never been fed (boot grace period).
pub fn watchdog_elapsed_secs() -> Option<u32> {
  let last = WATCHDOG_LAST_FEED.load(Ordering::Relaxed);
  if last == 0 {
    return None;
  }
  let now = Instant::now().as_secs() as u32;
  Some(now.saturating_sub(last))
}

/// Record the boot instant (call once from `main`).
pub fn record_boot_time() {
  BOOT_INSTANT.store(Instant::now().as_secs() as u32, Ordering::Relaxed);
}

/// Increment the cumulative I²C error counter.
pub fn increment_i2c_errors() {
  I2C_ERROR_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Read the cumulative I²C error count.
pub fn i2c_error_total() -> u32 {
  I2C_ERROR_TOTAL.load(Ordering::Relaxed)
}

/// Compute uptime in seconds since boot.
pub fn uptime_secs() -> u32 {
  let boot = BOOT_INSTANT.load(Ordering::Relaxed);
  let now = Instant::now().as_secs() as u32;
  now.saturating_sub(boot)
}

/// Query the heap's free bytes.
///
/// Uses `esp_alloc::HEAP` stats when available; returns 0 if the
/// allocator doesn't expose usage info (shouldn't happen on ESP32-C6).
pub fn heap_free_bytes() -> usize {
  esp_alloc::HEAP.free()
}

/// Query the heap's total usable bytes.
pub fn heap_total_bytes() -> usize {
  esp_alloc::HEAP.used() + esp_alloc::HEAP.free()
}

/// JSON-serialisable health snapshot returned by `GET /api/health`.
#[derive(Serialize)]
pub struct HealthDto {
  /// Uptime in seconds since boot.
  pub uptime_secs: u32,
  /// Heap free bytes.
  pub heap_free: usize,
  /// Heap total bytes.
  pub heap_total: usize,
  /// Heap usage percentage (0–100).
  pub heap_usage_pct: u8,
  /// Cumulative I²C error count since boot.
  pub i2c_errors: u32,
  /// Whether WiFi is currently connected.
  pub wifi_connected: bool,
  /// Current RSSI (0–75).
  pub rssi: u8,
  /// Whether the tuner chip responded correctly at boot.
  pub tuner_ok: bool,
  /// POST result summary string.
  pub post_status: &'static str,
  /// `true` when the radio control task has fed the watchdog within
  /// the last [`WATCHDOG_TIMEOUT_SECS`] seconds.
  pub radio_task_alive: bool,
  /// Seconds since the last watchdog feed, or `null` if the task
  /// hasn't started its main loop yet.
  pub watchdog_elapsed_secs: Option<u32>,
}

impl HealthDto {
  /// Build a health snapshot from the current system state.
  pub async fn capture(post_result: &'static PostResult) -> Self {
    let uptime = uptime_secs();
    let free = heap_free_bytes();
    let total = heap_total_bytes();
    let usage_pct = if total > 0 {
      ((total - free) * 100 / total) as u8
    } else {
      0
    };

    let (wifi_connected, rssi) = {
      let state = crate::state::RADIO_STATE.lock().await;
      (state.wifi_connected, state.rssi)
    };

    Self {
      uptime_secs: uptime,
      heap_free: free,
      heap_total: total,
      heap_usage_pct: usage_pct,
      i2c_errors: i2c_error_total(),
      wifi_connected,
      rssi,
      tuner_ok: post_result.si4703_id.is_pass(),
      post_status: post_result.status_message(),
      radio_task_alive: watchdog_ok(),
      watchdog_elapsed_secs: watchdog_elapsed_secs(),
    }
  }
}
