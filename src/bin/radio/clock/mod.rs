//! NTP-disciplined wall-clock with safe fallback.
//!
//! ## Why a separate module?
//!
//! The radio binary needs a wall-clock for two reasons:
//!
//! 1. The web console wants to render listening-log entries with a
//!    real timestamp once the device knows the time.
//! 2. Future features (alarm clock, scheduled recordings, RDS-CT
//!    reconciliation) need a stable epoch to be useful.
//!
//! `embassy_time::Instant` is monotonic-since-boot and never drifts,
//! but it does not know what year it is. SNTP fills that gap by
//! anchoring the monotonic clock to UTC once the device reaches the
//! network.
//!
//! ## Design
//!
//! - [`sntp`] holds the **pure-logic** packet encoder / decoder.
//!   It has no I/O and runs unchanged under host `cargo test`.
//! - This module wraps it with a tiny mutable state — the offset
//!   between [`embassy_time::Instant`] and Unix epoch — guarded by a
//!   single [`AtomicU64`] for lock-free reads from any task / context.
//! - [`crate::tasks::ntp_task`] (added in `tasks.rs`) drives the
//!   actual UDP exchange and calls [`record_sync`] when an answer
//!   passes validation.
//!
//! Reads via [`wall_time_unix_secs`] return `None` before the first
//! sync, and a current Unix seconds value afterwards. Time advances
//! using the monotonic clock, so even if NTP later disagrees by a
//! few seconds, the returned timestamp never goes backwards within a
//! single boot — only the next [`record_sync`] adjusts the offset.

pub mod sntp;

use core::sync::atomic::Ordering;

use embassy_time::Instant;
use portable_atomic::AtomicU64;

/// Sentinel meaning "no sync has happened yet". `0` is also a valid
/// Unix epoch (1970-01-01) but the radio will never legitimately
/// boot believing the year is 1970, so re-using zero as the
/// "unknown" marker is safe.
const NOT_SYNCED: u64 = 0;

/// Offset that, when added to `Instant::now().as_secs()`, yields the
/// current Unix epoch seconds. Zero means "not yet synced".
///
/// Stored relaxed because we don't synchronise this with any other
/// shared state — readers see either the old offset or the new one,
/// and either is internally consistent.
static EPOCH_OFFSET_SECS: AtomicU64 = AtomicU64::new(NOT_SYNCED);

/// Record a successful SNTP sync.
///
/// Called by [`crate::tasks::ntp_task`] when a server reply has
/// passed [`sntp::decode_reply`]. The captured [`Instant`] represents
/// the moment the reply arrived — we subtract its uptime from the
/// reported Unix time to get a stable boot epoch, then store the
/// difference as the offset that [`wall_time_unix_secs`] adds back.
pub fn record_sync(unix_secs_at_reply: u64, reply_arrived: Instant) {
  // Defensive: a server that hands us 0 would re-trigger the
  // "unsynced" sentinel and silently disable the clock. Skip it.
  if unix_secs_at_reply == NOT_SYNCED {
    return;
  }
  let uptime_secs = reply_arrived.as_secs();
  let offset = unix_secs_at_reply.saturating_sub(uptime_secs);
  EPOCH_OFFSET_SECS.store(offset, Ordering::Relaxed);
}

/// Current wall-clock time as Unix epoch seconds, or `None` if SNTP
/// has not synced since boot.
#[must_use]
pub fn wall_time_unix_secs() -> Option<u64> {
  let offset = EPOCH_OFFSET_SECS.load(Ordering::Relaxed);
  if offset == NOT_SYNCED {
    return None;
  }
  Some(offset.saturating_add(Instant::now().as_secs()))
}

/// Cheap "have we ever synced?" probe for status surfaces.
///
/// Currently only used in the future health-endpoint draft; kept on
/// the public API so adding the `synced` boolean to `/api/health`
/// later doesn't require revisiting the clock module.
#[allow(dead_code)]
#[must_use]
pub fn is_synced() -> bool {
  EPOCH_OFFSET_SECS.load(Ordering::Relaxed) != NOT_SYNCED
}
