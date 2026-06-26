//! In-memory rolling log of what the listener was tuned to.
//!
//! Roadmap #9. A snapshot of the radio state (frequency / RSSI /
//! station name / radio text) is appended every
//! [`SAMPLE_INTERVAL_SECS`] seconds by [`crate::tasks::logger_task`],
//! and the web console renders the most recent entries on its own
//! panel so listeners can scroll back through "what was that song
//! the host just played?".
//!
//! ## Why RAM-only?
//!
//! README #9 asks for a "rolling flash buffer". Persisting across
//! reboots would need a wear-levelling scheme on top of `esp-storage`
//! plus a versioned schema, which is well past the 1-day budget the
//! roadmap allocates this feature. Keeping the log in SRAM (about
//! 3 KiB at the configured capacity) gives 90 % of the user value
//! \u2014 a session-scoped replay panel \u2014 without spending the flash
//! design budget that #11 (OTA) really needs. A flash-backed history
//! can be added later as a strict superset.
//!
//! ## Sampling rules
//!
//! - One sample every [`SAMPLE_INTERVAL_SECS`] seconds (10 s).
//! - A new sample is **only** appended when something the listener
//!   actually cares about has changed: the frequency, or the decoded
//!   PS station name. Otherwise the sampler keeps quiet so the log
//!   doesn't fill up with 360 identical rows after an hour on the
//!   same station.
//! - Capacity is [`LOG_CAPACITY`]; the oldest entry is overwritten
//!   FIFO when the buffer is full.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;

use crate::state::RadioState;

// ============================================================================
// Configuration
// ============================================================================

/// Maximum number of log entries kept in memory.
///
/// 64 entries at 10 s/entry covers the most recent ~10 minutes of
/// distinct listening events (after de-duplication, real wall-clock
/// coverage is typically much longer because the sampler skips
/// unchanged frames).
pub const LOG_CAPACITY: usize = 64;

/// Sampling cadence for [`crate::tasks::logger_task`].
///
/// 10 s is short enough to catch song-level PS changes on stations
/// that actually rotate the PS field, and long enough that the loop
/// barely shows up in CPU profiles.
pub const SAMPLE_INTERVAL_SECS: u64 = 10;

/// Maximum bytes copied from the live `RadioState::station_name` into
/// each log entry. RDS PS is by spec at most 8 ASCII chars; we set
/// the inline-buffer cap there.
const PS_MAX: usize = 8;

/// Maximum bytes copied from the live `RadioState::radio_text` into
/// each log entry. Real RT can run up to 64 chars; truncating to 24
/// keeps each `LogEntry` small enough that the whole 64-entry buffer
/// stays around 4 KiB without losing the gist of the message.
const RT_MAX: usize = 24;

// ============================================================================
// Types
// ============================================================================

/// Single sampled snapshot.
///
/// `Clone` (cheap because all fields are inline, no heap) so the web
/// layer can copy a slice out under the lock and then drop it
/// immediately, rather than serialising while holding the mutex.
///
/// Strings are stored as fixed-size byte buffers + a length prefix
/// rather than `heapless::String` so the project doesn't need to
/// pull in another crate just for this feature; [`Self::ps_str`] /
/// [`Self::rt_str`] return safe `&str` views.
#[derive(Clone, Debug)]
pub struct LogEntry {
  /// Monotonic seconds since boot when the entry was captured.
  ///
  /// We keep boot-relative timestamps rather than wall-clock time so
  /// the log keeps making sense before RDS-CT has synced. The web
  /// layer renders this as `mm:ss ago` relative to the latest entry.
  pub uptime_secs: u32,
  /// FM frequency in 0.1 MHz units (matches the on-wire format used
  /// elsewhere in the firmware).
  pub freq_x10: u16,
  /// RSSI dBμV reading at sampling time (0..=75 per Si4703 spec).
  pub rssi: u8,
  ps_buf: [u8; PS_MAX],
  ps_len: u8,
  rt_buf: [u8; RT_MAX],
  rt_len: u8,
}

impl LogEntry {
  const EMPTY: Self = Self {
    uptime_secs: 0,
    freq_x10: 0,
    rssi: 0,
    ps_buf: [0; PS_MAX],
    ps_len: 0,
    rt_buf: [0; RT_MAX],
    rt_len: 0,
  };

  /// Borrow the PS station-name as `&str`. Always returns valid UTF-8
  /// because [`capture`] only fills [`Self::ps_buf`] up to a
  /// char-boundary truncation point.
  pub fn ps_str(&self) -> &str {
    // `capture` only ever writes UTF-8 byte sequences sliced at a
    // `char_indices` boundary, so this should never fail. We use the
    // safe path to avoid `unsafe` for a non-hot-path (called at most
    // once per 5 s web poll).
    core::str::from_utf8(&self.ps_buf[..self.ps_len as usize]).unwrap_or("")
  }

  /// Borrow the truncated RadioText as `&str`. Same UTF-8 invariant
  /// as [`Self::ps_str`].
  pub fn rt_str(&self) -> &str {
    core::str::from_utf8(&self.rt_buf[..self.rt_len as usize]).unwrap_or("")
  }
}

impl Default for LogEntry {
  fn default() -> Self {
    Self::EMPTY
  }
}
/// Fixed-size FIFO ring buffer of [`LogEntry`].
///
/// Stored inline (no heap) so the global lock can sit in `.bss`. The
/// occupied range is `[head, head + len) mod LOG_CAPACITY`; reads
/// always walk in chronological order via [`Self::iter_chronological`].
pub struct LogBuffer {
  entries: [LogEntry; LOG_CAPACITY],
  /// Index of the *oldest* entry. Advances when the buffer is full
  /// and we overwrite the oldest slot.
  head: usize,
  /// Number of valid entries (0..=`LOG_CAPACITY`).
  len: usize,
}

impl LogBuffer {
  /// Build an empty buffer suitable for `static` initialisation.
  #[allow(
    clippy::large_stack_frames,
    reason = "const-evaluated for the static `LISTENING_LOG`; the \
              compiler emits the resulting value into `.bss` so no \
              real stack frame is materialised at runtime."
  )]
  pub const fn new() -> Self {
    Self {
      entries: [LogEntry::EMPTY; LOG_CAPACITY],
      head: 0,
      len: 0,
    }
  }

  /// Append `entry`, FIFO-evicting the oldest when full.
  pub fn push(&mut self, entry: LogEntry) {
    if self.len < LOG_CAPACITY {
      let idx = (self.head + self.len) % LOG_CAPACITY;
      self.entries[idx] = entry;
      self.len += 1;
    } else {
      // Buffer full: overwrite the oldest slot, then advance head.
      self.entries[self.head] = entry;
      self.head = (self.head + 1) % LOG_CAPACITY;
    }
  }

  /// Return a reference to the most recent entry, if any.
  ///
  /// Currently unused outside the test module — retained as part of
  /// `LogBuffer`'s natural read API so future features (e.g. "jump
  /// to last station") don't have to re-derive it from
  /// [`Self::iter_chronological`].
  #[allow(dead_code)]
  pub fn latest(&self) -> Option<&LogEntry> {
    if self.len == 0 {
      return None;
    }
    let idx = (self.head + self.len - 1) % LOG_CAPACITY;
    Some(&self.entries[idx])
  }

  /// Number of stored entries.
  ///
  /// Currently unused outside the test module; retained for the same
  /// reason as [`Self::latest`].
  #[allow(dead_code)]
  pub fn len(&self) -> usize {
    self.len
  }

  /// Iterate entries in chronological order (oldest first).
  pub fn iter_chronological(&self) -> impl Iterator<Item = &LogEntry> {
    (0..self.len).map(move |i| &self.entries[(self.head + i) % LOG_CAPACITY])
  }
}

impl Default for LogBuffer {
  #[allow(
    clippy::large_stack_frames,
    reason = "identical to `LogBuffer::new`; only used in tests where \
              stack depth doesn't matter."
  )]
  fn default() -> Self {
    Self::new()
  }
}

// ============================================================================
// Globals
// ============================================================================

/// Process-wide listening log, locked by an embassy mutex so
/// [`crate::tasks::logger_task`] (writer) and the web layer (reader)
/// can't race.
pub static LISTENING_LOG: Mutex<CriticalSectionRawMutex, LogBuffer> = Mutex::new(LogBuffer::new());

// ============================================================================
// Capture helper
// ============================================================================

/// Build a [`LogEntry`] from a snapshot of [`RadioState`] taken at
/// `uptime_secs`.
///
/// Truncates the PS / RT strings to the inline-storage caps without
/// allocating; the conversion path is `&str` -> fixed-size byte
/// buffer, which only copies bytes.
pub fn capture(uptime_secs: u32, state: &RadioState) -> LogEntry {
  let mut out = LogEntry::EMPTY;
  out.uptime_secs = uptime_secs;
  out.freq_x10 = state.freq_mhz_x10;
  out.rssi = state.rssi;
  let ps_bytes = clip_to_buf(&state.station_name, PS_MAX);
  out.ps_buf[..ps_bytes.len()].copy_from_slice(ps_bytes);
  out.ps_len = ps_bytes.len() as u8;
  let rt_bytes = clip_to_buf(&state.radio_text, RT_MAX);
  out.rt_buf[..rt_bytes.len()].copy_from_slice(rt_bytes);
  out.rt_len = rt_bytes.len() as u8;
  out
}

/// Return the longest prefix of `src` that fits in `cap` bytes
/// without breaking a UTF-8 char boundary.
fn clip_to_buf(src: &str, cap: usize) -> &[u8] {
  if src.len() <= cap {
    return src.as_bytes();
  }
  // Walk char boundaries to find the last position that still fits.
  let mut end = 0usize;
  for (i, _) in src.char_indices() {
    if i > cap {
      break;
    }
    end = i;
  }
  // `end` is the *start* of the last char that *might* still fit;
  // include that char only if its full byte range is <= cap.
  if let Some(c) = src[end..].chars().next()
    && end + c.len_utf8() <= cap
  {
    end += c.len_utf8();
  }
  &src.as_bytes()[..end]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(uptime: u32, freq: u16) -> LogEntry {
    let mut e = LogEntry::EMPTY;
    e.uptime_secs = uptime;
    e.freq_x10 = freq;
    e.rssi = 30;
    e
  }

  #[test]
  fn push_below_capacity_appends() {
    let mut buf = LogBuffer::new();
    buf.push(entry(1, 1015));
    buf.push(entry(2, 1020));
    assert_eq!(buf.len(), 2);
    assert_eq!(buf.latest().unwrap().freq_x10, 1020);
  }

  #[test]
  fn push_at_capacity_evicts_oldest() {
    let mut buf = LogBuffer::new();
    for i in 0..(LOG_CAPACITY as u32 + 5) {
      buf.push(entry(i, 1000 + i as u16));
    }
    assert_eq!(buf.len(), LOG_CAPACITY);
    let first = buf.iter_chronological().next().unwrap();
    // After overflow the oldest visible entry should be the 6th
    // pushed (we pushed CAP + 5, evicted the first 5).
    assert_eq!(first.uptime_secs, 5);
  }

  #[test]
  fn iter_chronological_after_wrap() {
    let mut buf = LogBuffer::new();
    for i in 0..(LOG_CAPACITY as u32 + 3) {
      buf.push(entry(i, 1000));
    }
    let collected: alloc::vec::Vec<u32> = buf.iter_chronological().map(|e| e.uptime_secs).collect();
    assert_eq!(collected.len(), LOG_CAPACITY);
    assert_eq!(*collected.first().unwrap(), 3);
    assert_eq!(*collected.last().unwrap(), LOG_CAPACITY as u32 + 2);
  }

  #[test]
  fn clip_truncates_at_char_boundary() {
    // Two-byte UTF-8 char "é" + ASCII; with cap=3 we should keep "aé"
    // (3 bytes) but not "aéb".
    let bytes = clip_to_buf("aéb", 3);
    assert_eq!(bytes, "aé".as_bytes());
  }

  #[test]
  fn clip_passthrough_when_short() {
    let bytes = clip_to_buf("BBC", 8);
    assert_eq!(bytes, b"BBC");
  }
}
