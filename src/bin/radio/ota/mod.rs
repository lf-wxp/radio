//! Over-the-air update support.
//!
//! This module groups the OTA pipeline:
//!
//! - [`writer`] — chunked NOR-flash writer that streams an image into the
//!   inactive OTA slot and activates it on success.
//!
//! HTTP downloader, integrity verification and the web-console UI are added
//! in subsequent milestones (see `docs/ota-design.md` § Roadmap).

pub mod writer;

#[expect(
  unused_imports,
  reason = "Re-exported for #11-3 (HTTP downloader) which will land in a follow-up commit; \
    keeps the public surface visible from `crate::ota` once wired in."
)]
pub use writer::{OtaError, OtaWriter};
