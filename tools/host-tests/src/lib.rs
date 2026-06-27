//! Host-side unit tests for pure-logic modules of the `radio` binary.
//!
//! ## Why this crate exists
//!
//! The main `radio` binary is locked to `riscv32imac-unknown-none-elf`
//! (`.cargo/config.toml`), so `cargo test` on the host can't even
//! *compile* it — `esp-hal`, `embassy-net` and friends are no_std and
//! target-specific.
//!
//! This crate is a **standalone host workspace** (note the empty
//! `[workspace]` trick in `Cargo.toml`) that mirrors a small set of
//! pure-logic functions from the binary so they can run under
//! `cargo test` on the developer's host. It has zero firmware
//! dependencies — only `core::str` parsing and primitive types.
//!
//! ## What's mirrored here
//!
//! | Module | Source of truth | What's tested |
//! | --- | --- | --- |
//! | [`mdns_parser`] | `src/bin/radio/mdns.rs` | mDNS query decode + response packet layout |
//! | [`sntp`] | `src/bin/radio/clock/sntp.rs` | SNTPv4 packet encode + reply validation |
//! | [`url_parser`] | `src/bin/radio/ota/http_download.rs` | `http://<ipv4>[:port]/path` parser |
//! | [`text`] | `src/bin/radio/listening_log.rs` | UTF-8-safe slice clipping |
//! | [`rt_plus_parser`] | `src/si4703/mod.rs` | RT+ (RadioText Plus) bit-field parser |
//!
//! ## Sync discipline
//!
//! These copies are **manual mirrors**, not the originals. When you
//! change one of the source modules:
//!
//! 1. Mirror the change here.
//! 2. Run `cargo make host-test` (which runs `cargo test` in this crate).
//! 3. Commit both files together.
//!
//! The mirrored functions are deliberately small and the protocols
//! they implement (mDNS RFC 6762, IPv4 dotted-quad, UTF-8) don't
//! evolve, so drift is unlikely. We accept the duplication in
//! exchange for a binary that stays purely no_std.

#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]

pub mod mdns_parser;
pub mod rt_plus_parser;
pub mod sntp;
pub mod text;
pub mod url_parser;
