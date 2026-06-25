//! Concurrent tasks for the radio binary.
//!
//! Two embassy tasks run alongside the UI loop:
//!
//! - [`input_task`]: reads the rotary encoder and emits `RadioCommand`s.
//! - [`radio_control_task`]: applies commands to the Si4703 chip and
//!   refreshes RSSI/RDS into the shared [`crate::state::RADIO_STATE`].

use defmt::info;
use embassy_futures::select::{Either, select};
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::i2c::master::I2c;

use alloc::string::String;

use radio::rotary_encoder::RotaryEncoder;
use radio::si4703::{RdsClockTime, RdsDecoder, SeekDirection, Si4703};

use crate::state::{
  DEFAULT_FREQ_X10, INPUT_CMDS, LONG_PRESS_MS, RADIO_STATE, RadioCommand, STATION_NAME_PLACEHOLDER,
  TUNE_STEP_X10, clamp_freq, publish_clock, publish_freq, publish_radio_text, publish_station_name,
};

/// Tracks wall-clock time derived from RDS Group 4A (Clock-Time).
///
/// CT is broadcast roughly once per minute on the 0-second boundary. We
/// anchor the most recent CT against [`Instant::now`] and synthesise the
/// minute hand by adding elapsed time, so the displayed clock advances
/// even between CT bursts (and survives a station that drops out of CT
/// for a few minutes).
#[derive(Clone, Copy)]
struct WallClock {
  /// UTC time at the anchor instant, in minutes since midnight `[0, 1440)`.
  anchor_utc_minutes: u32,
  /// Local-time offset in half-hours `[-24, 24]` (as transmitted by RDS).
  local_offset_half_hours: i8,
  /// Monotonic snapshot taken when the CT frame above was decoded.
  anchor_instant: Instant,
}

impl WallClock {
  fn from_ct(ct: RdsClockTime, now: Instant) -> Self {
    Self {
      anchor_utc_minutes: u32::from(ct.utc_hour) * 60 + u32::from(ct.utc_minute),
      local_offset_half_hours: ct.local_offset_half_hours,
      anchor_instant: now,
    }
  }

  /// Compute the current local `(hour, minute)`, wrapping across midnight.
  ///
  /// Uses i32 arithmetic so a negative offset (e.g. UTC-5) cannot
  /// underflow even when the anchor is just past midnight UTC.
  fn local_hh_mm(&self, now: Instant) -> (u8, u8) {
    let elapsed_min = now
      .checked_duration_since(self.anchor_instant)
      .map(|d| d.as_secs() / 60)
      .unwrap_or(0);
    let total_local = i32::from(self.local_offset_half_hours) * 30
      + (self.anchor_utc_minutes as i32)
      + elapsed_min as i32;
    let wrapped = total_local.rem_euclid(24 * 60);
    ((wrapped / 60) as u8, (wrapped % 60) as u8)
  }
}

// ============================================================================
// Tasks
// ============================================================================

/// Reads the rotary encoder and translates physical actions into commands.
///
/// The encoder fires 4 quadrature counts per detent; we accumulate raw
/// counts into a residual and emit one `TuneRelative` step per detent.
/// Carrying the residual across iterations preserves slow rotations
/// (3+3+3 raw counts -> 2 detents) instead of integer-dividing them away.
///
/// The push button distinguishes a short press (`SeekUp`) from a long
/// press (`ToggleMute`).
#[embassy_executor::task]
pub async fn input_task(mut encoder: RotaryEncoder<'static, 0>) -> ! {
  /// Number of raw PCNT counts per encoder detent (KY-040 typically emits 4).
  const COUNTS_PER_DETENT: i32 = 4;

  let mut residual: i32 = 0;
  let mut press_start: Option<Instant> = None;
  let mut long_press_fired = false;

  loop {
    // --- Rotation handling: accumulate raw counts, emit per-detent steps ---
    let raw = encoder.delta();
    if raw != 0 {
      let total = residual + raw;
      let steps = total.div_euclid(COUNTS_PER_DETENT);
      residual = total.rem_euclid(COUNTS_PER_DETENT);
      // Clamp to i16 to protect the typed command payload.
      let steps_i16 = steps.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
      if steps_i16 != 0 {
        let payload = steps_i16.saturating_mul(TUNE_STEP_X10);
        // try_send: if the queue is full (radio task busy on a long I2C op),
        // we drop this delta rather than block the input loop. The encoder
        // will still produce the next event.
        let _ = INPUT_CMDS.try_send(RadioCommand::TuneRelative(payload));
      }
    }

    // --- Button handling: short = seek, long = mute toggle ---
    let pressed = encoder.is_button_pressed();
    match (press_start, pressed) {
      (None, true) => {
        press_start = Some(Instant::now());
        long_press_fired = false;
      }
      (Some(start), true) => {
        if !long_press_fired && start.elapsed() >= Duration::from_millis(LONG_PRESS_MS) {
          let _ = INPUT_CMDS.try_send(RadioCommand::ToggleMute);
          long_press_fired = true;
        }
      }
      (Some(_), false) => {
        if !long_press_fired {
          let _ = INPUT_CMDS.try_send(RadioCommand::SeekUp);
        }
        press_start = None;
        long_press_fired = false;
      }
      (None, false) => {}
    }

    Timer::after(Duration::from_millis(20)).await;
  }
}

/// Owns the Si4703 + I2C bus and processes input commands and periodic status.
///
/// Architecture:
/// - On each loop iteration we `select` between an input command and a
///   200 ms tick. Status refresh (RSSI + RDS) only happens on the tick
///   branch, so command bursts don't trigger redundant I2C reads.
/// - We `yield_now()` between the two synchronous I2C reads to give the
///   UI render task and the input task a chance to run.
#[embassy_executor::task]
pub async fn radio_control_task(
  mut radio_chip: Si4703,
  mut i2c: I2c<'static, esp_hal::Blocking>,
) -> ! {
  let mut rds = RdsDecoder::new();
  let mut last_rds_name = String::from(STATION_NAME_PLACEHOLDER);
  let mut last_rds_text = String::new();
  let mut i2c_error_count: u32 = 0;
  // Wall clock derived from RDS-CT; `None` until first 4A group seen.
  let mut wall_clock: Option<WallClock> = None;

  loop {
    match select(
      INPUT_CMDS.receive(),
      Timer::after(Duration::from_millis(200)),
    )
    .await
    {
      Either::First(command) => {
        handle_command(
          &mut radio_chip,
          &mut i2c,
          command,
          &mut rds,
          &mut wall_clock,
        )
        .await;
      }
      Either::Second(_) => {
        refresh_status(
          &mut radio_chip,
          &mut i2c,
          &mut rds,
          &mut last_rds_name,
          &mut last_rds_text,
          &mut i2c_error_count,
          &mut wall_clock,
        )
        .await;
      }
    }
  }
}

// ============================================================================
// Helpers
// ============================================================================

/// Apply a single `RadioCommand` to the chip and update shared state.
async fn handle_command(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  command: RadioCommand,
  rds: &mut RdsDecoder,
  wall_clock: &mut Option<WallClock>,
) {
  match command {
    RadioCommand::TuneRelative(steps_x10) => {
      let current = radio_chip
        .current_frequency(i2c)
        .unwrap_or(DEFAULT_FREQ_X10);
      let next = clamp_freq(i32::from(current) + i32::from(steps_x10));
      info!("Tune: {} -> {}", current, next);
      if radio_chip.tune(i2c, next).await.is_ok() {
        rds.reset();
        *wall_clock = None;
        publish_freq(next).await;
        publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
        publish_radio_text(String::new()).await;
        publish_clock(None).await;
      }
    }
    RadioCommand::SeekUp => seek(radio_chip, i2c, rds, wall_clock, SeekDirection::Up).await,
    RadioCommand::ToggleMute => {
      let new_muted = {
        let state = RADIO_STATE.lock().await;
        !state.muted
      };
      if radio_chip.set_mute(i2c, new_muted).is_ok() {
        let mut s = RADIO_STATE.lock().await;
        s.muted = new_muted;
        s.dirty = true;
        info!("Mute: {}", new_muted);
      }
    }
  }
}

/// Run a seek in the given direction and publish the result on success.
async fn seek(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  rds: &mut RdsDecoder,
  wall_clock: &mut Option<WallClock>,
  direction: SeekDirection,
) {
  match radio_chip.seek(i2c, direction).await {
    Ok(Some(freq)) => {
      info!("Seek -> {}", freq);
      rds.reset();
      *wall_clock = None;
      publish_freq(freq).await;
      publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
      publish_radio_text(String::new()).await;
      publish_clock(None).await;
    }
    Ok(None) => info!("Seek: end of band"),
    Err(_) => info!("Seek: I2C error"),
  }
}

/// Read RSSI + RDS and copy them into shared state.
///
/// Yields cooperatively between the two I2C transactions so other tasks
/// (UI render, input poll) can run on the executor.
async fn refresh_status(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  rds: &mut RdsDecoder,
  last_rds_name: &mut String,
  last_rds_text: &mut String,
  i2c_error_count: &mut u32,
  wall_clock: &mut Option<WallClock>,
) {
  let rssi = match radio_chip.rssi(i2c) {
    Ok(v) => {
      *i2c_error_count = 0;
      v
    }
    Err(_) => {
      *i2c_error_count = i2c_error_count.saturating_add(1);
      if *i2c_error_count >= 10 {
        info!("I2C: {} consecutive read failures", *i2c_error_count);
        let mut s = RADIO_STATE.lock().await;
        s.station_name.clear();
        s.station_name.push_str("I2C ERR!");
        s.dirty = true;
        drop(s);
      }
      0
    }
  };

  // Yield between I2C transactions so we don't monopolize the executor.
  yield_now().await;

  if let Ok(Some((a, b, c, d))) = radio_chip.read_rds(i2c) {
    rds.process(a, b, c, d);
    // Always re-decode — the underlying buffer may have changed even when
    // PS isn't yet "complete". Cheap (≤8 chars / ≤64 chars).
    let new_name = rds.station_name_string();
    if !new_name.is_empty() && new_name != *last_rds_name {
      *last_rds_name = new_name;
    }
    let new_text = rds.radio_text_string();
    if new_text != *last_rds_text {
      *last_rds_text = new_text;
    }
    // Re-anchor the wall clock whenever a fresh CT frame arrives.
    if let Some(ct) = rds.take_clock_time() {
      *wall_clock = Some(WallClock::from_ct(ct, Instant::now()));
      info!(
        "RDS-CT: UTC {}:{:02} offset={} half-hours",
        ct.utc_hour, ct.utc_minute, ct.local_offset_half_hours
      );
    }
  }

  // Compute the latest local clock snapshot (if we have one) so the UI
  // sees the minute hand advance even between CT bursts.
  let clock_snapshot = wall_clock.as_ref().map(|wc| wc.local_hh_mm(Instant::now()));

  let mut state = RADIO_STATE.lock().await;
  state.rssi = rssi;
  if state.station_name != *last_rds_name && !last_rds_name.is_empty() {
    state.station_name.clear();
    state.station_name.push_str(last_rds_name);
  } else if state.station_name.is_empty() {
    state.station_name.push_str(STATION_NAME_PLACEHOLDER);
  }
  if state.radio_text != *last_rds_text {
    state.radio_text.clear();
    state.radio_text.push_str(last_rds_text);
  }
  if state.clock_hh_mm != clock_snapshot {
    state.clock_hh_mm = clock_snapshot;
  }
  state.volume = radio_chip.volume();
  state.dirty = true;
}
