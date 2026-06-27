//! Concurrent tasks for the radio binary.
//!
//! Two embassy tasks run alongside the UI loop:
//!
//! - [`input_task`]: reads the rotary encoder and emits `RadioCommand`s.
//! - [`radio_control_task`]: applies commands to the Si4703 chip and
//!   refreshes RSSI/RDS into the shared [`crate::state::RADIO_STATE`].

use defmt::info;
use embassy_futures::select::{Either3, select3};
use embassy_futures::yield_now;
use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::i2c::master::I2c;

use alloc::string::String;

use radio::rotary_encoder::RotaryEncoder;
use radio::si4703::{RdsClockTime, RdsDecoder, SeekDirection, Si4703};

use crate::ota;
use crate::presets::PresetStore;
use crate::state::{
  DEFAULT_FREQ_X10, INPUT_CMDS, LAST_TUNED_DEBOUNCE_MS, LONG_PRESS_MS, OTA_CMDS, OtaCommand,
  RADIO_STATE, RadioCommand, STATION_NAME_PLACEHOLDER, TUNE_STEP_X10, ULTRA_LONG_PRESS_MS,
  clamp_freq, publish_af_status, publish_clock, publish_freq, publish_presets, publish_pty,
  publish_radio_text, publish_rt_plus, publish_station_name,
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

/// Three-tier state machine for the encoder push button.
///
/// The transitions live entirely in [`input_task`] — this enum is a
/// type-safe alternative to a `bool` pair that would otherwise need
/// to encode the same three states.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PressStage {
  /// Button is not currently pressed.
  Idle,
  /// Pressed, but the long-press threshold has not yet been crossed.
  Holding,
  /// Long-press fired (`SavePreset`); waiting for either release or
  /// the ultra-long threshold.
  SaveFired,
  /// Ultra-long-press fired (`ToggleMute`); waiting for release.
  MuteFired,
}

/// Hysteresis controller that automatically forces mono mode on the
/// Si4703 when the signal becomes too weak to render clean stereo.
///
/// Stereo separation amplifies noise on weak FM carriers, so commercial
/// receivers all do this kind of "blend to mono" trick. We use a simple
/// dwell-time based hysteresis instead of an analog blend because the
/// Si4703 only exposes a hard boolean MONO flag:
///
/// - RSSI ≤ [`RSSI_LOW`] sustained for [`DWELL_TICKS`] consecutive ticks
///   → engage mono.
/// - RSSI ≥ [`RSSI_HIGH`] sustained for [`DWELL_TICKS`] ticks → release.
///
/// The deadband (`RSSI_LOW < x < RSSI_HIGH`) prevents thrashing when the
/// signal hovers near the threshold. The dwell window absorbs single-tick
/// RSSI dips (cars passing under bridges, hand-on-antenna, etc.).
struct MonoController {
  /// Whether we have currently *forced* mono. `false` means the chip is
  /// allowed to operate in stereo (its default after `init`).
  engaged: bool,
  /// Counter of consecutive ticks meeting the *opposite* condition.
  /// Reset to zero whenever the predicate fails.
  dwell: u8,
}

impl MonoController {
  /// Engage threshold (RSSI ≤ this → start counting toward mono).
  /// Si4703 RSSI tops out at 75; values below ~25 are typically noisy.
  const RSSI_LOW: u8 = 25;
  /// Release threshold (RSSI ≥ this → start counting toward stereo).
  const RSSI_HIGH: u8 = 35;
  /// Number of consecutive 200 ms ticks the predicate must hold to act
  /// — at 5 Hz, 10 ticks ≈ 2 s of stable signal.
  const DWELL_TICKS: u8 = 10;

  fn new() -> Self {
    Self {
      engaged: false,
      dwell: 0,
    }
  }

  /// Returns `Some(target)` when the chip's mono flag should be flipped,
  /// otherwise `None`. Caller is responsible for issuing the I2C write.
  fn observe(&mut self, rssi: u8) -> Option<bool> {
    let want_engage = rssi <= Self::RSSI_LOW;
    let want_release = rssi >= Self::RSSI_HIGH;

    match (self.engaged, want_engage, want_release) {
      (false, true, _) => {
        self.dwell = self.dwell.saturating_add(1);
        if self.dwell >= Self::DWELL_TICKS {
          self.engaged = true;
          self.dwell = 0;
          return Some(true);
        }
      }
      (true, _, true) => {
        self.dwell = self.dwell.saturating_add(1);
        if self.dwell >= Self::DWELL_TICKS {
          self.engaged = false;
          self.dwell = 0;
          return Some(false);
        }
      }
      // In the deadband or already in the steady state — reset dwell.
      _ => self.dwell = 0,
    }
    None
  }

  fn engaged(&self) -> bool {
    self.engaged
  }
}

/// Decision-only controller for RDS-AF (Alternative Frequency) follow.
///
/// The actual probe (tune → measure RSSI → verify PI → commit / rollback)
/// is implemented in [`run_af_probe`] because it needs the chip + I2C
/// bus. This struct only decides *when* a probe is allowed:
///
/// 1. **Sustained weak signal** — RSSI must stay `≤ RSSI_WEAK` for at
///    least [`Self::WEAK_DWELL_TICKS`] consecutive 200 ms ticks. The
///    threshold is intentionally lower than [`MonoController::RSSI_LOW`]
///    so the auto-mono blend gets a chance to recover the listen first;
///    we only escalate to AF when even mono sounds bad.
/// 2. **Cooldown** — after every probe attempt (success or failure),
///    we refuse to probe again for [`Self::COOLDOWN_TICKS`] ticks. This
///    keeps a flapping signal from causing rapid back-to-back hiccups.
///
/// All thresholds are tick-counted so we don't need wall-clock
/// arithmetic; the radio control task ticks at a steady 5 Hz.
struct AfFollower {
  /// Consecutive ticks observed below [`Self::RSSI_WEAK`].
  weak_dwell: u8,
  /// Remaining cooldown ticks after the last probe attempt.
  cooldown: u16,
}

impl AfFollower {
  /// RSSI ≤ this is considered "weak enough to try AF".
  ///
  /// Below the auto-mono engage threshold (25): we only escalate to a
  /// disruptive frequency probe when the cheaper mono blend has already
  /// failed to make the audio comfortable.
  const RSSI_WEAK: u8 = 18;
  /// 5 s of sustained weak signal before probing (5 Hz × 25 = 5 s).
  const WEAK_DWELL_TICKS: u8 = 25;
  /// 30 s cool-down after each probe attempt (5 Hz × 150 = 30 s).
  const COOLDOWN_TICKS: u16 = 150;
  /// Probe candidate is only adopted if its RSSI exceeds the current
  /// frequency's RSSI by at least this much (anti-flap hysteresis).
  const RSSI_IMPROVE_MARGIN: u8 = 6;

  const fn new() -> Self {
    Self {
      weak_dwell: 0,
      cooldown: 0,
    }
  }

  /// Tick the controller with the current RSSI.
  ///
  /// Returns `true` when the caller should run a probe. The follower
  /// internally arms its cooldown when it returns `true`, so the caller
  /// can rely on "only one probe per signal-loss event".
  fn observe(&mut self, rssi: u8, af_list_len: usize) -> bool {
    if self.cooldown > 0 {
      self.cooldown -= 1;
      // Don't accumulate dwell during cooldown either, so the next
      // probe waits for a fresh weak streak rather than firing the
      // moment the cooldown expires.
      self.weak_dwell = 0;
      return false;
    }

    if rssi > Self::RSSI_WEAK {
      self.weak_dwell = 0;
      return false;
    }

    // RSSI is weak. Count up; gate on having a usable AF list (no point
    // probing if no candidates are known).
    self.weak_dwell = self.weak_dwell.saturating_add(1);
    if self.weak_dwell < Self::WEAK_DWELL_TICKS || af_list_len == 0 {
      return false;
    }

    // Trigger! Arm the cooldown and reset dwell so we don't fire again
    // immediately even if the probe is a no-op.
    self.cooldown = Self::COOLDOWN_TICKS;
    self.weak_dwell = 0;
    true
  }
}

/// Mutable per-tick state shared between [`refresh_status`] invocations.
///
/// Bundled into one struct so the function signature stays under clippy's
/// `too_many_arguments` threshold and so the call site reads naturally:
/// `refresh_status(&mut chip, &mut i2c, &mut ctx).await`.
struct RefreshContext<'a> {
  rds: &'a mut RdsDecoder,
  last_rds_name: &'a mut String,
  last_rds_text: &'a mut String,
  i2c_error_count: &'a mut u32,
  wall_clock: &'a mut Option<WallClock>,
  mono_ctl: &'a mut MonoController,
  af_ctl: &'a mut AfFollower,
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
/// ## Tune acceleration
///
/// Detent-to-detent timing is mapped onto a step multiplier so that
/// fast rotations cover the FM band quickly while slow rotations keep
/// the precise 0.1 MHz granularity:
///
/// | ms / detent  | multiplier | feel        |
/// |--------------|------------|-------------|
/// | `..=40`      | ×5         | flick / sweep |
/// | `41..=100`   | ×3         | fast scrub  |
/// | `101..=250`  | ×2         | quick scan  |
/// | `>250`       | ×1         | fine tune   |
///
/// Multipliers are deliberately moderate: a 5-detent flick at the
/// fastest tier moves 2.5 MHz, which feels snappy without overshooting
/// across most of the 20.5 MHz FM band.
///
/// The multiplier is **reset to ×1** when the user reverses direction
/// or pauses for longer than [`ACCEL_IDLE_RESET_MS`] ms — this stops the
/// frequency from "running away" at the end of a fast spin and makes
/// fine adjustments deterministic after any pause.
///
/// The push button drives a three-tier gesture state machine:
///
/// | hold time         | command       | rationale                                   |
/// |-------------------|---------------|---------------------------------------------|
/// | `< 800 ms`        | `CyclePreset` | most frequent action; falls back to `SeekUp` inside the radio task when no preset is saved |
/// | `800..=2500 ms`   | `SavePreset`  | saving a station should be quick & one-handed, so it sits at the medium tier |
/// | `> 2500 ms`       | `ToggleMute`  | rarer, intentionally awkward to avoid accidental mutes |
///
/// Each tier fires *exactly once* per press: while the user is still
/// holding past `LONG_PRESS_MS` we send the save command immediately so
/// they get tactile feedback (UI badge), and only escalate to mute if
/// they keep holding past `ULTRA_LONG_PRESS_MS`. Releasing without
/// reaching the long-press threshold replays as a short-press.
#[embassy_executor::task]
pub async fn input_task(mut encoder: RotaryEncoder<'static, 0>) -> ! {
  /// Number of raw PCNT counts per encoder detent (KY-040 typically emits 4).
  const COUNTS_PER_DETENT: i32 = 4;
  /// Pause length (ms) that resets the acceleration multiplier back to ×1.
  ///
  /// Picked empirically: a deliberate "stop and adjust" feels like at
  /// least half a second of stillness, and the 20 ms polling loop gives
  /// us 25 samples of headroom inside that window.
  const ACCEL_IDLE_RESET_MS: u64 = 500;

  let mut residual: i32 = 0;
  // Tracks how far through the gesture timeline the current press has
  // already escalated. `None` means "not pressed".
  let mut press_start: Option<Instant> = None;
  let mut press_stage: PressStage = PressStage::Idle;
  // Acceleration state: the timestamp of the last wake that produced
  // motion and the sign of that motion. `None` means "no motion yet
  // this session", which is treated identically to "idle for > reset".
  let mut last_motion_at: Option<Instant> = None;
  let mut last_motion_sign: i8 = 0;

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
        let now = Instant::now();
        let sign: i8 = if steps_i16 > 0 { 1 } else { -1 };

        // Time since the last detent burst, in ms. Saturating-on-None
        // gives ∞, which trips the idle reset and forces ×1.
        let elapsed_ms = last_motion_at.map_or(u64::MAX, |t| (now - t).as_millis());

        // Direction reversal or long pause => fall back to fine tune.
        let multiplier: i16 = if sign != last_motion_sign || elapsed_ms > ACCEL_IDLE_RESET_MS {
          1
        } else {
          // Average ms between detents in this burst. `unsigned_abs`
          // is safe (steps_i16 != 0 here) and divisor cannot be zero.
          let detents = u64::from(steps_i16.unsigned_abs());
          let ms_per_det = (elapsed_ms / detents).max(1);
          match ms_per_det {
            0..=40 => 5,
            41..=100 => 3,
            101..=250 => 2,
            _ => 1,
          }
        };

        last_motion_at = Some(now);
        last_motion_sign = sign;

        // Compose final payload in 0.1-MHz units. Two saturating muls
        // keep the value bounded inside i16; the radio task additionally
        // clamps to band limits, so an over-large delta is harmless.
        let scaled = steps_i16.saturating_mul(multiplier);
        let payload = scaled.saturating_mul(TUNE_STEP_X10);
        // try_send: if the queue is full (radio task busy on a long I2C op),
        // we drop this delta rather than block the input loop. The encoder
        // will still produce the next event.
        let _ = INPUT_CMDS.try_send(RadioCommand::TuneRelative(payload));
      }
    }

    // --- Button handling: short = cycle/seek, long = save, ultra = mute ---
    let pressed = encoder.is_button_pressed();
    match (press_start, pressed) {
      (None, true) => {
        press_start = Some(Instant::now());
        press_stage = PressStage::Holding;
      }
      (Some(start), true) => {
        let held = start.elapsed();
        // Escalate stages at each threshold crossing; `match` ensures we
        // emit exactly one command per stage transition.
        match press_stage {
          PressStage::Holding if held >= Duration::from_millis(LONG_PRESS_MS) => {
            let _ = INPUT_CMDS.try_send(RadioCommand::SavePreset);
            press_stage = PressStage::SaveFired;
          }
          PressStage::SaveFired if held >= Duration::from_millis(ULTRA_LONG_PRESS_MS) => {
            let _ = INPUT_CMDS.try_send(RadioCommand::ToggleMute);
            press_stage = PressStage::MuteFired;
          }
          _ => {}
        }
      }
      (Some(_), false) => {
        if matches!(press_stage, PressStage::Holding) {
          // Released before any long-press fired — it's a short press.
          let _ = INPUT_CMDS.try_send(RadioCommand::CyclePreset);
        }
        press_start = None;
        press_stage = PressStage::Idle;
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
/// - The task also owns the [`PresetStore`]: every preset save writes
///   immediately, while `last_tuned` is debounced for
///   [`LAST_TUNED_DEBOUNCE_MS`] to keep flash erase counts low.
#[embassy_executor::task]
#[allow(
  clippy::large_stack_frames,
  reason = "the task aggregates RDS decoder, RDS string buffers, MonoController, AfFollower, and \
            the optional preset store + WiFi stack handle. ~1.1 KiB stays well under the 16 KiB \
            Embassy task stack on ESP32-C6."
)]
pub async fn radio_control_task(
  mut radio_chip: Si4703,
  mut i2c: I2c<'static, esp_hal::Blocking>,
  preset_store: PresetStore<'static>,
  stack: Option<Stack<'static>>,
) -> ! {
  let mut rds = RdsDecoder::new();
  let mut last_rds_name = String::from(STATION_NAME_PLACEHOLDER);
  let mut last_rds_text = String::new();
  let mut i2c_error_count: u32 = 0;
  // Wall clock derived from RDS-CT; `None` until first 4A group seen.
  let mut wall_clock: Option<WallClock> = None;
  // Auto-mono hysteresis controller. See [`MonoController`] for the
  // exact thresholds; lifted out so its state survives across ticks.
  let mut mono_ctl = MonoController::new();
  // RDS-AF (Alternative Frequency) follower. Decision-only; the actual
  // probe runs in [`run_af_probe`] when this controller arms.
  let mut af_ctl = AfFollower::new();
  // Tracks the most recent tune that hasn't yet been persisted.
  // `Some(instant)` means "we owe flash a `last_tuned` write"; when the
  // instant is older than `LAST_TUNED_DEBOUNCE_MS` we flush it.
  let mut last_tuned_pending: Option<(u16, Instant)> = None;

  // The preset store owns the singleton flash handle. We wrap it in an
  // `Option` so the OTA path can `take()` the store, surrender the
  // flash to [`crate::ota::run_job`] for the duration of the download,
  // and put a fresh store back together on completion. `None` is only
  // ever observed transiently while the OTA job runs; every other code
  // path is allowed to `expect()` the inner value.
  let mut preset_store: Option<PresetStore<'static>> = Some(preset_store);

  loop {
    match select3(
      INPUT_CMDS.receive(),
      Timer::after(Duration::from_millis(200)),
      OTA_CMDS.wait(),
    )
    .await
    {
      Either3::First(command) => {
        crate::diagnostics::watchdog_feed();
        let store = preset_store
          .as_mut()
          .expect("preset store available outside OTA");
        handle_command(
          &mut radio_chip,
          &mut i2c,
          command,
          &mut rds,
          &mut wall_clock,
          store,
          &mut last_tuned_pending,
        )
        .await;
      }
      Either3::Second(_) => {
        crate::diagnostics::watchdog_feed();
        let store = preset_store
          .as_mut()
          .expect("preset store available outside OTA");
        let probe_armed = {
          let mut ctx = RefreshContext {
            rds: &mut rds,
            last_rds_name: &mut last_rds_name,
            last_rds_text: &mut last_rds_text,
            i2c_error_count: &mut i2c_error_count,
            wall_clock: &mut wall_clock,
            mono_ctl: &mut mono_ctl,
            af_ctl: &mut af_ctl,
          };
          refresh_status(&mut radio_chip, &mut i2c, &mut ctx).await
        };
        // AF probe is intentionally outside the refresh borrow scope:
        // it issues its own I2C tunes, so the per-tick `RefreshContext`
        // borrow has to be released first.
        if probe_armed {
          run_af_probe(
            &mut radio_chip,
            &mut i2c,
            &mut rds,
            &mut wall_clock,
            store,
            &mut last_tuned_pending,
          )
          .await;
        }
        // Opportunistic flash flush: piggy-back on the 200 ms tick
        // instead of a third `select` arm. Worst-case latency is one
        // tick beyond the debounce window, which is fine.
        flush_last_tuned_if_due(store, &mut last_tuned_pending);
      }
      Either3::Third(cmd) => {
        crate::diagnostics::watchdog_feed();
        handle_ota_command(&mut preset_store, &mut last_tuned_pending, stack, cmd).await;
      }
    }
  }
}

/// Hand the flash handle to the OTA pipeline and re-establish the
/// preset store on completion.
///
/// Steps:
///
/// 1. Drop any pending `last_tuned` write — we don't want a flash
///    op in flight when we surrender the handle. The frequency is
///    re-armed for persistence by the next tune.
/// 2. `pause()` the store to extract the [`FlashStorage`].
/// 3. Run the download via [`crate::ota::run_job`].
/// 4. `resume()` the paused token with the returned flash handle.
///
/// If the WiFi stack is unavailable (offline boot), publish a
/// `Failed("offline")` and return without touching flash.
async fn handle_ota_command(
  store_slot: &mut Option<PresetStore<'static>>,
  last_tuned_pending: &mut Option<(u16, Instant)>,
  stack: Option<Stack<'static>>,
  cmd: OtaCommand,
) {
  let OtaCommand::Start(url) = cmd;

  let Some(stack) = stack else {
    defmt::warn!("OTA requested but WiFi stack is offline");
    crate::state::publish_ota_progress(crate::state::OtaProgress::Failed("offline")).await;
    return;
  };

  // Drop the debounce so we don't try to flash the storage partition
  // moments before yielding the handle. The next tune will re-arm it.
  *last_tuned_pending = None;

  let Some(store) = store_slot.take() else {
    defmt::warn!("OTA already in progress — ignoring duplicate request");
    return;
  };

  let (flash, paused) = store.pause();
  let flash = ota::run_job(stack, flash, url).await;
  *store_slot = Some(paused.resume(flash));
}

/// Persist `last_tuned` once the debounce window has elapsed.
///
/// **Blocking note**: the underlying `save_set` call erases one 4 KB
/// NOR flash sector (~20–40 ms on ESP32-C6) synchronously. This blocks
/// the executor for that duration, but since the debounce window is 30 s
/// the actual trigger rate is at most once per tune session — negligible
/// impact on the 200 ms tick cadence and input responsiveness.
///
/// **OTA interlock**: skips entirely while `RADIO_STATE.ota_in_progress`
/// is true. During an OTA the flash handle has been loaned out via
/// [`PresetStore::pause`], so issuing a write here would either panic
/// (if the loan was already taken) or — worse — race the OTA writer.
/// The debounce timer keeps ticking; we'll flush on the next idle tick
/// after OTA completes.
///
/// Failures are non-fatal — we log and clear the pending mark so we
/// don't busy-loop retrying on a dead flash.
fn flush_last_tuned_if_due(store: &mut PresetStore<'static>, pending: &mut Option<(u16, Instant)>) {
  let Some((freq, since)) = *pending else {
    return;
  };
  if since.elapsed() < Duration::from_millis(LAST_TUNED_DEBOUNCE_MS) {
    return;
  }
  // OTA cooperative interlock: avoid touching flash while the handle
  // is loaned out. `try_lock` keeps this synchronous (the surrounding
  // function is called from a non-async tick) and tolerates the rare
  // case where the UI task happens to hold the lock — we'll just try
  // again on the next 200 ms tick.
  if let Ok(state) = RADIO_STATE.try_lock()
    && state.ota_in_progress
  {
    return;
  }
  match store.record_last_tuned(freq) {
    Ok(()) => info!("Flash: last_tuned <- {}", freq),
    Err(e) => info!("Flash: last_tuned save failed: {}", e),
  }
  *pending = None;
}

/// Common tail of a successful chip-side tune.
///
/// Resets RDS state for the new station, publishes the fresh frequency
/// and cleared placeholders to [`RADIO_STATE`], updates the preset
/// indicator only when the active slot actually changes, and arms the
/// flash debounce. Shared by every code path that issues a successful
/// `Si4703::tune` (manual rotary, web console, preset cycle, AF probe).
async fn apply_tuned(
  rds: &mut RdsDecoder,
  wall_clock: &mut Option<WallClock>,
  preset_store: &PresetStore<'static>,
  last_tuned_pending: &mut Option<(u16, Instant)>,
  freq_x10: u16,
) {
  rds.reset();
  *wall_clock = None;
  publish_freq(freq_x10).await;
  publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
  publish_radio_text(String::new()).await;
  publish_rt_plus(None, None).await;
  publish_clock(None).await;
  publish_pty(None).await;
  // Only update preset indicator when the active slot actually changes
  // (e.g. tuning away from a saved frequency). This avoids acquiring
  // the state lock on every rotary tick during fast tuning.
  let new_idx = preset_store.snapshot().position(freq_x10).map(|i| i as u8);
  let old_idx = RADIO_STATE.lock().await.preset_idx;
  if new_idx != old_idx {
    publish_presets(preset_store.snapshot(), freq_x10).await;
  }
  *last_tuned_pending = Some((freq_x10, Instant::now()));
}

// ============================================================================
// Helpers
// ============================================================================

/// Apply a single `RadioCommand` to the chip and update shared state.
#[allow(
  clippy::large_stack_frames,
  reason = "async state machine of handle_command holds the union of all RadioCommand \
            arms' frames, including transient String allocations (RDS placeholder, \
            station name, radio text) and a PresetSet by-value copy; ~1.7 KiB total \
            is well under the 16 KiB Embassy task stack on ESP32-C6."
)]
async fn handle_command(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  command: RadioCommand,
  rds: &mut RdsDecoder,
  wall_clock: &mut Option<WallClock>,
  preset_store: &mut PresetStore<'static>,
  last_tuned_pending: &mut Option<(u16, Instant)>,
) {
  match command {
    RadioCommand::TuneRelative(steps_x10) => {
      let current = radio_chip
        .current_frequency(i2c)
        .unwrap_or(DEFAULT_FREQ_X10);
      let next = clamp_freq(i32::from(current) + i32::from(steps_x10));
      info!("Tune: {} -> {}", current, next);
      if radio_chip.tune(i2c, next).await.is_ok() {
        apply_tuned(rds, wall_clock, preset_store, last_tuned_pending, next).await;
      }
    }
    RadioCommand::TuneAbsolute(freq_x10) => {
      let target = clamp_freq(i32::from(freq_x10));
      info!("Tune (abs): -> {}", target);
      if radio_chip.tune(i2c, target).await.is_ok() {
        apply_tuned(rds, wall_clock, preset_store, last_tuned_pending, target).await;
      }
    }
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
    RadioCommand::SavePreset => {
      let current = radio_chip
        .current_frequency(i2c)
        .unwrap_or(DEFAULT_FREQ_X10);
      match preset_store.save_freq(current) {
        Ok(idx) => {
          info!("Preset saved: freq={} slot={}", current, idx);
          publish_presets(preset_store.snapshot(), current).await;
        }
        Err(e) => info!("Preset save failed: {}", e),
      }
    }
    RadioCommand::CyclePreset => {
      let current = radio_chip
        .current_frequency(i2c)
        .unwrap_or(DEFAULT_FREQ_X10);
      match preset_store.snapshot().next_after(current) {
        Some(target) if target != current => {
          info!("Preset cycle: {} -> {}", current, target);
          if radio_chip.tune(i2c, target).await.is_ok() {
            rds.reset();
            *wall_clock = None;
            publish_freq(target).await;
            publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
            publish_radio_text(String::new()).await;
            publish_rt_plus(None, None).await;
            publish_clock(None).await;
            publish_pty(None).await;
            publish_presets(preset_store.snapshot(), target).await;
            *last_tuned_pending = Some((target, Instant::now()));
          }
        }
        _ => {
          // Empty preset table or only one slot equal to `current`:
          // fall back to the legacy short-press behaviour so the
          // gesture remains useful from cold boot.
          info!("Preset cycle: empty/duplicate, falling back to seek");
          seek(
            radio_chip,
            i2c,
            rds,
            wall_clock,
            SeekDirection::Up,
            preset_store,
            last_tuned_pending,
          )
          .await;
        }
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
  preset_store: &mut PresetStore<'static>,
  last_tuned_pending: &mut Option<(u16, Instant)>,
) {
  match radio_chip.seek(i2c, direction).await {
    Ok(Some(freq)) => {
      info!("Seek -> {}", freq);
      rds.reset();
      *wall_clock = None;
      publish_freq(freq).await;
      publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
      publish_radio_text(String::new()).await;
      publish_rt_plus(None, None).await;
      publish_clock(None).await;
      publish_pty(None).await;
      publish_presets(preset_store.snapshot(), freq).await;
      *last_tuned_pending = Some((freq, Instant::now()));
    }
    Ok(None) => info!("Seek: end of band"),
    Err(_) => info!("Seek: I2C error"),
  }
}

/// Read RSSI + RDS and copy them into shared state.
///
/// Yields cooperatively between the two I2C transactions so other tasks
/// (UI render, input poll) can run on the executor.
#[allow(
  clippy::large_stack_frames,
  reason = "async state machine of refresh_status holds two transient String allocations \
            (RDS PS / RT decode buffers) plus the lock guard; ~1 KiB total is negligible \
            against the 16 KiB Embassy task stack on ESP32-C6."
)]
async fn refresh_status(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  ctx: &mut RefreshContext<'_>,
) -> bool {
  let (rssi, stereo) = match radio_chip.rssi_stereo(i2c) {
    Ok(v) => {
      *ctx.i2c_error_count = 0;
      v
    }
    Err(_) => {
      *ctx.i2c_error_count = ctx.i2c_error_count.saturating_add(1);
      crate::diagnostics::increment_i2c_errors();
      if *ctx.i2c_error_count >= 10 {
        info!("I2C: {} consecutive read failures", *ctx.i2c_error_count);
        let mut s = RADIO_STATE.lock().await;
        s.station_name.clear();
        s.station_name.push_str("I2C ERR!");
        s.dirty = true;
        drop(s);
      }
      (0, false)
    }
  };

  // Auto-mono hysteresis: when RSSI drops out, force MONO on the chip
  // so the user hears less hiss; release back to stereo once it recovers.
  if let Some(target) = ctx.mono_ctl.observe(rssi) {
    match radio_chip.set_mono(i2c, target) {
      Ok(()) => info!(
        "Auto-mono: {} (RSSI={}, threshold {}/{})",
        target,
        rssi,
        MonoController::RSSI_LOW,
        MonoController::RSSI_HIGH
      ),
      Err(_) => info!("Auto-mono: I2C write failed"),
    }
  }

  // Yield between I2C transactions so we don't monopolize the executor.
  yield_now().await;

  if let Ok(Some((a, b, c, d))) = radio_chip.read_rds(i2c) {
    ctx.rds.process(a, b, c, d);
    // Always re-decode — the underlying buffer may have changed even when
    // PS isn't yet "complete". Cheap (≤8 chars / ≤64 chars).
    let new_name = ctx.rds.station_name_string();
    if !new_name.is_empty() && new_name != *ctx.last_rds_name {
      *ctx.last_rds_name = new_name;
    }
    let new_text = ctx.rds.radio_text_string();
    if new_text != *ctx.last_rds_text {
      *ctx.last_rds_text = new_text;
    }
    // Re-anchor the wall clock whenever a fresh CT frame arrives.
    if let Some(ct) = ctx.rds.take_clock_time() {
      *ctx.wall_clock = Some(WallClock::from_ct(ct, Instant::now()));
      info!(
        "RDS-CT: UTC {}:{:02} offset={} half-hours",
        ct.utc_hour, ct.utc_minute, ct.local_offset_half_hours
      );
    }
  }

  // RT+ tags are derived from the *current* RT buffer, so harvest them
  // after `process` has had a chance to absorb the latest group. The
  // decoder returns `None` between songs (item-running bit clear) and
  // while the title/artist range falls outside the partial RT — in
  // both cases we want the UI to fall back to the raw RT scroller, so
  // we forward the `Option`s verbatim.
  let rt_plus_title = ctx.rds.rt_plus_title();
  let rt_plus_artist = ctx.rds.rt_plus_artist();

  // Compute the latest local clock snapshot (if we have one) so the UI
  // sees the minute hand advance even between CT bursts.
  let clock_snapshot = ctx
    .wall_clock
    .as_ref()
    .map(|wc| wc.local_hh_mm(Instant::now()));

  // Latest Programme Type label (cheap: just bit-shift + match on cached u8).
  let pty_snapshot = ctx.rds.pty_label();

  let mut state = RADIO_STATE.lock().await;
  state.rssi = rssi;
  state.stereo = stereo;
  state.auto_mono = ctx.mono_ctl.engaged();
  if state.station_name != *ctx.last_rds_name && !ctx.last_rds_name.is_empty() {
    state.station_name.clear();
    state.station_name.push_str(ctx.last_rds_name);
  } else if state.station_name.is_empty() {
    state.station_name.push_str(STATION_NAME_PLACEHOLDER);
  }
  if state.radio_text != *ctx.last_rds_text {
    state.radio_text.clear();
    state.radio_text.push_str(ctx.last_rds_text);
  }
  if state.rt_plus_title != rt_plus_title {
    state.rt_plus_title = rt_plus_title;
  }
  if state.rt_plus_artist != rt_plus_artist {
    state.rt_plus_artist = rt_plus_artist;
  }
  if state.clock_hh_mm != clock_snapshot {
    state.clock_hh_mm = clock_snapshot;
  }
  if state.pty_label != pty_snapshot {
    state.pty_label = pty_snapshot;
  }
  // Snapshot the current AF list size for the UI badge. Always
  // reflects the *current* station; cleared on station change because
  // `RdsDecoder::reset` wipes the AF list.
  let af_count = ctx.rds.alt_freqs().len() as u8;
  if state.af_count != af_count {
    state.af_count = af_count;
  }
  state.volume = radio_chip.volume();
  state.dirty = true;
  drop(state);

  // Decide whether the AF follower wants us to launch a probe this tick.
  // Done last so all I2C reads above complete before we hand the bus
  // to the (potentially long-running) probe routine.
  ctx.af_ctl.observe(rssi, af_count as usize)
}

/// Maximum number of AF candidates we'll probe in one cycle.
///
/// Bounded so a malicious or buggy broadcaster can't pin the radio
/// off-frequency for an unbounded duration. Real RDS lists are usually
/// 1–5 entries; 8 is a safe ceiling that still completes in under ~1.5 s
/// of audible silence even on slow STC.
const AF_PROBE_LIMIT: usize = 8;

/// Per-AF settle delay before sampling RSSI, in milliseconds.
///
/// The Si4703 STC flag clears within ~60 ms of a tune, but RSSI takes
/// another ~80–100 ms to stabilise on the new carrier. 120 ms is the
/// shortest delay that gives reproducible numbers in bench testing.
const AF_SETTLE_MS: u64 = 120;

/// Maximum time we'll wait for the PI code to reappear on a candidate
/// frequency before declaring "this is not the same programme".
///
/// RDS group repetition rate is ~10 groups/s so a healthy broadcaster
/// re-emits Block A roughly every 100 ms. 800 ms of polling gives us
/// 8 chances and rejects dead carriers without dragging probe latency.
const AF_PI_VERIFY_MS: u64 = 800;

/// Probe the AF list and switch to a stronger candidate if one carries
/// the same Programme Identification (PI) code as the original station.
///
/// **Audible side-effect**: this routine briefly tunes off the original
/// frequency, which sounds like a half-second of silence followed by
/// either renewed audio (success) or a return to the original carrier
/// (probe failed / rejected). The [`AfFollower`] gates this so it only
/// happens after sustained signal loss; real-world cadence is at most
/// once every 30 s on a marginal station.
#[allow(
  clippy::large_stack_frames,
  reason = "async state machine of run_af_probe holds the candidates buffer \
            (16 B), the verify-loop's RDS read tuple, and several transient \
            String allocations for publish_* calls; ~2.3 KiB total is \
            negligible against the 16 KiB Embassy task stack on ESP32-C6, \
            and the routine fires at most once every 30 s by design."
)]
async fn run_af_probe(
  radio_chip: &mut Si4703,
  i2c: &mut I2c<'static, esp_hal::Blocking>,
  rds: &mut RdsDecoder,
  wall_clock: &mut Option<WallClock>,
  preset_store: &mut PresetStore<'static>,
  last_tuned_pending: &mut Option<(u16, Instant)>,
) {
  // Snapshot the AF list and PI *before* we touch the chip — the very
  // first off-frequency tune below will reset RDS state and wipe both.
  let original_freq = match radio_chip.current_frequency(i2c) {
    Ok(f) => f,
    Err(_) => {
      info!("AF probe: aborted (cannot read current frequency)");
      return;
    }
  };
  let Some(original_pi) = rds.pi() else {
    info!("AF probe: aborted (no PI cached for original station)");
    return;
  };
  let mut candidates = [0u16; AF_PROBE_LIMIT];
  let candidate_count = {
    let src = rds.alt_freqs();
    let mut n = 0;
    for &freq in src {
      if n == AF_PROBE_LIMIT {
        break;
      }
      // Skip the original frequency itself — some broadcasters list
      // their own carrier, which would result in a no-op probe.
      if freq == original_freq {
        continue;
      }
      candidates[n] = freq;
      n += 1;
    }
    n
  };
  if candidate_count == 0 {
    info!("AF probe: aborted (no candidates after filtering original)");
    return;
  }

  info!(
    "AF probe: start (orig={} PI=0x{:04x} candidates={})",
    original_freq, original_pi, candidate_count
  );
  publish_af_status(rds.alt_freqs().len() as u8, true).await;

  // Phase 1: sweep candidates and pick the strongest.
  let mut best_freq: Option<u16> = None;
  let mut best_rssi: u8 = 0;
  // Re-read the original RSSI immediately so the comparison is
  // contemporaneous (signal levels can move several units in 200 ms).
  let original_rssi = radio_chip.rssi(i2c).unwrap_or(0);
  for &freq in &candidates[..candidate_count] {
    if radio_chip.tune(i2c, freq).await.is_err() {
      info!("AF probe: tune({}) failed, skipping", freq);
      continue;
    }
    Timer::after(Duration::from_millis(AF_SETTLE_MS)).await;
    let rssi = radio_chip.rssi(i2c).unwrap_or(0);
    info!("AF probe: candidate {} RSSI={}", freq, rssi);
    if rssi > best_rssi {
      best_rssi = rssi;
      best_freq = Some(freq);
    }
  }

  // Phase 2: decide. Require a meaningful improvement so we don't
  // burn an audible hiccup for marginal RSSI gains.
  let Some(target) = best_freq
    .filter(|_| best_rssi >= original_rssi.saturating_add(AfFollower::RSSI_IMPROVE_MARGIN))
  else {
    info!(
      "AF probe: no improvement (best_rssi={} orig_rssi={}); rolling back",
      best_rssi, original_rssi
    );
    if radio_chip.tune(i2c, original_freq).await.is_err() {
      info!("AF probe: rollback tune failed!");
    }
    publish_af_status(rds.alt_freqs().len() as u8, false).await;
    return;
  };

  // Phase 3: commit — land on the candidate, wait for the new station's
  // RDS to deliver a PI, and verify it matches before we publish.
  if radio_chip.tune(i2c, target).await.is_err() {
    info!("AF probe: final tune({}) failed, rolling back", target);
    let _ = radio_chip.tune(i2c, original_freq).await;
    publish_af_status(rds.alt_freqs().len() as u8, false).await;
    return;
  }

  // Reset the decoder so the next `read_rds` populates state fresh
  // for the (potentially) new station; the original AF list is
  // discarded along with PS/RT to avoid stale UI strings.
  rds.reset();
  *wall_clock = None;

  // Poll for a PI on the new frequency. We accept any block whose PI
  // matches the original; any other PI — or a verify timeout — means
  // the AF entry was lying or the carrier is silent, so we roll back.
  let verify_deadline = Instant::now() + Duration::from_millis(AF_PI_VERIFY_MS);
  let mut verified = false;
  while Instant::now() < verify_deadline {
    if let Ok(Some((a, b, c, d))) = radio_chip.read_rds(i2c) {
      rds.process(a, b, c, d);
      if rds.pi() == Some(original_pi) {
        verified = true;
        break;
      }
    }
    Timer::after(Duration::from_millis(60)).await;
  }

  if !verified {
    info!(
      "AF probe: PI mismatch on {} (expected 0x{:04x}, got {:?}); rolling back",
      target,
      original_pi,
      rds.pi()
    );
    rds.reset();
    let _ = radio_chip.tune(i2c, original_freq).await;
    publish_af_status(rds.alt_freqs().len() as u8, false).await;
    return;
  }

  // Success path: publish the new frequency the same way handle_command
  // does after a manual tune so the UI / preset indicator stay coherent.
  info!(
    "AF probe: switched {} -> {} (RSSI {} -> {})",
    original_freq, target, original_rssi, best_rssi
  );
  publish_freq(target).await;
  publish_station_name(String::from(STATION_NAME_PLACEHOLDER)).await;
  publish_radio_text(String::new()).await;
  publish_rt_plus(None, None).await;
  publish_clock(None).await;
  publish_pty(None).await;
  publish_presets(preset_store.snapshot(), target).await;
  publish_af_status(rds.alt_freqs().len() as u8, false).await;
  *last_tuned_pending = Some((target, Instant::now()));
}

// ============================================================================
// Listening-log sampler
// ============================================================================

/// Periodically snapshot [`RADIO_STATE`] into the global
/// [`crate::listening_log::LISTENING_LOG`] ring buffer.
///
/// Wakes every [`SAMPLE_INTERVAL_SECS`](crate::listening_log::SAMPLE_INTERVAL_SECS)
/// seconds and only writes a new entry when the listener has actually
/// moved (different frequency) or the broadcaster has rotated the PS
/// station name. Without that gate the buffer would fill up with 360
/// identical rows per hour on a stable station and the replay panel
/// in the web console would lose its purpose.
#[embassy_executor::task]
pub async fn logger_task() -> ! {
  use crate::listening_log::{LISTENING_LOG, SAMPLE_INTERVAL_SECS, capture};

  let mut last_freq: u16 = 0;
  let mut last_ps_first_byte: Option<u8> = None;

  loop {
    Timer::after(Duration::from_secs(SAMPLE_INTERVAL_SECS)).await;

    // Take a snapshot under the radio-state lock; release before
    // touching the log mutex so the two locks are never held
    // simultaneously (defensive even though no other path takes both).
    let (entry, freq, ps_first) = {
      let state = RADIO_STATE.lock().await;
      let uptime = (Instant::now().as_secs()) as u32;
      let snap = capture(uptime, &state);
      let ps_first = state.station_name.as_bytes().first().copied();
      (snap, state.freq_mhz_x10, ps_first)
    };

    // Skip the very first sample if nothing meaningful is loaded yet
    // (boot placeholder freq + empty PS would just pollute the log).
    if entry.freq_x10 == 0 {
      continue;
    }

    let changed = freq != last_freq || ps_first != last_ps_first_byte;
    if !changed {
      continue;
    }

    last_freq = freq;
    last_ps_first_byte = ps_first;

    let mut log = LISTENING_LOG.lock().await;
    log.push(entry);
  }
}
