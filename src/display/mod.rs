//! ST7789 Display Driver Module
//!
//! Provides a reusable abstraction for driving ST7789 LCD screens via SPI,
//! integrated with the Slint UI framework for embedded rendering.
//!
//! # Features
//! - Slint platform implementation for ESP32 (`EspPlatform`)
//! - Line-by-line rendering to ST7789 via `LineBufferProvider`
//! - Display configuration constants
//!
//! # Example
//! ```no_run
//! use radio::display::{EspPlatform, DisplayLineBuffer, DISPLAY_WIDTH, DISPLAY_HEIGHT};
//! ```

use alloc::rc::Rc;
use core::ops::Range;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use mipidsi::models::ST7789;
use slint::platform::software_renderer::{LineBufferProvider, MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{Platform, WindowAdapter};

extern crate alloc;

/// Display width in pixels
pub const DISPLAY_WIDTH: u16 = 240;

/// Display height in pixels
pub const DISPLAY_HEIGHT: u16 = 320;

/// Slint platform implementation for ESP32.
///
/// Bridges the Slint UI framework with the Embassy async runtime,
/// providing time tracking and window management.
pub struct EspPlatform {
  window: Rc<MinimalSoftwareWindow>,
}

impl EspPlatform {
  /// Create a new platform instance with the given window.
  pub fn new(window: Rc<MinimalSoftwareWindow>) -> Self {
    Self { window }
  }
}

impl Platform for EspPlatform {
  fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
    Ok(self.window.clone())
  }

  fn duration_since_start(&self) -> core::time::Duration {
    let ticks = embassy_time::Instant::now().as_millis();
    core::time::Duration::from_millis(ticks)
  }
}

/// Line buffer provider for rendering Slint UI to an ST7789 display.
///
/// Implements Slint's `LineBufferProvider` trait to render the UI line by line,
/// which minimizes RAM usage on resource-constrained MCUs.
///
/// # Type Parameters
/// - `DI`: The display interface type (e.g., SPI interface)
/// - `RST`: The reset pin type
pub struct DisplayLineBuffer<'a, DI, RST>
where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  display: &'a mut mipidsi::Display<DI, ST7789, RST>,
  line_buffer: [Rgb565Pixel; DISPLAY_WIDTH as usize],
}

impl<'a, DI, RST> DisplayLineBuffer<'a, DI, RST>
where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  /// Create a new line buffer provider for the given display.
  ///
  /// # Arguments
  /// - `display`: Mutable reference to the ST7789 display instance
  pub fn new(display: &'a mut mipidsi::Display<DI, ST7789, RST>) -> Self {
    Self {
      display,
      line_buffer: [Rgb565Pixel(0); DISPLAY_WIDTH as usize],
    }
  }
}

impl<DI, RST> LineBufferProvider for DisplayLineBuffer<'_, DI, RST>
where
  DI: mipidsi::interface::Interface,
  RST: embedded_hal::digital::OutputPin,
  Rgb565: mipidsi::interface::InterfacePixelFormat<DI::Word>,
{
  type TargetPixel = Rgb565Pixel;

  fn process_line(
    &mut self,
    line: usize,
    range: Range<usize>,
    render_fn: impl FnOnce(&mut [Self::TargetPixel]),
  ) {
    // Render one line of pixels to buffer
    let buffer = &mut self.line_buffer[range.clone()];
    render_fn(buffer);

    // Convert and write the rendered line to display
    let pixels = buffer.iter().enumerate().map(|(x, pixel)| {
      let raw = pixel.0;
      embedded_graphics::Pixel(
        Point::new((range.start + x) as i32, line as i32),
        Rgb565::new(
          ((raw >> 11) & 0x1F) as u8,
          ((raw >> 5) & 0x3F) as u8,
          (raw & 0x1F) as u8,
        ),
      )
    });

    let _ = self.display.draw_iter(pixels);
  }
}
