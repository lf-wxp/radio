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

/// Number of buckets in the boot-time RSSI spectrum sweep.
///
/// 52 buckets across the 87.5–108.0 MHz FM band gives a 0.4 MHz step
/// (≈ 4 channels per bucket at the Si4703's 100 kHz spacing). That is
/// dense enough to make individual stations stand out as distinct
/// peaks while keeping the sweep under ~3.5 s of blocking I²C traffic
/// at boot.
pub const SPECTRUM_LEN: usize = 52;

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
  /// True when the Si4703 is currently locked onto a stereo pilot.
  /// Updated on every refresh tick (~5 Hz) by reading STATUSRSSI bit 8.
  pub stereo: bool,
  /// True when the radio-control task has *automatically* forced mono
  /// mode in response to a sustained low RSSI. Used by the UI to show
  /// an `auto-MO` hint instead of the regular stereo indicator.
  pub auto_mono: bool,
  /// Decoded RDS PS (programme service) name.
  pub station_name: String,
  /// Decoded RDS RT (RadioText) message; empty when station does not broadcast RT.
  pub radio_text: String,
  /// Local time `(hour, minute)` decoded from RDS CT, or `None` until
  /// the first valid CT frame is received on the current station.
  pub clock_hh_mm: Option<(u8, u8)>,
  /// Short Programme Type label (e.g. `"News"`, `"Pop M"`) decoded from
  /// every RDS Block B. `None` when no RDS group has been received yet
  /// or when the broadcaster reports PTY 0 ("no programme type").
  pub pty_label: Option<&'static str>,
  pub wifi_connected: bool,
  pub wifi_ssid: String,
  pub status: &'static str,
  /// Snapshot of the RSSI band sweep captured at boot time.
  ///
  /// Each `spectrum[i]` is the chip-reported RSSI (`0..=75`) at the
  /// centre of bucket `i`, where bucket 0 is at the band's bottom
  /// frequency and bucket [`SPECTRUM_LEN`] − 1 sits one step below the
  /// band top. All zeros until [`crate::main`] runs the boot sweep.
  ///
  /// Stored inline as a fixed-size array (no heap, [`Copy`]-like
  /// semantics) so the UI render task can copy it cheaply on every
  /// frame without allocating.
  pub spectrum: [u8; SPECTRUM_LEN],
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
      stereo: false,
      auto_mono: false,
      station_name: String::new(),
      radio_text: String::new(),
      clock_hh_mm: None,
      pty_label: None,
      wifi_connected: false,
      wifi_ssid: String::new(),
      status: "Booting...",
      spectrum: [0; SPECTRUM_LEN],
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

/// Replace the shared spectrum snapshot.
///
/// Caller is expected to pass the freshly captured sweep buffer; the
/// internal copy is `memcpy`-cheap (52 bytes) so we don't bother with
/// a dirty diff.
pub async fn publish_spectrum(spectrum: &[u8; SPECTRUM_LEN]) {
  let mut state = RADIO_STATE.lock().await;
  state.spectrum.copy_from_slice(spectrum);
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

/// Update the shared Programme Type (PTY) snapshot.
///
/// Pass `None` to hide the badge (e.g. on station change before any RDS
/// group is decoded, or when PTY = 0 "None").
pub async fn publish_pty(label: Option<&'static str>) {
  let mut state = RADIO_STATE.lock().await;
  if state.pty_label != label {
    state.pty_label = label;
    state.dirty = true;
  }
}

/// Update the shared status line.
pub async fn set_status(status: &'static str) {
  let mut state = RADIO_STATE.lock().await;
  state.status = status;
  state.dirty = true;
}
