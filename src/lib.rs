//! # ESP-Radio Library
//!
//! Reusable hardware drivers and functional modules for the ESP32/C6 FM radio project.
//!
//! ## Modules
//!
//! - [`display`] — ST7789 LCD display driver + Slint platform integration
//! - [`rotary_encoder`] — KY-040 rotary encoder driver (based on PCNT hardware peripheral)
//! - [`si4703`] — Si4703 FM receiver chip I2C driver
//! - [`wifi_provision`] — SoftAP portal WiFi provisioning + Flash credential persistence

#![no_std]

pub mod display;
pub mod rotary_encoder;
pub mod si4703;
pub mod wifi_provision;
