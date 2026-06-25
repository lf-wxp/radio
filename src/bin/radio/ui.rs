//! UI render glue: pulls the latest [`crate::state::RadioState`] snapshot
//! into the Slint window, then drives the software renderer.

use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use slint::platform::software_renderer::MinimalSoftwareWindow;

use radio::display::DisplayLineBuffer;

use crate::RadioWindow;
use crate::state::{RADIO_STATE, RadioState, SPECTRUM_LEN};

/// Mirror a [`RadioState`] snapshot into the Slint component.
fn apply_state_to_ui(ui: &RadioWindow, snapshot: &RadioState) {
  ui.set_freq_mhz_x10(snapshot.freq_mhz_x10 as i32);
  ui.set_rssi(snapshot.rssi as i32);
  ui.set_volume(snapshot.volume as i32);
  ui.set_muted(snapshot.muted);
  ui.set_wifi_connected(snapshot.wifi_connected);
  ui.set_wifi_ssid(snapshot.wifi_ssid.as_str().into());
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

  // Push the boot-time RSSI sweep into the Slint `[int]` model. We
  // rebuild a `VecModel` every refresh because the snapshot is small
  // (52 ints) and Slint copies the model handle by reference internally.
  let spectrum_vec: alloc::vec::Vec<i32> = snapshot.spectrum.iter().map(|&v| v as i32).collect();
  ui.set_spectrum(slint::ModelRc::new(slint::VecModel::from(spectrum_vec)));
  ui.set_spectrum_cursor(spectrum_cursor_for(snapshot.freq_mhz_x10));
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
