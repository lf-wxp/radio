//! Shared state, commands and helpers used across the radio binary.
//!
//! Lives in the binary crate (not `lib.rs`) because nothing here should
//! leak out to library consumers; it's pure orchestration glue.

use alloc::string::String;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;

use radio::si4703::Station;

// ============================================================================
// Constants
// ============================================================================

/// Default frequency in MHz x 10 used as a fallback when scan finds nothing.
pub const DEFAULT_FREQ_X10: u16 = 875;

/// Tuning step in MHz x 10 (i.e. 0.1 MHz).
pub const TUNE_STEP_X10: i16 = 1;

/// Long-press threshold for the encoder button (milliseconds).
pub const LONG_PRESS_MS: u64 = 800;

/// Maximum number of stations remembered during the boot-time scan.
pub const MAX_SCAN_STATIONS: usize = 20;

// ============================================================================
// Shared state types
// ============================================================================

/// Commands sent from the input task to the radio-control task.
#[derive(Clone, Copy, Debug, defmt::Format)]
pub enum RadioCommand {
  /// Tune by a relative number of 0.1 MHz steps (positive = up).
  TuneRelative(i16),
  /// Seek to the next station upwards.
  SeekUp,
  /// Toggle mute.
  ToggleMute,
}

/// Snapshot of radio state shared with the UI thread.
///
/// Mixed field types are intentional:
/// - `station_name: String` — RDS PS is 8 bytes, but after decoding
///   (UTF-8 / GB2312 → placeholders / Latin-1) it may take 0–16 chars,
///   so a heap `String` keeps the rendering path simple.
/// - `radio_text: String` — RDS RT is up to 64 bytes; allocating once
///   per message is cheap compared with the I2C / scrolling cost.
/// - `wifi_ssid: String` — variable length, set rarely (once on connect),
///   so a single heap allocation is acceptable.
/// - `status: &'static str` — only ever points at compile-time strings.
/// - `clock_hh_mm: Option<(u8, u8)>` — latest local time decoded from
///   RDS Group 4A (Clock-Time). `None` until the broadcaster sends a CT
///   frame (typically within 60 s of tuning).
///
/// The `dirty` flag protects the UI loop from cloning the whole struct
/// (and the heap-allocated SSID) on every render frame.
#[derive(Clone, Debug)]
pub struct RadioState {
  pub freq_mhz_x10: u16,
  pub rssi: u8,
  pub volume: u8,
  pub muted: bool,
  /// Decoded RDS PS (programme service) name.
  pub station_name: String,
  /// Decoded RDS RT (RadioText) message; empty when station does not broadcast RT.
  pub radio_text: String,
  /// Local time `(hour, minute)` decoded from RDS CT, or `None` until
  /// the first valid CT frame is received on the current station.
  pub clock_hh_mm: Option<(u8, u8)>,
  pub wifi_connected: bool,
  pub wifi_ssid: String,
  pub status: &'static str,
  /// True when fields have been mutated since the UI last read them.
  pub dirty: bool,
}

impl RadioState {
  pub const fn boot() -> Self {
    Self {
      freq_mhz_x10: DEFAULT_FREQ_X10,
      rssi: 0,
      volume: 8,
      muted: false,
      station_name: String::new(),
      radio_text: String::new(),
      clock_hh_mm: None,
      wifi_connected: false,
      wifi_ssid: String::new(),
      status: "Booting...",
      dirty: true,
    }
  }
}

// ============================================================================
// Globals (shared between tasks)
// ============================================================================

/// Bounded queue of input commands.
///
/// Capacity is small (8) but enough to absorb a burst of rotary deltas plus
/// a button event while the radio control task is busy on a long I2C op
/// (e.g. the 5 s STC wait inside [`radio::si4703::Si4703::tune`]). Rotary
/// deltas are pre-aggregated by `input_task` so a single tick produces at
/// most one `TuneRelative` enqueue.
pub static INPUT_CMDS: Channel<CriticalSectionRawMutex, RadioCommand, 8> = Channel::new();

/// Shared radio state for the UI to read.
pub static RADIO_STATE: Mutex<CriticalSectionRawMutex, RadioState> = Mutex::new(RadioState::boot());

// ============================================================================
// Helpers (pure / state-mutating utilities)
// ============================================================================

/// Default placeholder shown when no RDS PS name has been decoded yet.
pub const STATION_NAME_PLACEHOLDER: &str = "--------";

/// Choose the station with the highest RSSI from a slice.
///
/// Returns `None` if the slice is empty, allowing the caller to decide
/// on a fallback strategy explicitly.
pub fn pick_strongest(stations: &[Station]) -> Option<u16> {
  stations
    .iter()
    .max_by_key(|s| s.rssi)
    .map(|s| s.freq_mhz_x10)
}

/// Clamp a candidate frequency (MHz x 10) to the US/Europe FM band.
///
/// The clamped range `875..=1080` (i.e. 87.5–108.0 MHz) is well within
/// `u16::MAX`, so the trailing `as u16` is lossless.
pub fn clamp_freq(freq: i32) -> u16 {
  freq.clamp(875, 1080) as u16
}

/// Update the shared frequency snapshot.
pub async fn publish_freq(freq_mhz_x10: u16) {
  let mut state = RADIO_STATE.lock().await;
  state.freq_mhz_x10 = freq_mhz_x10;
  state.dirty = true;
}

/// Update the shared station-name snapshot.
pub async fn publish_station_name(name: String) {
  let mut state = RADIO_STATE.lock().await;
  state.station_name = name;
  state.dirty = true;
}

/// Update the shared RadioText (RT) snapshot.
pub async fn publish_radio_text(text: String) {
  let mut state = RADIO_STATE.lock().await;
  state.radio_text = text;
  state.dirty = true;
}

/// Update the shared local clock snapshot.
///
/// Pass `None` to clear the clock (e.g. on station change before the new
/// CT is decoded), or `Some((hh, mm))` to publish a fresh wall-clock value.
pub async fn publish_clock(hh_mm: Option<(u8, u8)>) {
  let mut state = RADIO_STATE.lock().await;
  if state.clock_hh_mm != hh_mm {
    state.clock_hh_mm = hh_mm;
    state.dirty = true;
  }
}

/// Update the shared status line.
pub async fn set_status(status: &'static str) {
  let mut state = RADIO_STATE.lock().await;
  state.status = status;
  state.dirty = true;
}
