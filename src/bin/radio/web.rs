//! LAN web console — single-page HTML UI + minimal JSON API.
//!
//! Architecture:
//!
//! - One `embassy` task ([`web_task`]) owns the listening socket and
//!   serves at most one connection at a time (picoserve's design).
//! - Read-side endpoints (`GET /api/state`) snapshot
//!   [`crate::state::RADIO_STATE`] and serialise it directly.
//! - Write-side endpoints (`POST /api/tune`, etc.) push a
//!   [`RadioCommand`] into the existing [`INPUT_CMDS`] channel, so the
//!   web client and the rotary encoder share the exact same command
//!   path inside [`crate::tasks::radio_control_task`]. This keeps the
//!   chip's I2C ownership safely single-threaded without any new
//!   synchronisation primitives.
//!
//! ## Security
//!
//! There is **no authentication**. The console is intended for trusted
//! home networks only — do not expose port 80 to the public internet.
//! `picoserve`'s own README warns against direct exposure as well.

use embassy_net::Stack;
use picoserve::extract::Json;
use picoserve::response::StatusCode;
use picoserve::routing::{get, post};
use picoserve::{AppBuilder, AppRouter, Router};
use serde::{Deserialize, Serialize};

use crate::diagnostics::{self, HealthDto};
use crate::state::{INPUT_CMDS, PRESET_EMPTY, RADIO_STATE, RadioCommand, publish_web_ip};

// ============================================================================
// Configuration
// ============================================================================

/// TCP listening port for the web console.
const WEB_PORT: u16 = 80;

/// Per-connection HTTP framing buffer.
///
/// Sized for our largest expected payload (the embedded HTML page,
/// served chunk-by-chunk) plus headers. 2 KiB is plenty.
const HTTP_BUFFER_SIZE: usize = 2048;

/// Per-connection TCP rx/tx buffer sizes.
///
/// 1 KiB rx is enough for a single short request. tx is bumped to 4 KiB
/// because `GET /api/log` returns up to [`crate::listening_log::LOG_CAPACITY`]
/// entries, each ~110 B of JSON, which can total ~7 KiB. picoserve
/// streams the response in chunks but the underlying smoltcp tx ring
/// must be at least one MSS to avoid stalling.
const TCP_RX_BUFFER_SIZE: usize = 1024;
const TCP_TX_BUFFER_SIZE: usize = 4096;

// ============================================================================
// JSON DTOs
// ============================================================================

/// Owned snapshot returned by `GET /api/state`.
///
/// All numeric fields use the same units as the on-device state
/// (`freq_x10` in 0.1 MHz, RSSI in 0..=75, volume in 0..=15) so the
/// browser-side JS doesn't need any unit conversion. Owned strings
/// (rather than borrowed `&str`) so we can release the
/// [`RADIO_STATE`] mutex before serialisation.
#[derive(Serialize)]
struct StateDto {
  freq_x10: u16,
  rssi: u8,
  volume: u8,
  muted: bool,
  stereo: bool,
  auto_mono: bool,
  station_name: alloc::string::String,
  radio_text: alloc::string::String,
  pty_label: Option<&'static str>,
  /// Decoded RDS clock as `"HH:MM"`, empty when no CT received yet.
  clock: alloc::string::String,
  af_count: u8,
  af_following: bool,
  preset_idx: Option<u8>,
  /// Saved preset frequencies in MHz × 10. `0` for empty slots.
  presets: [u16; crate::state::MAX_PRESETS],
  wifi_ssid: alloc::string::String,
  wifi_connected: bool,
}

/// One row in the `GET /api/log` response.
///
/// Mirrors [`crate::listening_log::LogEntry`] but with owned strings
/// so the listening-log mutex is released before serialisation, and
/// with the field names the front-end JS expects.
#[derive(Serialize)]
struct LogEntryDto {
  /// Boot-relative seconds; the front-end converts this to `mm:ss ago`.
  uptime_secs: u32,
  freq_x10: u16,
  rssi: u8,
  ps: alloc::string::String,
  rt: alloc::string::String,
}

/// Wrapper for `GET /api/log`. Wrapping the array in an object keeps
/// us free to add more top-level fields (capacity, head index, etc.)
/// without breaking the JSON contract with the browser.
#[derive(Serialize)]
struct LogDto {
  capacity: u16,
  entries: alloc::vec::Vec<LogEntryDto>,
}
/// Body for `POST /api/tune`.
#[derive(Deserialize)]
struct TuneBody {
  /// Target frequency in 0.1 MHz units (e.g. `1015` = 101.5 MHz).
  freq_x10: u16,
}

// ============================================================================
// Embedded HTML
// ============================================================================

/// Single-page console served at `GET /`.
///
/// Phone-friendly: 16 px base font, 44 px-tall buttons, no external
/// network requests (CSS + JS are inline). Polls `/api/state` once per
/// second and posts compact JSON for every action so the radio task
/// never sees a flood of commands.
const INDEX_HTML: &str = include_str!("web/index.html");

// ============================================================================
// Router
// ============================================================================

pub struct AppProps;

impl AppBuilder for AppProps {
  type PathRouter = impl picoserve::routing::PathRouter;

  fn build_app(self) -> Router<Self::PathRouter> {
    Router::new()
      .route(
        "/",
        get(|| async {
          // `picoserve::response::File::html` would be slightly nicer
          // but constructing it requires an extra import; a tuple of
          // (Content-Type header, body) is the idiomatic shortcut.
          ([("content-type", "text/html; charset=utf-8")], INDEX_HTML)
        }),
      )
      .route("/api/state", get(handle_get_state))
      .route("/api/log", get(handle_get_log))
      .route("/api/tune", post(handle_post_tune))
      .route("/api/tune/up", post(handle_post_tune_up))
      .route("/api/tune/down", post(handle_post_tune_down))
      .route("/api/preset/cycle", post(handle_post_preset_cycle))
      .route("/api/preset/save", post(handle_post_preset_save))
      .route("/api/mute", post(handle_post_mute))
      .route("/api/health", get(handle_get_health))
  }
}

// ============================================================================
// Handlers
// ============================================================================

/// `GET /api/state` \u2014 return a JSON snapshot of [`RADIO_STATE`].
///
/// Allocates two transient `String`s (PS / RT decoded text) per call;
/// at the polling cadence the browser uses (1 Hz) this is well under
/// what `esp-alloc` can sustain.
async fn handle_get_state() -> picoserve::response::Json<StateDto> {
  let state = RADIO_STATE.lock().await;
  let dto = StateDto {
    freq_x10: state.freq_mhz_x10,
    rssi: state.rssi,
    volume: state.volume,
    muted: state.muted,
    stereo: state.stereo,
    auto_mono: state.auto_mono,
    station_name: state.station_name.clone(),
    radio_text: state.radio_text.clone(),
    pty_label: state.pty_label,
    clock: match state.clock_hh_mm {
      Some((h, m)) => alloc::format!("{:02}:{:02}", h, m),
      None => alloc::string::String::new(),
    },
    af_count: state.af_count,
    af_following: state.af_following,
    preset_idx: state.preset_idx,
    presets: state.presets.freqs,
    wifi_ssid: state.wifi_ssid.clone(),
    wifi_connected: state.wifi_connected,
  };
  picoserve::response::Json(dto)
}

/// `GET /api/log` — return the full listening-log ring buffer in
/// chronological order (oldest first).
///
/// The browser reverses the array client-side so the most recent
/// entry sits at the top of the panel; doing the reverse here would
/// just waste cycles.
async fn handle_get_log() -> picoserve::response::Json<LogDto> {
  let log = crate::listening_log::LISTENING_LOG.lock().await;
  let entries: alloc::vec::Vec<LogEntryDto> = log
    .iter_chronological()
    .map(|e| LogEntryDto {
      uptime_secs: e.uptime_secs,
      freq_x10: e.freq_x10,
      rssi: e.rssi,
      ps: alloc::string::String::from(e.ps_str()),
      rt: alloc::string::String::from(e.rt_str()),
    })
    .collect();
  picoserve::response::Json(LogDto {
    capacity: crate::listening_log::LOG_CAPACITY as u16,
    entries,
  })
}

/// `POST /api/tune` \u2014 jump to an exact frequency.
///
/// Body: `{ "freq_x10": 1015 }`.
/// Returns `400 Bad Request` when the frequency is outside the FM band;
/// otherwise enqueues a [`RadioCommand::TuneAbsolute`] and returns
/// `204 No Content` (empty body).
async fn handle_post_tune(Json(body): Json<TuneBody>) -> Result<(), StatusCode> {
  // Mirror the FM band bounds enforced by `clamp_freq`. Reject obviously
  // bogus inputs early so the dial never even attempts the I2C tune.
  if !(875..=1080).contains(&body.freq_x10) {
    return Err(StatusCode::BAD_REQUEST);
  }
  send_command(RadioCommand::TuneAbsolute(body.freq_x10)).await;
  Ok(())
}

/// `POST /api/tune/up` \u2014 nudge +0.1 MHz.
async fn handle_post_tune_up() {
  send_command(RadioCommand::TuneRelative(1)).await;
}

/// `POST /api/tune/down` \u2014 nudge \u22120.1 MHz.
async fn handle_post_tune_down() {
  send_command(RadioCommand::TuneRelative(-1)).await;
}

/// `POST /api/preset/cycle` \u2014 jump to the next saved preset.
async fn handle_post_preset_cycle() {
  send_command(RadioCommand::CyclePreset).await;
}

/// `POST /api/preset/save` \u2014 persist the current frequency.
///
/// No-op when the current frequency is already saved. The radio task
/// FIFO-evicts the oldest slot once all eight are full.
async fn handle_post_preset_save() -> Result<(), StatusCode> {
  // Reject the save when no station is dialled in yet: the boot-time
  // placeholder is `PRESET_EMPTY` and saving that would just clutter
  // the table.
  let cur = RADIO_STATE.lock().await.freq_mhz_x10;
  if cur == PRESET_EMPTY {
    return Err(StatusCode::CONFLICT);
  }
  send_command(RadioCommand::SavePreset).await;
  Ok(())
}

/// `POST /api/mute` \u2014 toggle mute.
async fn handle_post_mute() {
  send_command(RadioCommand::ToggleMute).await;
}

// ============================================================================
// Helpers
// ============================================================================

/// `GET /api/health` — return a JSON health snapshot for remote diagnostics.
///
/// Includes uptime, heap usage, I²C error count, WiFi status, RSSI, and
/// the POST result. Designed for monitoring dashboards and quick
/// troubleshooting without physical access to the device.
async fn handle_get_health() -> picoserve::response::Json<HealthDto> {
  let post = diagnostics::get_post_result();
  let dto = if let Some(post_ref) = post {
    HealthDto::capture(post_ref).await
  } else {
    // POST hasn't completed yet (shouldn't happen in practice since
    // the web task starts after POST, but handle gracefully).
    let free = diagnostics::heap_free_bytes();
    let total = diagnostics::heap_total_bytes();
    let usage_pct = if total > 0 {
      ((total - free) * 100 / total) as u8
    } else {
      0
    };
    HealthDto {
      uptime_secs: diagnostics::uptime_secs(),
      heap_free: free,
      heap_total: total,
      heap_usage_pct: usage_pct,
      i2c_errors: diagnostics::i2c_error_total(),
      wifi_connected: false,
      rssi: 0,
      tuner_ok: false,
      post_status: "pending",
      radio_task_alive: diagnostics::watchdog_ok(),
      watchdog_elapsed_secs: diagnostics::watchdog_elapsed_secs(),
    }
  };
  picoserve::response::Json(dto)
}

/// Push a command through the shared input channel.
///
/// Blocks (asynchronously) when the channel is full so the radio task
/// always processes commands in arrival order. Channel capacity (8) is
/// large enough that this branch only triggers under pathological
/// flooding from a buggy client.
async fn send_command(cmd: RadioCommand) {
  INPUT_CMDS.send(cmd).await;
}

// ============================================================================
// Embassy task
// ============================================================================

/// Long-running task that listens for incoming HTTP connections.
///
/// Runs forever; on every connection it serves a single request/response
/// cycle (keep-alive intentionally off to free the socket for the next
/// client quickly). `picoserve`'s `listen_and_serve` re-creates the
/// `TcpSocket` between connections so a misbehaving client cannot wedge
/// the server.
#[embassy_executor::task]
#[allow(
  clippy::large_stack_frames,
  reason = "the task owns its 2 KiB HTTP framing buffer and 1 KiB × 2 TCP \
            buffers on its own stack so picoserve can re-create the \
            socket between connections without re-allocating heap. \
            ~4.4 KiB still fits comfortably inside the 16 KiB \
            Embassy task stack on ESP32-C6."
)]
pub async fn web_task(
  stack: Stack<'static>,
  app: &'static AppRouter<AppProps>,
  config: &'static picoserve::Config,
) -> ! {
  // Wait until DHCP has handed us an IPv4 address — picoserve will
  // happily start listening before then but the LCD's IP badge would
  // remain blank, confusing the user. Republish on every (re)bind so a
  // lease change is reflected.
  stack.wait_config_up().await;
  if let Some(cfg) = stack.config_v4() {
    publish_web_ip(Some(cfg.address.address().octets())).await;
  }

  let mut http_buffer = [0u8; HTTP_BUFFER_SIZE];
  let mut tcp_rx = [0u8; TCP_RX_BUFFER_SIZE];
  let mut tcp_tx = [0u8; TCP_TX_BUFFER_SIZE];

  loop {
    picoserve::Server::new(app, config, &mut http_buffer)
      .listen_and_serve(0u8, stack, WEB_PORT, &mut tcp_rx, &mut tcp_tx)
      .await;
  }
}
