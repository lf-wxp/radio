//! Rotary Encoder Driver Module
//!
//! Supports KY-040 type incremental rotary encoders with quadrature outputs (S1/S2) and push button (KEY).
//!
//! # Hardware Connection
//! Rotary encoders typically have 5 pins:
//! - **GND** -> Ground
//! - **5V**  -> Power supply (the module has built-in level conversion, 3.3V also works)
//! - **S1**  -> CLK/A signal, connect to GPIO (requires pull-up)
//! - **S2**  -> DT/B signal, connect to GPIO (requires pull-up)
//! - **KEY** -> Button signal, pulled low when pressed, connect to GPIO (requires pull-up)
//!
//! # How It Works
//! Uses ESP32's PCNT (Pulse Counter) hardware peripheral to decode quadrature signals,
//! no software polling required, precise and reliable. PCNT configures A/B signals via two cross-coupled channels,
//! automatically determining rotation direction and incrementing/decrementing the counter value.
//!
//! # Example
//! ```no_run
//! use radio::rotary_encoder::{RotaryEncoder, EncoderConfig};
//!
//! // Create encoder with default configuration
//! let config = EncoderConfig::default();
//! let encoder = RotaryEncoder::new(pcnt_unit, pin_a, pin_b, pin_key, config);
//! ```

use core::sync::atomic::Ordering;

use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::pcnt::{Pcnt, channel, unit};
use portable_atomic::AtomicI32;

// ============================================================================
// Constants
// ============================================================================

/// Default PCNT filter threshold (APB_CLK cycle count)
/// Used to filter glitch signals caused by mechanical bounce
const DEFAULT_FILTER_THRESHOLD: u16 = 800;

/// Default PCNT high limit, triggers interrupt and resets counter when reached
const DEFAULT_HIGH_LIMIT: i16 = 100;

/// Default PCNT low limit, triggers interrupt and resets counter when reached
const DEFAULT_LOW_LIMIT: i16 = -100;

/// Button debounce delay (milliseconds)
const DEBOUNCE_MS: u64 = 50;

// ============================================================================
// Public Types
// ============================================================================

/// Rotary encoder configuration
#[derive(Clone, Copy, Debug, defmt::Format)]
pub struct EncoderConfig {
  /// PCNT filter threshold (0-1023), used to filter glitch signals.
  /// Higher values provide stronger filtering, but slower response. Set to 0 to disable filtering.
  pub filter_threshold: u16,
  /// PCNT high limit (positive number), triggers overflow interrupt when count reaches this value
  pub high_limit: i16,
  /// PCNT low limit (negative number), triggers overflow interrupt when count reaches this value
  pub low_limit: i16,
}

impl Default for EncoderConfig {
  fn default() -> Self {
    Self {
      filter_threshold: DEFAULT_FILTER_THRESHOLD,
      high_limit: DEFAULT_HIGH_LIMIT,
      low_limit: DEFAULT_LOW_LIMIT,
    }
  }
}

/// Rotation direction
#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum Direction {
  /// Clockwise rotation
  Clockwise,
  /// Counter-clockwise rotation
  CounterClockwise,
  /// No rotation
  None,
}

/// Encoder event
#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum EncoderEvent {
  /// Rotation event, contains direction and steps
  Rotate(Direction, i32),
  /// Button pressed
  ButtonPress,
  /// Button released
  ButtonRelease,
}

/// Encoder initialization error
#[derive(Clone, Copy, Debug, PartialEq, Eq, defmt::Format)]
pub enum EncoderError {
  /// Invalid filter threshold (exceeds 1023)
  InvalidFilterThreshold,
  /// Invalid high limit (must be positive)
  InvalidHighLimit,
  /// Invalid low limit (must be negative)
  InvalidLowLimit,
}

// ============================================================================
// Rotary Encoder Driver
// ============================================================================

/// Rotary encoder driver
///
/// Uses PCNT hardware peripheral to decode quadrature signals, providing precise rotation detection.
/// Also supports button detection (with debouncing).
///
/// # Generic Parameters
/// - `UNIT`: PCNT unit number (0-3)
pub struct RotaryEncoder<'d, const UNIT: usize> {
  /// PCNT unit, used to read counter value
  unit: unit::Unit<'d, UNIT>,
  /// Button input pin
  button: Input<'d>,
  /// Overflow accumulator (updated in interrupt)
  overflow_value: &'static AtomicI32,
  /// Last read counter value (used to calculate delta)
  last_value: i32,
  /// Configuration
  config: EncoderConfig,
}

/// Overflow accumulator for PCNT Unit0
static OVERFLOW_UNIT0: AtomicI32 = AtomicI32::new(0);
/// Overflow accumulator for PCNT Unit1
static OVERFLOW_UNIT1: AtomicI32 = AtomicI32::new(0);
/// Overflow accumulator for PCNT Unit2
static OVERFLOW_UNIT2: AtomicI32 = AtomicI32::new(0);
/// Overflow accumulator for PCNT Unit3
static OVERFLOW_UNIT3: AtomicI32 = AtomicI32::new(0);

/// Get reference to overflow accumulator for specified PCNT unit.
///
/// # Panics
/// Panics if `unit` is not in the range 0..=3.
fn overflow_atomic(unit: usize) -> &'static AtomicI32 {
  match unit {
    0 => &OVERFLOW_UNIT0,
    1 => &OVERFLOW_UNIT1,
    2 => &OVERFLOW_UNIT2,
    3 => &OVERFLOW_UNIT3,
    _ => panic!("PCNT unit index out of range: {unit}"),
  }
}

impl<'d, const UNIT: usize> RotaryEncoder<'d, UNIT> {
  /// Create a new rotary encoder instance
  ///
  /// # Arguments
  /// - `unit`: PCNT unit (obtained from `Pcnt` instance)
  /// - `pin_a`: S1/CLK signal pin
  /// - `pin_b`: S2/DT signal pin
  /// - `pin_key`: KEY button pin
  /// - `config`: Encoder configuration
  ///
  /// # Errors
  /// Returns `EncoderError` if configuration parameters are invalid
  pub fn new(
    unit: unit::Unit<'d, UNIT>,
    pin_a: Input<'d>,
    pin_b: Input<'d>,
    pin_key: Input<'d>,
    config: EncoderConfig,
  ) -> Result<Self, EncoderError> {
    // Validate configuration
    if config.filter_threshold > 1023 {
      return Err(EncoderError::InvalidFilterThreshold);
    }
    if config.high_limit <= 0 {
      return Err(EncoderError::InvalidHighLimit);
    }
    if config.low_limit >= 0 {
      return Err(EncoderError::InvalidLowLimit);
    }

    // Configure PCNT unit
    unit
      .set_low_limit(Some(config.low_limit))
      .map_err(|_| EncoderError::InvalidLowLimit)?;
    unit
      .set_high_limit(Some(config.high_limit))
      .map_err(|_| EncoderError::InvalidHighLimit)?;

    // Configure filter
    if config.filter_threshold > 0 {
      unit
        .set_filter(Some(config.filter_threshold.min(1023)))
        .map_err(|_| EncoderError::InvalidFilterThreshold)?;
    } else {
      let _ = unit.set_filter(None);
    }

    // Clear counter
    unit.clear();

    // Get input signals
    let input_a = pin_a.peripheral_input();
    let input_b = pin_b.peripheral_input();

    // Configure channel 0: A as control signal, B as edge signal
    let ch0 = &unit.channel0;
    ch0.set_ctrl_signal(input_a.clone());
    ch0.set_edge_signal(input_b.clone());
    ch0.set_ctrl_mode(channel::CtrlMode::Reverse, channel::CtrlMode::Keep);
    ch0.set_input_mode(channel::EdgeMode::Increment, channel::EdgeMode::Decrement);

    // Configure channel 1: B as control signal, A as edge signal
    let ch1 = &unit.channel1;
    ch1.set_ctrl_signal(input_b);
    ch1.set_edge_signal(input_a);
    ch1.set_ctrl_mode(channel::CtrlMode::Reverse, channel::CtrlMode::Keep);
    ch1.set_input_mode(channel::EdgeMode::Decrement, channel::EdgeMode::Increment);

    // Enable interrupt and resume counting
    unit.listen();
    unit.resume();

    // Reset overflow accumulator
    let overflow = overflow_atomic(UNIT);
    overflow.store(0, Ordering::SeqCst);

    Ok(Self {
      unit,
      button: pin_key,
      overflow_value: overflow,
      last_value: 0,
      config,
    })
  }

  /// Get current absolute counter value
  ///
  /// Returns the accumulated count since creation or last reset.
  /// Positive values indicate clockwise rotation, negative values indicate counter-clockwise rotation.
  pub fn value(&self) -> i32 {
    self.unit.counter.get() as i32 + self.overflow_value.load(Ordering::SeqCst)
  }

  /// Get delta value since last call
  ///
  /// Returns the count change since last call to `delta()`.
  /// Positive values indicate clockwise rotation, negative values indicate counter-clockwise rotation.
  pub fn delta(&mut self) -> i32 {
    let current = self.value();
    let delta = current - self.last_value;
    self.last_value = current;
    delta
  }

  /// Get rotation direction
  ///
  /// Determines rotation direction based on current delta.
  pub fn direction(&mut self) -> Direction {
    let d = self.delta();
    if d > 0 {
      Direction::Clockwise
    } else if d < 0 {
      Direction::CounterClockwise
    } else {
      Direction::None
    }
  }

  /// Reset counter to zero
  pub fn reset(&mut self) {
    self.unit.clear();
    self.overflow_value.store(0, Ordering::SeqCst);
    self.last_value = 0;
  }

  /// Check if button is pressed (active low)
  pub fn is_button_pressed(&self) -> bool {
    self.button.is_low()
  }

  /// Asynchronously wait for button press event (with debouncing)
  ///
  /// Blocks until button is pressed. Internal debounce logic is implemented.
  pub async fn wait_for_button_press(&mut self) {
    self.button.wait_for_falling_edge().await;
    // Debounce delay
    embassy_time::Timer::after(embassy_time::Duration::from_millis(DEBOUNCE_MS)).await;
  }

  /// Asynchronously wait for button release event (with debouncing)
  ///
  /// Blocks until button is released. Internal debounce logic is implemented.
  pub async fn wait_for_button_release(&mut self) {
    self.button.wait_for_rising_edge().await;
    // Debounce delay
    embassy_time::Timer::after(embassy_time::Duration::from_millis(DEBOUNCE_MS)).await;
  }

  /// Asynchronously wait for button click (press followed by release)
  ///
  /// Blocks until a complete button click action is performed.
  pub async fn wait_for_click(&mut self) {
    self.wait_for_button_press().await;
    self.wait_for_button_release().await;
  }

  /// Handle PCNT interrupt
  ///
  /// This method should be called in the PCNT interrupt handler to process counter overflow.
  /// When the counter reaches high/low limit, hardware automatically resets the counter,
  /// this method accumulates the overflow value to maintain continuous counting.
  pub fn handle_interrupt(&self) {
    if self.unit.interrupt_is_set() {
      let events = self.unit.events();
      if events.high_limit {
        self
          .overflow_value
          .fetch_add(self.config.high_limit as i32, Ordering::SeqCst);
      } else if events.low_limit {
        self
          .overflow_value
          .fetch_add(self.config.low_limit as i32, Ordering::SeqCst);
      }
      self.unit.reset_interrupt();
    }
  }

  /// Get encoder configuration
  pub fn config(&self) -> &EncoderConfig {
    &self.config
  }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Initialize PCNT peripheral and set up interrupt handling
///
/// This is a convenience function to initialize PCNT and register the interrupt handler.
/// Returns the initialized `Pcnt` instance.
///
/// # Arguments
/// - `pcnt_peripheral`: PCNT peripheral instance
/// - `handler`: Interrupt handler function
pub fn init_pcnt_with_interrupt<'d>(
  pcnt_peripheral: esp_hal::peripherals::PCNT<'d>,
  handler: esp_hal::interrupt::InterruptHandler,
) -> Pcnt<'d> {
  let mut pcnt = Pcnt::new(pcnt_peripheral);
  pcnt.set_interrupt_handler(handler);
  pcnt
}

/// Create input pin configuration with pull-up
///
/// Signal pins of rotary encoder typically require pull-up resistors.
pub fn encoder_input_config() -> InputConfig {
  InputConfig::default().with_pull(Pull::Up)
}

/// Common processing logic for PCNT interrupt handling helper
///
/// Called in interrupt handler to process overflow events for the specified PCNT unit.
///
/// # Arguments
/// - `unit_num`: PCNT unit number
/// - `high_limit`: High limit value
/// - `low_limit`: Low limit value
pub fn handle_pcnt_overflow(unit_num: usize, high_limit: i16, low_limit: i16) {
  use esp_hal::peripherals::PCNT;

  let pcnt = PCNT::regs();
  let int_raw = pcnt.int_raw().read();

  if int_raw.cnt_thr_event_u(unit_num as u8).bit() {
    let status = pcnt.u_status(unit_num).read();
    let overflow = overflow_atomic(unit_num);

    if status.h_lim().bit() {
      overflow.fetch_add(high_limit as i32, Ordering::SeqCst);
    } else if status.l_lim().bit() {
      overflow.fetch_add(low_limit as i32, Ordering::SeqCst);
    }

    // Clear interrupt
    pcnt
      .int_clr()
      .write(|w| w.cnt_thr_event_u(unit_num as u8).set_bit());
  }
}
