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
///
/// Holding the button between [`LONG_PRESS_MS`] and [`ULTRA_LONG_PRESS_MS`]
/// triggers `SavePreset`; sustained hold past [`ULTRA_LONG_PRESS_MS`]
/// upgrades the gesture to `ToggleMute`.
pub const LONG_PRESS_MS: u64 = 800;

/// Ultra-long-press threshold (milliseconds).
///
/// Reached only when the user keeps holding past [`LONG_PRESS_MS`] for
/// another ~1.7 s; primarily used to keep mute toggle accessible without
/// crowding the more frequent `SavePreset` shortcut.
pub const ULTRA_LONG_PRESS_MS: u64 = 2_500;

/// Maximum number of preset (favourite) stations stored on Flash.
///
/// 8 fits a single 4-bit slot index and shows comfortably as one row of
/// dots on a 240-pixel wide UI; raising it requires bumping
/// [`PresetSet`]'s on-Flash record version (see `presets::storage`).
pub const MAX_PRESETS: usize = 8;

/// Sentinel value stored in an empty [`PresetSet`] slot.
///
/// `0` is below the FM band's lower bound (`875` = 87.5 MHz) so it
/// can never collide with a real frequency.
pub const PRESET_EMPTY: u16 = 0;

/// Returns `true` when the 8-byte PS buffer holds the "unknown"
/// sentinel (all zeros or all ASCII spaces). Used by both the flash
/// write path ([`crate::tasks::preset_ps_snapshot`]) and the JSON
/// serialisation path ([`crate::web::decode_preset_ps`]) to avoid
/// duplicating the sentinel check.
#[inline]
pub fn is_ps_unknown(buf: &[u8; 8]) -> bool {
  buf.iter().all(|&b| b == 0 || b == b' ')
}

/// Quiet-period (milliseconds) before persisting `last_tuned` to Flash.
///
/// Tuning hammers the encoder; we coalesce bursts so we only write
/// Flash once the dial has settled. 30 s is short enough that a normal
/// "tune then walk away" still saves before power-off, yet long enough
/// to keep Flash erase counts in the low thousands across years of use.
pub const LAST_TUNED_DEBOUNCE_MS: u64 = 30_000;

/// Minimum interval between consecutive metadata-fill attempts.
///
/// 5 s is long enough for a full RDS PS name to scroll past (typically
/// 4–8 s) while still being responsive enough that the UI label appears
/// within one listen session. Keeps CPU and flash-check overhead to
/// ~0.2 calls/s instead of the raw 5 Hz tick rate.
pub const META_FILL_INTERVAL_MS: u64 = 5_000;

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
  /// Tune to an exact frequency in MHz × 10 (e.g. `1015` = 101.5 MHz).
  ///
  /// Used by the LAN web console to jump to an arbitrary station
  /// without first having to read the current frequency. The radio
  /// task clamps to the FM band before issuing the I2C tune.
  TuneAbsolute(u16),
  /// Toggle mute.
  ToggleMute,
  /// Save the current frequency into the next preset slot.
  ///
  /// If the frequency is already saved, the command is a no-op.
  /// Otherwise it overwrites the oldest slot when the table is full,
  /// keeping the working set bounded by [`crate::state::MAX_PRESETS`].
  SavePreset,
  /// Cycle to the next saved preset (wraps around).
  ///
  /// Falls back to a `seek-up` inside the radio task when the preset
  /// table is empty, so the gesture stays useful from cold boot.
  CyclePreset,
  /// Re-run the boot-time RSSI sweep across the whole FM band.
  ///
  /// Tunes through [`SPECTRUM_LEN`] buckets, samples RSSI on each, and
  /// republishes [`RadioState::spectrum`]. The radio is briefly
  /// off-air for the duration of the sweep (~4 s on a 52-bucket
  /// band-plan; the buckets each settle for ~80 ms), then returns to
  /// the original frequency.
  ///
  /// Triggered by the LAN web console "Scan" button. The handler is
  /// careful to feed the watchdog mid-sweep because a full 52-bucket
  /// pass would otherwise come within ~600 ms of the 5 s timeout.
  SpectrumScan,
}

/// Snapshot of the user's saved presets, copied by value into
/// [`RadioState`] for the input task and UI to read without needing a
/// second lock.
///
/// On-disk persistence lives in `presets::storage::PresetStore`; this
/// type is the in-memory mirror.
///
/// ## Metadata cache (schema v2)
///
/// In addition to the bare frequencies, every slot can optionally
/// carry the RDS Programme Identification (`pi`) and Programme
/// Service (`ps`) name decoded the last time the listener visited
/// that station. They are populated lazily — either at the moment a
/// preset is saved (if RDS has already locked) or shortly afterwards
/// by a small background task that watches the live RDS feed and
/// fills in any gaps the next time the listener tunes back. The UI
/// uses `ps` (when present) as a friendly label on preset buttons so
/// the user sees `BBC R1` instead of the raw `97.7` digits.
///
/// Sentinel values:
/// - `pi[i] == 0` means PI is unknown for slot `i` (real PI codes
///   never use 0; the RDS spec reserves it for "no PI yet").
/// - `ps[i] == [0; 8]` means PS is unknown for slot `i`. Real PS names
///   are 8 ASCII-printable chars (or `0x20` padding) so an all-zero
///   buffer is unambiguous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresetSet {
  /// Frequencies in MHz x 10. Empty slots hold [`PRESET_EMPTY`].
  pub freqs: [u16; MAX_PRESETS],
  /// Last tuned frequency (MHz x 10), restored on next boot.
  /// `0` means "unset" — fall back to the boot scan in that case.
  pub last_tuned: u16,
  /// Cached RDS Programme Identification per slot (`0` = unknown).
  pub pi: [u16; MAX_PRESETS],
  /// Cached RDS Programme Service name per slot.
  ///
  /// Each entry is the 8-byte raw PS buffer (ASCII / Latin-1 with
  /// `0x20` padding). `[0; 8]` means unknown. We store the raw bytes
  /// rather than a decoded `String` so the type stays [`Copy`] and
  /// matches the on-Flash schema 1:1.
  pub ps: [[u8; 8]; MAX_PRESETS],
}

impl PresetSet {
  /// All slots empty; used both as the boot default and after a wipe.
  #[must_use]
  pub const fn empty() -> Self {
    Self {
      freqs: [PRESET_EMPTY; MAX_PRESETS],
      last_tuned: PRESET_EMPTY,
      pi: [0; MAX_PRESETS],
      ps: [[0; 8]; MAX_PRESETS],
    }
  }

  /// Return how many slots currently hold a real frequency.
  #[must_use]
  pub fn used(&self) -> usize {
    self.freqs.iter().filter(|&&f| f != PRESET_EMPTY).count()
  }

  /// Find the slot index storing `freq_x10`, if any.
  #[must_use]
  pub fn position(&self, freq_x10: u16) -> Option<usize> {
    self.freqs.iter().position(|&f| f == freq_x10)
  }

  /// Cached PI for slot `idx`, or `None` when the RDS broadcaster
  /// hasn't been heard yet for that frequency.
  #[must_use]
  #[allow(
    dead_code,
    reason = "public read-side API reserved for future UI surfaces \
              (LCD label rendering) that don't go through the web \
              JSON path."
  )]
  pub fn pi_for(&self, idx: usize) -> Option<u16> {
    self.pi.get(idx).copied().filter(|&pi| pi != 0)
  }

  /// Cached raw PS buffer for slot `idx`, or `None` when unknown.
  ///
  /// Returned as the raw 8-byte array; the bytes are ASCII / Latin-1
  /// with `0x20` padding (i.e. the wire format from RDS Group 0A).
  /// Callers that want a printable label should strip trailing
  /// whitespace and run a UTF-8 lossy conversion.
  #[must_use]
  #[allow(
    dead_code,
    reason = "public read-side API reserved for future UI surfaces \
              (LCD label rendering) that don't go through the web \
              JSON path."
  )]
  pub fn ps_for(&self, idx: usize) -> Option<&[u8; 8]> {
    self.ps.get(idx).filter(|buf| buf.iter().any(|&b| b != 0))
  }

  /// Insert `freq_x10` into the next free slot, returning that index.
  ///
  /// If the frequency is already saved, returns its existing index.
  /// If all slots are full, the first slot is overwritten (FIFO).
  ///
  /// Metadata for newly-claimed slots starts blank; use
  /// [`Self::set_meta`] (or [`Self::save_with_meta`]) to populate
  /// PI / PS once RDS reports them.
  pub fn save(&mut self, freq_x10: u16) -> usize {
    if let Some(idx) = self.position(freq_x10) {
      return idx;
    }
    if let Some(idx) = self.freqs.iter().position(|&f| f == PRESET_EMPTY) {
      self.freqs[idx] = freq_x10;
      // No metadata yet — keep sentinels.
      self.pi[idx] = 0;
      self.ps[idx] = [0; 8];
      return idx;
    }
    // FIFO eviction: shift left, append. Metadata follows the freqs
    // so the cached PI/PS for the evicted slot 0 is dropped together
    // with the frequency.
    self.freqs.copy_within(1.., 0);
    self.pi.copy_within(1.., 0);
    self.ps.copy_within(1.., 0);
    let last = MAX_PRESETS - 1;
    self.freqs[last] = freq_x10;
    self.pi[last] = 0;
    self.ps[last] = [0; 8];
    last
  }

  /// Save `freq_x10` and seed its RDS metadata in one shot.
  ///
  /// Convenience around [`Self::save`] + [`Self::set_meta`] for the
  /// case where the listener taps "Save" while RDS is already locked
  /// — we capture the station identity atomically rather than
  /// waiting for the background watcher to fill it in.
  ///
  /// Pass `None` for either field to leave the corresponding cache
  /// untouched (useful when only PI is known but PS hasn't scrolled
  /// past yet, or vice-versa).
  pub fn save_with_meta(&mut self, freq_x10: u16, pi: Option<u16>, ps: Option<[u8; 8]>) -> usize {
    let idx = self.save(freq_x10);
    self.set_meta(idx, pi, ps);
    idx
  }

  /// Update the cached PI / PS for an existing slot.
  ///
  /// Called by the background "fill RDS metadata" loop after the
  /// listener has tuned to a saved preset for long enough to decode
  /// PI and a full PS name. `None` arguments leave the existing
  /// cache entry alone — important because PI typically locks
  /// within ~1 s while a complete PS name can take 5–10 s, and we
  /// don't want the early call (PI only) to wipe the PS cache from
  /// a previous session.
  ///
  /// No-op when `idx` is out of range so callers don't need to
  /// double-check after a `position()` lookup that may have changed
  /// under FIFO eviction.
  pub fn set_meta(&mut self, idx: usize, pi: Option<u16>, ps: Option<[u8; 8]>) {
    if idx >= MAX_PRESETS {
      return;
    }
    if let Some(pi) = pi {
      self.pi[idx] = pi;
    }
    if let Some(ps) = ps {
      self.ps[idx] = ps;
    }
  }

  /// Return the next saved frequency after `current` (wrap-around).
  ///
  /// `None` when the table is empty. Note: if all occupied slots hold
  /// the same frequency as `current`, the returned value will equal
  /// `current` — callers should guard against this (e.g. `Some(t) if
  /// t != current`) to avoid a redundant tune.
  #[must_use]
  pub fn next_after(&self, current: u16) -> Option<u16> {
    if self.used() == 0 {
      return None;
    }
    // Start search from the slot after `current` (or 0 if not present).
    let start = self.position(current).map_or(0, |i| i + 1);
    (0..MAX_PRESETS)
      .map(|offset| (start + offset) % MAX_PRESETS)
      .map(|i| self.freqs[i])
      .find(|&f| f != PRESET_EMPTY)
  }
}

impl Default for PresetSet {
  fn default() -> Self {
    Self::empty()
  }
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
#[allow(
  clippy::large_stack_frames,
  reason = "the auto-derived `Clone` of RadioState pulls in three heap String \
            buffers, an inline 52-byte spectrum, and a PresetSet — all of \
            which the UI render path intentionally clones on every dirty \
            tick. ~1.1 KiB stays well under the 16 KiB Embassy task stack."
)]
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
  /// RT+ "item title" — song / programme title parsed out of the RT
  /// buffer using the broadcaster-supplied (start, len) tag. `None`
  /// when the station does not transmit RT+ or while the broadcaster
  /// has signalled that no song is currently in progress (the
  /// "item-running" bit is clear). UI code typically renders
  /// `“{artist} — {title}”` only when both fields are present and
  /// falls back to the raw RT scroller otherwise.
  pub rt_plus_title: Option<String>,
  /// RT+ "item artist" — see [`Self::rt_plus_title`].
  pub rt_plus_artist: Option<String>,
  /// Local time `(hour, minute)` decoded from RDS CT, or `None` until
  /// the first valid CT frame is received on the current station.
  pub clock_hh_mm: Option<(u8, u8)>,
  /// Short Programme Type label (e.g. `"News"`, `"Pop M"`) decoded from
  /// every RDS Block B. `None` when no RDS group has been received yet
  /// or when the broadcaster reports PTY 0 ("no programme type").
  pub pty_label: Option<&'static str>,
  /// Number of distinct alternative-frequency (AF) entries decoded
  /// from RDS group 0A on the current station. `0` until the
  /// broadcaster transmits an AF list (or when the station does not
  /// participate in AF). The UI surfaces this as a small `AF·N` badge
  /// so listeners know the receiver may switch frequencies on weak
  /// signal.
  pub af_count: u8,
  /// `true` while an AF probe is actively executing (chip is briefly
  /// off the original frequency tuning candidate AFs). The UI uses
  /// this to flash an indicator and suppress transient RSSI/RDS
  /// updates that don't reflect the listener's chosen station.
  pub af_following: bool,
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
  /// In-memory mirror of the persisted preset table.
  ///
  /// Read by `input_task` to drive the smart short-press fallback
  /// ("cycle preset if any saved, else SeekUp") and by the UI to
  /// render the `P n/m` indicator. Written exclusively by the radio
  /// control task after a successful Flash store.
  pub presets: PresetSet,
  /// Slot index of the currently tuned preset, or `None` if the dial
  /// sits on a frequency that hasn't been saved.
  pub preset_idx: Option<u8>,
  /// IPv4 address of the device's web console, in network-byte order.
  ///
  /// `None` until WiFi has joined a STA network and DHCP completes; the
  /// UI hides the badge in that case. Surfacing the address on the
  /// LCD lets the listener type it into a phone browser without
  /// digging through the router admin panel.
  pub web_ip: Option<[u8; 4]>,
  /// True while an OTA update is actively writing to flash.
  ///
  /// Set by `ota_task` while it owns the flash handle on loan from the
  /// preset store (see [`crate::presets::PresetStore::pause`]). Other
  /// flash writers (the `last_tuned` debounce flush, preset save) MUST
  /// short-circuit while this is true so they don't try to write a
  /// flash sector that no longer belongs to them. The OTA writer
  /// itself doesn't touch the `storage` partition, so this is purely a
  /// defensive interlock for the cooperative ownership transfer.
  pub ota_in_progress: bool,
  /// Lifecycle phase of the most recent OTA job.
  ///
  /// Starts at [`OtaProgress::Idle`] and is advanced by `ota_task`. The
  /// LAN web console polls this through `GET /api/state` (via
  /// [`OtaProgressDto`]) to drive its progress bar / status label.
  pub ota_progress: OtaProgress,
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
      rt_plus_title: None,
      rt_plus_artist: None,
      clock_hh_mm: None,
      pty_label: None,
      af_count: 0,
      af_following: false,
      wifi_connected: false,
      wifi_ssid: String::new(),
      status: "Booting...",
      spectrum: [0; SPECTRUM_LEN],
      presets: PresetSet::empty(),
      preset_idx: None,
      web_ip: None,
      ota_in_progress: false,
      ota_progress: OtaProgress::Idle,
      dirty: true,
    }
  }
}

// ============================================================================
// OTA progress state machine
// ============================================================================

/// Lifecycle phase of an OTA download + flash job.
///
/// Carries enough information for the LAN web console to render a useful
/// status line without separate polling endpoints. Stays [`Copy`] (no
/// heap fields, error reasons are `&'static str`) so cloning into the
/// JSON serialiser is `memcpy`-cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum OtaProgress {
  /// No OTA job has been started yet, or the previous job's terminal
  /// state has been observed and reset.
  Idle,
  /// Resolving + connecting to the upstream HTTP server.
  Connecting,
  /// Streaming bytes into the inactive slot.
  ///
  /// `total = 0` means the server didn't return a `Content-Length`
  /// header (e.g. chunked transfer); the UI should fall back to an
  /// indeterminate spinner in that case.
  Downloading { received: u32, total: u32 },
  /// Final flush + OTA-data flip is in progress (sub-second).
  Activating,
  /// New image staged successfully; waiting for a manual reboot.
  Success,
  /// Update aborted. The running image is unchanged.
  ///
  /// The reason string **must** be a compile-time `&'static str` literal
  /// (not a `Box::leak`-ed dynamic string). `publish_ota_progress` relies
  /// on `PartialEq` to deduplicate publishes, and `&'static str` equality
  /// compares content — but pointer-interning assumptions in future
  /// optimisations could break if dynamic strings are introduced.
  Failed(&'static str),
}

/// Bounded queue of input commands.
///
/// Capacity is small (8) but enough to absorb a burst of rotary deltas plus
/// a button event while the radio control task is busy on a long I2C op
/// (e.g. the 5 s STC wait inside [`radio::si4703::Si4703::tune`]). Rotary
/// deltas are pre-aggregated by `input_task` so a single tick produces at
/// most one `TuneRelative` enqueue.
pub static INPUT_CMDS: Channel<CriticalSectionRawMutex, RadioCommand, 8> = Channel::new();

/// Single-slot mailbox for the OTA controller task.
///
/// Decoupled from [`INPUT_CMDS`] for two reasons:
///
/// 1. **Resource policy** — OTA uses the flash peripheral; only one
///    job can be in flight. A single-slot signal naturally rate-limits
///    re-triggers (a second `StartOta` posted while the first is
///    running silently overwrites the queued URL, but the in-flight
///    job keeps going).
/// 2. **Lifetime** — the OTA task pauses the preset store, takes the
///    flash, runs for ~30 s, then hands flash back. Routing this
///    through the radio control task would block tuning for the full
///    download.
pub static OTA_CMDS: embassy_sync::signal::Signal<CriticalSectionRawMutex, OtaCommand> =
  embassy_sync::signal::Signal::new();

/// Commands accepted by the OTA controller task.
#[derive(Clone, Debug)]
pub enum OtaCommand {
  /// Begin downloading + flashing a firmware image from a plain-HTTP URL.
  Start(String),
}

impl defmt::Format for OtaCommand {
  fn format(&self, fmt: defmt::Formatter<'_>) {
    match self {
      // Avoid leaking arbitrary URL contents through the defmt
      // ringbuffer; the length is enough for diagnostics.
      Self::Start(url) => defmt::write!(fmt, "Start(<url len={}>)", url.len()),
    }
  }
}

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

/// Update the shared RT+ (RadioText Plus) tag pair.
///
/// Both fields move together: a station that publishes one half of the
/// pair without the other is rare in practice, and treating them
/// atomically lets the UI flip from "raw RT scroller" to "artist — title
/// chip" in a single dirty tick.
///
/// Pass `(None, None)` when the broadcaster clears the "item running"
/// bit (between songs / during station IDs) so the UI hides the chip.
pub async fn publish_rt_plus(title: Option<String>, artist: Option<String>) {
  let mut state = RADIO_STATE.lock().await;
  if state.rt_plus_title != title || state.rt_plus_artist != artist {
    state.rt_plus_title = title;
    state.rt_plus_artist = artist;
    state.dirty = true;
  }
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

/// Update the shared AF list size and "probe in progress" indicator.
///
/// Called at the end of every refresh tick (with the latest list size
/// and `following = false`) and from inside [`crate::tasks::run_af_probe`]
/// (with `following = true`) so the UI can briefly highlight the badge.
pub async fn publish_af_status(af_count: u8, af_following: bool) {
  let mut state = RADIO_STATE.lock().await;
  if state.af_count != af_count || state.af_following != af_following {
    state.af_count = af_count;
    state.af_following = af_following;
    state.dirty = true;
  }
}

/// Publish the device's IPv4 address (octets in network-byte order) so
/// the LCD can show the web-console URL.
///
/// Pass `None` to clear the badge (e.g. on WiFi disconnect).
pub async fn publish_web_ip(ip: Option<[u8; 4]>) {
  let mut state = RADIO_STATE.lock().await;
  if state.web_ip != ip {
    state.web_ip = ip;
    state.dirty = true;
  }
}

/// Mark whether an OTA update is in flight.
///
/// Acts as the cooperative interlock between the OTA writer (which
/// borrows the flash handle from the preset store via
/// [`crate::presets::PresetStore::pause`]) and the radio control task
/// (which would otherwise try to flush `last_tuned` mid-update). Idle
/// debounce work observes this flag on every tick, so a missed publish
/// just delays a flash write by 200 ms — never produces a races on the
/// flash peripheral itself.
pub async fn publish_ota_in_progress(in_progress: bool) {
  let mut state = RADIO_STATE.lock().await;
  if state.ota_in_progress != in_progress {
    state.ota_in_progress = in_progress;
    state.dirty = true;
  }
}

/// Publish a new [`OtaProgress`] phase.
///
/// Cheap to call: bails out early when the phase is unchanged so the
/// UI's `dirty`-driven render loop doesn't get woken up just because
/// the downloader passed another HTTP chunk through. The downloader
/// reports byte counts via [`OtaProgress::Downloading`] explicitly so
/// callers should re-publish on every meaningful threshold (currently
/// every ~1% in [`crate::ota::run`]).
pub async fn publish_ota_progress(progress: OtaProgress) {
  let mut state = RADIO_STATE.lock().await;
  if state.ota_progress != progress {
    state.ota_progress = progress;
    state.dirty = true;
  }
}

/// Update the shared status line.
pub async fn set_status(status: &'static str) {
  let mut state = RADIO_STATE.lock().await;
  state.status = status;
  state.dirty = true;
}

/// Publish a fresh `PresetSet` snapshot together with the recomputed
/// active-slot index for the given current frequency.
///
/// Called by `radio_control_task` after every successful Flash store
/// and after a tune that may have moved onto / off a saved preset.
pub async fn publish_presets(presets: PresetSet, current_freq_x10: u16) {
  let preset_idx = presets.position(current_freq_x10).map(|i| i as u8);
  let mut state = RADIO_STATE.lock().await;
  if state.presets != presets || state.preset_idx != preset_idx {
    state.presets = presets;
    state.preset_idx = preset_idx;
    state.dirty = true;
  }
}
