//! Hardware initialisation: display, tuner I2C, rotary encoder.
//!
//! Each function takes ownership of the peripherals it needs and returns
//! ready-to-use driver handles. Keeping this module focused on init makes
//! `main.rs` a clear linear orchestration script.

use alloc::boxed::Box;

use embassy_time::{Duration, Timer};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::Blocking;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::pcnt::Pcnt;
use esp_hal::peripherals;
use esp_hal::spi::Mode as SpiMode;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use mipidsi::Builder;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation, Rotation};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use static_cell::StaticCell;

use radio::display::{DISPLAY_HEIGHT, DISPLAY_WIDTH, EspPlatform};
use radio::rotary_encoder::{EncoderConfig, RotaryEncoder};
use radio::si4703::{ChannelSpacing, FmBand, Si4703};

/// Backing buffer for the ST7789 SPI interface.
static SPI_BUF: StaticCell<[u8; 960]> = StaticCell::new();

/// Concrete display type used by both the boot screen and the UI loop.
///
/// Aliasing keeps the long generic signature out of caller sites.
pub type DisplayDevice = mipidsi::Display<
  SpiInterface<
    'static,
    ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, embedded_hal_bus::spi::NoDelay>,
    Output<'static>,
  >,
  ST7789,
  Output<'static>,
>;

/// Bundle of GPIO pin instances used by [`init_display`].
pub struct DisplayPins {
  pub sck: peripherals::GPIO3<'static>,
  pub mosi: peripherals::GPIO0<'static>,
  pub cs: peripherals::GPIO1<'static>,
  pub dc: peripherals::GPIO2<'static>,
  pub rst: peripherals::GPIO22<'static>,
  pub blk: peripherals::GPIO23<'static>,
}

/// Result of [`init_display`]: the framebuffer-capable display plus the
/// Slint software window already wired up to the global platform.
pub struct DisplayBundle {
  pub display: DisplayDevice,
  pub window: alloc::rc::Rc<MinimalSoftwareWindow>,
}

/// Initialise SPI2 + ST7789 + Slint software platform.
///
/// Must be called exactly once.
#[allow(
  clippy::large_stack_frames,
  reason = "SPI_BUF.init([0u8; 960]) materialises 960 bytes on the stack once"
)]
pub fn init_display(spi: peripherals::SPI2<'static>, pins: DisplayPins) -> DisplayBundle {
  let cs = Output::new(pins.cs, Level::High, OutputConfig::default());
  let dc = Output::new(pins.dc, Level::Low, OutputConfig::default());
  let rst = Output::new(pins.rst, Level::High, OutputConfig::default());
  // Backlight: keep on for the lifetime of the program. The Output handle
  // does not implement Drop, so simply binding it with `_` keeps the pin
  // configured (no `mem::forget` required).
  let _blk = Output::new(pins.blk, Level::High, OutputConfig::default());

  let spi_config = SpiConfig::default()
    .with_frequency(Rate::from_mhz(40))
    .with_mode(SpiMode::_0);
  let spi_bus = Spi::new(spi, spi_config)
    .expect("SPI init failed")
    .with_sck(pins.sck)
    .with_mosi(pins.mosi);

  let spi_device = ExclusiveDevice::new_no_delay(spi_bus, cs).expect("SPI device failed");
  let spi_buf = SPI_BUF.init([0u8; 960]);
  let spi_interface = SpiInterface::new(spi_device, dc, spi_buf);

  let mut delay = embassy_time::Delay;
  let mut display = Builder::new(ST7789, spi_interface)
    .display_size(DISPLAY_WIDTH, DISPLAY_HEIGHT)
    .display_offset(0, 0)
    .orientation(Orientation::new().rotate(Rotation::Deg0))
    .color_order(ColorOrder::Rgb)
    .invert_colors(ColorInversion::Inverted)
    .reset_pin(rst)
    .init(&mut delay)
    .expect("Display init failed");
  display.clear(Rgb565::BLACK).expect("clear failed");

  let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
  window.set_size(slint::PhysicalSize::new(
    DISPLAY_WIDTH as u32,
    DISPLAY_HEIGHT as u32,
  ));
  let platform = EspPlatform::new(window.clone());
  slint::platform::set_platform(Box::new(platform)).expect("set platform failed");

  DisplayBundle { display, window }
}

/// Pins required by [`init_tuner`].
pub struct TunerPins {
  pub sda: peripherals::GPIO6<'static>,
  pub scl: peripherals::GPIO7<'static>,
  pub rst: peripherals::GPIO10<'static>,
}

/// Reset the Si4703 (low->high while SDIO is held low) and bring up I2C.
///
/// Returns the chip wrapper plus its owned I2C bus. The caller is
/// responsible for calling [`Si4703::init`] and reacting to its result.
pub async fn init_tuner(
  i2c0: peripherals::I2C0<'static>,
  pins: TunerPins,
) -> (Si4703, I2c<'static, Blocking>) {
  // Si4703 needs a special reset sequence: SDIO low while RST goes low->high.
  let mut rst_pin = Output::new(pins.rst, Level::Low, OutputConfig::default());
  let mut sdio_pin = Output::new(pins.sda, Level::Low, OutputConfig::default());
  sdio_pin.set_low();
  rst_pin.set_low();
  Timer::after(Duration::from_millis(10)).await;
  rst_pin.set_high();
  Timer::after(Duration::from_millis(10)).await;
  // Drop the binding so GPIO6 can be reclaimed below for I2C SDA. The
  // Output type does not implement Drop, but releasing the binding is
  // still the symbolic handover point.
  let _released_sdio = sdio_pin;

  // SAFETY: GPIO6 was exclusively owned by `sdio_pin` (now moved to
  // `_released_sdio` above) and is no longer used by any other peripheral
  // in this program. The reset sequence is complete, so the pin can be
  // safely reclaimed for I2C SDA. No other code path accesses GPIO6
  // between this point and the I2C initialization below.
  let sda_pin = unsafe { peripherals::GPIO6::steal() };
  let i2c_config = I2cConfig::default().with_frequency(Rate::from_khz(100));
  let i2c = I2c::new(i2c0, i2c_config)
    .expect("I2C init failed")
    .with_sda(sda_pin)
    .with_scl(pins.scl);

  let radio_chip = Si4703::new(FmBand::UsEurope, ChannelSpacing::Spacing100K);
  (radio_chip, i2c)
}

/// Pins required by [`init_encoder`].
pub struct EncoderPins {
  pub a: peripherals::GPIO11<'static>,
  pub b: peripherals::GPIO18<'static>,
  pub key: peripherals::GPIO19<'static>,
}

/// Initialise the rotary encoder + push button on PCNT unit 0.
///
/// `pcnt` is expected to already have its interrupt handler wired up by
/// the caller (the handler must live in `main.rs` because the
/// `#[esp_hal::handler]` attribute requires a top-level `fn`).
pub fn init_encoder(pcnt: Pcnt<'static>, pins: EncoderPins) -> RotaryEncoder<'static, 0> {
  let input_config = InputConfig::default().with_pull(Pull::Up);
  let pin_a = Input::new(pins.a, input_config);
  let pin_b = Input::new(pins.b, input_config);
  let pin_key = Input::new(pins.key, input_config);

  RotaryEncoder::new(pcnt.unit0, pin_a, pin_b, pin_key, EncoderConfig::default())
    .expect("Encoder init failed")
}
