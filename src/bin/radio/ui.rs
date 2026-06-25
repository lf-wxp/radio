//! UI render glue: pulls the latest [`crate::state::RadioState`] snapshot
//! into the Slint window, then drives the software renderer.

use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use slint::platform::software_renderer::MinimalSoftwareWindow;

use radio::display::DisplayLineBuffer;

use crate::RadioWindow;
use crate::state::{RADIO_STATE, RadioState};

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
