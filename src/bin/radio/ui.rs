//! UI render glue: pulls the latest [`crate::state::RadioState`] snapshot
//! into the Slint window, then drives the software renderer.

use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use slint::platform::software_renderer::MinimalSoftwareWindow;

use radio::display::DisplayLineBuffer;

use crate::RadioWindow;
use crate::state::{OtaProgress, RADIO_STATE, RadioState, SPECTRUM_LEN};

/// Mirror a [`RadioState`] snapshot into the Slint component.
#[allow(
  clippy::large_stack_frames,
  reason = "holds the full RadioState snapshot (heap Strings + 52-byte \
            spectrum + PresetSet) plus a few transient format String \
            allocations; ~1 KiB is well under the 16 KiB Embassy task stack."
)]
fn apply_state_to_ui(ui: &RadioWindow, snapshot: &RadioState) {
  ui.set_freq_mhz_x10(snapshot.freq_mhz_x10 as i32);
  ui.set_rssi(snapshot.rssi as i32);
  ui.set_volume(snapshot.volume as i32);
  ui.set_muted(snapshot.muted);
  ui.set_wifi_connected(snapshot.wifi_connected);
  ui.set_wifi_ssid(snapshot.wifi_ssid.as_str().into());
  // Web console IP — dotted-quad ASCII when known, empty string
  // otherwise (the Slint side hides the IP form when empty).
  let web_ip = match snapshot.web_ip {
    Some([a, b, c, d]) => alloc::format!("{}.{}.{}.{}", a, b, c, d),
    None => alloc::string::String::new(),
  };
  ui.set_web_ip(web_ip.as_str().into());
  ui.set_status_text(snapshot.status.into());
  ui.set_station_name(snapshot.station_name.as_str().into());
  ui.set_radio_text(snapshot.radio_text.as_str().into());
  // Format the wall clock as `HH:MM` (zero-padded). When no CT has been
  // decoded yet, push an empty string so the Slint component hides the
  // badge entirely (`visible: clock-text != ""`).
  let clock_text = match snapshot.clock_hh_mm {
    Some((h, m)) => alloc::format!("{:02}:{:02}", h, m),
    None => alloc::string::String::new(),
  };
  ui.set_clock_text(clock_text.as_str().into());
  // Programme Type badge: empty string keeps the Slint component hidden
  // (`visible: pty-label != ""`). The label is already a `&'static str`
  // from `radio::si4703::pty_label`, so no allocation is required.
  ui.set_pty_label(snapshot.pty_label.unwrap_or("").into());
  // Stereo / mono indicator. Three states reflected in one string:
  //   "ST"      — Si4703 reports stereo lock
  //   "MO"      — plain mono (no stereo pilot detected)
  //   "MO*"     — auto-mono engaged (we forced it because of weak RSSI)
  let stereo_text = if snapshot.auto_mono {
    "MO*"
  } else if snapshot.stereo {
    "ST"
  } else {
    "MO"
  };
  ui.set_stereo_text(stereo_text.into());
  ui.set_stereo_active(snapshot.stereo && !snapshot.auto_mono);

  // RDS-AF badge: empty when the broadcaster has not announced any
  // alternative frequencies, `AF→` while a probe is in flight (so the
  // listener sees why audio briefly dipped), and `AF·N` otherwise.
  let af_text = if snapshot.af_following {
    alloc::string::String::from("AF→")
  } else if snapshot.af_count == 0 {
    alloc::string::String::new()
  } else {
    alloc::format!("AF·{}", snapshot.af_count)
  };
  ui.set_af_text(af_text.as_str().into());
  ui.set_af_active(snapshot.af_following);

  // Preset indicator: empty when nothing saved (component hides itself
  // via `visible: preset-text != ""`); otherwise `P {idx}/{used}` when
  // current freq is on a saved slot, or `P -/{used}` when not.
  let used = snapshot.presets.used();
  let preset_text = if used == 0 {
    alloc::string::String::new()
  } else {
    match snapshot.preset_idx {
      Some(idx) => alloc::format!("P {}/{}", idx + 1, used),
      None => alloc::format!("P -/{}", used),
    }
  };
  ui.set_preset_text(preset_text.as_str().into());

  // Push the boot-time RSSI sweep into the Slint `[int]` model. We
  // rebuild a `VecModel` every refresh because the snapshot is small
  // (52 ints) and Slint copies the model handle by reference internally.
  let spectrum_vec: alloc::vec::Vec<i32> = snapshot.spectrum.iter().map(|&v| v as i32).collect();
  ui.set_spectrum(slint::ModelRc::new(slint::VecModel::from(spectrum_vec)));
  ui.set_spectrum_cursor(spectrum_cursor_for(snapshot.freq_mhz_x10));

  // -------- OTA progress overlay --------
  //
  // Map the [`OtaProgress`] state machine onto the six Slint
  // properties consumed by the modal in `radio_ui.slint`. The overlay
  // is hidden in `Idle` and shown otherwise; the progress bar value
  // is normalised to 0.0..1.0, with sentinel waypoints for the phases
  // that don't carry a byte counter.
  apply_ota_to_ui(ui, snapshot.ota_progress);
}

/// Translate an [`OtaProgress`] phase into the Slint overlay's
/// six in-out properties.
///
/// Pulled out of [`apply_state_to_ui`] purely for readability — it has
/// six setters and a multi-arm match that would otherwise dwarf the
/// rest of the snapshot apply.
fn apply_ota_to_ui(ui: &RadioWindow, progress: OtaProgress) {
  // Sentinel progress values for the phases without a byte counter.
  // Picked so a glance at the bar reveals which phase is current
  // even on a 240px-wide screen.
  const CONNECTING_PROGRESS: f32 = 0.05;
  const ACTIVATING_PROGRESS: f32 = 0.95;

  let (active, status, value, indeterminate, success, failed) = match progress {
    OtaProgress::Idle => (
      false,
      alloc::string::String::new(),
      0.0,
      false,
      false,
      false,
    ),
    OtaProgress::Connecting => (
      true,
      alloc::string::String::from("Connecting\u{2026}"),
      CONNECTING_PROGRESS,
      false,
      false,
      false,
    ),
    OtaProgress::Downloading { received, total } => {
      // `total == 0` means the server didn't send Content-Length;
      // fall back to an indeterminate spinner and just report the
      // running byte count. Casts here are safe up to 4 GiB which
      // is far above any realistic firmware size (current image is
      // ~1.4 MiB).
      let recv_kib = (received as f32) / 1024.0;
      if total == 0 {
        (
          true,
          alloc::format!("Downloading {:.1} KiB", recv_kib),
          0.5,
          true,
          false,
          false,
        )
      } else {
        let total_kib = (total as f32) / 1024.0;
        let ratio = (received as f32) / (total as f32);
        let pct = (ratio * 100.0) as u32;
        (
          true,
          alloc::format!(
            "Downloading {:.1} / {:.1} KiB ({}%)",
            recv_kib,
            total_kib,
            pct,
          ),
          ratio.clamp(0.0, 1.0),
          false,
          false,
          false,
        )
      }
    }
    OtaProgress::Activating => (
      true,
      alloc::string::String::from("Activating\u{2026}"),
      ACTIVATING_PROGRESS,
      false,
      false,
      false,
    ),
    OtaProgress::Success => (
      true,
      alloc::string::String::from("Update staged"),
      1.0,
      false,
      true,
      false,
    ),
    OtaProgress::Failed(reason) => (
      true,
      alloc::format!("Failed: {}", reason),
      1.0,
      false,
      false,
      true,
    ),
  };

  ui.set_ota_active(active);
  ui.set_ota_status_text(status.as_str().into());
  ui.set_ota_progress(value);
  ui.set_ota_indeterminate(indeterminate);
  ui.set_ota_success(success);
  ui.set_ota_failed(failed);
}

/// Compute the index of the spectrum bucket containing `freq_mhz_x10`.
///
/// Mirrors the bucket layout used by [`radio::si4703::Si4703::sweep_rssi`]:
/// bucket `i` covers `[bottom + span*i/N, bottom + span*(i+1)/N)`. We
/// look up which slot a frequency lands in using the inverse formula
/// `i = (freq - bottom) * N / span`, clamping at both ends so the cursor
/// stays valid for out-of-band edge cases (e.g. before the chip is tuned).
///
/// FM band bounds are hard-coded here to match the default
/// [`radio::si4703::Band`] used at boot. If the firmware ever exposes a
/// runtime band switch, this helper should be parameterised accordingly.
fn spectrum_cursor_for(freq_mhz_x10: u16) -> i32 {
  // FM US/Europe band: 87.5 .. 108.0 MHz.
  const BOTTOM_X10: u16 = 875;
  const TOP_X10: u16 = 1080;

  if freq_mhz_x10 < BOTTOM_X10 {
    return 0;
  }
  let span = u32::from(TOP_X10 - BOTTOM_X10);
  // SAFETY (logical): freq_mhz_x10 ≥ BOTTOM_X10 here, subtraction is safe.
  let offset = u32::from(freq_mhz_x10 - BOTTOM_X10);
  let n = SPECTRUM_LEN as u32;
  let idx = (offset * n / span).min(n - 1);
  idx as i32
}

/// Drive Slint's software renderer once, painting any dirty regions to the
/// physical display via [`DisplayLineBuffer`].
fn paint<DI, RST>(
  window: &alloc::rc::Rc<MinimalSoftwareWindow>,
  display: &mut mipidsi::Display<DI, mipidsi::models::ST7789, RST>,
) where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  slint::platform::update_timers_and_animations();
  window.draw_if_needed(|renderer| {
    let line_buffer = DisplayLineBuffer::new(display);
    renderer.render_by_line(line_buffer);
  });
}

/// Render exactly one frame (used during boot to show progress messages).
pub async fn render_once<DI, RST>(
  window: &alloc::rc::Rc<MinimalSoftwareWindow>,
  ui: &RadioWindow,
  display: &mut mipidsi::Display<DI, mipidsi::models::ST7789, RST>,
) where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  let snapshot = RADIO_STATE.lock().await.clone();
  apply_state_to_ui(ui, &snapshot);
  paint(window, display);
}

/// Render the Slint UI to the display in a tight loop at ~10 fps.
pub async fn run_loop<DI, RST>(
  window: &alloc::rc::Rc<MinimalSoftwareWindow>,
  display: &mut mipidsi::Display<DI, mipidsi::models::ST7789, RST>,
  ui_weak: &slint::Weak<RadioWindow>,
) -> !
where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  loop {
    if let Some(ui) = ui_weak.upgrade() {
      // Only clone when state changes (avoid unnecessary heap allocation)
      let mut guard = RADIO_STATE.lock().await;
      if guard.dirty {
        guard.dirty = false;
        let snapshot = guard.clone();
        drop(guard);
        apply_state_to_ui(&ui, &snapshot);
      } else {
        drop(guard);
      }
    }
    paint(window, display);
    Timer::after(Duration::from_millis(100)).await;
  }
}
