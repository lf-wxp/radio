//! Over-the-air update support.
//!
//! This module groups the OTA pipeline:
//!
//! - [`writer`] — chunked NOR-flash writer that streams an image into the
//!   inactive OTA slot and activates it on success.
//! - [`http_download`] — plain-HTTP downloader that feeds the writer.
//! - [`run_job`] — top-level state-machine driver invoked by the radio
//!   control task when an `OtaCommand::Start` arrives.
//! - [`mark_current_app_valid`] — anti-rollback latch flipped on a
//!   successful boot so the bootloader doesn't revert on the next reset.

pub mod http_download;
pub mod writer;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;

use defmt::{info, warn};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN;
use esp_storage::FlashStorage;

use crate::state::{OtaProgress, publish_ota_in_progress, publish_ota_progress};

pub use writer::{OtaError, OtaWriter};

/// Mark the running app image as `Valid`, defeating bootloader rollback.
///
/// Call once per successful boot, after the critical subsystems (WiFi,
/// display, tuner POST) have come up. The bootloader records this in
/// the OTA-data sector, so the next reboot won't roll back to the
/// previous slot even if the user power-cycles immediately.
///
/// On chips without `bootloader_app_rollback` enabled this is a no-op
/// in practice (the bootloader simply ignores the state field). Failing
/// the call is non-fatal — we log and continue so a corrupt OTA-data
/// sector doesn't brick the device on every boot.
pub fn mark_current_app_valid(flash: &mut FlashStorage<'_>) {
  // 3 KiB partition-table buffer on the heap to keep the stack frame
  // small (the surrounding `main` already lives close to its
  // `clippy::large_stack_frames` budget).
  let mut buf: Box<[u8]> = vec![0u8; PARTITION_TABLE_MAX_LEN].into_boxed_slice();
  // Length is set by `vec![0; PARTITION_TABLE_MAX_LEN]` so the
  // conversion is infallible. One deref unwraps the `Box`; the
  // resulting `&mut [u8]` is what `TryFrom<&mut [u8; N]>` is impl'd
  // for.
  let pt_arr: &mut [u8; PARTITION_TABLE_MAX_LEN] = (&mut *buf)
    .try_into()
    .expect("buf length matches PARTITION_TABLE_MAX_LEN");

  match OtaUpdater::new(flash, pt_arr) {
    Ok(mut updater) => {
      if let Err(e) = updater.set_current_ota_state(OtaImageState::Valid) {
        warn!("OTA: mark_current_app_valid failed: {:?}", e);
      } else {
        info!("OTA: current image committed (rollback disarmed)");
      }
    }
    Err(e) => {
      // Most likely cause: partition table doesn't include
      // `ota_0`/`ota_1`/`otadata`. Devices flashed with the
      // factory-only layout boot fine without OTA, so this is a
      // diagnostic, not a fatal.
      warn!("OTA: cannot open OtaUpdater for commit: {:?}", e);
    }
  }
}

/// Run one full OTA job against the supplied URL.
///
/// Sequence:
///
/// 1. Publish [`OtaProgress::Connecting`] and flag `ota_in_progress`.
/// 2. Allocate an [`OtaWriter`] (locates inactive slot, header buffer).
/// 3. Stream the body via [`http_download::download_to_writer`].
/// 4. Either [`OtaWriter::finalize`] on success or [`OtaWriter::abort`]
///    on any error.
/// 5. Publish a terminal [`OtaProgress::Success`] / [`OtaProgress::Failed`]
///    and return the released [`FlashStorage`] handle to the caller so
///    the preset store can resume.
///
/// The flash handle is **always** returned, even on failure; the caller
/// must `resume()` the preset store immediately or risk leaking the
/// singleton flash peripheral.
pub async fn run_job(
  stack: embassy_net::Stack<'static>,
  flash: FlashStorage<'static>,
  url: String,
) -> FlashStorage<'static> {
  publish_ota_in_progress(true).await;
  publish_ota_progress(OtaProgress::Connecting).await;

  // Phase 1: open the writer (resolves inactive slot, allocates 4 KiB
  // sector buffer). Failure here is rare (only happens on a corrupt
  // partition table) but we still need to surface it.
  let writer = match OtaWriter::begin(flash, None) {
    Ok(w) => w,
    Err((e, flash)) => {
      warn!("OTA begin failed: {:?}", e);
      publish_ota_progress(OtaProgress::Failed("init")).await;
      publish_ota_in_progress(false).await;
      return flash;
    }
  };

  // Phase 2: download. Take ownership of the writer for the duration
  // so any error path can `abort()` it cleanly.
  let result = run_download(stack, &url, writer).await;
  publish_ota_in_progress(false).await;
  result
}

/// Inner helper that owns the writer for the duration of one download
/// + finalize attempt. Splitting it out keeps `run_job` flat and lets
///   us use `?` ergonomics on the writer + downloader paths.
async fn run_download(
  stack: embassy_net::Stack<'static>,
  url: &str,
  mut writer: OtaWriter<'static>,
) -> FlashStorage<'static> {
  // Stream the response body into the writer.
  if let Err(e) = http_download::download_to_writer(stack, url, &mut writer).await {
    warn!("OTA download failed: {:?}", e);
    let reason = http_failure_reason(&e);
    publish_ota_progress(OtaProgress::Failed(reason)).await;
    return writer.abort();
  }

  // Phase 3: finalize (pads tail, flips OTA-data).
  publish_ota_progress(OtaProgress::Activating).await;
  match writer.finalize() {
    Ok(flash) => {
      info!("OTA success: image staged; reboot to run new firmware");
      publish_ota_progress(OtaProgress::Success).await;
      flash
    }
    Err((e, flash)) => {
      warn!("OTA finalize failed: {:?}", e);
      publish_ota_progress(OtaProgress::Failed("activate")).await;
      flash
    }
  }
}

/// Map a download-side error to a short, UI-friendly reason string.
#[must_use]
fn http_failure_reason(e: &http_download::HttpError) -> &'static str {
  use http_download::HttpError;
  match e {
    HttpError::BadUrl => "bad url",
    HttpError::ConnectFailed => "connect",
    HttpError::Io => "io",
    HttpError::BadStatus(_) => "http status",
    HttpError::HeadersTooLarge => "headers",
    HttpError::Truncated => "truncated",
    HttpError::Writer(OtaError::BadImageHeader { .. }) => "bad image",
    HttpError::Writer(OtaError::ImageTooLarge { .. }) => "too large",
    HttpError::Writer(OtaError::SizeMismatch { .. }) => "size",
    HttpError::Writer(OtaError::SlotNotFound) => "no slot",
    HttpError::Writer(OtaError::Flash(_)) => "flash",
    HttpError::Writer(OtaError::Partition(_)) => "partition",
  }
}
